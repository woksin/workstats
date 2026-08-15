from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timedelta
from typing import Iterable
import unicodedata

from .model import GitCommit, HumanSignal, Interval, Session
from .timeutil import build_human_intervals, build_session_intervals, calendar_days, clip_interval, local_timezone, split_interval, union_seconds


DIMENSIONS = {"repo", "root", "cwd", "provider", "model", "day", "month"}


def _safe_value(value: str) -> str:
    return "".join(character if not unicodedata.category(character).startswith("C") else "�"
                   for character in value)[:4096]


def _keys_for_interval(interval: Interval, dimensions: tuple[str, ...]) -> list[tuple[tuple[str, ...], Interval]]:
    calendar = next((name for name in dimensions if name in {"day", "month"}), None)
    pieces = split_interval(interval, calendar) if calendar else [(None, interval)]
    result = []
    for calendar_key, piece in pieces:
        values = {
            "repo": piece.repo, "root": piece.root, "cwd": piece.cwd,
            "provider": piece.provider, "model": piece.model,
            "day": calendar_key, "month": calendar_key,
        }
        result.append((tuple(_safe_value(str(values[name])) for name in dimensions), piece))
    return result


def _commit_key(commit: GitCommit, dimension: str) -> str:
    local = commit.timestamp.astimezone(local_timezone())
    value = {
        "repo": commit.repo, "root": commit.root, "cwd": commit.cwd,
        "provider": "git", "model": "—",
        "day": local.strftime("%Y-%m-%d"),
        "month": local.strftime("%Y-%m"),
    }[dimension]
    return _safe_value(value)


def build_report(
    sessions: list[Session],
    commits: list[GitCommit],
    gap_cap: timedelta,
    since: datetime | None,
    until: datetime | None,
    dimensions: tuple[str, ...],
    human_idle: timedelta = timedelta(minutes=15),
    isolated_credit: timedelta = timedelta(minutes=5),
) -> dict[str, object]:
    unknown = set(dimensions) - DIMENSIONS
    if unknown:
        raise ValueError(f"unknown grouping dimension(s): {', '.join(sorted(unknown))}")
    intervals = [
        clipped for session in sessions for interval in build_session_intervals(session, gap_cap)
        if (clipped := clip_interval(interval, since, until)) is not None
    ]
    filtered_commits = [commit for commit in commits if (not since or commit.timestamp >= since) and (not until or commit.timestamp < until)]
    human_signals = [
        HumanSignal(point.timestamp, session.provider, session.session_id, session.cwd,
                    session.repo, session.root, f"{session.provider}_prompt", point.model)
        for session in sessions if not session.is_subagent for point in session.human_points
    ]
    human_signals.extend(
        HumanSignal(commit.timestamp, "git", commit.sha, commit.cwd, commit.repo,
                    commit.root, "commit", "—")
        for commit in filtered_commits
    )
    filtered_human_signals = [
        signal for signal in human_signals
        if (not since or signal.timestamp >= since) and (not until or signal.timestamp < until)
    ]
    human_intervals = [
        clipped for interval in build_human_intervals(filtered_human_signals, human_idle, isolated_credit)
        if (clipped := clip_interval(interval, since, until)) is not None
    ]
    session_roles = {(session.provider, session.session_id): session.is_subagent for session in sessions}
    buckets: dict[tuple[str, ...], dict[str, object]] = {}

    def bucket(key: tuple[str, ...]) -> dict[str, object]:
        if key not in buckets:
            buckets[key] = {
                "key": dict(zip(dimensions, key)), "active_seconds": 0.0, "sessions": set(),
                "foreground_sessions": set(), "subagent_sessions": set(), "ai_intervals": [],
                "human_seconds": 0.0, "human_intervals": [], "human_blocks": set(),
                "human_signals": set(), "human_days": set(),
                "providers": set(), "models": set(), "commits": set(), "files": set(),
                "additions": 0, "deletions": 0, "ignored_additions": 0,
                "ignored_deletions": 0, "first_seen": None, "last_seen": None,
                "active_days": set(),
            }
        return buckets[key]

    for interval in intervals:
        for key, piece in _keys_for_interval(interval, dimensions):
            row = bucket(key)
            row["active_seconds"] += piece.seconds
            row["sessions"].add((piece.provider, piece.session_id))
            row["ai_intervals"].append(piece)
            role_key = (piece.provider, piece.session_id)
            if session_roles.get(role_key, False):
                row["subagent_sessions"].add(role_key)
            else:
                row["foreground_sessions"].add(role_key)
            row["providers"].add(piece.provider)
            row["models"].add(piece.model)
            row["active_days"].update(day for day, _ in split_interval(piece, "day"))
            row["first_seen"] = min(filter(None, (row["first_seen"], piece.start)), default=piece.start)
            row["last_seen"] = max(filter(None, (row["last_seen"], piece.end)), default=piece.end)

    for interval in human_intervals:
        for key, piece in _keys_for_interval(interval, dimensions):
            row = bucket(key)
            row["human_seconds"] += piece.seconds
            row["human_intervals"].append(piece)
            row["human_blocks"].add(piece.session_id)
            row["first_seen"] = min(filter(None, (row["first_seen"], piece.start)), default=piece.start)
            row["last_seen"] = max(filter(None, (row["last_seen"], piece.end)), default=piece.end)

    for signal in filtered_human_signals:
        local = signal.timestamp.astimezone(local_timezone())
        values = {
            "repo": signal.repo, "root": signal.root, "cwd": signal.cwd,
            "provider": signal.provider, "model": signal.model,
            "day": local.strftime("%Y-%m-%d"), "month": local.strftime("%Y-%m"),
        }
        row = bucket(tuple(_safe_value(values[name]) for name in dimensions))
        row["human_signals"].add((signal.timestamp, signal.kind, signal.session_id))
        row["human_days"].add(local.date().isoformat())

    active_session_keys = {(interval.provider, interval.session_id) for interval in intervals}
    eligible_session_keys = set(active_session_keys)
    single_point_dates: set[str] = set()
    for session in sessions:
        session_key = (session.provider, session.session_id)
        first = session.first_seen
        if first is None or (since and first < since) or (until and first >= until):
            continue
        eligible_session_keys.add(session_key)
        if session_key in active_session_keys:
            continue
        model = session.points[0].model if session.points else "unknown"
        local = first.astimezone(local_timezone())
        values = {
            "repo": session.repo, "root": session.root, "cwd": session.cwd,
            "provider": session.provider, "model": model,
            "day": local.strftime("%Y-%m-%d"), "month": local.strftime("%Y-%m"),
        }
        row = bucket(tuple(_safe_value(values[name]) for name in dimensions))
        row["sessions"].add(session_key)
        if session.is_subagent:
            row["subagent_sessions"].add(session_key)
        else:
            row["foreground_sessions"].add(session_key)
        row["providers"].add(session.provider)
        row["models"].add(model)
        day = local.date().isoformat()
        row["active_days"].add(day)
        single_point_dates.add(day)
        row["first_seen"] = min(filter(None, (row["first_seen"], first)), default=first)
        row["last_seen"] = max(filter(None, (row["last_seen"], first)), default=first)

    for commit in filtered_commits:
        key = tuple(_commit_key(commit, name) for name in dimensions)
        row = bucket(key)
        row["commits"].add(commit.sha)
        row["files"].update(commit.files)
        row["additions"] += commit.additions
        row["deletions"] += commit.deletions
        row["ignored_additions"] += commit.ignored_additions
        row["ignored_deletions"] += commit.ignored_deletions
        row["active_days"].add(commit.timestamp.astimezone(local_timezone()).date().isoformat())
        row["first_seen"] = min(filter(None, (row["first_seen"], commit.timestamp)), default=commit.timestamp)
        row["last_seen"] = max(filter(None, (row["last_seen"], commit.timestamp)), default=commit.timestamp)

    rows: list[dict[str, object]] = []
    for row in buckets.values():
        first, last = row["first_seen"], row["last_seen"]
        active_days = len(row["active_days"])
        output = {
            "key": row["key"], "active_seconds": round(row["active_seconds"], 3),
            "parallel_agent_seconds": round(row["active_seconds"], 3),
            "ai_wall_seconds": round(union_seconds(row["ai_intervals"]), 3),
            "human_estimated_seconds": round(row["human_seconds"], 3),
            "human_signal_count": len(row["human_signals"]),
            "work_block_count": len(row["human_blocks"]),
            "session_count": len(row["sessions"]), "commit_count": len(row["commits"]),
            "foreground_session_count": len(row["foreground_sessions"]),
            "subagent_session_count": len(row["subagent_sessions"]),
            "file_count": len(row["files"]), "additions": row["additions"], "deletions": row["deletions"],
            "ignored_additions": row["ignored_additions"], "ignored_deletions": row["ignored_deletions"],
            "net_lines": row["additions"] - row["deletions"], "active_days": active_days,
            "human_active_days": len(row["human_days"]),
            "calendar_days": calendar_days(first, last),
            "average_human_seconds_per_active_day": round(
                row["human_seconds"] / len(row["human_days"]), 3) if row["human_days"] else 0.0,
            "average_active_seconds_per_active_day": round(row["active_seconds"] / active_days, 3) if active_days else 0.0,
            "average_active_seconds_per_calendar_day": round(row["active_seconds"] / calendar_days(first, last), 3) if calendar_days(first, last) else 0.0,
            "first_seen": first.isoformat() if first else None, "last_seen": last.isoformat() if last else None,
            "providers": sorted(row["providers"]), "models": sorted(row["models"]),
        }
        rows.append(output)
    def row_sort_key(row: dict[str, object]) -> tuple[object, ...]:
        # Calendar reports should read like a timeline: newest period first.
        # Within a period, rank work areas by estimated human effort.
        calendar_key = str(row["key"].get("month") or row["key"].get("day") or "")
        return (
            calendar_key,
            row["human_estimated_seconds"],
            row["ai_wall_seconds"],
            row["commit_count"],
        )

    if "month" in dimensions or "day" in dimensions:
        rows.sort(key=row_sort_key, reverse=True)
    else:
        rows.sort(key=lambda row: (row["human_estimated_seconds"], row["ai_wall_seconds"], row["commit_count"]), reverse=True)

    all_times = (
        [item.start for item in intervals] + [item.end for item in intervals] +
        [item.start for item in human_intervals] + [item.end for item in human_intervals] +
        [item.timestamp for item in filtered_human_signals]
    )
    active_dates = {day for interval in intervals for day, _ in split_interval(interval, "day")}
    active_dates.update(single_point_dates)
    active_dates.update(commit.timestamp.astimezone(local_timezone()).date().isoformat() for commit in filtered_commits)
    human_dates = {
        signal.timestamp.astimezone(local_timezone()).date().isoformat()
        for signal in filtered_human_signals
    }
    provider_seconds: dict[str, float] = defaultdict(float)
    model_seconds: dict[str, float] = defaultdict(float)
    for interval in intervals:
        provider_seconds[interval.provider] += interval.seconds
        model_seconds[interval.model] += interval.seconds
    return {
        "methodology": {
            "human_work": "foreground human prompts and authored commits clustered into non-overlapping work blocks",
            "human_idle_threshold_seconds": human_idle.total_seconds(),
            "isolated_signal_credit_seconds": isolated_credit.total_seconds(),
            "human_estimate_caveat": "an evidence-based estimate, not stopwatch or attendance data",
            "ai_time": "consecutive transcript activity intervals capped at the idle gap; exact Codex task intervals are merged when available",
            "deduplication": "headline time is the union of all AI intervals; grouped AI totals may overlap across parallel repos/providers",
            "gap_cap_seconds": gap_cap.total_seconds(),
            "scope": "local retained transcripts and locally available Git repositories only",
        },
        "observed": {
            "first_seen": min(all_times).isoformat() if all_times else None,
            "last_seen": max(all_times).isoformat() if all_times else None,
        },
        "summary": {
            "human_estimated_seconds": round(sum(item.seconds for item in human_intervals), 3),
            "human_active_days": len(human_dates),
            "average_human_seconds_per_active_day": round(
                sum(item.seconds for item in human_intervals) / len(human_dates), 3) if human_dates else 0.0,
            "work_block_count": len({item.session_id for item in human_intervals}),
            "human_signal_count": len({(item.timestamp, item.kind, item.session_id) for item in filtered_human_signals}),
            "prompt_signal_count": len({
                (item.timestamp, item.kind, item.session_id)
                for item in filtered_human_signals if item.kind.endswith("_prompt")
            }),
            "commit_signal_count": len({
                (item.timestamp, item.kind, item.session_id)
                for item in filtered_human_signals if item.kind == "commit"
            }),
            "deduplicated_active_seconds": round(union_seconds(intervals), 3),
            "attributed_active_seconds": round(sum(item.seconds for item in intervals), 3),
            "agent_wall_seconds": round(union_seconds(intervals), 3),
            "parallel_agent_seconds": round(sum(item.seconds for item in intervals), 3),
            "session_count": len(eligible_session_keys),
            "foreground_session_count": len({
                key for key in eligible_session_keys if not session_roles.get(key, False)
            }),
            "subagent_session_count": len({
                key for key in eligible_session_keys if session_roles.get(key, False)
            }),
            "commit_count": len({commit.sha for commit in filtered_commits}),
            "additions": sum(commit.additions for commit in filtered_commits),
            "deletions": sum(commit.deletions for commit in filtered_commits),
            "ignored_additions": sum(commit.ignored_additions for commit in filtered_commits),
            "ignored_deletions": sum(commit.ignored_deletions for commit in filtered_commits),
            "active_days": len(active_dates),
            "provider_seconds": {key: round(value, 3) for key, value in sorted(provider_seconds.items())},
            "model_seconds": {key: round(value, 3) for key, value in sorted(model_seconds.items(), key=lambda item: item[1], reverse=True)},
        },
        "group_by": list(dimensions),
        "rows": rows,
    }
