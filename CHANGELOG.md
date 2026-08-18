# Changelog

Notable changes, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versions are not set by hand. A merged pull request labelled `major`, `minor`
or `patch` decides the next one, and the
[release workflow](.github/workflows/release.yml) cuts the release from it —
so the tag, the GitHub release, and the binaries all come from that label.
This file is the readable summary of what changed; the release notes on each
tag are the generated list of pull requests.

## [Unreleased]

### Added

- Work composition: changed Git lines bucketed into `source`, `test`, `docs`,
  `config`, `assets`, and `other` from the file path alone, with a
  test-lines-per-source-line ratio. Shown in the dashboard, per row in JSON,
  and as `{area}_files` / `{area}_additions` / `{area}_deletions` columns in
  CSV. It measures churn, not the size of the codebase.
- Change shapes: each commit described as `new code`, `revision`, `removal`,
  `tests`, `docs`, `config`, `assets`, or `mixed` from the area holding at
  least 60% of its changed lines and its addition/deletion balance. Commit
  messages and file contents are never read.
- A committed-output count pairing foreground AI sessions with authored
  commits in the same repository within one `--human-idle` window. Sessions in
  repositories Git did not scan are excluded from both sides, so the remainder
  genuinely reflects reading, review, and uncommitted work.
- Token usage tracking (input, output, cache-read, cache-creation) for Claude
  Code, Codex, GitHub Copilot CLI, and Gemini CLI, grouped by the existing
  `root`, `repo`, `cwd`, `provider`, `model`, `day`, and `month` dimensions. A
  `Tokens` summary line, table column, and `--raw` provider/model breakdown,
  plus token fields in JSON and CSV output.
- `workstats update` and `workstats update --check` to explicitly check for
  and install newer releases from GitHub, verifying the download against the
  release's published `SHA256SUMS` before replacing the running binary.
- An opt-in, throttled (~daily) background update check with a one-line
  footer notice (`--check-updates`, `WORKSTATS_CHECK_UPDATES`, or
  `check_updates` in the config file; suppressible with `--no-update-check` /
  `WORKSTATS_NO_UPDATE_CHECK`). Normal runs remain fully local unless
  explicitly opted in.

### Changed

- Releases are now cut automatically from a merged pull request's `major` /
  `minor` / `patch` label instead of a manually pushed `v*` tag, and publish a
  Homebrew formula alongside the binaries.

## 0.6.1 — 2026-08-16

- Stopped treating dense autonomous foreground transcript output as continuous human presence.
- Replaced per-event activity signals with bounded foreground session start/end evidence.
- Made repository-filtered runs infer locally available Git checkouts from matching AI sessions.
- Added regression coverage for unattended agent output and Git roots outside `--dir`.

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
