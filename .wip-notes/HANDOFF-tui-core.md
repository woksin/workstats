# HANDOFF — TUI core (Cargo.toml, src/tui/mod.rs, app.rs, event.rs, state.rs)

Owner: TUI-core agent. **This file is the contract.** `views.rs`, `search.rs` and
`diff.rs` are written by sibling agents against exactly the signatures below.
Nothing here is provisional — if you need a change, write it into YOUR handoff
file, do not edit my files.

**Status: DONE.** All four owned files are written and parse clean
(`rustfmt --edition 2024 --check` passes on every one; no `cargo` was run, per
PLAN.md rule 1). The crate does not compile yet because `src/tui/views.rs`,
`src/tui/search.rs` and `src/tui/diff.rs` do not exist yet, and because
`src/main.rs` still lacks `mod tui;` — see §6.

File names are load-bearing: `mod.rs` declares `mod app; mod diff; mod event;
mod search; mod state; mod views;`, so the three files must be exactly
`src/tui/views.rs`, `src/tui/search.rs`, `src/tui/diff.rs`.

---

## 0. Dependencies (pinned, verified against crates.io)

Added to `[dependencies]`, alphabetically, nothing else:

```toml
crossterm = "0.29"
ratatui = "0.30"
```

Verified by downloading and reading the actual crate sources, not from memory:

* `ratatui 0.30.2` — `edition = "2024"`, `rust-version = "1.88.0"`. Exact match
  for this repo's MSRV. Default features are
  `["all-widgets", "crossterm", "layout-cache", "macros", "underline-color"]`.
* `ratatui-crossterm 0.1.2` (pulled in by the `crossterm` feature) defaults to
  `crossterm_0_29`, i.e. **crossterm 0.29**. So the direct `crossterm = "0.29"`
  dependency unifies to the same crate version ratatui uses — `KeyEvent` and
  friends are one type on both sides. Do not bump one without the other.
* `crossterm 0.29.0` default features include `events`; no extra features needed.

`Cargo.lock` is not mine and was not touched; cargo regenerates it on the first
build (the release build already dropped `--locked`, commit `525e2cd`).

### ratatui 0.30 API notes (this is NOT 0.29 — check before you type)

Confirmed present in the 0.30.2 / ratatui-core 0.1.2 / ratatui-widgets 0.3.2
sources:

* `ratatui::{Frame, Terminal, TerminalOptions, Viewport, DefaultTerminal}`
* `ratatui::{run, init, try_init, restore, try_restore}` (module `ratatui::init`);
  `try_init` enters raw mode + alternate screen **and installs a panic hook**
* `ratatui::crossterm` re-export (same crate as the direct `crossterm` dep)
* `ratatui::backend::{Backend, CrosstermBackend, TestBackend, WindowSize, ClearType}`
* `ratatui::layout::*`, `ratatui::style::{Color, Modifier, Style, Stylize}`,
  `ratatui::text::{Line, Masked, Span, Text}`, `ratatui::symbols`, `ratatui::border`
* `ratatui::widgets::{Bar, BarChart, BarGroup, Block, BorderType, Borders, Cell,
  Clear, Fill, Gauge, HighlightSpacing, LineGauge, List, ListDirection, ListItem,
  ListState, Paragraph, RenderDirection, Row, Scrollbar, ScrollbarOrientation,
  ScrollbarState, Sparkline, StatefulWidget, Table, TableState, Tabs, Widget, Wrap}`
* `ratatui::prelude::*` re-exports the common set.
* `Frame::area()`, `Frame::render_widget`, `Frame::render_stateful_widget`,
  `Frame::set_cursor_position`, `Frame::buffer_mut`
* `Terminal::draw<F: FnOnce(&mut Frame)>(&mut self) -> Result<CompletedFrame<'_>, B::Error>`
  (`B::Error` is `std::io::Error` for `CrosstermBackend<Stdout>`)
* `Terminal` already restores the cursor from its own `Drop`.

Note `ratatui::widgets::Cell` exists — which is why my row-cell type is called
**`Field`**, not `Cell`. No import collision.

---

## 1. Entry point

```rust
// src/tui/mod.rs
pub fn run(report: &crate::model::Report, commits: Vec<crate::model::GitCommit>) -> anyhow::Result<()>
```

It refuses to start when stdout is not a TTY or `TERM=dumb` (same judgement
`src/progress.rs` makes), builds the `App` **before** taking the terminal over,
and drives the loop through a `TerminalGuard` whose `Drop` calls
`ratatui::restore()` — so an `Err` out of the loop or a panic unwinding through
it still hands the shell back.

---

## 2. `src/tui/state.rs` — the shared vocabulary

```rust
/// One level of the drill-down. Variant order IS the drill-down order.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelKind { Overview, Repo, Period, Category, Commit, File, Diff }

impl LevelKind {
    pub const fn child(self) -> Option<Self>;
    pub const fn label(self) -> &'static str;   // "repositories", "periods", …
}

pub struct Level {
    pub kind: LevelKind,
    pub key: String,        // row identity chosen at the PARENT level
    pub label: String,      // human-readable form of `key`
    pub selected: usize,
    pub offset: usize,
}
impl Level { pub fn new(kind, key: impl Into<String>, label: impl Into<String>) -> Self; }

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Grain { #[default] Month, Day }
impl Grain {
    pub const fn label(self) -> &'static str;   // "month" / "day"
    pub const fn toggled(self) -> Self;
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Sort { pub column: usize, pub descending: bool }

pub struct Column {
    pub title: &'static str,
    pub width: u16,     // 0 means "take whatever is left"
    pub numeric: bool,  // right-align when rendering
}

/// One cell. `value` is the sort key when present; otherwise `text` sorts
/// case-insensitively.
#[derive(Clone, Debug)]
pub struct Field { pub text: String, pub value: Option<f64> }

/// One row of the current level, already filtered and sorted.
#[derive(Clone, Debug)]
pub struct Entry { pub id: String, pub fields: Vec<Field> }
impl Entry { pub fn haystack(&self) -> String; }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode { #[default] Normal, Filter, Search, SaveView, Views }

/// The `?` overlay's contents: (keys, what it does). Render these verbatim.
pub const KEYBINDINGS: &[(&str, &str)];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SavedView {
    pub name: String,
    pub path: Vec<String>,   // drill-down keys from the overview downwards
    pub grain: Grain,
    pub sort: Sort,
    pub filter: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SavedViews { pub version: u32, pub views: Vec<SavedView> }
```

`entry.fields.len()` always equals `app.columns().len()`. At `LevelKind::Diff`
there are no columns and no rows — render `app.diff()` instead.

### Saved-view file format

Persisted next to the config file — `default_config_path().parent()/views.json`,
i.e. `~/.config/workstats/views.json` (or `$XDG_CONFIG_HOME/workstats/`,
`%APPDATA%\workstats\`), overridable with `WORKSTATS_VIEWS`. **Never the cache
directory.** Written through a `NamedTempFile` + rename so an interrupted save
cannot truncate the file. A missing or unparseable file is an empty bookmark
list, never an error.

```json
{
  "version": 1,
  "views": [
    {
      "name": "widget monthly",
      "path": ["/Users/me/repos/widget", "2026-06", "source"],
      "grain": "month",
      "sort": { "column": 2, "descending": true },
      "filter": "src/"
    }
  ]
}
```

Bounded like the rest of the config: at most 64 views, name 1–64 bytes with no
control characters, path at most 5 keys with no control characters and each key
at most 4096 bytes. A saved view never stores the diff level — restoring one
must not read a file's contents unasked.

---

## 3. `src/tui/app.rs` — what `views.rs` may call

`App` has **no lifetime parameter**; everything is owned.

```rust
impl App {
    // --- what to draw ------------------------------------------------------
    pub fn level(&self) -> LevelKind;
    pub fn breadcrumb(&self) -> Vec<String>;      // ["workstats", "widget", "2026-06", …]
    pub fn columns(&self) -> &'static [Column];
    pub fn rows(&self) -> &[Entry];               // already filtered and sorted
    pub fn selected(&self) -> usize;              // index into rows()
    pub fn offset(&self) -> usize;                // scroll offset to seed TableState
    pub fn sort(&self) -> Sort;
    pub fn grain(&self) -> Grain;
    pub fn filter(&self) -> &str;
    pub fn mode(&self) -> Mode;
    pub fn input(&self) -> &str;                  // Search / SaveView text being typed
    pub fn status(&self) -> Option<&str>;         // one-line footer message
    pub fn help_visible(&self) -> bool;
    pub fn summary(&self) -> &[(String, String)]; // report facts for the header strip
    pub fn saved_views(&self) -> &[SavedView];
    pub fn views_selected(&self) -> usize;
    pub fn search_hits(&self) -> &[SearchRow];
    pub fn search_selected(&self) -> usize;
    pub fn diff(&self) -> Option<&DiffView>;      // Some(..) only at LevelKind::Diff
    pub fn diff_offset(&self) -> usize;           // first visible diff line
    pub fn should_quit(&self) -> bool;

    // --- what the renderer tells the app ----------------------------------
    /// The table body height in rows. Call it EVERY frame: PageUp/PageDown
    /// have no other way to learn the page size.
    pub fn set_viewport(&mut self, rows: usize);
    /// Sync the scroll offset back after rendering so it survives the frame
    /// and a trip down into a child level and back.
    pub fn set_offset(&mut self, offset: usize);

    // --- driven by the event loop, not by the renderer ---------------------
    pub fn new(report: &Report, commits: Vec<GitCommit>) -> Self;
    pub fn apply(&mut self, action: Action);
    pub fn jump_to(&mut self, path: &[String]);
}

/// One row of the search overlay.
pub struct SearchRow {
    pub label: String,
    pub kind: &'static str,   // "repo" | "commit" | "file"
    pub indices: Vec<usize>,  // matched CHAR positions in `label`, for highlighting
    // (a private `path` field carries the jump target; the renderer never needs it)
}
```

**Every accessor above must actually be used by `views.rs`.** CI runs
`cargo clippy --all-targets -- -D warnings`, and an unused item in a binary
crate is a `dead_code` warning that fails the build. If you genuinely do not
want one, say so in your handoff and I will drop it.

### The single rendering entry point (views agent implements this)

```rust
// src/tui/views.rs — this is the ONLY pub item the file needs
use ratatui::Frame;
use super::app::App;

pub fn draw(frame: &mut Frame, app: &mut App);
```

`&mut App` is deliberate: it is how `set_viewport` and `set_offset` work.
The event loop calls it as `terminal.draw(|frame| views::draw(frame, app))?`.

Suggested layout — yours to refine:

```
┌ breadcrumb (app.breadcrumb() joined with " › ")                ┐
├ summary strip (app.summary(): label / value pairs)             ┤
├ table: app.columns() headers, app.rows() body, app.selected()  │
│        highlighted; mark the sort column and its direction.    │
│   ...at LevelKind::Diff instead: app.diff() coloured by kind,  │
│      scrolled to app.diff_offset(), "truncated" noted.         │
├ footer: mode, filter or input text, sort, grain, app.status()  ┤
└────────────────────────────────────────────────────────────────┘
overlays (draw over the table, Clear first):
  help            when app.help_visible()          → state::KEYBINDINGS
  saved views     when app.mode() == Mode::Views   → app.saved_views(), app.views_selected()
  search          when app.mode() == Mode::Search  → app.input(), app.search_hits(), app.search_selected()
  save-view name  when app.mode() == Mode::SaveView→ app.input()
  filter is NOT an overlay — it filters live, show app.filter() in the footer
```

A `Column` with `width == 0` takes the remaining space (`Constraint::Fill(1)`);
the rest are `Constraint::Length(width)`. `numeric` means right-aligned.
Keep it readable on both a dark and a light terminal: do not paint a background
over the whole page.

---

## 4. `src/tui/search.rs` — search agent implements this

Called from `app.rs`. It must NOT depend on `state.rs` or `app.rs` — that keeps
it unit-testable on plain strings.

```rust
/// Fuzzy subsequence score for `needle` inside `haystack`; higher is better.
/// `None` when it does not match at all. An EMPTY NEEDLE MUST RETURN Some(0)
/// — the live filter calls this on every keystroke and an empty filter matches
/// everything. Case-insensitive. A match at a word boundary or at the start
/// should outscore one buried mid-token.
pub fn score(needle: &str, haystack: &str) -> Option<i64>;

/// A prebuilt haystack over repository names, commit lines and file paths.
pub struct Index;   // no lifetime parameter

/// One thing the user can jump to.
pub struct Target {
    /// "repo" | "commit" | "file". A &'static str rather than another enum
    /// crossing the seam.
    pub kind: &'static str,
    /// Displayed, and what the needle is matched against.
    pub label: String,
    /// Drill-down keys from the overview downwards, ready for App::jump_to.
    pub path: Vec<String>,
}

/// Ordered best-first by `Index::query`, so the score does not escape.
pub struct Hit {
    /// Index into the Vec<Target> that was passed to Index::build.
    pub target: usize,
    /// Matched CHAR positions in that target's label.
    pub indices: Vec<usize>,
}

impl Index {
    pub fn build(targets: Vec<Target>) -> Self;
    /// Best `limit` matches, best first. An empty needle returns an empty Vec
    /// (the overlay shows nothing until something is typed).
    pub fn query(&self, needle: &str, limit: usize) -> Vec<Hit>;
    pub fn target(&self, index: usize) -> Option<&Target>;
}
```

Exactly those four items are used by `app.rs`: `score`, `Index`, `Target`,
`Hit`. Anything else you add must be `fn` (private) or it will trip
`dead_code` under `-D warnings`.

Constraints:
* `Index::build(Vec::new())` must work — it is how the tests construct an App.
* The index can hold up to `MAX_SEARCH_TARGETS = 200_000` entries and `query`
  runs on **every keystroke**. A linear scan with a cheap first-character
  reject is fine; a full N×M dynamic program per entry is not.
* `score` is also the live row filter (`app.rs::rebuild` calls
  `score(&self.filter, &row.haystack())`), so it must be cheap on short
  haystacks too.

`app.rs` builds the `Vec<Target>` (repositories, every commit, and each distinct
file deduplicated to its most recent commit). You own the algorithm.

---

## 5. `src/tui/diff.rs` — diff agent implements this

```rust
use std::path::PathBuf;
use anyhow::Result;

pub struct DiffRequest {
    /// The repository working directory (GitCommit::cwd). Run `git -C <cwd>`.
    pub cwd: PathBuf,
    pub sha: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffKind { Meta, Hunk, Added, Removed, Context }

pub struct DiffLine { pub kind: DiffKind, pub text: String }

pub struct DiffView {
    pub title: String,
    pub lines: Vec<DiffLine>,
    /// True when the diff was cut short by the size cap.
    pub truncated: bool,
}

pub fn load(request: &DiffRequest) -> Result<DiffView>;
```

`app.rs` calls `diff::load` once, when Enter is pressed at the File level, and
keeps the `DiffView` in a private field that is set to `None` the moment the
level is left or the grain changes.

Hard requirements (from PLAN.md — this is the first time the tool reads file
contents, so it is the one place the privacy property could break):

* **Display only.** Never write a `DiffView` to the cache, a report, a saved
  view, or any file. Never log it. Never return an owned copy to a caller that
  could store it.
* Shell out the way `src/git.rs` does: find git with `crate::git::git_executable()`,
  pass `--no-pager` and `-c core.quotePath=false`, and put the path after `--`
  so a path beginning with `-` cannot be read as a flag. Suggested:
  `git --no-pager -C <cwd> -c core.quotePath=false show --no-color
  --format=%H%n%an%n%aI%n%s --patch -- <path>`
* Cap it: stop at roughly 2 MiB or 20 000 lines and set `truncated`. A binary
  file must come back as a single `Meta` line, not as bytes.
* Errors are `anyhow` with context naming the sha and the path. `App` turns an
  `Err` into a status-bar message and stays where it is — it does not push the
  diff level.
* Add a module doc comment stating the display-only rule. It is load-bearing.

---

## 6. Notes for the INTEGRATION agent (`src/main.rs`)

1. Add `mod tui;` next to the other module declarations. **Nothing in
   `src/tui/**` compiles until this lands.**
2. Expose it as a subcommand — **not** the default; the repo cut 1.0.0 and
   changing the default would be breaking. The explorer needs the same report
   the normal path builds, so the least invasive wiring is a branch inside
   `run()`, after the report is built and before any `print_*`:
   ```rust
   if arguments.ui {
       return tui::run(&report, commits);
   }
   ```
   `commits` is still owned and unused at that point (`build_report` only
   borrowed it), so it moves cleanly. That borrow is why `tui::run` takes
   `(&Report, Vec<GitCommit>)` rather than two borrows or two owned values.
   If you prefer a real `workstats ui` subcommand it has to re-run the same
   pipeline; either shape works against this signature.
3. Non-TUI runs must be unaffected: no extra work on the normal path, stdout
   stays machine-readable for `--format json|csv`. `tui::run` errors out when
   stdout is not a TTY, so a piped `workstats ui` reports
   "`workstats ui` needs an interactive terminal on stdout; use `--format json`
   or `--format csv` …" and exits 2 through the existing `main` handler,
   instead of spraying escape codes.
4. `README.md` / `CHANGELOG.md` (docs agent): mention `WORKSTATS_VIEWS`, that
   saved views live in the **config** directory, and that the diff viewer is
   the only feature that reads file contents and does so display-only.

## 7. Requests for files I do not own

* **`src/git.rs` — commit subjects.** `--pretty=format:W%x09%H%x09%aI` carries no
  subject and `GitCommit` has no `subject` field, so the "fuzzy search across
  commit subjects" half of the PLAN cannot be built as written. Requested:
  append `%x09%s` to the pretty format, add `pub subject: String` to `GitCommit`
  (`src/model.rs`), and fill it in `parse_git_log`. Until then the explorer and
  the search index use a **derived** change summary (`"7 files · source, test"`),
  which is honest and useful but is not the commit message. Switching
  `CommitRecord::summary` over afterwards is a one-line edit in
  `src/tui/state.rs` (`describe_change`).
* **`src/git.rs` — per-file line counts.** `GitCommit::files` is `Vec<String>`;
  the numstat additions/deletions are folded into the commit total and the
  category tally, so the Commit level cannot show `+/-` per file. A
  `Vec<ChangedFile { path, additions, deletions }>` would fix that. Not blocking.

## 8. Design decisions worth knowing

* **Drill-down is an explicit stack** (`Vec<Level>`), never below one element.
  Each frame keeps its own `selected` and `offset`, which is what makes Esc land
  you back exactly where you were. The breadcrumb is just the stack's labels.
* **Esc peels one layer at a time**: overlay → filter → level. Pressing it once
  too often must not throw away your position.
* **Sort is per level.** Descending into a child resets to that level's
  `default_sort`; a column index left over from a wider level is clamped to 0
  in `rebuild`, so a saved view from a nine-column level cannot panic a
  two-column one.
* **Toggling the grain (`p`) truncates the stack to the repo level**, because a
  period key belongs to exactly one grain and everything below it would name a
  bucket that no longer exists. It also rebuilds the search index, whose paths
  embed period keys.
* **AI and human seconds on the overview are joined from the report rows and
  only when the run grouped by `repo`.** Otherwise those two columns stay at
  zero rather than inventing a split. Git output is always exact.
* **The Commit level lists every path the commit touched**, not just the
  category that was drilled through: a commit is one atomic change and showing
  half of it misleads. The parent's file count is per category, so the two
  numbers can legitimately differ.
* **The File level shows that path's whole history in the repository**, ignoring
  the period in the breadcrumb, because "when else did this change?" is the
  question a reader has once they are looking at a single file.

## 9. Audit letters

None fixed here — this is new code. Context only: **V** (misc UX) is much of
what the explorer answers, but nothing in `src/output.rs` was touched. The diff
viewer is new surface for the privacy property in PLAN.md "PROJECT CONTEXT";
see §5.
