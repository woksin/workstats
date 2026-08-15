# Changelog

All notable changes to `workstats` are documented here.

## 0.6.0 — 2026-08-15

- Made the human-work estimate supervision-inclusive instead of prompt-only.
- Added foreground agent activity and exact foreground interval edges as human-work signals.
- Increased the default continuity window to one hour and setup/review credit to 30 minutes.
- Added `--review-credit` while retaining `--isolated-credit` as a compatible alias.
- Continued to exclude subagents and globally deduplicate overlapping human time.

## 0.5.1 — 2026-08-15

- Removed the retired reference implementation, its project metadata, and its test suite.
- Moved the remaining behavioral checks into the native Rust test suite.
- Removed the obsolete `gitstats-legacy` installer payload; `gitstats` remains a native alias.

## 0.5.0 — 2026-08-15

- Added automatic local-history discovery with `workstats sources`.
- Added Gemini CLI, GitHub Copilot CLI, and OpenCode adapters.
- Added the content-free Workstats Events JSONL format for any CLI, IDE, or API wrapper.
- Added `workstats record` for safely appending provider-neutral activity and prompt signals.
- Replaced the closed provider enum with repeatable `--provider`, `--exclude-provider`,
  `--history PROVIDER=PATH`, and `--events FILE` options.
- Made the current directory the portable default Git scope and removed local-layout assumptions
  from repository and source-root labels.

## 0.4.0 — 2026-08-15

- Rebuilt the CLI in Rust with parallel, streaming transcript parsing.
- Added an incremental SQLite transcript index and range pruning.
- Added the human-work estimate, overlap-safe AI wall time, and agent concurrency.
- Added animated interactive progress with clean JSON and CSV output.
- Added native Windows, Linux, and macOS support and release automation.
