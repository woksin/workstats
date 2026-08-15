from __future__ import annotations

import json
import re
import sqlite3
from pathlib import Path
from urllib.parse import quote

from .model import ActivityPoint, Diagnostics, Session
from .paths import PathResolver, lossy_claude_cwd
from .timeutil import nearest_models, parse_epoch_milliseconds, parse_timestamp


MAX_JSONL_LINE_BYTES = 8 * 1024 * 1024
_MODEL = re.compile(r"(?:[A-Za-z0-9][A-Za-z0-9._:/+<>-]{0,127}|<synthetic>)\Z")


def _safe_model(value: object) -> str:
    return value if isinstance(value, str) and _MODEL.fullmatch(value) else "unknown"


def _read_json_lines(path: Path, diagnostics: Diagnostics):
    try:
        with path.open("rb") as stream:
            line_number = 0
            while True:
                raw = stream.readline(MAX_JSONL_LINE_BYTES + 1)
                if not raw:
                    break
                line_number += 1
                if len(raw) > MAX_JSONL_LINE_BYTES:
                    diagnostics.malformed_lines += 1
                    diagnostics.warn(f"oversized JSONL line skipped: {path}:{line_number}")
                    while raw and not raw.endswith(b"\n"):
                        raw = stream.readline(MAX_JSONL_LINE_BYTES + 1)
                    continue
                try:
                    value = json.loads(raw.decode("utf-8", errors="replace"))
                except (json.JSONDecodeError, RecursionError, UnicodeError):
                    diagnostics.malformed_lines += 1
                    if diagnostics.malformed_lines <= 20:
                        diagnostics.warn(f"malformed JSONL skipped: {path}:{line_number}")
                    continue
                if isinstance(value, dict):
                    yield value
    except OSError as error:
        diagnostics.unreadable_files += 1
        diagnostics.warn(f"unreadable transcript skipped: {path}: {error}")


def read_claude_sessions(root: Path, resolver: PathResolver, diagnostics: Diagnostics) -> list[Session]:
    if not root.is_dir():
        diagnostics.warn(f"Claude history not found: {root}")
        return []
    sessions: list[Session] = []
    for path in sorted(root.rglob("*.jsonl")):
        points: list[ActivityPoint] = []
        cwd: str | None = None
        session_id: str | None = None
        version: str | None = None
        current_model = "unknown"
        human_points: list = []
        for record in _read_json_lines(path, diagnostics):
            record_type = record.get("type")
            if record_type not in {"user", "assistant"}:
                continue
            if cwd is None and isinstance(record.get("cwd"), str):
                cwd = record["cwd"]
            if session_id is None and isinstance(record.get("sessionId"), str):
                session_id = record["sessionId"]
            if version is None and isinstance(record.get("version"), str):
                version = record["version"]
            if record_type == "assistant":
                message = record.get("message")
                if isinstance(message, dict) and isinstance(message.get("model"), str):
                    current_model = _safe_model(message["model"])
            timestamp = parse_timestamp(record.get("timestamp"))
            if timestamp:
                points.append(ActivityPoint(timestamp, current_model))
                if record_type == "user":
                    message = record.get("message")
                    content = message.get("content") if isinstance(message, dict) else None
                    human = isinstance(content, str) or (
                        isinstance(content, list) and any(
                            isinstance(item, dict) and item.get("type") in {"text", "image"}
                            for item in content
                        )
                    )
                    if (human and not record.get("isMeta") and not record.get("isSidechain") and
                            not record.get("isCompactSummary") and not record.get("sourceToolUseID") and
                            not record.get("isVisibleInTranscriptOnly")):
                        human_points.append(ActivityPoint(timestamp, current_model))
        if not points:
            diagnostics.skipped_sessions += 1
            continue
        models_at = {point.timestamp: point.model for point in nearest_models(points)}
        human_points = [ActivityPoint(point.timestamp, models_at.get(point.timestamp, point.model))
                        for point in human_points]
        approximate = cwd is None
        if approximate:
            cwd = lossy_claude_cwd(path.parent)
            diagnostics.approximate_cwds += 1
        canonical_cwd, repo, source_root = resolver.describe(cwd)
        unique_session_id = f"{session_id or path.stem}:{path.relative_to(root)}"
        sessions.append(Session("claude", unique_session_id, path, canonical_cwd, repo,
                                source_root, points, human_points=human_points,
                                is_subagent="subagents" in path.parts,
                                approximate_cwd=approximate, version=version))
    return sessions


def read_codex_sqlite_metadata(path: Path, diagnostics: Diagnostics) -> tuple[dict[str, dict[str, str]], dict[str, dict[str, str]]]:
    by_path: dict[str, dict[str, str]] = {}
    by_id: dict[str, dict[str, str]] = {}
    if not path.is_file():
        return by_path, by_id
    connection: sqlite3.Connection | None = None
    try:
        uri = f"file:{quote(str(path))}?mode=ro&immutable=1"
        connection = sqlite3.connect(uri, uri=True, timeout=0.1)
        tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if "threads" not in tables:
            diagnostics.warn(f"Codex metadata database has no threads table: {path}")
            return by_path, by_id
        columns = {row[1] for row in connection.execute("PRAGMA table_info(threads)")}
        selected = [name for name in ("id", "rollout_path", "cwd", "model", "source", "git_origin_url") if name in columns]
        if not selected:
            return by_path, by_id
        query = "SELECT " + ", ".join(f'"{name}"' for name in selected) + " FROM threads"
        for row in connection.execute(query):
            metadata = {name: value for name, value in zip(selected, row) if isinstance(value, str) and value}
            rollout_path = metadata.get("rollout_path")
            session_id = metadata.get("id")
            if rollout_path:
                by_path[str(Path(rollout_path).expanduser().resolve(strict=False))] = metadata
            if session_id:
                by_id[session_id] = metadata
    except (sqlite3.Error, OSError) as error:
        diagnostics.warn(f"Codex metadata database ignored: {path}: {error}")
    finally:
        if connection is not None:
            connection.close()
    return by_path, by_id


def _exact_codex_interval(payload: dict[str, object], model: str):
    candidates = [payload]
    for key in ("item", "result", "task"):
        nested = payload.get(key)
        if isinstance(nested, dict):
            candidates.append(nested)
    for candidate in candidates:
        start = parse_epoch_milliseconds(candidate.get("started_at_ms"))
        end = parse_epoch_milliseconds(candidate.get("completed_at_ms"))
        if start and end and end > start:
            return start, end, model
    return None


def read_codex_sessions(
    root: Path,
    resolver: PathResolver,
    diagnostics: Diagnostics,
    sqlite_path: Path | None = None,
) -> list[Session]:
    if not root.is_dir():
        diagnostics.warn(f"Codex history not found: {root}")
        return []
    by_path, by_id = read_codex_sqlite_metadata(sqlite_path, diagnostics) if sqlite_path else ({}, {})
    sessions: list[Session] = []
    for path in sorted(root.rglob("rollout-*.jsonl")):
        points_by_cwd: dict[str | None, list[ActivityPoint]] = {}
        exact_by_cwd: dict[str | None, list[tuple]] = {}
        cwd: str | None = None
        metadata_cwd: str | None = None
        session_id: str | None = None
        current_model = "unknown"
        human_by_cwd: dict[str | None, list] = {}
        is_subagent = False
        for record in _read_json_lines(path, diagnostics):
            record_type = record.get("type")
            payload = record.get("payload")
            if not isinstance(payload, dict):
                payload = {}
            if record_type == "session_meta":
                candidate_id = payload.get("id") or payload.get("session_id")
                if isinstance(candidate_id, str):
                    session_id = candidate_id
                is_subagent = (is_subagent or bool(payload.get("parent_thread_id")) or
                               isinstance(payload.get("source"), dict))
                if isinstance(payload.get("cwd"), str):
                    cwd = payload["cwd"]
                    metadata_cwd = cwd
                if isinstance(payload.get("model"), str):
                    current_model = _safe_model(payload["model"])
                continue
            if record_type == "turn_context":
                if isinstance(payload.get("cwd"), str):
                    cwd = payload["cwd"]
                if isinstance(payload.get("model"), str):
                    current_model = _safe_model(payload["model"])
                continue
            if record_type not in {"response_item", "event_msg"}:
                continue
            timestamp = parse_timestamp(record.get("timestamp"))
            if timestamp:
                points_by_cwd.setdefault(cwd, []).append(ActivityPoint(timestamp, current_model))
                if (record_type == "response_item" and payload.get("type") == "message" and
                        payload.get("role") == "user" and not is_subagent):
                    human_by_cwd.setdefault(cwd, []).append(ActivityPoint(timestamp, current_model))
            if record_type == "event_msg":
                exact = _exact_codex_interval(payload, current_model)
                if exact:
                    exact_by_cwd.setdefault(cwd, []).append(exact)

        resolved_path = str(path.resolve(strict=False))
        metadata = by_path.get(resolved_path) or (by_id.get(session_id, {}) if session_id else {})
        if session_id is None:
            candidate = metadata.get("id")
            session_id = candidate if isinstance(candidate, str) else path.stem.removeprefix("rollout-")
        if metadata_cwd is None and isinstance(metadata.get("cwd"), str):
            metadata_cwd = metadata["cwd"]
        fallback_model = _safe_model(metadata.get("model"))
        cwd_keys = set(points_by_cwd) | set(exact_by_cwd) | set(human_by_cwd)
        if not cwd_keys:
            diagnostics.skipped_sessions += 1
            continue
        for cwd_key in sorted(cwd_keys, key=lambda value: value or ""):
            points = points_by_cwd.get(cwd_key, [])
            exact_intervals = exact_by_cwd.get(cwd_key, [])
            human_points = human_by_cwd.get(cwd_key, [])
            if fallback_model != "unknown":
                points = [ActivityPoint(point.timestamp, fallback_model if point.model == "unknown" else point.model)
                          for point in points]
                exact_intervals = [(start, end, fallback_model if model == "unknown" else model)
                                   for start, end, model in exact_intervals]
            resolved_cwd = cwd_key or metadata_cwd
            approximate = resolved_cwd is None
            if approximate:
                resolved_cwd = str(path.parent)
                diagnostics.approximate_cwds += 1
            canonical_cwd, repo, source_root = resolver.describe(resolved_cwd)
            split_id = session_id if len(cwd_keys) == 1 else f"{session_id}:{canonical_cwd}"
            sessions.append(Session("codex", split_id, path, canonical_cwd, repo, source_root,
                                    points, exact_intervals, human_points=human_points,
                                    is_subagent=is_subagent, approximate_cwd=approximate))
    return sessions
