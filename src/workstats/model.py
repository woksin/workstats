from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path


@dataclass(frozen=True, slots=True)
class ActivityPoint:
    timestamp: datetime
    model: str = "unknown"


@dataclass(frozen=True, slots=True)
class HumanSignal:
    timestamp: datetime
    provider: str
    session_id: str
    cwd: str
    repo: str
    root: str
    kind: str
    model: str = "unknown"


@dataclass(frozen=True, slots=True)
class Interval:
    start: datetime
    end: datetime
    provider: str
    model: str
    session_id: str
    cwd: str
    repo: str
    root: str
    approximate_cwd: bool = False

    @property
    def seconds(self) -> float:
        return max(0.0, (self.end - self.start).total_seconds())


@dataclass(slots=True)
class Session:
    provider: str
    session_id: str
    source_file: Path
    cwd: str
    repo: str
    root: str
    points: list[ActivityPoint] = field(default_factory=list)
    exact_intervals: list[tuple[datetime, datetime, str]] = field(default_factory=list)
    human_points: list[ActivityPoint] = field(default_factory=list)
    is_subagent: bool = False
    approximate_cwd: bool = False
    version: str | None = None

    @property
    def first_seen(self) -> datetime | None:
        candidates = [point.timestamp for point in self.points]
        candidates.extend(start for start, _, _ in self.exact_intervals)
        candidates.extend(point.timestamp for point in self.human_points)
        return min(candidates) if candidates else None

    @property
    def last_seen(self) -> datetime | None:
        candidates = [point.timestamp for point in self.points]
        candidates.extend(end for _, end, _ in self.exact_intervals)
        candidates.extend(point.timestamp for point in self.human_points)
        return max(candidates) if candidates else None


@dataclass(frozen=True, slots=True)
class GitCommit:
    sha: str
    timestamp: datetime
    repo: str
    cwd: str
    root: str
    additions: int
    deletions: int
    files: tuple[str, ...]
    ignored_additions: int = 0
    ignored_deletions: int = 0


@dataclass(slots=True)
class Diagnostics:
    malformed_lines: int = 0
    unreadable_files: int = 0
    approximate_cwds: int = 0
    skipped_sessions: int = 0
    git_errors: int = 0
    messages: list[str] = field(default_factory=list)

    def warn(self, message: str) -> None:
        if len(self.messages) < 100:
            self.messages.append(message)

    def as_dict(self) -> dict[str, object]:
        return {
            "malformed_lines": self.malformed_lines,
            "unreadable_files": self.unreadable_files,
            "approximate_cwds": self.approximate_cwds,
            "skipped_sessions": self.skipped_sessions,
            "git_errors": self.git_errors,
            "messages": self.messages,
        }
