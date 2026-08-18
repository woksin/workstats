# HANDOFF — integration (`src/main.rs`)

Owned and edited: `src/main.rs` only. `src/tui/mod.rs` already existed, so I did
not touch it (my brief allowed creating it only if missing). No cargo command
was run; verification was standalone `rustfmt --edition 2024 --check src/main.rs`,
which follows every `mod` declaration and therefore parsed the whole crate
including `src/tui/**`.

---

## 0. BLOCKERS THE ORCHESTRATOR MUST HANDLE (files I do not own)

### 0.1 `Cargo.lock` has no `ratatui` / `crossterm` — CI will fail on `--locked`

`Cargo.lock` is tracked (`git ls-files` lists it) and unmodified, and contains
neither crate. `.github/workflows/ci.yml` runs

```
cargo build --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

All three fail with "the lock file needs to be updated but --locked was passed"
until the lock is regenerated. **After your first `cargo build`, commit the
regenerated `Cargo.lock`.** The TUI-core agent deliberately left it alone
(`HANDOFF-tui-core.md` §0).

### 0.2 `tests/rust_cli.rs` must pass `--no-default-events` (consequence of fix Q)

Requested by `HANDOFF-foundation.md` §"NEEDED FROM OTHER AGENTS" item 2 and not
applied by anyone — the file is unmodified in `git status`. I did not edit it
because it is not mine.

`sources_and_open_event_recording_form_a_complete_integration_path` (line 163)
asserts `session_count == 1` while passing `--events <tmp>`. Fix Q now *always*
loads the developer's real `~/.local/share/workstats/events.jsonl` as well, so on
any machine that has ever run `workstats record` the assertion fails. One-line
fix — add to that `run(&[...])` array:

```rust
        "--no-default-events",
```

### 0.3 `src/timeutil.rs` is not rustfmt-clean — `cargo fmt --all -- --check` will fail

Three spots, all introduced by the time agent. `rustfmt --edition 2024 --check
src/main.rs` reports only these and nothing else in the whole tree:

* `src/timeutil.rs:46` — `Ok(Duration::microseconds((seconds * 1_000_000.0).round() as i64))`
  must wrap.
* `src/timeutil.rs:550` — two `assert_eq!(parse_timestamp(...).unwrap(), intervals[0].start/.end)`
  lines must wrap.
* `src/timeutil.rs:566` — `assert_eq!(BTreeMap::from([("m", 900.0), ("n", 300.0)]), seconds_by_model(&intervals))`
  must wrap.

Everything else — `src/*.rs` and all of `src/tui/**` — is clean.

---

## 1. What landed in `src/main.rs`

### 1.1 `mod tui;` and the `workstats ui` subcommand

* `mod tui;` added between `mod timeutil;` and `mod update;`. Nothing in
  `src/tui/**` compiled before this.
* The report-shaping flags were lifted out of `Arguments` into a new
  **`ReportArguments`** (`#[derive(Debug, Args)]`), which is `#[command(flatten)]`ed
  into `Arguments` **and** carried by `Command::Ui(Box<ReportArguments>)`. That is
  what makes `workstats ui` honour `--dir`, `--since/--until`, `--repo`,
  `--repo-exact`, `--provider`, `--group-by` and the rest with one definition
  rather than a parallel one that can drift.
* `Arguments` is now only `{ command: Option<Command>, report: ReportArguments }`.
  Its `#[command(...)]` attributes (name/version/about/after_help) are unchanged.
* `run` became `fn run(arguments: ReportArguments, presentation: Presentation)`.
  The parameter kept the name `arguments`, so the ~250-line body is otherwise
  untouched.
* `Presentation::{Print, Explore}` decides only the last step. The explorer
  branch sits immediately after `progress.finish(...)`:

  ```rust
  if presentation == Presentation::Explore {
      return tui::run(&report, commits);
  }
  ```

  `commits` is owned and free there (`build_report` only borrowed it), which is
  the shape `HANDOFF-tui-core.md` §6 asked for. Non-explorer runs execute exactly
  the same code as before this change — no extra work, stdout untouched.
* `workstats ui --format json|csv` is refused **before any scanning** rather than
  silently ignored.

**Invocation shape:** the flags go *after* the subcommand — `workstats ui --dir .
--since 2026-01`. Flags placed *before* it (`workstats --dir . ui`) are parsed
into the top-level `ReportArguments` and then unused, which is what already
happens for `sources`, `classify`, `record` and `update`. I considered
`args_conflicts_with_subcommands = true` to make that a hard error and rejected
it: if clap's conflict check did not exclude default-valued args it would break
*every* subcommand invocation, and I cannot compile to find out. See §3.3 if you
want it after all.

### 1.2 AUDIT V — the grouping shortcuts

`--by-repo`, `--matrix` and `--by-dir` each rewrote `dimensions` wholesale, so
two together, or either with `--group-by`, meant one silently won.

* `--group-by` is now `Option<String>` with `conflicts_with_all = ["by_repo",
  "matrix", "by_dir"]`. It lost `default_value = "repo"` **on purpose**: with a
  default, "the user asked for repo" and "nobody said" are the same state to
  clap's conflict check. The default now lives in `const DEFAULT_GROUP_BY: &str
  = "repo"` and is documented in the flag's help text instead of clap's
  `[default: repo]` line.
* `--by-repo` gets `conflicts_with_all = ["matrix", "by_dir"]`, `--matrix` gets
  `conflicts_with = "by_dir"`. Conflicts are symmetric in clap, so those three
  declarations cover all six pairs among the four flags.
* Their help strings now read `Alias for --group-by month,repo` /
  `repo,month` / `cwd`, which is what they actually do.
* The whole computation moved into `fn grouping_dimensions(&ReportArguments) ->
  Result<Vec<String>>` so it is testable without building a report. Its logic,
  including both `bail!` messages, is byte-identical to what was inline.

### 1.3 AUDIT V — a non-existent `--dir`

New `fn scan_directory(explicit, from_environment, current) -> Result<PathBuf>`.
It takes its three candidates as arguments instead of reading the environment so
the precedence *and* the message are unit-testable. A path that is not a
directory is now an error naming where the value came from:

```
workstats: --dir does not name an existing directory: /repos/wigdet
workstats: WORKSTATS_DIR does not name an existing directory: /repos/wigdet
workstats: the current working directory does not name an existing directory: /gone
```

It fires regardless of `--no-git`: a typo'd scan root is a user error either
way, and it would otherwise be reported as `inputs.git_root` in the JSON.

### 1.4 AUDIT V — parse errors name the flag and the value

* `duration_flag(flag, value)` and `bound_flag(flag, value, until)` wrap
  `timeutil::parse_duration` / `parse_bound` with `anyhow` context. All five call
  sites use them: `--gap-cap`, `--human-idle`, `--review-credit`, `--since`,
  `--until`. Example: `workstats: invalid --gap-cap "5x": duration must look like
  30s, 5m, or 1h`.
* `workstats record`'s three RFC 3339 errors now echo the value —
  `invalid --timestamp "2026-13-01"`, and the same for `--started-at` /
  `--completed-at`.

### 1.5 Tests added (`src/main.rs::tests`, all correct by inspection only)

* `the_ui_subcommand_takes_the_report_flags`
* `the_grouping_shortcuts_expand_and_default_to_repo`
* `the_grouping_shortcuts_conflict_instead_of_overriding_each_other`
* `a_bad_duration_or_date_names_the_flag_and_the_value`
* `a_missing_scan_directory_is_an_error_that_names_where_it_came_from`

plus a `report_arguments(&["--flag", ...])` helper that parses through
`Arguments` so the tests exercise the real clap definition.

---

## 2. Reconciliation — what I checked and what I found

### 2.1 Registry vs TUI: no contradiction

`src/tui/state.rs` uses `crate::classify::{CategoryTally, active_registry}`,
calls `registry.classify(path) -> usize`, stores `FileRecord::category: usize`,
carries `CategoryTally` on `CommitRecord`, and builds its category list from
`registry.names()`. That is exactly the registry design in PLAN.md — nothing
assumes six categories or a fixed order. The Category *level* has static
`&'static [Column]` because categories are its **rows**, not its columns, so a
configured registry needs no change there. Nothing needed resolving.

`main.rs` installs the registry (`classify::install(config.category_registry()?)`)
before any classification, and `tui::run` is reached long after, so
`active_registry()` inside the explorer is the configured one and not the
built-in fallback.

### 2.2 Signatures verified against the files that define them

| used by | item | defined |
|---|---|---|
| `main.rs` | `tui::run(&Report, Vec<GitCommit>) -> Result<()>` | `src/tui/mod.rs:29` |
| `tui/mod.rs` | `App::new(&Report, Vec<GitCommit>)` | `src/tui/app.rs:66` |
| `tui/mod.rs` | `event::run(&mut DefaultTerminal, &mut App)` | `src/tui/event.rs:119` |
| `tui/state.rs` | `paths::{default_config_path, home_dir}` | `src/paths.rs:262, :245` — both `pub` |
| `tui/state.rs` | `timeutil::{local_date, local_month}` | `src/timeutil.rs:422, :426` — both `pub` |
| `tui/diff.rs` | `git::git_executable()` | `src/git.rs:46` — `pub` |
| `tui/state.rs` | every `Report`/`Summary`/`ReportRow`/`GitCommit` field it reads | present in `src/model.rs` |

Every `App` accessor listed in `HANDOFF-tui-core.md` §3 is called from
`views.rs` or `event.rs` (I checked all 24), and `jump_to` is called from within
`app.rs`, so nothing should trip `dead_code` under `-D warnings`.

### 2.3 Requests from other agents that are NOT satisfied (none block the build)

1. **`GitCommit::subject`** (`HANDOFF-tui-core.md` §7, echoed by search/diff).
   `src/model.rs` still has no `subject` and `src/git.rs` still asks
   `--pretty=format:W%x09%H%x09%aI`. `state.rs::describe_change` derives a change
   summary instead, which is what the explorer and the search index show. Fuzzy
   search over *commit subjects* therefore does not exist yet. Switching it on
   later is: add `%x09%s` to the pretty format, add the field, fill it in
   `parse_git_log`, then one line in `state.rs::describe_change`.
2. **Per-file `+/-`** (`HANDOFF-tui-core.md` §7). `GitCommit::files` is still
   `Vec<String>`, so the Commit level cannot show per-file line counts.
3. **`state.rs` number formatting** (`HANDOFF-tui-views.md` §3). Still divergent:
   `group_digits` (`state.rs:893`) pushes `' '`, not `','`, so the status bar
   says `1,204` while the column beside it says `1 204`; `Field::hours` renders
   `5.1` where `output.rs` renders `5h 09m`; `report_summary`'s Tokens row is
   ungrouped digits rather than `18.4M`. Cosmetic, three one-line edits in a file
   I do not own — and any `state.rs` test asserting the current format has to
   move with them.
4. **`unicode-width` as a direct dependency** (`HANDOFF-tui-views.md` §4).
   Not added. Consequence is a clipped CJK/emoji path, never a panic.
5. **`sample_app` test fixture in `app.rs`** (`HANDOFF-tui-views.md` §4). Not
   added, so `views::draw` has no full-frame smoke test.
6. **`src/tui/mod.rs` module doc** (`HANDOFF-tui-search-diff.md` §3). It says the
   diff level "shells out to `git show`"; for a single file `diff.rs` now runs
   `git log --max-count=1 --follow`. Cosmetic wording only; I left the file alone
   because it already existed and is the TUI-core agent's.

---

## 3. Things I could not verify without compiling — check these first if the build breaks

**3.1 `conflicts_with_all` on flags that carry a clap default.**
`--by-repo`/`--matrix`/`--by-dir` are `ArgAction::SetTrue`, so each is present in
the matcher with `ValueSource::DefaultValue` even when not passed. I am relying
on clap 4's validator filtering on `ArgMatcher::check_explicit`, which returns
false for `ValueSource::DefaultValue`. If that is wrong, the symptom is loud and
unmistakable: **`workstats --by-repo` alone errors with an argument conflict**,
and `the_grouping_shortcuts_conflict_instead_of_overriding_each_other` fails on
its `is_ok()` assertions. Fix would be to drop the `conflicts_with*` attributes
and check the combination by hand in `grouping_dimensions`. (I already removed
the analogous risk on `--group-by` by making it `Option<String>`.)

**3.2 `Command::Ui(Box<ReportArguments>)`.** Relies on clap's
`impl<T: Args> Args for Box<T>`. The existing `Record(Box<RecordArguments>)`
compiles today, so this should be safe.

**3.3 If you *do* want `workstats --dir . ui` to be an error** rather than
silently ignored, add `args_conflicts_with_subcommands = true` to the
`#[command(...)]` on `Arguments`. Test it against `sources`/`record` immediately
— that is the setting whose failure mode I was unwilling to risk blind.

**3.4 `ratatui` / `crossterm` API surface.** I did not verify any ratatui call in
`views.rs`/`event.rs`/`mod.rs` against the 0.30 API; the TUI agents did that by
reading the crate sources. I did confirm from the cached crates.io metadata in
the scratchpad that `ratatui 0.30.2` and `crossterm 0.29.0` are the newest
non-yanked releases of those lines. One item on the seam is not in the TUI-core
API list: `views.rs` imports `ratatui::widgets::Padding`.

**3.5 First run after this lands rebuilds every user's transcript cache**
(`PARSER_VERSION` 2 → 3, `HANDOFF-ai.md`). Expected, and worth a CHANGELOG line.

---

## 4. For the docs agent

`README.md` and `CHANGELOG.md` are still unmodified. Beyond what the other
handoffs already list, `workstats ui` needs:

* the invocation shape — `workstats ui [FILTERS]`, flags **after** `ui`;
* that it needs an interactive terminal on stdout and errors out (exit 2) when
  redirected or when `TERM=dumb`, and that `--format json|csv` is refused;
* `WORKSTATS_VIEWS`, and that saved views live in the **config** directory
  (`~/.config/workstats/views.json`), never the cache;
* that the diff viewer is the only feature that reads file contents, display-only
  — never cached, never in a report, never in a saved view — and that large diffs
  are truncated (2 MiB / 20 000 lines / 2 000 characters per line);
* AUDIT V behaviour changes: `--by-repo`/`--matrix`/`--by-dir`/`--group-by` are
  now mutually exclusive (previously one silently won), a non-existent `--dir`
  is now an error instead of an all-zero report with exit 0, and `--group-by`'s
  default no longer appears as clap's `[default: repo]` (it is in the help text).
