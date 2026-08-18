# workstats — verified defects

Every item below was reproduced against the real binary with fixtures or measured
against real local data. File:line refer to the repo at `/Volumes/sourcecode/repos/woksin/workstats`.

## A. CRITICAL — Claude token totals are ~2.08x inflated

`src/ai.rs:1585-1601`, struct `ClaudeMessage` at `src/ai.rs:211-216`.

Claude Code writes ONE JSONL `assistant` record per content block of a single API
response (text block, then each `tool_use` block). Every such record repeats the
SAME `message.id`, `requestId`, and a byte-identical `message.usage`, differing only
in `timestamp`. `parse_claude_file` pushes a `TokenEvent` per record, so one API
response is counted 2-12 times.

Measured over 40 real transcripts: 13,275 assistant records with usage but only
6,302 distinct `(message.id, requestId)` responses; 3,960 (63%) repeated.
Reported 4,340,178,840 tokens vs true 2,088,199,312 — **2.08x inflation**.

Fix: capture `message.id` and top-level `requestId` (neither is deserialized today)
and keep only one TokenEvent per `(message.id, requestId)` — last occurrence wins.
Records with no id must still be counted (do not silently drop them).

## B. HIGH — Range-pruned cache returns zero tokens (Copilot)

`src/ai.rs:2065-2086` (`file_time_range`), `src/cache.rs:147-164`, `src/cache.rs:190`.

`file_time_range` derives cached min/max from `points`, `human_points`, and
`exact_intervals` but NEVER from `token_events`. Copilot token events come from the
`session.shutdown` record (`src/ai.rs:969-996`), which is not an activity-point
record type (`src/ai.rs:1029-1037`), so the shutdown lies after `max_micros` — by up
to 1h45m in real data. When it falls on the other side of a day boundary, `lookup`
returns `Pruned` with `token_events` cleared, so the same query answers differently
warm vs cold.

Fix: include `token_events` timestamps in `file_time_range`. `Session::first_seen` /
`last_seen` (`src/model.rs:97-113`) share the omission — decide deliberately whether
they should too (they feed the committed-output pairing; tokens are not activity, so
prefer a separate accessor rather than widening first_seen/last_seen semantics).
Bump `PARSER_VERSION` in `src/cache.rs:11` so stale entries are recomputed.

## C. HIGH — Copilot token events dropped when shutdown cwd has no activity points

`src/ai.rs:1058-1085`. The emit loop iterates `points_by_cwd` and only removes the
matching `token_events_by_cwd` entry; token events under a cwd absent from
`points_by_cwd` are never emitted. A `session.context_changed` after the last
activity event loses the whole session's usage. `parse_codex_file` does this
correctly by building `cwd_keys` from the union of all maps (`src/ai.rs:1758-1762`).

Fix: mirror the Codex union approach.

## D. MEDIUM — Copilot subagent model overwrites the foreground model

`src/ai.rs:963-965` runs BEFORE the `is_agent_event` early return at
`src/ai.rs:1024-1028`, so `current_model` is updated from records that are then
discarded as subagent traffic; subsequent foreground points are misattributed.

Fix: move the `data.model` assignment below the `is_agent_event` guard.

## E. MEDIUM — One bad cell discards the entire OpenCode database

`src/ai.rs:1269-1273`, `src/ai.rs:1305-1317`. `row.get(...)?` propagates out of the
whole closure (`src/ai.rs:1203`), so a single NULL `type`/`data` or a REAL
`time_created` aborts the read and every OpenCode session is lost. OpenCode declares
`time_created` NUMERIC and SQLite may store it as REAL. The sibling column uses
`row.get(3).unwrap_or(None)`, so per-row tolerance was clearly the intent.

Fix: per-row tolerance — skip the offending row, count it, keep the rest.

## F. MEDIUM — Event-format sessions collapse and flip role

`src/ai.rs:1114-1156` keys sessions by `(provider, session_id, cwd, is_subagent)`,
but `src/aggregate.rs:20` / `:182-190` key by `(provider, session_id)` with
last-writer-wins. Codex/Copilot avoid this by appending cwd to the session id when a
file has several (`src/ai.rs:1795-1799`, `:1070-1073`); `parse_event_file` does not.
A foreground session is then reported as a subagent.

Fix: apply the same cwd-suffixing in `parse_event_file`.

## G. MEDIUM-LOW — Legacy Gemini `.json` read one byte per syscall

`src/ai.rs:813-815` uses `serde_json::from_reader(File)` with no `BufReader`;
serde_json's `IoRead` issues one syscall per byte. Measured ~90x slower (6.2s vs
0.07s on 19MB). It also bypasses `max_line_bytes`, so the file is read fully into
memory unbounded.

Fix: wrap in `BufReader` and add a size guard.

## H. LOW — Codex counts re-emitted `token_count` events twice

`src/ai.rs:1840-1865`. Codex sometimes re-emits a `token_count` event whose
cumulative `total_token_usage` is unchanged but which still carries the previous
`last_token_usage`. Measured 209/8150 events (2.6%), ~2.1% overcount.

Fix: skip an event whose `total_token_usage` is unchanged from the previous one.
NOTE: the delta-vs-cumulative choice itself is correct — do not change it.

## I. LOW — Cache fingerprints miss out-of-band metadata

`src/ai.rs:624-637` builds the Codex fingerprint from `metadata.by_path` only, but
`src/ai.rs:1742-1757` falls back to `metadata.by_id`; a rollout matched only by id
gets the constant `"codex-v2:none"` and never invalidates. Gemini has the same shape:
cwd comes from an external `.project_root` marker (`src/ai.rs:1413-1430`) while the
fingerprint is the constant `"gemini-v1"` (`src/ai.rs:690`).

Fix: fold the id-matched metadata and the marker's content/mtime into the fingerprint.

## J. LOW — Panic on an absurd duration flag

`src/timeutil.rs:141` (also `:279`, `:284`). `parse_duration` (`:39-41`) saturates at
`i64::MAX` microseconds and `DateTime + TimeDelta` panics on overflow.
`--gap-cap 99999999999999999999h` panics.

Fix: clamp in `parse_duration` to a sane maximum (e.g. one year) and reject beyond.

## K. NIT — `read_bounded_line` off-by-one

`src/ai.rs:2012`: `line` still holds the trailing `\n`, so the effective limit is
`MAX_JSONL_LINE_BYTES - 1`.

## L. Git rename paths are mangled (VERIFIED by me)

`git log --numstat` emits renames as a single field in three forms:
  `src/a.rs => tests/b.rs`         (full)
  `tests/{a.rs => b.rs}`           (basename brace)
  `{src => tests}/helper.rs`       (directory brace)
`src/git.rs:407` takes this whole string as the path, so `classify()`, the ignore
globs, `--path`, and `--path-exclude` all operate on a non-path. Verified: a file
moved `src/ -> tests/` with 3 additions and 2 deletions was booked to `source`.
Real repos hit this constantly (HomeApp: 758 such entries, content-factory: 122).

Fix: resolve to the NEW path before classifying/globbing — replace each `{a => b}`
with `b`, else take the right side of a bare `a => b`. Do NOT pass `--no-renames`;
that turns every large move into thousands of phantom added/deleted lines.

## M. Non-ASCII paths are octal-escaped and quoted (VERIFIED by me)

Git's `core.quotePath` defaults to true, so `tests/spørsmål_test.rs` arrives as
`"tests/sp\303\270rsm\303\245l_test.rs"`. The wrapping quote breaks extension
detection (`.rs"` != `.rs`) and breaks the ignore globs. Verified:
`src_æøå.rs` classified as `other` not `source`, and `node_modules/pakke_æøå.js`
was NOT ignored at all and counted as authored work.

Fix: pass `-c core.quotePath=false` to `git log` in `src/git.rs` (before the `log`
subcommand). Consider also pinning `LC_ALL` for stable output.

## N. CSV breaks negative numbers (VERIFIED by me)

`src/output.rs` `neutralize_formula` prefixes `'` to any cell starting with
`= + - @`. A negative `net_lines` therefore ships as `'-1`, breaking numeric parsing
in the "made for pipes" output. Verified: `net_lines = ['-1]`.

Fix: only neutralize when the cell is NOT a valid number (parse as f64 first).

## O. `--repo` filters sessions and commits asymmetrically

`src/main.rs:826-832` matches `session.repo || session.cwd || session.root`;
`src/git.rs:217-221` matches only `repo || cwd` — `root` is missing on the Git side
even though `describe()` computes it two lines earlier. A filter matching a
source-root label returns AI sessions with no Git output.

Fix: include `root` on the Git side.

## P. Diagnostics messages never reach the table output

`src/output.rs:338-350` prints four counters but never `diagnostics.messages`. A
user who typos a `--history` or `--events` path gets a clean-looking report with
silently missing data, exit 0. The messages exist and are in JSON output.

Fix: print the messages (bounded, e.g. first 5 plus a count) in the table footer.

## Q. `--events` replaces the default event log instead of adding to it

`src/main.rs:384`: the default event log is only appended when `event_paths` is
empty, so passing any `--events` silently drops everything written by
`workstats record`. README:260 says the default log "is loaded automatically".

Fix: always include the default log unless explicitly suppressed; document it.

## R. A privacy rejection is reported as a "malformed line"

A record carrying `content`/`prompt`/etc is rejected (correct) but counted as
`malformed_lines`, so the table says "1 malformed lines". The real message
("content-bearing event record(s) skipped") is JSON-only.

Fix: separate counter and a distinct user-facing message.

## S. `--raw` renders a flat global model list as if nested under the last provider

`src/output.rs:178-201` prints all providers, then a separately-sorted GLOBAL top-12
model list indented four spaces, so a model appears visually nested under an
unrelated provider. The `.take(12)` truncation is silent.

Fix: either nest models under their provider, or label the list as global and say
how many were omitted.

## T. Internal placeholder leaks into user-facing output

`<synthetic>` (`src/ai.rs:2045`) appears as a model name in the table, `--raw`, and
JSON. Alongside `unknown` (`src/main.rs:794`) and `—` for commits, there are three
spellings of "no model".

Fix: settle on one and render it consistently.

## U. Zero tests on the default table output

`src/output.rs` `print_table` is ~265 lines with ~12 branches and is the DEFAULT
output; no test invokes it. All integration tests use `--format json` or `csv`.

## V. Misc UX

- `--by-repo`/`--matrix`/`--by-dir` silently discard an explicit `--group-by` and
  each other (`src/main.rs:308-319`) — should be a clap `conflicts_with` group.
- Bad `--dir` exits 0 with an all-zero report; the diagnostic exists internally.
- Error messages omit the offending flag and value.
- `--top` bounds the table but not the sparkline built from all rows.
- Summary block label widths are inconsistent (values land in columns 23/22/25);
  banner rule is 94 wide while the table rule is 106.
- CSV lacks `change_shapes`, summary, methodology, diagnostics that JSON has.
- CSV `first_seen`/`last_seen` are UTC while `day`/`month` group keys are local, so
  re-deriving the month from `first_seen` contradicts the `month` column.
- JSON has duplicate aliases: `agent_wall_seconds` == `deduplicated_active_seconds`,
  `parallel_agent_seconds` == `attributed_active_seconds` == row `active_seconds`.
