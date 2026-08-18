# HANDOFF — foundation (classify/model/aggregate/paths/git/output/main)

Branch `feature/explorer-and-configurable-categories`. Nothing was compiled or
tested here, per the hard rules.

## TARGET FEATURE 1 — configurable classification

### `src/classify.rs` (rewritten)

- `Category` enum, `CATEGORY_COUNT`, `Shape::ALL`, `tally_add`, `tally_merge`,
  `touched_lines` are **gone**. New public surface:
  - `CategoryRegistry` — ordered `Vec<CategoryDef>`; position IS match priority.
    `builtin()`, `from_config(&BTreeMap<String, CategoryRules>, CategoryMode)`,
    `len()`, `name(i)`, `names()`, `index_of(name)`,
    `classify(path) -> usize`, `explain(path) -> Classification`,
    `change_shape(&CategoryTally) -> Option<Shape>`. Accessors nothing calls yet
    (`fallback()`, `definition(i)`, `is_empty()`) were deliberately left out:
    CI runs `cargo clippy --all-targets -- -D warnings` and an unused `pub fn`
    in a private module of a binary crate is a `dead_code` error. Add them back
    in the same commit as the first caller.
  - `active_registry() -> &'static CategoryRegistry` and
    `install(registry) -> Result<()>` (a `OnceLock`; falls back to the built-ins
    if never installed, so unit tests need no setup). `main.rs` installs it right
    after `load_config`, before anything classifies.
  - Free `classify(path) -> usize` and `change_shape(tally)` delegate to the
    active registry — same call sites as before, now returning an index.
  - `CategoryTally` is a struct over `Vec<CategoryLines>`; `add(index, +, -)`,
    `merge(&other)`, `get(index)`, `touched()`. It grows only to the highest
    index used, so merging is a zip and an empty tally allocates nothing.
  - `ShapeTally` is a `BTreeMap<Shape, usize>` wrapper (`add`, `iter`, `total`).
  - `Shape` gained `Area(String)` and lost `Tests/Docs/Config/Assets`.
- Built-in parity: the six areas are `test, docs, config, assets, source, other`
  **in match order**, with every historical rule preserved (`.specs`/`.tests`
  directory suffixes, `for_`/`when_`/`given_` directory prefixes, bare `given`,
  `when_`/`given_` name prefixes, `.test.`/`.spec.` name-contains, and
  `CAMEL_TEST_SUFFIXES` matched against ORIGINAL casing). Existing lookalike
  tests (`latest.rs`, `Forecast`, `OpenApi.Specification`) are unchanged and pass
  by inspection.
- Two rule kinds beyond the plan's list were required for exact parity:
  `stems` (`DOC_STEMS`, `TEST_STEMS`) and `name_contains` (`.test.`, `.spec.`).
  `cased_stem_suffixes` is the case-sensitive one.
- Config shape (in the existing JSON config file):
  ```json
  { "categories": { "test": { "directory_prefixes": ["it_"] },
                    "ai":   { "directories": [".claude"], "names": ["CLAUDE.md"] },
                    "corpus": { "directories": ["corpus"], "code_like": true } },
    "category_mode": "extend" }
  ```
  Rule sets: `directories, directory_prefixes, directory_suffixes, extensions,
  names, name_prefixes, name_suffixes, name_contains, stems, stem_suffixes,
  cased_stem_suffixes, globs, code_like`. Unknown keys are a hard parse error
  (`deny_unknown_fields`) so a typo is loud; that means a typo makes
  `load_config` warn and ignore the WHOLE config (pre-existing behaviour for any
  malformed config), and the warning now reaches the table footer (fix P).
- Bounds: ≤32 categories total, ≤128 rules per category, rule strings ≤128 bytes
  (globs ≤256), no empty strings, no control characters. Names must be
  `^[a-z][a-z0-9_-]{0,31}$`; `ignored` is reserved (it would collide with the
  existing `ignored_additions` CSV column). Bad `category_mode` is a hard error.
- Normalisation: every case-insensitive rule is lowercased on load (so
  `"CLAUDE.md"` works), extensions may be written `".rs"` or `"rs"`, directories
  may carry slashes. `globs` match the ORIGINAL path, case-sensitively.

### Decisions worth knowing (document these)

1. **Registry order is match order AND display order.** The default CSV column
   order therefore becomes `test_*, docs_*, config_*, assets_*, source_*,
   other_*` (previously source first). Column NAMES are unchanged; the
   integration test looks columns up by name, so it still passes.
2. **New categories are matched BEFORE the built-ins**, sorted by name (JSON
   object order is not preserved by `serde_json` without `preserve_order`).
   Otherwise `.claude/settings.json` would be `config`, not `ai`.
3. **Shapes**: dominant category name, or `new code`/`revision`/`removal` when
   that category is `code_like`, or `mixed` when nothing reaches 60% **or** when
   the dominant category is the fallback (`other`) — the latter preserves
   today's behaviour. `CategoryDef::shape_name` exists only so the built-in
   `test` category keeps emitting the documented `tests` shape; every other
   category's shape is its own name.
4. `Methodology.composition` changed from `&'static str` to `String` because it
   now lists the configured category names.

### `workstats classify <PATH>...`

New subcommand: prints path, category, rule kind, and the exact rule literal
that matched (`--format table|json|csv`, `--config`). Table output ends with the
registry in match order. A path that matched nothing reports rule `fallback`.

## Audit fixes in these files

- **L** `src/git.rs::renamed_target` resolves `old => new`, `dir/{old => new}`,
  and `{old => new}/file` to the new path before classifying, globbing, or
  recording the file, and collapses the empty component `{old => }` leaves.
  `--no-renames` deliberately NOT used.
- **M** `git log` now runs with `-c core.quotePath=false` (before `log`).
  `LC_ALL` was NOT pinned — it does not affect path quoting and would only
  change message language.
- **N** `neutralize_formula` only prefixes `'` when the cell does not parse as
  `f64`, so `net_lines = -1` stays `-1`.
- **O** the Git-side repo filter now also matches `root`, like `main.rs`.
- **P** `print_table` prints the first 5 `diagnostics.messages` as `Warning:`
  lines plus a count of the rest, with control characters replaced.
- **Q** the default event log is now ALWAYS loaded unless the new
  `--no-default-events` flag is passed; event paths are deduplicated by
  canonical path so `--events <default>` cannot double-count.
- **R** `Diagnostics` gained `content_rejections: u64` (serde default, merged in
  `Diagnostics::merge`) and `print_table` reports it as a separate
  "Privacy: N record(s) … skipped, as designed." line.

## NEEDED FROM OTHER AGENTS

1. **AI-adapter agent (`src/ai.rs`) — required for fix R.** At `src/ai.rs:1177`
   change
   `result.diagnostics.malformed_lines += sensitive_records;` to
   `result.diagnostics.content_rejections += sensitive_records;`
   (keep the existing `warn(...)` message). The field already exists in
   `model.rs` and is already reported separately in the table footer. Any test
   asserting `malformed_lines == 1` for a content-bearing record must move to
   `content_rejections`.
2. **Integration/test agent (`tests/rust_cli.rs`) — consequence of fix Q.**
   `sources_and_open_event_recording_form_a_complete_integration_path` asserts
   `session_count == 1` while passing `--events <tmp>`; the developer's real
   default event log is now also loaded, so that test must pass
   `--no-default-events` (or set `WORKSTATS_EVENTS` to a temp path).
3. **Docs agent (`README.md`, `CHANGELOG.md`)** — document: the `categories` /
   `category_mode` config block and every rule kind; that CSV area columns are
   derived from the registry (config-dependent names AND order, default order
   now test→docs→config→assets→source→other); the `workstats classify`
   subcommand; `--no-default-events`; that the default event log is always
   loaded; and that shape names follow category names (`tests` retained).
4. **TUI agents** — use `classify::active_registry()` for names/columns and
   `CategoryTally::get(index)`; do not assume six categories or a fixed order.

## Assumptions

- A process-wide `OnceLock` registry is acceptable because the config is read
  once per run; it is installed exactly once in `run()`, and no unit test
  installs one (they build local registries), so tests stay order-independent.
- Rule attribution reports the first matching rule kind in a fixed
  most-specific-first order; within a category the chosen category cannot depend
  on that order.
- I formatted only the seven files I own with `rustfmt --edition 2024` (not
  `cargo fmt`), which also served as a parse check. No cargo command was run.
