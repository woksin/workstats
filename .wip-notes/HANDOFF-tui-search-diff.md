# HANDOFF — TUI search and diff (`src/tui/search.rs`, `src/tui/diff.rs`)

Owner: TUI search/diff agent. Both files are written against the contract in
`HANDOFF-tui-core.md` §4 and §5. **Status: DONE.** Neither file was compiled
with cargo (PLAN.md rule 1) — see "How this was verified" below for what was
done instead.

No file outside the two I own was touched.

---

## 1. `src/tui/search.rs`

Public surface is exactly the four items `app.rs` imports, nothing more:

```rust
pub fn score(needle: &str, haystack: &str) -> Option<i64>;
pub struct Target { pub kind: &'static str, pub label: String, pub path: Vec<String> }
pub struct Hit    { pub target: usize, pub indices: Vec<usize> }
pub struct Index;
impl Index {
    pub fn build(targets: Vec<Target>) -> Self;
    pub fn query(&self, needle: &str, limit: usize) -> Vec<Hit>;
    pub fn target(&self, index: usize) -> Option<&Target>;
}
```

Everything else in the file is a private `fn` or the private `struct Ranked`, so
nothing trips `dead_code` under `-D warnings`. No dependency on `state.rs` or
`app.rs`; no external crates at all (only `std::cmp::Reverse`).

### Algorithm

* **Scoring.** Subsequence match, case-insensitive. Per matched character:
  `SCORE_MATCH 16`; `BONUS_CONSECUTIVE 8` when it continues the previous match;
  otherwise a token-start bonus (`BONUS_BOUNDARY 10` after any non-alphanumeric
  or at index 0, `BONUS_CAMEL 6` for a lowercase→uppercase hump) minus a gap
  penalty (`5` to open, `1` per further character skipped). The **first**
  matched character's boundary bonus counts double. Finally the skipped prefix
  is subtracted, capped at `12`. Empty needle ⇒ `Some(0)`, as the contract
  requires for the live row filter.
* **Alignment.** A full optimum needs an N×M dynamic program, which the
  contract rules out. Instead, per probe two alignments are scored — the
  leftmost greedy one and the tightest one ending at the same place — and the
  probe restarts past the tight start, up to `MAX_ALIGNMENTS = 4` times. Both
  halves are load-bearing: the leftmost one is what finds `my**T**est**C**ase`
  for `tc`, the tightest is what finds `lib`.**rs** rather than `sr`+`s` in
  `src/lib.rs`. Both are covered by tests.
* **Case folding** is one-character-in, one-character-out (`fold`), so recorded
  positions index the *original* label the renderer highlights.
  `char::to_lowercase` can expand a character into several and would silently
  desynchronise them.

### Performance

No folded copy of any label is stored; the only per-entry precomputation is a
`u64` presence bitmask, and a needle character absent from a label rejects it
for the cost of one AND. `query` allocates one `Vec<char>` scratch and one
bounded `Vec<Ranked>` (≤ `limit`) **per keystroke**, not per candidate, and
match positions are computed only for the ≤ `limit` rows that will be drawn.

Measured on this machine (release, synthetic paths ~50 chars):

| index size | build | worst keystroke (every entry matches) | rejected needle |
|---|---|---|---|
| 200 000 (`MAX_SEARCH_TARGETS`) | 18 ms | 31 ms | 0.09 ms |
| 20 000 | 2.8 ms | 5.7 ms | 0.006 ms |

31 ms at the absolute documented ceiling is a redraw, not a stall. If it ever
needs to be faster, drop `MAX_ALIGNMENTS` to 2 — that halves the inner work and
only costs the "later window is better" case.

17 unit tests, all passing (see below): scorer semantics, the three ordering
properties (boundary > mid-token, run > scattered, shallow > deep), stable
ranking, the limit, char-vs-byte positions on a non-ASCII label, and a
property-style test that the bitmask reject never hides a real match.

---

## 2. `src/tui/diff.rs`

Public surface is exactly `DiffRequest`, `DiffKind`, `DiffLine`, `DiffView`,
`load` — the contract's §5, unchanged.

### The privacy property

The module doc states the display-only rule, and the rule is enforced by shape
as well as by comment: **`DiffView` deliberately derives nothing.** Not `Clone`,
not `Debug`, not `Serialize`. It therefore cannot be copied into another store,
printed into a log, or serialised into the JSON report or `views.json` even by
accident. **Do not add those derives.** `App` already drops it on `ascend`,
`jump_to` and `toggle_grain`.

A second, new safety property: this is the only place where bytes from a tracked
file reach a terminal, so every line goes through `clip`, which

* trims to `MAX_LINE_CHARS = 2000` characters with a `…`,
* expands tabs to four spaces (a terminal cell cannot hold a tab), and
* replaces every control character **and** every bidirectional format character
  (`U+200E/F`, `U+202A..E`, `U+2066..9`) with `·`.

Without that, a file containing `ESC [ 2 J` could repaint the reader's screen and
a `U+202E` could reorder what they are shown. Tested.

### Bounds

`MAX_DIFF_BYTES = 2 MiB`, `MAX_DIFF_LINES = 20 000`, `MAX_LINE_CHARS = 2000`
(read budget 4 bytes/char, tail discarded in 64 KiB chunks). Hitting any of them
sets `truncated`; the renderer should say so. A truncated read closes the pipe,
which ends `git` with a broken pipe — a non-zero status is therefore **not**
treated as an error when `truncated` is set. Both caps are tested.

### The Git invocation — this deviates from the contract's suggestion

The contract suggested `git show <sha> -- <path>`. That is what runs for a
**whole-commit** diff (`request.path` empty). For a **single file** it runs:

```
git --no-pager -C <cwd> -c core.quotePath=false log --max-count=1 --follow
    --no-color --find-renames --format=%H%n%an%n%aI%n%s --patch
    <sha> --not <sha>^@ -- <path>
```

Why, verified against real repositories rather than assumed:

* A pathspec is applied **before** rename detection, so `git show <sha> -- c.txt`
  reports a file moved from `a.txt` as an unrelated *new file* — 758 such entries
  in one repo per AUDIT **L**. `--follow` is the only thing that resolves it, and
  `--follow` belongs to `git log`.
* `--not <sha>^@` pins the walk to that single commit. Without it `--follow`
  keeps walking and answers with an **earlier** commit when this one does not
  touch the path — a wrong answer that looks like a right one. Verified working
  on root commits too (`^@` is empty there).
* `-c core.quotePath=false` for the same reason as `src/git.rs` (AUDIT **M**):
  otherwise the pathspec stops matching the report's own paths.
* The revision is validated as a plain object name (4–64 hex characters) before
  it reaches Git, because it is a positional argument *and* is spliced into
  `<sha>^@`. The path always goes after `--`.

**Renames** therefore render as their real `similarity index` / `rename from` /
`rename to` metadata lines. **Binary files** come back as the single
`Binary files … differ` `Meta` line — `--binary` is never passed, so no bytes are
emitted at all.

### When the path is not in the commit

Git prints *nothing at all* — not even the commit header — when a pathspec
matches no change. That absence is the only signal separating this case from a
binary file whose whole diff is metadata, so `parse` returns `None` on a missing
header rather than guessing. `load` then falls back to the whole commit and
inserts a `Meta` line saying so, instead of showing a blank pane. Tested against
a real repository.

---

## 3. Notes for other agents

**For the views agent (`src/tui/views.rs`)** — nothing here needs a change from
you, but two things are worth knowing:

* `DiffView::title` is already `"<9-char sha> <subject> — <path>"`, matching
  `CommitRecord::short_sha` (9 chars) so the diff names the commit exactly as the
  row it was opened from. Do not re-derive it.
* `view.lines[0]` is a `Meta` line carrying `"<author> · <date>"`. Everything
  after it is Git's own patch output, with the blank separator line removed.
  Suggested styling: `Meta` dim, `Hunk` cyan/bold, `Added` green, `Removed` red,
  `Context` default. Render `truncated` in the footer.
* `DiffView` is not `Clone`/`Debug` on purpose — see above. Borrow it, do not
  copy it.

**For the integration agent** — no change needed in `src/main.rs` beyond the
`mod tui;` already requested in `HANDOFF-tui-core.md` §6. `diff.rs` uses
`crate::git::git_executable()`, which is already `pub`.

**For the docs agent** — worth documenting:

* The diff viewer is the only feature that reads file contents, and it does so
  display-only: never cached, never in a report, never in a saved view.
* It refuses a revision that is not a plain hexadecimal object name.
* Very large diffs are truncated (2 MiB / 20 000 lines / 2 000 characters per
  line) and say so.

**Cosmetic, for whoever owns `src/tui/mod.rs`** — its module doc says the diff
level "shells out to `git show`". For a single file it is now `git log
--follow` (same process, same output shape, see §2). Not wrong enough to block
anything; reword if you are editing that file anyway.

**Not requested from anyone.** The commit-subject request in
`HANDOFF-tui-core.md` §7 also improves search: `search_targets` currently feeds
the derived change summary into the index, so "fuzzy search across commit
subjects" is only as good as that summary. The moment `GitCommit::subject`
exists and `CommitRecord::summary` carries it, the search index picks it up with
no change to `search.rs`.

---

## 4. Assumptions

* `key_of(&stack, LevelKind::Repo)` is `GitCommit::cwd`, i.e. a real repository
  working directory usable as `git -C`. Confirmed in `state.rs`:
  `RepoRecord::key` is documented as exactly that.
* `key_of(&stack, LevelKind::File)` is repository-relative, which is what a
  pathspec needs. It comes from `--numstat`, so it is.
* Row ids at the File level are full commit SHAs (`CommitRecord::sha`), which
  pass the object-name check. A shortened or otherwise non-hex id would be
  refused with a status-bar message rather than handed to Git.
* Commit SHAs never contain characters needing shell quoting — nothing here goes
  through a shell (`Command` with separate args throughout), so this only
  matters for `<sha>^@`, which the hex check already covers.

## 5. How this was verified (no cargo was run)

`cargo build/test/check/clippy/fmt` were **not** run, per PLAN.md rule 1.
Instead, using the standalone toolchain binaries, which touch neither the cargo
registry nor `target/`:

* `rustfmt --edition 2024 --check` — clean on both files.
* `rustc --edition 2024 --test` on `search.rs` standalone (it has no external
  dependencies): compiles with no warnings, **17/17 tests pass**.
* `rustc --edition 2024 --test` on a scratch copy of `diff.rs` in the scratchpad,
  with a stub `crate::git::git_executable` and `--extern` pointed at the
  **already-built** `libanyhow`/`libtempfile` rlibs in `target/debug/deps`
  (read-only): compiles with no warnings, **11/11 tests pass**, including the
  end-to-end test that builds a real repository, renames a file, and checks that
  `load` reports the rename and falls back for a missing path.
* `clippy-driver` (not `cargo clippy`) over both: **no lints**, verified against
  a probe file that clippy does fire on, so the silence is real.
* All scratch copies and binaries were deleted afterwards.

The real crate still will not build until `src/tui/views.rs` exists and
`src/main.rs` declares `mod tui;`.

## 6. Audit letters

None fixed here — this is new code. Context: **L** (mangled rename paths) is why
the single-file diff uses `--follow`, and **M** (octal-escaped non-ASCII paths)
is why `-c core.quotePath=false` is passed. The diff viewer is new surface for
the privacy property in PLAN.md "PROJECT CONTEXT"; §2 above is how it is held.
