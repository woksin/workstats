# HANDOFF — docs (`README.md`, `CHANGELOG.md`)

Owned and edited: `README.md` and `CHANGELOG.md` only. No cargo command was run.
Everything documented below was read out of the source on this branch, not out of
the other agents' handoffs alone — where a handoff and the code disagreed, the
code won (see "Corrections" at the end).

## README

* **Sample output block regenerated** to match `output.rs::print_table` exactly
  (94-wide banner, `Git commits` / `Average / active day` / `Observed` lines, the
  `Agent work` column, the `(human involvement first; …)` table title, the
  106-wide rule). Rendered with a script against the real format strings rather
  than written by hand; shares and the test/source ratio are arithmetically
  consistent with the numbers shown.
* **New `### Explore it interactively`** under "Start here": invocation shape
  (flags AFTER `ui`), the seven drill-down levels, why Commit and File are
  scoped the way they are, the full key table (verbatim from
  `state::KEYBINDINGS`), what search covers, saved views
  (`~/.config/workstats/views.json`, `WORKSTATS_VIEWS`, ≤64, never the cache,
  never the diff level), and the TTY / `--format` refusals.
* **New `### Make the areas your own`** under "What was worked on": the
  `categories` / `category_mode` config block, extend vs replace, every rule kind
  in a table, case-folding rules, new categories matching before the built-ins,
  the bounds (32 / 128 / 128 bytes / 256 for globs / name grammar / `ignored`
  reserved), the two distinct failure modes (bounds → hard error; unknown rule
  key → JSON error, config ignored, `Warning:` under the report), and a
  `workstats classify` example whose columns are byte-exact for the `{:<52}
  {:<10} {:<18} {}` format.
* The area table was **reordered into match order** (`test, docs, config,
  assets, source, other`) because the following sentence says "in that order".
  Change-shape table generalised: shapes are code-like-dominant or the area's own
  name, and `mixed` now also covers a fallback-dominant diff.
* **CSV callout** (`> [!IMPORTANT]`): area columns are registry-derived, so both
  names and order are config-dependent; default order is now `test_*` first, not
  `source_*`; read by header name.
* **Privacy**: the existing bullets are untouched. Added one bullet ("no file
  contents in reports or the cache either …") and a full subsection
  `### The diff viewer is the one place file contents are read` stating
  display-only: never cached, never in a report, never in a saved view, never
  sent anywhere, in memory only while on screen — plus the hex-object-name
  check, the path after `--`, the 2 MiB / 20 000 line / 2 000 char caps, control
  and bidi stripping, and binary files as one metadata line.
* Drift fixed: bare command is "current directory" with an explicit paragraph on
  Git-scope vs machine-wide AI scope; `--dir`/`WORKSTATS_DIR` must exist;
  `--raw`, `--depth`, `--no-ignore`, `--config`, `--by-repo`/`--matrix`/
  `--by-dir` (now mutually exclusive), `--no-default-events`, `WORKSTATS_DIR`,
  `WORKSTATS_VIEWS` all documented; `--events` adds to rather than replaces the
  default log; diagnostics and the privacy-rejection line now reach the table;
  `--repo` matches repo/cwd/root on both sides; durations bounded at 8784h.
* One line outside the brief: the Homebrew sentence in "Development and
  releases" now says the step is skipped **with a warning** and fails the release
  if the README advertises an unconfigured tap, which is what
  `.github/workflows/release.yml` does after the release-workflow agent's change
  (`HANDOFF-release-workflow.md` §2, §7).

## CHANGELOG

`[Unreleased]` now has Added / Changed / Fixed in Keep a Changelog order. The
two pre-existing Fixed entries were kept verbatim and folded into the single
Fixed list (their blank-line separators were removed so the list is tight like
every released section).

Headline is the Claude token fix, stated as a user would see it: totals were
roughly doubled, are now correct, and **numbers will halve against an older
report**. Every other Fixed entry names the observable symptom, not the code.

## Corrections to other agents' handoffs

1. `HANDOFF-tui-core.md` §4 and PLAN.md say search covers **commit subjects**.
   It does not — `GitCommit` still has no `subject` and `app.rs::search_targets`
   indexes `short_sha + describe_change(...)`. The README documents what
   actually ships ("a summary derived from the files it touched"), which is also
   the honest privacy story.
2. AUDIT **S** (`--raw` global model list rendered as if nested) and **T**
   (`<synthetic>` leaking as a model name) are **not fixed** — `output.rs:189-212`
   still prints the flat top-12 list indented four spaces, and `ai.rs:2241` still
   accepts `<synthetic>`. Neither is claimed in the CHANGELOG.
3. AUDIT **V** is only partly fixed. Documented: the grouping conflicts, the
   non-existent `--dir`, and flag-naming error messages. NOT documented, because
   they did not ship: `--top` vs the sparkline, the summary label widths, CSV
   missing `change_shapes`/summary/methodology, CSV `first_seen` in UTC vs local
   `month`, and the duplicate JSON aliases.
4. AUDIT **K** (`read_bounded_line` off-by-one) and **U** (no tests on
   `print_table`) have no user-visible symptom, so neither has a CHANGELOG entry.

## Assumptions

* The CHANGELOG's "the transcript index is rebuilt once on the first run after
  upgrading" is the user-facing statement of `PARSER_VERSION` 2 → 3.
* The classify example assumes the config shown just above it (`ai` carrying
  `.claude`), which is why `Categories in match order` starts with `ai`.
* `README.md` still advertises no Homebrew tap (commit `1bce4cf` removed it), so
  the release workflow's guard stays on its warn-and-skip branch.
