from __future__ import annotations

import argparse
import csv
import json
import os
import re
import subprocess
import sys
from calendar import month_name
from datetime import datetime, timedelta
from pathlib import Path

from .aggregate import DIMENSIONS, build_report
from .ai import read_claude_sessions, read_codex_sessions
from .git import read_git_commits
from .model import Diagnostics
from .paths import PathResolver, configured_rules, load_config
from .timeutil import build_session_intervals, parse_bound, parse_duration


VERSION = "0.2.0"


def _default_repo_root() -> Path:
    return Path(os.environ.get("WORKSTATS_DIR") or os.environ.get("GITSTATS_DIR") or "~/src/repos").expanduser()


def _default_author() -> str:
    configured = os.environ.get("WORKSTATS_AUTHOR") or os.environ.get("GITSTATS_AUTHOR")
    if configured:
        return configured
    for key in ("user.email", "user.name"):
        result = subprocess.run(
            ["git", "config", "--global", "--get", key],
            capture_output=True, text=True, check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            return re.escape(result.stdout.strip())
    return ""


def parser() -> argparse.ArgumentParser:
    details = (
        "Measures local Git output and active AI-assisted work from retained Codex and "
        "Claude Code transcripts. No transcript text is emitted and no network APIs are used."
    )
    result = argparse.ArgumentParser(prog="workstats", description=details)
    result.add_argument("--version", action="version", version=f"workstats {VERSION}")
    result.add_argument("-d", "--dir", type=Path, default=_default_repo_root(), help="Git repositories root")
    result.add_argument("-a", "--author", default=_default_author(), help="Git author regex")
    result.add_argument("-R", "--repo", help="case-insensitive repository/path substring filter")
    result.add_argument("--repo-exact", help="exact repo label or final folder name")
    result.add_argument("-s", "--since", help="inclusive YYYY-MM or YYYY-MM-DD")
    result.add_argument("-u", "--until", help="inclusive YYYY-MM or YYYY-MM-DD")
    result.add_argument("--gap-cap", default="5m", help="idle gap cap: 30s, 5m, 1h (default: 5m)")
    result.add_argument("--human-idle", default="15m", help="start a new hands-on work block after this idle gap (default: 15m)")
    result.add_argument("--isolated-credit", default="5m", help="time credited to an isolated human signal (default: 5m)")
    result.add_argument("--group-by", "--by", default="root", help="comma-separated: root,repo,cwd,provider,model,day,month")
    result.add_argument("--period", choices=("day", "month"), help="append a calendar grouping")
    result.add_argument("--provider", choices=("all", "codex", "claude"), default="all")
    result.add_argument("--format", choices=("table", "json", "csv"), default="table")
    result.add_argument("--top", type=int, default=30, help="maximum table rows (0 means all)")
    result.add_argument("--no-git", action="store_true", help="skip Git history")
    result.add_argument("--no-ai", action="store_true", help="skip all AI histories")
    result.add_argument("--no-codex", action="store_true")
    result.add_argument("--no-claude", action="store_true")
    result.add_argument("--codex-dir", type=Path, default=Path("~/.codex/sessions").expanduser())
    result.add_argument("--codex-db", type=Path, default=Path("~/.codex/state_5.sqlite").expanduser())
    result.add_argument("--claude-dir", type=Path, default=Path("~/.claude/projects").expanduser())
    result.add_argument("--config", type=Path, help="JSON config (default: ~/.config/workstats/config.json)")
    result.add_argument("--source-rule", action="append", default=[], metavar="REGEX=NAME", help="custom source-root rule; repeatable")
    result.add_argument("--depth", type=int, default=4, help="Git repository discovery depth")
    result.add_argument("--path", action="append", default=[], help="Git file include glob; repeatable/comma-separated")
    result.add_argument("-P", "--path-exclude", action="append", default=[], help="additional Git ignore glob")
    result.add_argument("--no-ignore", action="store_true", help="include generated/vendor Git paths")
    result.add_argument("--no-color", action="store_true")
    # Familiar gitstats flags retained as aliases. They choose useful groupings.
    result.add_argument("-r", "--by-repo", action="store_true", help="group by month and repo")
    result.add_argument("-m", "--matrix", action="store_true", help="alias for --group-by repo --period month")
    result.add_argument("-D", "--by-dir", action="store_true", help="group by exact working area")
    result.add_argument("--raw", "--show-agent-work", action="store_true", help="show detailed parallel agent/model activity")
    result.epilog = (
        "Human work is an estimate from foreground prompts and authored commits, not a stopwatch. "
        "Local history is retention-dependent; work on other machines is not visible."
    )
    return result


def _csv_globs(values: list[str]) -> tuple[str, ...]:
    return tuple(piece.strip() for value in values for piece in value.split(",") if piece.strip())


def _hours(seconds: object) -> str:
    value = float(seconds or 0)
    hours, remainder = divmod(int(round(value)), 3600)
    minutes = remainder // 60
    return f"{hours}h {minutes:02d}m"


def _number(value: object) -> str:
    return f"{int(value or 0):,}"


def _local_date(value: str) -> str:
    return datetime.fromisoformat(value).astimezone().date().isoformat()


def _label(row: dict[str, object], dimensions: list[str]) -> str:
    key = row["key"]
    values = []
    for name in dimensions:
        value = str(key[name])
        if name == "month" and re.fullmatch(r"\d{4}-\d{2}", value):
            year, month = (int(part) for part in value.split("-"))
            value = f"{month_name[month]} {year}"
        values.append(value)
    return " · ".join(values)


def _spark(values: list[float]) -> str:
    glyphs = "▁▂▃▄▅▆▇█"
    maximum = max(values, default=0)
    return "".join(glyphs[min(7, int(value / maximum * 7))] if maximum else glyphs[0] for value in values)


def print_table(report: dict[str, object], diagnostics: Diagnostics, top: int, raw: bool) -> None:
    summary = report["summary"]
    observed = report["observed"]
    dimensions = report["group_by"]
    rows = report["rows"] if top == 0 else report["rows"][:top]
    print("WORKSTATS  human work across local projects")
    print("═" * 94)
    print(f"  Estimated hands-on work  {_hours(summary['human_estimated_seconds'])}")
    print(f"  Active work days      {_number(summary['human_active_days'])}")
    print(f"  Average / active day  {_hours(summary['average_human_seconds_per_active_day'])}")
    print(f"  Work blocks             {_number(summary['work_block_count'])}  "
          f"({_number(summary['prompt_signal_count'])} prompts + "
          f"{_number(summary['commit_signal_count'])} commits observed)")
    print(f"  Git commits             {_number(summary['commit_count'])}")
    print(f"  Git lines               +{_number(summary['additions'])} / -{_number(summary['deletions'])}")
    if summary["ignored_additions"] or summary["ignored_deletions"]:
        print(f"  Ignored Git lines       +{_number(summary['ignored_additions'])} / -{_number(summary['ignored_deletions'])}")
    if observed["first_seen"]:
        print(f"  Observed                {_local_date(observed['first_seen'])} → {_local_date(observed['last_seen'])}")
    print()

    if summary["session_count"]:
        concurrency = (summary["parallel_agent_seconds"] / summary["agent_wall_seconds"]
                       if summary["agent_wall_seconds"] else 0)
        print("AI activity  (context only — these are not human hours)")
        print(f"  Agent wall clock      {_hours(summary['agent_wall_seconds'])}  "
              "(any agent active, overlap removed)")
        print(f"  Parallel agent work   {_hours(summary['parallel_agent_seconds'])}  "
              f"({concurrency:.1f}× concurrency)")
        print(f"  Sessions              {_number(summary['session_count'])}  "
              f"({_number(summary['foreground_session_count'])} foreground, "
              f"{_number(summary['subagent_session_count'])} subagents)")
        print()
        if raw:
            print("Parallel agent work by provider / model  (may overlap)")
            for provider, seconds in summary["provider_seconds"].items():
                print(f"  {provider:<24} {_hours(seconds):>10}")
            for model, seconds in list(summary["model_seconds"].items())[:12]:
                print(f"    {model:<34} {_hours(seconds):>10}")
            print()

    title = " × ".join(dimensions)
    print(f"By {title}  (hands-on estimate first; AI wall clock shown as context)")
    print(f"  {'Work area':<38} {'Human':>9} {'Days':>5} {'Avg/day':>9} {'Commits':>8} {'AI wall':>9} {'Agent work':>10}")
    print("  " + "─" * 96)
    for row in rows:
        label = _label(row, dimensions)
        if len(label) > 38:
            label = "…" + label[-37:]
        print(f"  {label:<38} {_hours(row['human_estimated_seconds']):>9} "
              f"{_number(row['human_active_days']):>5} "
              f"{_hours(row['average_human_seconds_per_active_day']):>9} "
              f"{_number(row['commit_count']):>8} {_hours(row['ai_wall_seconds']):>9} "
              f"{_hours(row['parallel_agent_seconds']):>10}")
    if top and len(report["rows"]) > top:
        print(f"  … {len(report['rows']) - top} more rows; use --top 0")
    calendar_dimension = next((name for name in ("day", "month") if name in dimensions), None)
    if calendar_dimension and rows:
        totals: dict[str, float] = {}
        for row in report["rows"]:
            period = str(row["key"][calendar_dimension])
            totals[period] = totals.get(period, 0.0) + float(row["human_estimated_seconds"])
        chronological = [totals[period] for period in sorted(totals)]
        print(f"\n  Human-work trend  {_spark(chronological)}  (oldest → newest)")
    print()
    human_idle_minutes = report["methodology"]["human_idle_threshold_seconds"] / 60
    credit_minutes = report["methodology"]["isolated_signal_credit_seconds"] / 60
    print(f"Hands-on estimate: foreground prompts + authored commits; {human_idle_minutes:g}m idle ends a work block; "
          f"isolated signals receive {credit_minutes:g}m.")
    print("This is a conservative activity estimate, not timesheet, attendance, or literal keyboard time.")
    print("AI wall removes overlap within each row; rows can overlap each other. Agent work sums parallel sessions.")
    inputs = report.get("inputs", {})
    if inputs.get("repo_filter") or inputs.get("repo_exact_filter"):
        print("Scope note: work blocks are recomputed from the selected repositories, so filtered totals can differ from an all-repo row.")
    print("Local retained history only. Missing/pruned transcripts and work on other machines are not visible.")
    if diagnostics.malformed_lines or diagnostics.unreadable_files or diagnostics.git_errors or diagnostics.approximate_cwds:
        print(f"Diagnostics: {diagnostics.malformed_lines} malformed lines, {diagnostics.unreadable_files} unreadable files, "
              f"{diagnostics.git_errors} Git errors, {diagnostics.approximate_cwds} approximate working directories.")


def print_csv(report: dict[str, object]) -> None:
    dimensions = report["group_by"]
    fields = dimensions + ["human_estimated_seconds", "human_active_days", "average_human_seconds_per_active_day", "work_block_count", "human_signal_count", "ai_wall_seconds", "parallel_agent_seconds", "active_seconds", "foreground_session_count", "subagent_session_count", "session_count", "commit_count", "file_count", "additions", "deletions", "ignored_additions", "ignored_deletions", "net_lines", "active_days", "calendar_days", "average_active_seconds_per_active_day", "average_active_seconds_per_calendar_day", "first_seen", "last_seen"]
    writer = csv.DictWriter(sys.stdout, fieldnames=fields)
    writer.writeheader()
    for row in report["rows"]:
        flat = {**row["key"], **{field: row.get(field) for field in fields if field not in dimensions}}
        flat = {key: "'" + value if isinstance(value, str) and value.startswith(("=", "+", "-", "@")) else value
                for key, value in flat.items()}
        writer.writerow(flat)


def _filter_sessions(sessions, pattern: str | None, exact: str | None):
    if exact:
        needle = exact.casefold()
        sessions = [session for session in sessions
                    if session.repo.casefold() == needle or Path(session.cwd).name.casefold() == needle]
    if not pattern:
        return sessions
    needle = pattern.casefold()
    return [session for session in sessions if needle in session.repo.casefold() or needle in session.cwd.casefold() or needle in session.root.casefold()]


def _compact_sessions(sessions, gap_cap):
    """Discard high-volume event points after reducing them to active ranges."""
    for session in sessions:
        compact = build_session_intervals(session, gap_cap)
        if compact:
            session.points = []
            session.exact_intervals = [(item.start, item.end, item.model) for item in compact]
    return sessions


def run(arguments: argparse.Namespace) -> int:
    diagnostics = Diagnostics()
    try:
        gap_cap = parse_duration(arguments.gap_cap)
        human_idle = parse_duration(arguments.human_idle)
        isolated_credit = parse_duration(arguments.isolated_credit)
        since = parse_bound(arguments.since, until=False)
        until = parse_bound(arguments.until, until=True)
        config = load_config(arguments.config, diagnostics)
        resolver = PathResolver(configured_rules(config, arguments.source_rule))
        dimensions = tuple(piece.strip() for piece in arguments.group_by.split(",") if piece.strip())
        if arguments.by_repo:
            dimensions = ("month", "repo")
        elif arguments.matrix:
            dimensions = ("repo", "month")
        elif arguments.by_dir:
            dimensions = ("cwd",)
        if arguments.period and arguments.period not in dimensions:
            dimensions += (arguments.period,)
        if not dimensions or set(dimensions) - DIMENSIONS or len(set(dimensions)) != len(dimensions):
            raise ValueError("--group-by must contain unique values from: " + ", ".join(sorted(DIMENSIONS)))
        if {"day", "month"}.issubset(dimensions):
            raise ValueError("day and month are alternative calendar groupings; choose one")
        if arguments.top < 0 or arguments.depth < 0:
            raise ValueError("--top and --depth must be zero or greater")
        if not arguments.no_git and not arguments.author:
            raise ValueError("Git author is not configured; set git config --global user.email or pass --author")
    except (ValueError, re.error) as error:
        print(f"workstats: {error}", file=sys.stderr)
        return 2

    commits = []
    if not arguments.no_git:
        commits = read_git_commits(arguments.dir.expanduser(), arguments.author, resolver, diagnostics,
                                   depth=arguments.depth, since=since, until=until,
                                   repo_filter=arguments.repo or arguments.repo_exact,
                                   path_includes=_csv_globs(arguments.path),
                                   path_excludes=_csv_globs(arguments.path_exclude), no_ignore=arguments.no_ignore)
        if arguments.repo_exact:
            needle = arguments.repo_exact.casefold()
            commits = [commit for commit in commits
                       if commit.repo.casefold() == needle or Path(commit.cwd).name.casefold() == needle]
    # Git can emit a large transient numstat stream. Collect it before retaining
    # compacted transcript histories to keep peak memory bounded on large machines.
    sessions = []
    if not arguments.no_ai:
        if not arguments.no_claude and arguments.provider in {"all", "claude"}:
            sessions.extend(_compact_sessions(
                read_claude_sessions(arguments.claude_dir.expanduser(), resolver, diagnostics), gap_cap))
        if not arguments.no_codex and arguments.provider in {"all", "codex"}:
            sessions.extend(_compact_sessions(
                read_codex_sessions(arguments.codex_dir.expanduser(), resolver, diagnostics, arguments.codex_db.expanduser()), gap_cap))
    sessions = _filter_sessions(sessions, arguments.repo, arguments.repo_exact)
    report = build_report(sessions, commits, gap_cap, since, until, dimensions,
                          human_idle=human_idle, isolated_credit=isolated_credit)
    report["diagnostics"] = diagnostics.as_dict()
    report["inputs"] = {
        "git_root": str(arguments.dir.expanduser()), "claude_root": str(arguments.claude_dir.expanduser()),
        "codex_root": str(arguments.codex_dir.expanduser()), "author": arguments.author,
        "repo_filter": arguments.repo, "repo_exact_filter": arguments.repo_exact,
        "human_idle": arguments.human_idle, "isolated_credit": arguments.isolated_credit,
    }
    if arguments.format == "json":
        json.dump(report, sys.stdout, indent=2, sort_keys=True)
        print()
    elif arguments.format == "csv":
        print_csv(report)
    else:
        print_table(report, diagnostics, arguments.top, arguments.raw)
    return 0


def main(argv: list[str] | None = None) -> int:
    return run(parser().parse_args(argv))


def legacy_main() -> int:
    print("gitstats is now workstats; showing the combined dashboard (use workstats --help).", file=sys.stderr)
    return main()
