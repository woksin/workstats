from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path

from .model import Diagnostics


@dataclass(frozen=True, slots=True)
class SourceRule:
    pattern: str
    replacement: str
    _compiled: re.Pattern[str] = field(init=False, repr=False, compare=False)

    def __post_init__(self) -> None:
        if len(self.pattern) > 512 or len(self.replacement) > 256:
            raise ValueError("source rule is too long")
        if ("|" in self.pattern or "(?" in self.pattern or
                re.search(r"\\[1-9]", self.pattern) or
                re.search(r"\)[*+{?]", self.pattern) or
                re.search(r"[*+}][*+{?]", self.pattern)):
            raise ValueError("source rule is outside the safe path-regex subset")
        if ".*" in self.pattern and not re.search(r"\.\*(?:\$)?$", self.pattern):
            raise ValueError("source rule permits .* only at the end")
        object.__setattr__(self, "_compiled", re.compile(self.pattern))
        self._compiled.sub(self.replacement, "", count=1)

    def apply(self, path: str) -> str | None:
        candidate = path[:4096]
        match = self._compiled.search(candidate)
        if match:
            return candidate[:match.start()] + match.expand(self.replacement) + candidate[match.end():]
        return None


def load_config(path: Path | None, diagnostics: Diagnostics) -> dict[str, object]:
    config_path = path or Path(os.environ.get("WORKSTATS_CONFIG", "~/.config/workstats/config.json")).expanduser()
    if not config_path.exists():
        return {}
    try:
        value = json.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        diagnostics.warn(f"config ignored ({config_path}): {error}")
        return {}
    if not isinstance(value, dict):
        diagnostics.warn(f"config ignored ({config_path}): top level must be an object")
        return {}
    return value


def parse_source_rule(value: str) -> SourceRule:
    if "=" not in value:
        raise ValueError("source rule must be REGEX=REPLACEMENT")
    pattern, replacement = value.split("=", 1)
    re.compile(pattern)
    return SourceRule(pattern, replacement)


def configured_rules(config: dict[str, object], command_line: list[str]) -> list[SourceRule]:
    if len(command_line) > 32:
        raise ValueError("at most 32 source rules are supported")
    rules = [parse_source_rule(value) for value in command_line]
    raw_rules = config.get("source_roots", [])
    if isinstance(raw_rules, list):
        for raw in raw_rules:
            if isinstance(raw, dict) and isinstance(raw.get("pattern"), str) and isinstance(raw.get("replacement"), str):
                rules.append(SourceRule(raw["pattern"], raw["replacement"]))
                if len(rules) > 32:
                    raise ValueError("at most 32 source rules are supported")
    return rules


class PathResolver:
    def __init__(self, rules: list[SourceRule] | None = None, home: Path | None = None) -> None:
        self.rules = rules or []
        self.home = (home or Path.home()).resolve()
        self._repo_cache: dict[str, str] = {}

    def canonicalize(self, cwd: str) -> str:
        path = Path(os.path.expandvars(os.path.expanduser(cwd)))
        try:
            return str(path.resolve(strict=False))
        except OSError:
            return os.path.abspath(str(path))

    def nearest_repo(self, cwd: str) -> str:
        canonical = self.canonicalize(cwd)
        if canonical in self._repo_cache:
            return self._repo_cache[canonical]
        path = Path(canonical)
        start = path if path.is_dir() else path.parent
        current = start
        while True:
            if (current / ".git").exists():
                answer = str(current)
                break
            if current.parent == current:
                answer = canonical
                break
            current = current.parent
        self._repo_cache[canonical] = answer
        return answer

    def source_root(self, path: str) -> str:
        canonical = self.canonicalize(path)
        for rule in self.rules:
            result = rule.apply(canonical)
            if result is not None:
                return result

        parts = Path(canonical).parts
        if "repos" in parts:
            index = len(parts) - 1 - list(reversed(parts)).index("repos")
            if index + 2 < len(parts):
                return f"repos/{parts[index + 1]}"
            return "repos/local"
        if "sourcecode" in parts:
            return "sourcecode/(other)"
        try:
            Path(canonical).relative_to(self.home)
            return "local (~)"
        except ValueError:
            pass
        if canonical == "/tmp" or canonical.startswith("/tmp/") or canonical.startswith("/private/tmp/"):
            return "tmp/scratch"
        return "other"

    def repo_label(self, repo: str) -> str:
        path = Path(repo)
        parts = path.parts
        if "repos" in parts:
            index = len(parts) - 1 - list(reversed(parts)).index("repos")
            remainder = parts[index + 1:]
            if remainder:
                return "/".join(remainder[:2])
        try:
            relative = path.relative_to(self.home)
            return "~" if str(relative) == "." else f"~/{relative}"
        except ValueError:
            return path.name or str(path)

    def describe(self, cwd: str) -> tuple[str, str, str]:
        canonical_cwd = self.canonicalize(cwd)
        repo_path = self.nearest_repo(canonical_cwd)
        return canonical_cwd, self.repo_label(repo_path), self.source_root(repo_path)


def lossy_claude_cwd(project_dir: Path, home: Path | None = None) -> str:
    encoded = project_dir.name
    decoded = encoded.replace("-", "/")
    if not decoded.startswith("/"):
        decoded = "/" + decoded
    return decoded
