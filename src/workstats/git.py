from __future__ import annotations

import fnmatch
import os
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path

from .model import Diagnostics, GitCommit
from .paths import PathResolver


DEFAULT_IGNORES = (
    "*/node_modules/*", "*/dist/*", "*/build/*", "*/out/*", "*/obj/*",
    "*/bin/*", "*/vendor/*", "*/coverage/*", "*/.next/*", "*/.nuxt/*",
    "*/.svelte-kit/*", "*/__snapshots__/*", "*/Pods/*", "*.min.js",
    "*.min.css", "*.map", "*.snap", "*.lock", "package-lock.json",
    "pnpm-lock.yaml", "yarn.lock", "composer.lock", "Cargo.lock",
    "poetry.lock", "Gemfile.lock", "go.sum",
)


def trusted_git() -> str | None:
    for candidate in ("/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"):
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return None


def discover_repositories(base: Path, depth: int, diagnostics: Diagnostics) -> list[Path]:
    if not base.is_dir():
        diagnostics.warn(f"Git scan root not found: {base}")
        return []
    found: dict[str, Path] = {}
    base = base.resolve()
    for current, directories, _ in os.walk(base, followlinks=False):
        path = Path(current)
        relative_depth = len(path.relative_to(base).parts)
        if ".git" in directories or (path / ".git").is_file():
            canonical = str(path.resolve())
            found.setdefault(canonical, path)
            directories[:] = []
            continue
        if relative_depth >= depth:
            directories[:] = []
            continue
        directories[:] = [name for name in directories if name not in {
            ".git", "node_modules", "bin", "obj", "dist", "build", "vendor", ".cache"
        }]
    return sorted(found.values())


def _ignored(path: str, patterns: tuple[str, ...]) -> bool:
    candidate = "/" + path.lstrip("/")
    return any(fnmatch.fnmatch(path, pattern) or fnmatch.fnmatch(candidate, pattern) for pattern in patterns)


def read_git_commits(
    base: Path,
    author: str,
    resolver: PathResolver,
    diagnostics: Diagnostics,
    *,
    depth: int = 4,
    since: datetime | None = None,
    until: datetime | None = None,
    repo_filter: str | None = None,
    path_includes: tuple[str, ...] = (),
    path_excludes: tuple[str, ...] = (),
    no_ignore: bool = False,
) -> list[GitCommit]:
    commits: list[GitCommit] = []
    seen: set[str] = set()
    ignores = path_excludes if no_ignore else DEFAULT_IGNORES + path_excludes
    git = trusted_git()
    if git is None:
        diagnostics.git_errors += 1
        diagnostics.warn("Git not found in trusted executable locations")
        return []
    for repo_path in discover_repositories(base, depth, diagnostics):
        cwd, repo, root = resolver.describe(str(repo_path))
        if repo_filter and repo_filter.casefold() not in repo.casefold() and repo_filter.casefold() not in cwd.casefold():
            continue
        command = [
            git, "--no-pager", "-C", str(repo_path), "log", "--regexp-ignore-case", f"--author={author}",
            "--no-merges", "--date=iso-strict", "--pretty=format:W%x09%H%x09%aI", "--numstat",
        ]
        if since:
            command.append(f"--since={since.isoformat()}")
        if until:
            command.append(f"--until={until.isoformat()}")
        current_sha = ""
        current_time: datetime | None = None
        additions = deletions = ignored_additions = ignored_deletions = 0
        files: list[str] = []
        matched_file = False
        repo_commits: list[GitCommit] = []
        repo_seen: set[str] = set()

        def emit() -> None:
            nonlocal additions, deletions, ignored_additions, ignored_deletions, files, matched_file
            if current_sha and current_time and matched_file and current_sha not in seen and current_sha not in repo_seen:
                repo_seen.add(current_sha)
                repo_commits.append(GitCommit(current_sha, current_time, repo, cwd, root, additions,
                                              deletions, tuple(files), ignored_additions, ignored_deletions))
            additions = deletions = ignored_additions = ignored_deletions = 0
            files = []
            matched_file = False

        try:
            with tempfile.TemporaryFile(mode="w+t", encoding="utf-8") as errors:
                process = subprocess.Popen(command, text=True, stdout=subprocess.PIPE, stderr=errors)
                if process.stdout is None:
                    raise OSError("Git stdout pipe was not created")
                with process.stdout:
                    for line in process.stdout:
                        line = line.rstrip("\n")
                        if line.startswith("W\t"):
                            emit()
                            _, current_sha, stamp = line.split("\t", 2)
                            try:
                                current_time = datetime.fromisoformat(stamp.replace("Z", "+00:00")).astimezone(timezone.utc)
                            except ValueError:
                                current_time = None
                            continue
                        fields = line.split("\t")
                        invalid_additions = fields[0] != "-" and not fields[0].isdigit() if fields else True
                        invalid_deletions = fields[1] != "-" and not fields[1].isdigit() if len(fields) > 1 else True
                        if len(fields) < 3 or invalid_additions or invalid_deletions:
                            continue
                        added = int(fields[0]) if fields[0].isdigit() else 0
                        removed = int(fields[1]) if fields[1].isdigit() else 0
                        file_path = fields[-1]
                        if path_includes and not any(fnmatch.fnmatch(file_path, pattern) for pattern in path_includes):
                            continue
                        matched_file = True
                        if _ignored(file_path, ignores):
                            ignored_additions += added
                            ignored_deletions += removed
                        else:
                            additions += added
                            deletions += removed
                            files.append(file_path)
                emit()
                returncode = process.wait()
                errors.seek(0)
                error_text = errors.read(1000)
        except OSError as error:
            diagnostics.git_errors += 1
            diagnostics.warn(f"Git unavailable for {repo_path}: {error}")
            continue
        if returncode:
            if "does not have any commits yet" in error_text:
                continue
            diagnostics.git_errors += 1
            diagnostics.warn(f"Git log failed for {repo_path}: {error_text.strip()[:200]}")
            continue
        seen.update(repo_seen)
        commits.extend(repo_commits)
    return commits
