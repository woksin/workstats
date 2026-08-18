# workstats — build plan and shared rules

Repo: `/Volumes/sourcecode/repos/woksin/workstats` (Rust CLI, edition 2024, Rust 1.88+).
Branch for all this work: `feature/explorer-and-configurable-categories`.

## HARD RULES FOR EVERY AGENT

1. **DO NOT RUN `cargo build`, `cargo test`, `cargo check`, `cargo clippy`, or
   `cargo fmt`.** The orchestrator builds and tests once, at the end, deliberately.
   Write code only. This is an explicit instruction from the repo owner.
2. **Stay inside your assigned files.** Another agent is editing the others right
   now. If you believe you must touch a file you do not own, DO NOT — write the
   needed change into `scratchpad/HANDOFF-<your-area>.md` instead and continue.
3. Match the surrounding code's style exactly: comment density, naming, error
   handling via `anyhow`, `Diagnostics` for user-visible warnings.
4. Comments explain WHY, not what. This codebase's comments are load-bearing
   explanations of non-obvious decisions. Do not add narration.
5. Write unit tests next to the code in `#[cfg(test)] mod tests`, in the existing
   style. You cannot run them; make them obviously correct by inspection.
6. When you finish, append a short summary of what you changed and any assumption
   you made to `scratchpad/HANDOFF-<your-area>.md`.

## PROJECT CONTEXT

`workstats` turns local Git history and AI-tool transcript metadata into a report
of human work time, Git output, and agent activity. Core privacy property: it never
reads prompt/response bodies or file contents, and makes no network calls except an
explicit `workstats update`. Preserve that property.

Key modules:
- `src/main.rs`      CLI (clap), orchestration, subcommands
- `src/ai.rs`        transcript adapters (claude/codex/gemini/copilot/opencode/events)
- `src/git.rs`       `git log --numstat` parsing, ignore globs
- `src/classify.rs`  file-area + diff-shape classification
- `src/aggregate.rs` bucketing into report rows
- `src/model.rs`     data types + serialized report shape
- `src/output.rs`    table / JSON / CSV rendering
- `src/cache.rs`     SQLite transcript index
- `src/timeutil.rs`  intervals, unions, calendar splitting
- `src/paths.rs`     source-root rules, config loading
- `src/sources.rs`   history discovery

Verified defects are in `scratchpad/AUDIT.md`, lettered A..V. Cite the letter in
your handoff notes.

## TARGET FEATURE 1 — Configurable classification

Today `Category` is a fixed 6-variant enum (`Source, Test, Docs, Config, Assets,
Other`) and `CategoryTally` is a fixed `[CategoryLines; 6]` array. The repo owner
wants categories to be user-configurable, including NEW categories beyond the
built-in six — specifically `ai`, `planning`, and `corpus`.

Design:
- Replace the enum with a runtime **registry** built once at startup: an ordered
  `Vec<CategoryDef>` where order IS match priority (first match wins). Categories
  are referenced by `usize` index everywhere a `Category` is used today.
- `CategoryTally` becomes a `Vec<CategoryLines>` sized to the registry, or a small
  wrapper struct owning that Vec. It must still be cheap to merge.
- A `CategoryDef` carries: `name`, and rule sets — `directories`, `directory_prefixes`,
  `directory_suffixes`, `extensions`, `names`, `name_prefixes`, `name_suffixes`,
  `stem_suffixes`, and optional `globs`.
- Built-in defaults reproduce TODAY'S behaviour exactly, including the recently
  added .NET/BDD test rules (`.specs`/`.tests` directory suffixes, `for_`/`when_`/
  `given_` directory prefixes, bare `given` directory, `when_`/`given_` filename
  prefixes) and the "checked against original casing" `CAMEL_TEST_SUFFIXES`.
- Config lives in the existing JSON config file (see `src/paths.rs` `load_config`,
  which already carries `source_roots` and `check_updates`). Shape:
  ```json
  { "categories": { "test": {"directory_prefixes": ["it_"]},
                    "ai":   {"directories": [".ai", ".claude"], "names": ["CLAUDE.md"]} },
    "category_mode": "extend" }
  ```
  `category_mode`: `"extend"` (default — user rules are added to the built-ins for
  that category, and unknown names create new categories) or `"replace"` (the named
  category's built-in rules are discarded).
- New categories must appear in the table, JSON `composition`, and CSV columns.
  CSV headers are derived from the registry, so they become config-dependent — that
  is acceptable and must be documented.
- `Shape` (diff shapes) currently maps 1:1 off the fixed enum. Generalise: a shape
  is either the dominant category's name, or `new code`/`revision`/`removal` when
  the dominant category is the one flagged `code_like` (source), or `mixed`.
  Mark `code_like` on a `CategoryDef` so a custom category can opt in.
- Bound the config: cap category count (e.g. 32) and rule count per category, and
  reject control characters in names. Follow the existing bounded-regex precedent in
  `src/paths.rs`.
- Add a `workstats classify <PATH>...` subcommand printing the matched category and
  WHICH RULE matched, so a user can debug their config. Also `--format json`.

## TARGET FEATURE 2 — Full TUI explorer

New `workstats ui` subcommand (NOT the default — the repo cut 1.0.0 today and
changing the default is a breaking change for later).

Use `ratatui` + `crossterm`. Add to `Cargo.toml` with pinned versions. The TUI must
not affect non-TUI runs: no new work on the normal code path, and stdout stays
machine-readable for `--format json|csv`.

Scope: full explorer with search.
- Drill-down: overview -> repo -> month/day -> category -> commit -> changed files -> diff
- Breadcrumb showing the current path; Esc/Backspace goes up; Enter descends
- Sortable columns; live filtering as you type
- Fuzzy search across repo names, file paths, and commit subjects
- Saved views (persist to the config directory, not the cache)
- A diff viewer, via `git show`. IMPORTANT: this is the first time the tool reads
  file contents. It must be display-only — never cached, never written to any
  report, never sent anywhere. Guard it and document it.
- Graceful degradation: if stdout is not a TTY, `workstats ui` must error clearly
  rather than emitting escape codes.
- Keybinding help overlay (`?`).

## SEQUENCING

Phase 1 (foundation) must land before Phases 2 and 3 touch the same types.
Phases run in workflow order; within a phase, agents own disjoint files.

## FILE OWNERSHIP (do not cross these lines)

- Foundation agent: `src/classify.rs`, `src/model.rs`, `src/aggregate.rs`,
  `src/paths.rs`, `src/git.rs`, `src/output.rs`, `src/main.rs`
- AI-adapter agent: `src/ai.rs`, `src/cache.rs`
- Time/CLI agent: `src/timeutil.rs`
- TUI agents: `src/tui/**` (all new), `Cargo.toml`
- Integration agent: `src/main.rs` (TUI wiring only), reconciliation
- Docs agent: `README.md`, `CHANGELOG.md`
