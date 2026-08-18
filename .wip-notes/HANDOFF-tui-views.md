# HANDOFF — TUI views (`src/tui/views.rs`)

Owner: TUI-views agent. One file, written against `HANDOFF-tui-core.md` §2–§5
exactly. No file outside `src/tui/views.rs` was touched.

**Status: DONE.** `rustfmt --edition 2024 --check src/tui/views.rs` passes (no
`cargo` was run, per PLAN.md rule 1). The crate still cannot compile until
`src/tui/search.rs` and `src/tui/diff.rs` exist and `src/main.rs` declares
`mod tui;`.

---

## 1. What is in the file

Single public item, exactly as the core contract specifies:

```rust
pub fn draw(frame: &mut Frame, app: &mut App);
```

Screen layout, top to bottom:

```
breadcrumb      workstats › widget › 2026-06 › source        (tail bold, head dim)
summary strip   Observed … · Commits … · Sessions … · Lines … · Human … · AI … · Tokens …
body            the table for the level, or the diff pane at LevelKind::Diff
message line    what is being typed, else app.status(), else the active filter
status bar      [mode ·] N rows · sort Commits ▼ · period month · ? keys · q quit
overlays        help (?), saved views (v), search (s), save-view (w)
```

* **Table levels** (Overview / Repo / Period / Category / Commit / File) all go
  through one `table()` builder driven by `app.columns()` and `app.rows()`, so
  a new level or a new column needs no change here — only in `state.rs`.
* **Sort indicators**: the sort column's header is cyan + bold with `▼`/`▲`.
  Every header also carries its `1`–`9` key, which is dropped before the arrow
  when the column is too narrow for both.
* **Diff pane**: title line (`app.diff().title`, position `1,234 / 20,000`, and
  a yellow `truncated` when the size cap hit), then the lines from
  `app.diff_offset()` coloured by `DiffKind`. Only the visible window is
  formatted, so a 20 000-line diff costs a screenful of work per frame.
* **Empty states**: every level has its own sentence saying *why* it is empty,
  and a filtered level says so and names the key that clears it. Nothing ever
  renders as a blank panel.
* **Help overlay** renders `state::KEYBINDINGS` verbatim; if the terminal is too
  short it spends the last row on `… N more` rather than ending silently.

## 2. Narrow and short terminals

* `band_heights` gives the body at least three rows (header + two rows) before
  any chrome gets any, and sacrifices bands in the order **summary →
  breadcrumb → message line → status bar**. The message line outlives the
  breadcrumb because it is what carries a `git show` failure.
* `visible_columns` drops columns from the right; the first column is always
  kept and left for the widget to clip, because it carries the row's identity.
  A dropped sort column is still named in the status bar.
* `column_widths` re-runs the same layout `Table::get_column_widths` runs
  (selection column, then the constraints at `spacing(1)`), so a cell is
  shortened to the width it will actually be given instead of being cut
  mid-word. **If anyone changes `column_spacing` or `highlight_spacing` on the
  table, `MARKER_WIDTH` / `COLUMN_SPACING` in this file must change with it.**
* Every panel returns early on an empty `Rect`, and `overlay()` shrinks rather
  than overflowing. There is a test that renders the table at widths 1..90 and
  heights 1, 2, 3, 8, 40 and one that renders the help overlay down to 1×1.

## 3. Number formatting — a divergence you should close

The task brief said to format numbers exactly the way `src/output.rs` does.
Those helpers (`hours`, `number`, `compact_tokens`, `percent`) are all private
`fn`s in a file I do not own, so I mirrored them:

* `views.rs::number` groups with a **comma**, like `output.rs::number`.

But almost every number the explorer shows is formatted in **`state.rs`**, not
here — `Field::count/lines/hours/share` and `report_summary` — and it does not
match `output.rs`:

| value | `output.rs` | `state.rs` today | fix |
|---|---|---|---|
| thousands | `1,234,567` | `1 234 567` | `group_digits`: push `','` not `' '` |
| hours | `5h 09m` | `5.1` / `5.1 h` | `Field::hours` + `report_summary` "Human"/"AI" |
| tokens | `18.4M` | `18 400 000` | `report_summary` "Tokens" → a `compact_tokens` mirror |

Until that lands the status bar says `1,204 commits` while the column beside it
says `1 204`. **Requested (state.rs owner, three one-line edits):** switch
`group_digits` to `','`, make `Field::hours` render `{h}h {mm}m`, and give
`report_summary`'s Tokens row the compact form. Nothing in `views.rs` needs to
change when you do.

## 4. Other requests for files I do not own

* **`src/tui/app.rs` — a test fixture.** `App::new` needs a whole
  `crate::model::Report`, which has no `Default`, so `draw` itself has no
  full-frame smoke test; I test the widget builders and every pure helper
  instead. A `#[cfg(test)] pub(super) fn sample_app(data: Dataset) -> App`
  beside the existing `app()` fixture would let `views.rs` add
  `draw(frame, &mut app)` at a dozen terminal sizes, which is the one thing my
  tests cannot cover.
* **`Cargo.toml` — `unicode-width`.** This file measures text in **chars**, not
  display cells. A CJK or emoji path therefore over-runs its column and is
  clipped by the widget: cosmetic, never a panic. `unicode-width` is already in
  the tree via `ratatui-core`; adding it as a direct dependency and swapping
  `chars().count()` for `UnicodeWidthStr::width` in `safe_chars`/`shorten`/
  `shorten_path` would make the truncation exact. Not blocking.

## 5. Assumptions I made against the contracts

* `DiffView { title: String, lines: Vec<DiffLine>, truncated: bool }` and
  `DiffLine { kind: DiffKind, text: String }` with `DiffKind` **`Copy`** — I
  read `line.kind` out of a borrowed `&DiffLine`. **Verified against the
  `src/tui/diff.rs` that landed**: all three match. `DiffView` deliberately
  derives nothing; `views.rs` only ever reads it through a shared borrow and
  never clones, stores, or Debug-prints it, and no test constructs one, so
  adding fields will not break this file.
* `SearchRow { label: String, kind: &'static str, indices: Vec<usize> }`, with
  `indices` in **char** positions. Out-of-range indices are ignored rather than
  panicking, and they need not be sorted.
* `app.rows()` is already filtered and sorted, `entry.fields.len()` equals
  `app.columns().len()`, and `app.columns()` is empty at `LevelKind::Diff`.
  All three are relied on; the last one is what makes the status bar say
  `no sort` there instead of indexing past the end.
* Every accessor listed in the core handoff §3 is called from here **except
  `should_quit`**, which `event.rs` owns. `set_viewport` is called on every
  frame from both body branches; `set_offset` is called only from the table
  branch (the diff level's scroll belongs to `App`).

## 6. Audit letters

* **M** (non-ASCII / quoted paths, `core.quotePath`): every string drawn goes
  through `safe_chars`, which replaces control characters with `·`. With
  `core.quotePath=false` the raw bytes of a path now reach the TUI, and an
  escape sequence inside a filename would otherwise be handed straight to the
  terminal. Same substitution `output.rs::safe_message` makes.
* **V** (misc UX): the explorer answers much of it — sort direction is shown,
  the breadcrumb says what is scoped, filtered counts are labelled `(filtered)`,
  and a truncated diff says so. Nothing in `src/output.rs` was touched.

## 7. Design decisions worth knowing

* **Foreground colours only.** No background is painted across the page, so the
  explorer is legible on a light terminal as well as a dark one. The only
  reversed style is the selected row.
* **A caret glyph (`▏`) rather than the terminal cursor.** The cursor would have
  to be positioned by whichever panel owns the input, and an overlay can be
  drawn over that panel later in the same frame.
* **Paths shorten from the front, prose from the back.** `clip` picks by
  whether the text contains a separator — the file name identifies a path, the
  first words identify a sentence. Diff lines always shorten from the back
  (they are code, and a `/` in them means division, not a directory).
* **`table()` is separate from `draw_table()`** purely so the widget can be
  built and rendered in tests without an `App`.
