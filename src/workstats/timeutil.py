from __future__ import annotations

import re
import heapq
import os
from calendar import monthrange
from datetime import datetime, timedelta, timezone
from typing import Iterable
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from .model import ActivityPoint, HumanSignal, Interval, Session


UTC = timezone.utc
_DURATION = re.compile(r"^(\d+(?:\.\d+)?)(s|m|h)$", re.IGNORECASE)


def local_timezone():
    configured = os.environ.get("TZ")
    candidates = [configured] if configured else []
    try:
        resolved = str(Path("/etc/localtime").resolve())
        marker = "/zoneinfo/"
        if marker in resolved:
            candidates.append(resolved.split(marker, 1)[1])
    except OSError:
        pass
    for candidate in candidates:
        try:
            return ZoneInfo(candidate)
        except (ZoneInfoNotFoundError, ValueError):
            continue
    return datetime.now().astimezone().tzinfo or UTC


def parse_timestamp(value: object) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed.astimezone(UTC)


def parse_epoch_milliseconds(value: object) -> datetime | None:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    try:
        return datetime.fromtimestamp(number / 1000.0, tz=UTC)
    except (OverflowError, OSError, ValueError):
        return None


def parse_duration(value: str) -> timedelta:
    match = _DURATION.fullmatch(value.strip())
    if not match:
        raise ValueError("duration must look like 30s, 5m, or 1h")
    amount = float(match.group(1))
    if amount <= 0:
        raise ValueError("duration must be greater than zero")
    factor = {"s": 1, "m": 60, "h": 3600}[match.group(2).lower()]
    return timedelta(seconds=amount * factor)


def parse_bound(value: str | None, *, until: bool, local_tz=None) -> datetime | None:
    if not value:
        return None
    tz = local_tz or local_timezone()
    if re.fullmatch(r"\d{4}-\d{2}", value):
        year, month = (int(part) for part in value.split("-"))
        start = datetime(year, month, 1, tzinfo=tz)
        if not until:
            return start.astimezone(UTC)
        next_month = datetime(year + (month == 12), 1 if month == 12 else month + 1, 1, tzinfo=tz)
        return next_month.astimezone(UTC)
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        start = datetime.strptime(value, "%Y-%m-%d").replace(tzinfo=tz)
        return (start + timedelta(days=1) if until else start).astimezone(UTC)
    raise ValueError("date must be YYYY-MM or YYYY-MM-DD")


def nearest_models(points: list[ActivityPoint]) -> list[ActivityPoint]:
    if not points:
        return []
    ordered = sorted(points, key=lambda point: point.timestamp)
    next_known = "unknown"
    future: list[str] = ["unknown"] * len(ordered)
    for index in range(len(ordered) - 1, -1, -1):
        if ordered[index].model != "unknown":
            next_known = ordered[index].model
        future[index] = next_known
    current = "unknown"
    result: list[ActivityPoint] = []
    for index, point in enumerate(ordered):
        if point.model != "unknown":
            current = point.model
        model = current if current != "unknown" else future[index]
        result.append(ActivityPoint(point.timestamp, model))
    return result


def merge_ranges(ranges: Iterable[tuple[datetime, datetime]]) -> list[tuple[datetime, datetime]]:
    ordered = sorted((start, end) for start, end in ranges if end > start)
    if not ordered:
        return []
    merged = [ordered[0]]
    for start, end in ordered[1:]:
        previous_start, previous_end = merged[-1]
        if start <= previous_end:
            merged[-1] = (previous_start, max(previous_end, end))
        else:
            merged.append((start, end))
    return merged


def build_session_intervals(session: Session, gap_cap: timedelta) -> list[Interval]:
    points = nearest_models(session.points)
    ranges: list[tuple[datetime, datetime, str]] = []
    for current, following in zip(points, points[1:]):
        if following.timestamp <= current.timestamp:
            continue
        ranges.append((current.timestamp, min(following.timestamp, current.timestamp + gap_cap), current.model))
    ranges.extend(session.exact_intervals)

    # Sweep the ranges instead of checking every range at every boundary. Large
    # Codex transcripts contain tens of thousands of events; the quadratic
    # formulation is both slow and memory hungry on real retained histories.
    events: dict[datetime, list[tuple[bool, int]]] = {}
    indexed = list(enumerate(ranges))
    for index, (start, end, _) in indexed:
        if end <= start:
            continue
        events.setdefault(start, []).append((True, index))
        events.setdefault(end, []).append((False, index))
    active: set[int] = set()
    heap: list[tuple[int, float, int]] = []
    result: list[Interval] = []
    previous: datetime | None = None
    for moment in sorted(events):
        while heap and heap[0][2] not in active:
            heapq.heappop(heap)
        if previous is not None and moment > previous and heap:
            model = ranges[heap[0][2]][2]
            interval = Interval(previous, moment, session.provider, model, session.session_id,
                                session.cwd, session.repo, session.root, session.approximate_cwd)
            if result and result[-1].end == interval.start and result[-1].model == interval.model:
                prior = result[-1]
                result[-1] = Interval(prior.start, interval.end, prior.provider, prior.model,
                                      prior.session_id, prior.cwd, prior.repo, prior.root,
                                      prior.approximate_cwd)
            else:
                result.append(interval)
        for starting, index in events[moment]:
            if not starting:
                active.discard(index)
        for starting, index in events[moment]:
            if starting:
                active.add(index)
                range_start, _, model = ranges[index]
                heapq.heappush(heap, (0 if model != "unknown" else 1, -range_start.timestamp(), index))
        previous = moment
    return result


def build_human_intervals(
    signals: list[HumanSignal],
    idle_threshold: timedelta,
    isolated_credit: timedelta,
) -> list[Interval]:
    """Estimate non-overlapping human work blocks from foreground evidence.

    Signals within the idle threshold form one block. Time between signals is
    divided at their midpoint, preserving one global human timeline while still
    attributing each slice to the nearest work area. Each block receives half of
    the isolated-signal credit at either edge.
    """
    if not signals:
        return []
    priority = {"claude_prompt": 3, "codex_prompt": 3, "commit": 1}
    by_timestamp: dict[datetime, HumanSignal] = {}
    for signal in signals:
        existing = by_timestamp.get(signal.timestamp)
        if existing is None or priority.get(signal.kind, 0) > priority.get(existing.kind, 0):
            by_timestamp[signal.timestamp] = signal
    ordered = [by_timestamp[key] for key in sorted(by_timestamp)]
    blocks: list[list[HumanSignal]] = []
    current: list[HumanSignal] = []
    for signal in ordered:
        if current and signal.timestamp - current[-1].timestamp > idle_threshold:
            blocks.append(current)
            current = []
        current.append(signal)
    if current:
        blocks.append(current)

    edge = isolated_credit / 2
    intervals: list[Interval] = []
    for block_index, block in enumerate(blocks):
        first_local = block[0].timestamp.astimezone(local_timezone())
        first_day = first_local.replace(hour=0, minute=0, second=0, microsecond=0).astimezone(UTC)
        last_local = block[-1].timestamp.astimezone(local_timezone())
        next_day = (last_local.replace(hour=0, minute=0, second=0, microsecond=0) +
                    timedelta(days=1)).astimezone(UTC)
        left = max(block[0].timestamp - edge, first_day)
        for index, signal in enumerate(block):
            if index + 1 < len(block):
                gap = block[index + 1].timestamp - signal.timestamp
                right = signal.timestamp + gap / 2
            else:
                right = min(signal.timestamp + edge, next_day)
            if right > left:
                intervals.append(Interval(
                    left, right, signal.provider, signal.model, f"work-block:{block_index}",
                    signal.cwd, signal.repo, signal.root,
                ))
            left = right
    return intervals


def clip_interval(interval: Interval, since: datetime | None, until: datetime | None) -> Interval | None:
    start = max(interval.start, since) if since else interval.start
    end = min(interval.end, until) if until else interval.end
    if end <= start:
        return None
    return Interval(start, end, interval.provider, interval.model, interval.session_id,
                    interval.cwd, interval.repo, interval.root, interval.approximate_cwd)


def union_seconds(intervals: Iterable[Interval]) -> float:
    return sum((end - start).total_seconds() for start, end in merge_ranges((item.start, item.end) for item in intervals))


def split_interval(interval: Interval, dimension: str, local_tz=None) -> list[tuple[str, Interval]]:
    tz = local_tz or local_timezone()
    if dimension not in {"day", "month"}:
        raise ValueError(f"unsupported calendar dimension: {dimension}")
    pieces: list[tuple[str, Interval]] = []
    cursor = interval.start
    while cursor < interval.end:
        local = cursor.astimezone(tz)
        if dimension == "day":
            boundary_local = local.replace(hour=0, minute=0, second=0, microsecond=0) + timedelta(days=1)
            key = local.strftime("%Y-%m-%d")
        else:
            year, month = local.year, local.month
            boundary_local = datetime(year + (month == 12), 1 if month == 12 else month + 1, 1, tzinfo=tz)
            key = local.strftime("%Y-%m")
        boundary = boundary_local.astimezone(UTC)
        end = min(interval.end, boundary)
        pieces.append((key, Interval(cursor, end, interval.provider, interval.model,
                                     interval.session_id, interval.cwd, interval.repo,
                                     interval.root, interval.approximate_cwd)))
        cursor = end
    return pieces


def calendar_days(first: datetime | None, last: datetime | None, local_tz=None) -> int:
    if not first or not last:
        return 0
    tz = local_tz or local_timezone()
    return max(1, (last.astimezone(tz).date() - first.astimezone(tz).date()).days + 1)


def month_days(key: str) -> int:
    year, month = (int(part) for part in key.split("-"))
    return monthrange(year, month)[1]
