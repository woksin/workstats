# HANDOFF — AI adapters (`src/ai.rs`, `src/cache.rs`)

Owned files: `src/ai.rs` and `src/cache.rs` only. Nothing outside them was touched.
Per the hard rules no cargo command was run (no build/test/check/clippy/fmt, nothing
touched the shared target dir). The only verification run was the standalone
`rustfmt --edition 2024 --check` on the two owned files — read-only, no target dir —
which confirms both parse and are already rustfmt-clean. Type checking is the
orchestrator's build; everything below is correct by inspection.

## A (CRITICAL) — Claude token totals ~2.08x inflated (fixed)

- `ClaudeRecord` now deserializes top-level `requestId`; `ClaudeMessage` now
  deserializes `id` (a new arm in the hand-written `deserialize_message` visitor).
- `parse_claude_file` keeps `counted_responses: HashMap<(message.id, requestId), usize>`
  indexing into `token_events`. A repeat of a key REPLACES the stored event
  (last occurrence wins — the repeats are identical apart from `timestamp`, and a
  streaming update can only grow, so the last one is the complete usage).
- A record with NEITHER id still pushes an event; nothing is dropped silently.
- Only token events are deduplicated. Activity points are untouched: they are unioned
  into intervals, so repeats there do not inflate time.
- Test: `claude_repeated_content_block_records_count_one_response` — 4 assistant
  records, two of them sharing `msg-one`/`req-one`, one with a different pair, one
  with no ids at all. Asserts 4 points, 3 token events, total 90 (not 120), and that
  the surviving shared event carries the LAST timestamp.

## B — range-pruned cache returned zero tokens (fixed)

- `file_time_range` now also chains `session.token_events` timestamps.
- `PARSER_VERSION` in `src/cache.rs` bumped 2 → 3 (with a comment saying what the
  version means), so entries written by an older build are recomputed rather than
  answered from — required both for this and for A.
- **Deliberately did NOT widen `Session::first_seen` / `last_seen`** (`src/model.rs`,
  not my file). Verified why: `aggregate.rs:246` uses `first_seen` for session
  eligibility and `aggregate.rs:709` uses both for committed-output pairing — those
  are activity semantics. Token events are filtered independently by their own
  timestamps at `aggregate.rs:139-156`, so nothing needs a wider `first_seen`.
  If the foundation/aggregate owner ever wants the token span on a `Session`, add a
  separate `token_span()` accessor rather than changing those two.
- Consequence the aggregate owner should know: a Copilot session can now be
  token-events-only (see C), i.e. `points.is_empty()` and `first_seen() == None`.
  The two loops above already `continue` on that, which is the correct behaviour —
  it contributes tokens, not activity.
- Tests: `file_time_range_covers_token_events_after_the_last_activity_point` in
  `src/ai.rs`, and `a_token_event_after_the_last_point_keeps_the_entry_in_range` in
  `src/cache.rs` (a `--since` between the last point and the shutdown usage must be a
  `Hit`, not a `Pruned` with the tokens cleared).

## C — Copilot dropped token events under a cwd with no points (fixed)

- The emit loop now iterates the union of `points_by_cwd` / `human_by_cwd` /
  `token_events_by_cwd` (`BTreeSet` of cwd keys), mirroring `parse_codex_file`.
  A cwd is emitted when it has points OR token events.
- Test: `copilot_usage_survives_a_context_change_after_the_last_activity`.

## D — Copilot subagent model overwrote the foreground model (fixed)

- The `data.model` assignment moved below the `is_agent_event` early return. The
  `subagent.completed` branch still reads `record.data.model` for its OWN interval,
  which is unchanged and correct.
- Test: `copilot_subagent_model_does_not_replace_the_foreground_model`.

## E — one bad cell discarded the whole OpenCode database (fixed)

- New `sqlite_text` / `sqlite_number` helpers read a cell through `Row::get_ref`
  (`ValueRef`) instead of a fixed Rust type, so a REAL `time_created` (OpenCode
  declares it NUMERIC) and a NULL `type`/`data`/`directory` cost ONE row.
- The closure now returns `(Vec<RawSession>, u64)`; skipped rows are added to
  `diagnostics.malformed_lines` with one warning naming the count and the file.
- Test: `opencode_skips_only_the_rows_it_cannot_read` — REAL `time_created`, a NULL
  `directory` row and a NULL `type` row; asserts 1 session, 1 point, 2 skipped.

## F — event-format sessions collapsed and flipped role (fixed)

- `parse_event_file` counts how many `(provider, session_id)` variants a file holds
  and, when there is more than one, suffixes `:{cwd}` (plus `:subagent` for the
  subagent variant, since two roles can share one cwd — that case cwd alone cannot
  separate). Single-variant ids are unchanged, so existing ids stay stable.
- Test: `event_sessions_keep_one_id_apart_across_directories_and_roles`, plus a new
  assertion in the existing open-events test that a single-variant id is untouched.

## G — legacy Gemini `.json` read one byte per syscall (fixed)

- Wrapped in `BufReader::with_capacity(128 * 1024, file)` and guarded by a new
  `pub const MAX_GEMINI_JSON_BYTES: u64 = 128 * 1024 * 1024`. `max_line_bytes` cannot
  bound a single-document file, and real legacy sessions reach tens of MB, so the
  bound is deliberately much larger than `MAX_JSONL_LINE_BYTES`. Oversize files take
  the existing `unreadable_files` + warning path (which also keeps them out of the
  cache).

## H — Codex counted re-emitted `token_count` events twice (fixed)

- `CodexTokenInfo` now also deserializes `total_token_usage`; `CodexTokenUsage`
  derives `Clone, Eq, PartialEq`. `codex_token_event` takes
  `previous_total: &mut Option<CodexTokenUsage>` (file-scoped state in
  `parse_codex_file`) and returns `None` when the cumulative total has not moved.
- The delta-vs-cumulative model is unchanged, as instructed: events still report
  `last_token_usage`.
- Test: `codex_token_count_repeated_at_an_unchanged_total_is_counted_once`. The
  existing `codex_token_count_events_report_per_turn_deltas` still passes — its two
  events have different totals.

## I — cache fingerprints missed out-of-band metadata (fixed)

- `codex_context_fingerprint(metadata, path)` falls back from `by_path` to `by_id`,
  keyed on the UUID at the end of the `rollout-<timestamp>-<thread id>.jsonl` name
  (`codex_rollout_id`). The fingerprint is computed before the file is read, so the
  in-file `session_id` is not available — the file name is the only pre-read source.
  A rollout whose in-file id differs from its file name still gets the constant, but
  the common case now invalidates.
- Gemini: `gemini_project_marker` (refactored out of `gemini_project_root`) returns
  the marker path AND its value; the fingerprint is now
  `gemini-v2:{size}:{mtime_ns}:{root}` (via `cache::file_context`) instead of the
  constant `gemini-v1`.

## K — `read_bounded_line` off-by-one (fixed)

- The trailing `\n` is no longer counted against `maximum`:
  `line.len() - usize::from(complete) > maximum`.

## Also done, outside the assigned list

- **Defect R (ai.rs half).** `parse_event_file` now increments
  `diagnostics.content_rejections` instead of `malformed_lines` for content-bearing
  records. The foundation agent had already added that field to `model::Diagnostics`
  and `output.rs:362` already prints it, so the field would otherwise always be zero.
  The existing test assertion was updated accordingly.

## Notes for the integrator

- `PARSER_VERSION = 3` means every user's warm cache is rebuilt once on the next run.
  That is intended (A and B both change what is stored/derived) and worth a CHANGELOG
  line from the docs agent.
- The time agent flagged `src/ai.rs` `timestamp - duration` in the Copilot
  `subagent.completed` branch as a possible overflow panic. Checked, not reachable:
  `duration_ms` is already guarded finite, `> 0`, and `<= 7 days`, and
  `parse_timestamp` only accepts RFC3339 (4-digit year, so no timestamp anywhere near
  `DateTime::MIN_UTC`). Left as-is rather than adding a guard that cannot fire.
- CSV/JSON shapes were not touched; no public type changed except the two new
  `pub const MAX_GEMINI_JSON_BYTES` and the private helpers listed above.
