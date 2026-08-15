from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from datetime import datetime, timedelta, timezone
from pathlib import Path

from workstats.aggregate import build_report
from workstats.ai import read_claude_sessions, read_codex_sessions
from workstats.cli import _filter_sessions, _label, main, print_csv
from workstats.git import read_git_commits
from workstats.model import ActivityPoint, Diagnostics, HumanSignal, Interval, Session
from workstats.paths import PathResolver, SourceRule
from workstats.timeutil import build_human_intervals, build_session_intervals, parse_bound, split_interval, union_seconds


UTC = timezone.utc


class WorkstatsTests(unittest.TestCase):
    def test_gap_cap_and_union(self) -> None:
        points = [ActivityPoint(datetime(2026, 1, 1, 10, minute, tzinfo=UTC), "m") for minute in (0, 2, 20)]
        session = Session("codex", "s", Path("x"), "/x", "x", "root", points)
        intervals = build_session_intervals(session, timedelta(minutes=5))
        self.assertEqual(420, sum(item.seconds for item in intervals))
        overlapping = intervals + [Interval(datetime(2026, 1, 1, 10, 1, tzinfo=UTC), datetime(2026, 1, 1, 10, 4, tzinfo=UTC), "claude", "c", "c", "/x", "x", "root")]
        self.assertEqual(420, union_seconds(overlapping))

    def test_human_work_blocks_are_non_overlapping_and_attributed(self) -> None:
        signals = [
            HumanSignal(datetime(2026, 1, 1, 10, 0, tzinfo=UTC), "claude", "a", "/a", "a", "root", "claude_prompt", "opus"),
            HumanSignal(datetime(2026, 1, 1, 10, 20, tzinfo=UTC), "codex", "b", "/b", "b", "root", "codex_prompt", "gpt"),
            HumanSignal(datetime(2026, 1, 1, 12, 0, tzinfo=UTC), "git", "c", "/c", "c", "root", "commit", "—"),
        ]
        intervals = build_human_intervals(signals, timedelta(minutes=30), timedelta(minutes=10))
        self.assertEqual(40 * 60, sum(item.seconds for item in intervals))
        self.assertEqual(40 * 60, union_seconds(intervals))
        self.assertEqual({"a", "b", "c"}, {item.repo for item in intervals})
        self.assertEqual(2, len({item.session_id for item in intervals}))

    def test_human_edge_credit_does_not_spill_into_empty_day(self) -> None:
        signal = HumanSignal(datetime(2026, 1, 1, 23, 58, tzinfo=UTC), "git", "c", "/c", "c", "root", "commit", "—")
        with patch.dict(os.environ, {"TZ": "UTC"}):
            intervals = build_human_intervals([signal], timedelta(minutes=30), timedelta(minutes=10))
        self.assertEqual(datetime(2026, 1, 2, 0, 0, tzinfo=UTC), intervals[0].end)

    def test_cross_month_is_split_in_local_calendar(self) -> None:
        interval = Interval(datetime(2026, 1, 31, 23, 59, tzinfo=UTC), datetime(2026, 2, 1, 0, 1, tzinfo=UTC), "codex", "m", "s", "/x", "x", "root")
        pieces = split_interval(interval, "month", UTC)
        self.assertEqual(["2026-01", "2026-02"], [key for key, _ in pieces])
        self.assertEqual([60, 60], [item.seconds for _, item in pieces])

    def test_inclusive_date_and_month_bounds(self) -> None:
        self.assertEqual(datetime(2026, 3, 1, tzinfo=UTC), parse_bound("2026-02", until=True, local_tz=UTC))
        self.assertEqual(datetime(2026, 2, 2, tzinfo=UTC), parse_bound("2026-02-01", until=True, local_tz=UTC))

    def test_claude_skips_malformed_and_extracts_model_and_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "-tmp-real-project"
            project.mkdir()
            transcript = project / "session.jsonl"
            transcript.write_text("{broken\n" + "\n".join([
                json.dumps({"type": "user", "timestamp": "2026-01-01T00:00:00Z", "cwd": str(project),
                            "sessionId": "same", "message": {"content": "hello"}}),
                json.dumps({"type": "assistant", "timestamp": "2026-01-01T00:01:00Z", "cwd": str(project), "sessionId": "same", "message": {"model": "claude-test"}}),
                json.dumps({"type": "assistant", "timestamp": "2026-01-01T00:02:00Z", "cwd": str(project), "sessionId": "same", "message": {"model": "prompt text\n\u001b[31m"}}),
            ]), encoding="utf-8")
            diagnostics = Diagnostics()
            sessions = read_claude_sessions(root, PathResolver(home=root), diagnostics)
            self.assertEqual(1, diagnostics.malformed_lines)
            self.assertEqual("claude-test", sessions[0].points[-2].model)
            self.assertEqual("unknown", sessions[0].points[-1].model)
            self.assertEqual("claude-test", sessions[0].human_points[0].model)
            self.assertEqual(str(project.resolve()), sessions[0].cwd)
            self.assertIn("session.jsonl", sessions[0].session_id)

    def test_claude_meta_user_message_is_not_human_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            records = [
                {"type": "user", "timestamp": "2026-01-01T00:00:00Z", "cwd": temporary,
                 "message": {"content": "real"}},
                {"type": "user", "timestamp": "2026-01-01T00:01:00Z", "cwd": temporary,
                 "isMeta": True, "message": {"content": "automatic"}},
            ]
            (project / "session.jsonl").write_text("\n".join(json.dumps(item) for item in records), encoding="utf-8")
            sessions = read_claude_sessions(root, PathResolver(home=root), Diagnostics())
            self.assertEqual(1, len(sessions[0].human_points))

    def test_oversized_jsonl_line_is_bounded_and_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            valid = json.dumps({"type": "user", "timestamp": "2026-01-01T00:00:00Z", "cwd": temporary})
            (project / "session.jsonl").write_bytes(b"x" * 1024 + b"\n" + valid.encode() + b"\n")
            diagnostics = Diagnostics()
            with patch("workstats.ai.MAX_JSONL_LINE_BYTES", 512):
                sessions = read_claude_sessions(root, PathResolver(home=root), diagnostics)
            self.assertEqual(1, diagnostics.malformed_lines)
            self.assertEqual(1, len(sessions))

    def test_codex_model_change_and_exact_interval(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transcript = root / "rollout-test.jsonl"
            second_cwd = str(root / "second")
            (root / "second").mkdir()
            records = [
                {"timestamp": "2026-01-01T00:00:00Z", "type": "session_meta", "payload": {"id": "s", "cwd": temporary}},
                {"timestamp": "2026-01-01T00:00:00Z", "type": "turn_context", "payload": {"model": "gpt-a", "cwd": temporary}},
                {"timestamp": "2026-01-01T00:00:00Z", "type": "response_item", "payload": {"type": "reasoning"}},
                {"timestamp": "2026-01-01T00:01:00Z", "type": "response_item", "payload": {"type": "message"}},
                {"timestamp": "2026-01-01T00:02:00Z", "type": "turn_context", "payload": {"model": "gpt-b", "cwd": second_cwd}},
                {"timestamp": "2026-01-01T00:02:00Z", "type": "response_item", "payload": {"type": "reasoning"}},
                {"timestamp": "2026-01-01T00:03:00Z", "type": "event_msg", "payload": {"type": "item_completed", "started_at_ms": 1767225720000, "completed_at_ms": 1767225780000}},
            ]
            transcript.write_text("\n".join(json.dumps(record) for record in records), encoding="utf-8")
            sessions = read_codex_sessions(root, PathResolver(home=root), Diagnostics())
            intervals = [item for session in sessions for item in build_session_intervals(session, timedelta(minutes=5))]
            self.assertEqual(2, len(sessions))
            self.assertEqual({"gpt-a", "gpt-b"}, {item.model for item in intervals})
            self.assertEqual(120, sum(item.seconds for item in intervals))

    def test_source_root(self) -> None:
        resolver = PathResolver(home=Path("/home/test"))
        self.assertEqual("repos/studio", resolver.source_root("/mnt/sourcecode/repos/studio/widget"))
        self.assertEqual("tmp/scratch", resolver.source_root("/private/tmp/a"))
        rule = SourceRule(r"^/work/clients/([^/]+)/.*", r"client/\1")
        self.assertEqual("client/acme", rule.apply("/work/clients/acme/repo"))
        with self.assertRaises(ValueError):
            SourceRule(r"^(a|aa)+$", "bad")

    def test_report_json_shape_and_calendar_group(self) -> None:
        session = Session("codex", "s", Path("x"), "/repo", "org/repo", "repos/org", [
            ActivityPoint(datetime(2026, 1, 1, 22, 59, tzinfo=UTC), "gpt"),
            ActivityPoint(datetime(2026, 1, 1, 23, 1, tzinfo=UTC), "gpt"),
        ])
        report = build_report([session], [], timedelta(minutes=5), None, None, ("day", "model"))
        self.assertEqual(120, report["summary"]["deduplicated_active_seconds"])
        self.assertEqual(2, len(report["rows"]))
        self.assertIn("human_estimated_seconds", report["summary"])
        self.assertEqual(report["summary"]["deduplicated_active_seconds"], report["summary"]["agent_wall_seconds"])
        self.assertEqual(report["summary"]["attributed_active_seconds"], report["summary"]["parallel_agent_seconds"])
        self.assertIn("ai_wall_seconds", report["rows"][0])
        self.assertIn("parallel_agent_seconds", report["rows"][0])
        json.dumps(report)

    def test_report_human_time_is_one_global_timeline(self) -> None:
        sessions = [
            Session("claude", "a", Path("a"), "/a", "a", "root", human_points=[
                ActivityPoint(datetime(2026, 1, 1, 10, 0, tzinfo=UTC), "opus"),
                ActivityPoint(datetime(2026, 1, 1, 10, 10, tzinfo=UTC), "opus"),
            ]),
            Session("codex", "b", Path("b"), "/b", "b", "root", human_points=[
                ActivityPoint(datetime(2026, 1, 1, 10, 5, tzinfo=UTC), "gpt"),
                ActivityPoint(datetime(2026, 1, 1, 10, 15, tzinfo=UTC), "gpt"),
            ]),
        ]
        report = build_report(sessions, [], timedelta(minutes=5), None, None, ("repo",))
        self.assertEqual(20 * 60, report["summary"]["human_estimated_seconds"])
        self.assertEqual(report["summary"]["human_estimated_seconds"],
                         sum(row["human_estimated_seconds"] for row in report["rows"]))
        self.assertLessEqual(max(row["average_human_seconds_per_active_day"] for row in report["rows"]), 24 * 3600)

    def test_months_are_named_and_sorted_newest_first(self) -> None:
        sessions = [
            Session("codex", "april", Path("a"), "/a", "a", "root", human_points=[
                ActivityPoint(datetime(2026, 4, 10, 10, tzinfo=UTC), "gpt"),
            ]),
            Session("codex", "may-small", Path("b"), "/b", "b", "root", human_points=[
                ActivityPoint(datetime(2026, 5, 10, 10, tzinfo=UTC), "gpt"),
            ]),
            Session("codex", "may-large", Path("c"), "/c", "c", "root", human_points=[
                ActivityPoint(datetime(2026, 5, 10, 11, tzinfo=UTC), "gpt"),
                ActivityPoint(datetime(2026, 5, 10, 11, 10, tzinfo=UTC), "gpt"),
            ]),
        ]
        report = build_report(sessions, [], timedelta(minutes=5), None, None, ("month", "repo"))
        self.assertEqual(["2026-05", "2026-05", "2026-04"],
                         [row["key"]["month"] for row in report["rows"]])
        self.assertEqual("May 2026 · c", _label(report["rows"][0], report["group_by"]))

    def test_exact_repo_filter_does_not_match_similar_names(self) -> None:
        sessions = [
            Session("codex", "a", Path("a"), "/repos/studio/widget", "studio/widget", "repos/studio"),
            Session("codex", "b", Path("b"), "/repos/misc/widget-tools", "misc/widget-tools", "repos/misc"),
        ]
        self.assertEqual([sessions[0]], _filter_sessions(sessions, None, "widget"))

    def test_single_timestamp_counts_as_session_without_time(self) -> None:
        session = Session("claude", "single", Path("x"), "/repo", "org/repo", "repos/org", [
            ActivityPoint(datetime(2026, 1, 1, 12, tzinfo=UTC), "claude-test"),
        ])
        report = build_report([session], [], timedelta(minutes=5), None, None, ("model",))
        self.assertEqual(1, report["summary"]["session_count"])
        self.assertEqual(0, report["summary"]["deduplicated_active_seconds"])
        self.assertEqual("claude-test", report["rows"][0]["key"]["model"])

    def test_temporary_git_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            repo = base / "org" / "repo"
            repo.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.name", "Test Author"], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.email", "test@example.com"], check=True)
            (repo / "code.txt").write_text("one\ntwo\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "code.txt"], check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-qm", "test"], check=True)
            commits = read_git_commits(base, "Test Author", PathResolver(home=base), Diagnostics(), depth=3)
            self.assertEqual(1, len(commits))
            self.assertEqual(2, commits[0].additions)
            filtered = read_git_commits(base, "Test Author", PathResolver(home=base), Diagnostics(),
                                        depth=3, path_includes=("*.md",))
            self.assertEqual([], filtered)

    def test_git_counts_commits_with_only_ignored_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            repo = base / "repo"
            repo.mkdir()
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.name", "Test Author"], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "user.email", "test@example.com"], check=True)
            (repo / "yarn.lock").write_text("generated\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "yarn.lock"], check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-qm", "generated"], check=True)
            commits = read_git_commits(base, "Test Author", PathResolver(home=base), Diagnostics(), depth=1)
            self.assertEqual(1, len(commits))
            self.assertEqual(0, commits[0].additions)
            self.assertEqual(1, commits[0].ignored_additions)

    def test_cli_rejects_day_and_month_together(self) -> None:
        error = io.StringIO()
        with contextlib.redirect_stderr(error):
            code = main(["--no-ai", "--no-git", "--group-by", "day,month"])
        self.assertEqual(2, code)
        self.assertIn("alternative calendar groupings", error.getvalue())

    def test_csv_neutralizes_formula_cells(self) -> None:
        report = {
            "group_by": ["root"],
            "rows": [{
                "key": {"root": "=2+2"}, "active_seconds": 0, "session_count": 0,
                "commit_count": 0, "file_count": 0, "additions": 0, "deletions": 0,
                "ignored_additions": 0, "ignored_deletions": 0, "net_lines": 0,
                "active_days": 0, "calendar_days": 0,
                "average_active_seconds_per_active_day": 0,
                "average_active_seconds_per_calendar_day": 0,
                "first_seen": None, "last_seen": None,
            }],
        }
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            print_csv(report)
        self.assertIn("'=2+2", output.getvalue())

    def test_launcher_does_not_import_from_callers_directory(self) -> None:
        project_root = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as temporary:
            (Path(temporary) / "workstats.py").write_text("raise SystemExit('shadowed')\n", encoding="utf-8")
            result = subprocess.run([str(project_root / "bin" / "workstats"), "--version"],
                                    cwd=temporary, text=True, capture_output=True, check=False)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("workstats 0.4.0", result.stdout)

    def test_cli_json_with_missing_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                code = main(["--dir", temporary, "--codex-dir", temporary + "/missing", "--claude-dir", temporary + "/missing", "--format", "json"])
            self.assertEqual(0, code)
            value = json.loads(output.getvalue())
            self.assertIn("methodology", value)
            self.assertIn("diagnostics", value)


if __name__ == "__main__":
    unittest.main()
