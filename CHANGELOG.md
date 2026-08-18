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

- `--agent-commits` reads commits a coding agent authored and reports them as
  output, never as human time. Once a branch has been fetched, that work is
  ordinary local Git history, and `--author` cannot see it because the agent is
  the author — so a second `git log` pass over the same repositories asks for
  the agent's commits instead of yours. **They add no human time, no work
  blocks, no setup/review credit, and no active human days**, and they are never
  folded into the commit, line, work-composition, or change-shape figures
  `--author` promises are yours: they get their own summary line, their own
  report section, their own `agent_commit_count` / `agent_additions` /
  `agent_deletions` fields in JSON and columns in CSV, and their own `git-agent`
  row under `--group-by provider`. A repository whose history is nothing but
  agent commits reports `Estimated human work  0h 00m`. The built-in identities
  — GitHub's Copilot coding agent, Copilot on github.com, and Claude — are
  matched on the tail of the e-mail address rather than on the numeric prefix,
  because GitHub has issued more than one id for the same Copilot account and
  anything keyed on the number finds part of an agent's work and silently misses
  the rest. `github-actions` and `dependabot` are deliberately not agents.
  `--agent-commits=REGEX` replaces the built-in identities rather than adding to
  them, and takes the same *basic* regular expression `--author` does.
- `--co-authors` reads the `Co-authored-by:` trailers on your own commits, so a
  commit you wrote with an agent can be described as such. It flags a commit
  already counted rather than adding one: the commit count, the changed lines,
  and the human estimate are identical with and without it. Copilot Autofix is
  counted separately from assisted development, because code scanning and
  writing code with an assistant are different activities. Only the trailer
  *values* are requested from Git; no other part of a commit message is read.
- `contrib/copilot-github-sync.sh` records the Copilot activity a clone cannot
  see — pull requests the coding agent opened and reviews it left — as
  `workstats record` events. It is a script rather than a flag on purpose:
  reading either means calling the GitHub API, and an HTTP client inside the
  binary would end both the "no network calls" and the "no credential discovery"
  guarantees. The call is made outside by your own authenticated `gh`, and what
  crosses back in is the content-free record the `record` subcommand already
  accepts — a provider, an identifier, a directory, a model name, and
  timestamps, with no titles, bodies, or review text. Every event is written
  with `--role subagent`, so nothing it records can become human time.
- Configurable file areas. The six built-in areas are now a registry rather
  than a fixed set: a `categories` block in the config file adds rules to
  `source`, `test`, `docs`, `config`, `assets`, or `other`, and any name the
  built-ins do not know creates a new area — `ai`, `planning`, `corpus`, or
  whatever the work actually is. New areas appear in the dashboard, in JSON
  `composition`, in CSV columns, and as change shapes. `category_mode` chooses
  `"extend"` (default; user rules are added to the built-in ones) or
  `"replace"` (the named area's built-in rules are discarded). Rules are plain
  strings, never regular expressions: directories, directory prefixes and
  suffixes, extensions, file names, name prefixes/suffixes/substrings, stems,
  stem suffixes, case-sensitive stem suffixes, and globs, plus `code_like` to
  opt an area into the `new code` / `revision` / `removal` shapes. Bounded like
  the source-root rules — at most 32 categories, 128 rules each, 128 bytes per
  rule (256 for a glob), lowercase names, no control characters.
- `workstats classify <PATH>...` prints the area each path lands in, which rule
  kind matched, and the exact rule literal, so a category config can be
  debugged without running a report. Supports `--config`, `--format json`, and
  `--format csv`.
- `workstats ui`: an interactive explorer over the same report the dashboard
  prints. It drills down from the overview through repository, month or day,
  file area, commit, and changed file to a diff; filters the current level as
  you type (`/`); fuzzy-searches repository names, file paths, and commits
  (`s`); sorts by any column; toggles month/day (`p`); and has a key-map
  overlay (`?`). Views can be saved and reopened (`w` / `v`); they live beside
  the config file as `views.json` (`WORKSTATS_VIEWS`), never in the cache, and
  hold only a drill-down path, sort, and filter. It requires an interactive
  terminal: with stdout redirected or under `TERM=dumb` it says so and exits
  instead of emitting escape codes, and `workstats ui --format json|csv` is
  refused before any scanning. `workstats` without `ui` is unchanged and does
  no extra work.
- The explorer's diff viewer is the first feature that reads the contents of a
  tracked file, and it is display-only: the patch is never cached, never
  written into a report, never stored in a saved view, and never sent anywhere.
  It lives in memory only while it is on screen. Diffs are truncated at roughly
  2 MiB, 20,000 lines, or 2,000 characters per line, and control and
  direction-override characters are replaced before drawing so a file cannot
  repaint the terminal.
- `--no-default-events` skips the event log written by `workstats record`.
- GitHub Copilot Chat in VS Code, as the provider `copilot-vscode`. Every
  prompt in a chat session counts as human involvement at the moment it was
  sent, and a turn VS Code timed contributes the interval it actually took
  rather than the activity estimate. Sessions are attributed to the workspace
  folder they were opened in. `Code`, `Code - Insiders`, and `VSCodium` are read
  when they exist; any other install is reachable with `--history
  copilot-vscode=PATH`. Copilot reports a premium-request multiplier rather than
  token counts, so these sessions carry no tokens. `copilot-chat`,
  `vscode-copilot`, and `vs-code-copilot` all name the same provider. Copilot
  is now covered on both surfaces that leave a timestamped local record — the
  CLI and Copilot Chat; inline completions leave none.
- `--month` and `--year` narrow a report to one calendar month or year.
  `--month 2026-07` and `--year 2026` name one outright, and both also accept
  `current` (`this`) and `last` (`previous`), resolved against the local
  calendar. They are filters, like `--since`/`--until` — `--group-by` and
  `--period` remain the groupings — so they compose with those and refuse to be
  combined with each other or with the bounds.
- `brew install woksin/workstats/workstats` installs and updates `workstats`
  again. The tap now exists and every release publishes a formula to it. The
  fully qualified name is the form to use: Homebrew 5.1.15 and newer will not
  load a formula from a third-party tap until it is trusted, and naming the tap
  in full trusts that one formula rather than everything in it.

### Changed

- CSV area columns are derived from the category registry, so they depend on
  the configuration: a new area adds `{area}_files` / `{area}_additions` /
  `{area}_deletions`, and a replaced one renames them. Their order now follows
  match order, which by default is `test_*`, `docs_*`, `config_*`, `assets_*`,
  `source_*`, `other_*` — previously `source` came first. Column names for the
  built-in areas are unchanged; read CSV by header name rather than by
  position.
- `--events` now *adds* logs to the default event log instead of replacing it.
  Passing any `--events` previously dropped everything `workstats record` had
  written. Paths are deduplicated by canonical path, so naming the default log
  explicitly cannot double-count it, and `--no-default-events` opts out.
- `--by-repo`, `--matrix`, `--by-dir`, and `--group-by` are now mutually
  exclusive. Combining two of them used to let one silently win.
- A `--dir` or `WORKSTATS_DIR` that does not name an existing directory is now
  an error saying which of the two was wrong, instead of an all-zero report and
  exit 0.
- Parse errors for `--gap-cap`, `--human-idle`, `--review-credit`, `--since`,
  `--until`, and `workstats record`'s three RFC 3339 flags now name the flag and
  echo the value that was rejected. Durations are bounded at `8784h` (366
  days).
- Diagnostic messages now appear under the table report (the first five, plus a
  count of the rest), so a mistyped `--history` or `--events` path is visible
  instead of producing a clean-looking report with data silently missing.
- Event records carrying prompt or response text are still rejected, but are
  now reported on their own line — `Privacy: N record(s) … skipped, as
  designed.` — rather than being counted as malformed input. A record carrying
  several such fields at once, which is the ordinary shape of an API-wrapper
  log writing both `response` and `output`, is reported the same way instead of
  as a JSON syntax error that was never there.
- The transcript index is rebuilt once on the first run after upgrading,
  because what it stores and how a cached time range is derived both changed.

### Fixed

- **Claude Code token totals were roughly doubled and are now correct.** Claude
  Code writes one transcript record per content block of a single API response
  — the text block, then one per tool call — and every one of them repeats the
  same identifiers and a byte-identical usage block, so one response was
  counted between 2 and 12 times. Measured across 40 real transcripts, 13,275
  records carrying usage described only 6,302 distinct responses — 63% of them
  written more than once — and the reported total was 2.08× the true figure.
  Responses are now counted once. **Anyone comparing against a report from an
  earlier version will see Claude token counts roughly halve; the new numbers
  are the right ones.** Time-based metrics were never affected.
- GitHub Copilot CLI token usage no longer disappears depending on the cache.
  Copilot reports usage in its end-of-session record, which can fall after the
  last activity signal — sometimes on the other side of a day boundary — so the
  same query answered with tokens on a cold run and zero on a warm one. Token
  timestamps are now part of the cached time range.
- GitHub Copilot CLI token usage is no longer dropped entirely when a session
  changes directory after its last activity event.
- A GitHub Copilot CLI subagent no longer overwrites the model recorded for the
  foreground session, which misattributed the work that followed it.
- A GitHub Copilot CLI session whose event log never recorded a working
  directory is now credited to the repository it actually ran in, read from the
  CLI's own session store, instead of landing under the transcript directory
  with an approximate location. A directory the event log did record still
  wins, and a session store naming a repository the directory contradicts is
  reported as a warning rather than silently preferred.
- A single unreadable row in the OpenCode database no longer discards every
  OpenCode session. OpenCode stores `time_created` as a numeric column that
  SQLite may hold as a floating-point value, which aborted the whole read. Bad
  rows are now skipped, counted, and reported.
- Sessions in the open event format that share a session id across directories
  or roles are no longer collapsed into one, which could report a foreground
  session as a subagent.
- Codex no longer double-counts a re-emitted `token_count` event whose
  cumulative total has not moved — about 2% of Codex tokens in a real sample.
- Legacy Gemini CLI `.json` sessions parse about 90× faster (6.2 s → 0.07 s on
  a 19 MB file) and are size-guarded instead of read into memory unbounded.
- Cached Codex and Gemini entries now invalidate when the out-of-band metadata
  they depend on changes, instead of being answered from a stale index.
- Renamed and moved files are counted against the path they moved *to*.
  `git log --numstat` reports a rename as a single field such as
  `src/a.rs => tests/b.rs` or `{src => tests}/helper.rs`, and that whole string
  was being treated as the path — so a file moved into `tests/` was still
  booked as source, and the ignore globs, `--path`, and `--path-exclude` never
  matched it. One real repository had 758 such entries.
- Non-ASCII paths are classified and ignored correctly. Git quotes and
  octal-escapes them by default, which broke extension detection and every
  ignore glob: `src_æøå.rs` was reported as `other` rather than `source`, and
  `node_modules/pakke_æøå.js` was counted as authored work. The same holds for
  a path holding a quote, a backslash, or a control character, which Git quotes
  whatever that setting says — `node_modules/a"b.js` is ignored again.
- A file whose name genuinely contains ` => ` is no longer mistaken for a
  renamed file. Git spelled a rename the same way, so `node_modules/a => b.js`
  arrived as `b.js`: it lost the directory the vendor rule matches on and was
  counted as authored source. Names holding `{`, `}`, a tab, or a newline are
  read correctly for the same reason. A file moved *out* of an ignored
  directory now counts as authored work from the commit that moved it, rather
  than staying ignored because of where it used to live.
- A path that is not valid UTF-8 no longer costs a repository the rest of its
  history. One such path ended the read, so every commit behind it disappeared
  from the report — silently, and with no warning. Those paths are now reported
  in their closest printable spelling and the scan continues.
- A Git author whose address contains `+` — `person+work@example.com`, the
  common form of a plus-addressed identity — no longer reports an empty
  history. `git log --author` takes a *basic* regular expression, in which `+`
  is already a literal and a backslash is what turns it into an operator, so
  escaping it asked Git for "one or more `n`" and matched nothing that address
  had ever committed. The same held for `?`, `(`, `)`, `{`, `}`, and `|` in an
  author name such as `A (Team)`.
- A release whose binaries failed to upload can no longer sit at `Latest` with
  nothing to download. The release workflow now checks the finished release
  against the assets it was supposed to carry, marks it a prerelease and fails
  the run when any are missing or when `SHA256SUMS` does not cover every binary
  — so `Latest`, `releases/latest/download/…`, and `workstats update` keep
  pointing at the last complete release. Re-running the failed jobs attaches
  the rest and restores the release to `Latest` automatically.
- CSV no longer breaks negative numbers. Formula neutralisation prefixed an
  apostrophe to any cell starting with `-`, so a negative `net_lines` shipped as
  `'-1`. Cells that parse as numbers are now left alone; genuine formulas are
  still neutralised.
- `--repo` now filters Git commits by the same three labels it already used for
  AI sessions — repository name, working directory, and source root — so a
  filter naming a source root no longer returns AI sessions with no Git output
  beside them. A filter that matches only a session's own nested working
  directory keeps its commits too: the checkout inferred from that session is
  no longer re-filtered against the repository root, which does not contain
  what the nested directory matched.
- An absurd duration such as `--gap-cap 99999999999999999999h` is a clean
  command-line error instead of a panic.
- Tests kept in a sibling project (`Arc.Core.Specs/`, `Fundamentals.Tests/`) or
  in BDD behaviour folders (`for_Subject/when_something/given/`) are counted as
  tests instead of source. Directory matching was exact, so `Core.Specs` did
  not match `specs` and whole .NET suites were reported as production code. On
  one real month this moved 1,752 files and ~66k changed lines out of `source`,
  taking the test-to-source ratio from 0.20 to 1.36.
- Generated and vendor directories at the repository root — `node_modules/`,
  `dist/`, `build/`, `bin/`, `vendor/`, and the rest — are ignored again. Git
  reports repository-relative paths, so a top-level directory had nothing
  before the first slash and escaped every `*/directory/*` rule, inflating
  additions, deletions, file counts, and work composition. Only nested copies
  were being ignored. The same root anchoring now applies to
  `--path-exclude` patterns written in that shape.
- `--raw` no longer reads as though each model belonged to the provider above
  it. The per-model figures were indented under the per-provider list, but they
  have always been totals across every provider. Provider and model are now two
  separately headed lists at the same indent, and a model list cut short says
  how many more there are.
- The human-work sparkline covers the rows the table actually printed. With
  `--top`, it was drawn from the whole report, so the picture disagreed with the
  numbers under it. It also names its direction now: oldest to newest, while
  the table lists rows newest first.
- The count of hidden diagnostics is the number of warnings raised, not the
  number the report kept. Only the first hundred are stored, so a run with more
  than that understated how much it was not showing.
- A crafted repository name, path, or warning can no longer reorder the line it
  appears on. Direction-override characters are replaced along with control
  characters everywhere text is drawn: the table, `--raw`, the CSV, the
  warnings, and every panel of `workstats ui`. A warning clipped at 200
  characters now ends in `…` instead of stopping mid-sentence.
- `--format json` carries the name a repository actually has. Replacement used
  to happen once, in the grouping key, so all three formats named a row the same
  way — but a key is an identifier that `jq`, a spreadsheet, and this tool's own
  explorer join back to the checkout, and a substituted character silently
  breaks that join. Worse, the key was also the bucket, so two repositories
  differing only by a replaced character collapsed into one row and under-counted
  both. Safety now belongs to the formats a terminal draws — the table, `--raw`
  and the CSV, which is read with `cat` as often as by a spreadsheet — while
  JSON is left faithful, where RFC 8259 escaping already makes a control
  character inert.
- A session with no recorded model is spelled `(no model)` throughout the table
  and `--raw`, rather than `unknown`, `<synthetic>`, or a bare dash depending on
  which adapter it came from — three spellings that split one figure across
  three rows. `--format json` and `--format csv` still carry the raw values.
- A zero is written as `0.0` rather than `-0.0`. Rounding kept the sign of what
  it was given, so a total that reached zero from below — a history with no
  human time in it, most often — produced a `-0.0` in JSON and CSV that reads
  like a bug and that some consumers stringify as one. Every derived seconds and
  share field is normalised at the point it is rounded.
- Counts agree with the nouns beside them: `1 commit`, not `1 commits`. This
  covers the summary lines, the truncation notes under the table and the model
  lists, the diagnostics footer, and the row counter in `workstats ui`.
- A long model id in `--raw` no longer pushes the figure beside it out of line.
  Bedrock-style ids run past the width of the name column, which was padded to a
  minimum rather than clipped to a maximum; names are now shortened from the
  left, keeping the version on the end that tells two builds of one model apart.

## 1.0.0 — 2026-08-18

### Removed

- **Breaking:** the `gitstats` alias and every remnant of it — the installed
  `gitstats` command, its Homebrew symlink, the legacy-entrypoint notice and
  `WORKSTATS_LEGACY_ENTRYPOINT`, and the `GITSTATS_DIR` / `GITSTATS_AUTHOR`
  environment fallbacks. Use `workstats`, `WORKSTATS_DIR`, and
  `WORKSTATS_AUTHOR` instead.

## 0.8.0 — 2026-08-18

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

## 0.7.0 — 2026-08-17

### Added

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
