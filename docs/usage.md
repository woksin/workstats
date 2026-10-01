# Usage

Everything beyond the short command list in the [README](../README.md#start-here): scope and authors, grouping and filtering, windows and comparisons, output formats, the interactive explorer, feeding in other tools, and a short guide to the commands for timesheets, branches, insights, a prompt line and multiple machines.

## Scope, branches and authors

**Git scope and AI scope are not the same thing.** The bare command scans Git
history under the current directory — `--dir PATH` or `WORKSTATS_DIR` moves that
root, `--depth N` (default 4) bounds how far below it repositories are
discovered. Retained AI history is always machine-wide, because that is how the
tools store it: a run in one checkout still sees sessions from everywhere unless
you narrow it with `--repo`, `--repo-exact`, `--since`/`--until`,
`--month`/`--year`/`--week`, or `--provider`. That asymmetry is deliberate and it is why
*committed output* counts only sessions in repositories Git actually scanned.

`--dir` and `WORKSTATS_DIR` must name a directory that exists. A path that does
not is an error naming which of the two was wrong, not an all-zero report.

Within each repository, **every local branch** is scanned, plus `HEAD` so a
detached checkout still counts — not only the branch you have checked out. A
commit reachable from several branches or worktrees is counted once.
Remote-tracking branches are not read, so a `git fetch` never changes a report.

**The author date decides the window.** `--since`, `--until`, `--month`,
`--year` and `--week` select commits by when they were *authored*, not when they were last
committed, so a commit written in March and rebased or amended in May is still
March work. (Git's own `--since`/`--until` use the committer date; workstats
only gives Git a widened lower bound to keep the history walk short, lists the
author dates, applies the exact window itself, and asks Git for line counts only
for the commits inside it — so a report on an old month does not pay for every
commit made since.)

Your Git author defaults to `git config --global user.email`, falling back to
`user.name`. Override it with `--author REGEX` or `WORKSTATS_AUTHOR`. If you
commit under several identities — work and personal addresses, an old name —
repeat the flag, `--author a@example.com -a b@example.com`, or list them under
`"authors"` in the [config file](configuration.md#inputs-and-index); Git ORs them into one
developer. The first source that names any wins, and sources do not combine:
`--author` flags, then `WORKSTATS_AUTHOR` (a single value), then the config
file's `authors`, then your global Git identity. The report's `inputs.author`
joins them with `, ` and `inputs.authors` lists them. Those identities are the
whole of what a report describes by default; commits a coding agent authored are
read only when you ask, and even then are reported apart from your own and never
as human time — see [Agent-authored commits](how-it-works.md#agent-authored-commits).

**Nested repositories are not scanned.** Discovery stops at the first `.git` it
finds, so a submodule or a clone nested inside another checkout is not scanned
as its own repository (its commits are not part of the parent's history
either). Point `--dir` at the nested repository to include it.

## Grouping and filtering

Dimensions can be composed: `root`, `repo`, `cwd`, `provider`, `model`, `day`,
`week`, `month`, `branch`, `issue`, `feature` and `engagement`. The default is
`repo`. `day`, `week` and `month` cut time into buckets, so a run uses at most
one of them.

The last four say *what the work was for*. `branch` is the Git branch a session
or commit was on, `issue` is the key cut from the branch name (`ACME-123`,
`#42`), `feature` is the issue, or the branch's slug when there is none, and
`engagement` is the client or contract from the config's `engagements`. Work
with no known branch, issue or engagement is grouped as `—` or `(unassigned)`
rather than dropped. How the branch is found, and what it costs, is in
[Branches and pull requests](branches.md); the engagement rules are in
[Timesheet](timesheet.md#engagements). Branch names can carry client or ticket
names, so review a grouped report before sharing it
([Privacy](privacy.md#branch-names-are-read-cached-and-reported)).

`week` is an ISO 8601 week: it starts on Monday and is labelled `2026-W09`. The
year in the label is the ISO week-numbering year, which is not always the
calendar year — Monday 29 December 2025 is in `2026-W01`, and 1 January 2027 is
still in `2026-W53` (2026 has 53 weeks; most years have 52). Labels are
zero-padded so they sort chronologically as text, in table, JSON and CSV output
alike. Like `day` and `month`, weeks are cut on the local calendar, so a
session that runs across Sunday midnight is split at local Monday 00:00, and a
week containing a daylight-saving change is simply an hour longer or shorter.

`repo` means the logical Git repository, not the checkout folder. Linked
worktrees share their Git common directory, and separately named clones that
have the same fetch remote share its normalized identity, so their sessions,
tokens, human involvement, and deduplicated commits appear in one project row.
Deduplication is scoped to each natural repository: worktrees and clones share
commit/file identity, while distinct `project_aliases` members that happen to
share a SHA or relative path remain separate. This reads local `.git` metadata
only and never contacts the remote. A deleted
Pi delegated worktree can still be attributed from the parent transcript's
bounded session header; that hint is used only when the child's own path has no
Git identity, and no message content is read. Live checkout identities are also
remembered in the transcript index, so a later report can recover a deleted
foreground worktree. Ambiguous directory reuse is left unresolved rather than
guessed. `--rebuild-cache` preserves this identity history; deleting the cache
file removes it. Use `cwd` (or `--by-dir`) when you deliberately want one row
per checkout/worktree.

Distinct repositories that form one product can be combined explicitly:

```json
{
  "project_aliases": {
    "cratis": {
      "label": "Cratis",
      "paths": ["/work/repos/cratis"],
      "remotes": [
        "https://github.com/acme/api.git",
        "git@github.com:acme/web.git"
      ]
    }
  }
}
```

A path matches every repository below it; remote spellings are normalized the
same way as discovered Git remotes. Alias members are ORed, overlapping aliases
are rejected, and the stable lowercase map key controls grouping while `label`
is display-only. Aliases change `repo`, never `cwd`, so `--by-dir` still shows
each checkout. `--explain-repository-attribution` (or `--explain-repos`) prints
the privacy-safe evidence used for every row and adds the structured explanation
to JSON; CSV and `workstats ui` reject it because their shape cannot represent
the one-to-many ledger.

```bash
workstats --group-by provider,model
workstats --group-by month,repo --top 0
workstats --repo service --path 'src/**' --path-exclude '**/*.generated.*'
workstats --depth 6 --no-ignore            # deeper discovery, generated files included
```

Three shortcuts exist for the groupings people ask for most: `--by-repo`
(`--group-by month,repo`), `--matrix` (`--group-by repo,month`), and `--by-dir`
(`--group-by cwd`). They are mutually exclusive with each other and with an
explicit `--group-by`, and combining them is an error rather than one of them
silently winning.

`--month`, `--year` and `--week` narrow the window a report covers, exactly as
`--since` and `--until` do. `--month 2026-07`, `--year 2026` and
`--week 2026-W09` name one outright; all three also accept `current` (or `this`)
and `last` (or `previous`), resolved against the local calendar. A week runs
Monday to Sunday, and naming one that does not exist (`2025-W53`) is an error
rather than a neighbouring week. They cannot be combined with each other or
with `--since`/`--until`.

They are filters, not groupings. `--group-by` and `--period` decide how the rows
*inside* that window are split, so the two compose rather than compete.

```bash
workstats --month last                     # the previous calendar month
workstats --month 2026-07 --group-by repo  # July, one row per repository
workstats --year 2026 --period month       # 2026, with a month column per row
workstats --week 2026-W09 --period day     # one ISO week, a row per day
workstats --month last --period week       # last month's weeks; the edge weeks are partial
```

### Comparing two windows

`--compare` reports the selected window and, beside it, an earlier one, with the
change between them. `--compare previous` takes the window immediately before
the selected one, of the same kind and length: the previous month for `--month`,
the previous ISO week for `--week`, the previous year for `--year`, and for
`--since`/`--until` the same number of days before `--since` (a range of whole
months steps back by whole months). Name a baseline outright with
`--compare 2026-06`, `--compare 2026-W09` or `--compare 2026`; it may not overlap
the selected window. Either way the selected window must have both ends, so
`--compare` needs `--month`, `--year`, `--week`, or `--since` with `--until`, and
is an error without one.

```bash
workstats --month 2026-07 --compare previous    # July against June
workstats --week last --compare previous        # last week against the one before
workstats --month 2026-07 --compare 2026-01     # July against January
```

```text
Comparison  (changes are estimates — not stopwatch times)
  Current   2026-07
  Previous  2026-06 (the window before)

  Measure                        Current    Previous  Change
  ───────────────────────────────────────────────────────────────────────────
  Estimated human work           62h 10m     48h 30m  +13h 40m (+28%)
  Active work days                    19          17  +2 (+12%)
  Git commits                        112          96  +16 (+17%)
  ...
  Share                          Current    Previous  Change
  ───────────────────────────────────────────────────────────────────────────
  source lines                       61%         55%  +6 pp
  test lines                         27%         34%  -7 pp
```

The comparison is headline-level: the estimate and active days, prompts,
sessions (foreground and subagent), commits and changed lines, agent-authored
output, AI co-authored commits, agent wall clock and parallel agent work, and
each file area's share of changed lines with its change in percentage points.
Grouped rows are not compared. The human-work and active-day figures and their
changes are estimates, as everywhere in workstats, and the output says so. A
change from zero has no percentage and shows `n/a`, never infinity. A quiet
window or a pruned history reads as a drop, so check the AI sessions line before
reading a change as a change in effort.

Each window is built by the same code a run of that window on its own uses, once
for the selected window and once for the baseline, so each side is exactly the
report that window would print alone: the same sources, the same Git checkouts
(`--dir` plus the checkout of every retained session), the same repository
labels and `--repo-exact` matching. The transcript index keeps the second pass
cheap. Adding `--compare` never changes the selected window's report, including
`inputs.git_scan_roots`, with one exception: when the baseline raises warnings
the selected window did not show, one extra warning says so, and running the
baseline on its own shows them. Warnings both windows share, such as a
malformed transcript line, are shown once and add nothing. Only the first 100
warnings of a run keep their text, so the number in that line counts only
those and is a floor; and when the selected window itself raised more than
100, no extra line is added at all, because its dropped warnings may be the
baseline's. In `--format json` the comparison is
an added `comparison` object with `current`, `previous` and `delta` (a `change`
and a `percent`, which is `null` when the earlier figure is zero, per figure;
`change_points` for shares). Table, Markdown and HTML show the same block just
after the summary. CSV, `workstats ui` and `workstats allocate` refuse
`--compare` with an error: CSV is one flat table with no place for a second
window, the explorer browses one report, and an allocation covers one period.
When the refused format came from `defaults.format` rather than `--format`, the
error says so and suggests passing `--format table` or `--format json`.

`--depth N` (default 4) bounds Git repository discovery below the scan root.
`--no-ignore` includes the generated and vendor paths — `node_modules/`,
`dist/`, `build/`, lockfiles, and the rest — that are otherwise counted
separately as ignored lines.

`--repo PATTERN` is a broad case-insensitive substring filter, matched against
the same three labels on both sides — the repository name, the working
directory, and the source root — so a pattern naming a source root selects
commits as well as AI sessions. `--repo-exact NAME` first matches final displayed
logical-repository names (including a disambiguation suffix when shown). If no
label matches, it falls back to a final checkout folder name. Label precedence
lets an explicit project alias be selected without also matching a checkout
folder with the same name, while preserving exact path-based selection. It
avoids mixing names such as `api` and `api-tools`.
`workstats` also scans locally available Git checkouts
for retained AI sessions—even when those checkouts are outside `--dir`. This
keeps Git and AI results aligned regardless of whether a repository filter is
used, without assuming a particular projects folder; `--dir` remains the
primary Git discovery root.

Source roots are customizable without exposing local paths in the repository:

```json
{
  "source_roots": [
    {"pattern": "^/work/clients/([^/]+)/.*", "replacement": "client/\\1"}
  ]
}
```

Or repeat `--source-rule 'REGEX=NAME'` on the command line. Rules are limited to
a deliberately safe regex subset and bounded in size/count.

## Reports made for pipes

```bash
workstats --format json > workstats.json
workstats --explain-human-time                     # readable calculation ledger
workstats --format json --explain-human-time \
  | jq '.human_time_explanation.blocks'            # structured calculation ledger
workstats --group-by month,repo --format csv > workstats.csv
workstats --format markdown > workstats.md         # tables for a PR, issue, or wiki
workstats --format html > workstats.html           # one self-contained page
workstats allocate -p Ada --sub claude=2 --month 2026-08 --format markdown
```

The animated status line lives on stderr and appears only in a real terminal.
It disables itself when redirected, in CI, or under `TERM=dumb`, so stdout stays
machine-readable. Use `--no-progress`, `WORKSTATS_NO_PROGRESS=1`, `--no-color`,
or `NO_COLOR` when you want explicit control. `workstats ui` is interactive
only — it writes no machine-readable output and refuses `--format
json|csv|markdown|html` rather than pretending otherwise.

CSV columns for the file areas follow the
[category registry](configuration.md#make-the-areas-your-own), so read them by header name.
`--format markdown` prints GitHub-flavoured tables with the same sections and
figures as the table view — summary, grouped rows, the agent-authored section,
notes and warnings — and escapes `|`, backticks, `<` and the other characters
Markdown acts on in repository names and paths. It also defuses what GitHub
links from plain text: a zero-width space follows an `@` and sits between `#` or
`GH-` and a number, so a repository called `@scope/pkg` does not mention anyone
and `#123` does not link an issue when the report is pasted into a PR. The
Markdown and HTML notes, warnings and `cwd`/`root` row labels replace your
home directory with `~`, so a
document does not carry your username or client folder names; the table view
shows paths as they are. A run that took values from the config ends its notes
with `Config defaults: …`, in the table, Markdown, HTML and `allocate` outputs
alike. `--format html` prints one
static page to stdout: inline CSS only, no JavaScript, no fonts, images or
links, and a `Content-Security-Policy` of `default-src 'none'`, so it opens
identically offline and never phones home. It follows the reader's light or
dark setting, and every value is HTML-escaped. Both work for `workstats` and
`workstats allocate`; `sources` and `classify` stay table, JSON, or CSV. Like
JSON and CSV they print no update notice, and progress stays on stderr.

A calculation ledger is one-to-many relative to CSV's grouped rows, so
`--explain-human-time` supports table and JSON output and deliberately rejects
`--format csv`, `markdown`, and `html` rather than silently omitting detail.

## Report flags added for the newer commands

Three flags work on every report command, `workstats ui` included:

- `--daily` adds human and agent figures for each local day to JSON output (they
  are always present in HTML and the explorer). It also draws the
  [calendar heatmap](calendar.md) in Markdown.
- `--import FILE` folds a bundle written by `workstats export` into the report,
  repeatable; see [Merging machines](merge.md).
- `--no-goals` leaves the config's [`goals`](configuration.md#goals) out of the
  report, the digest and `now`.

## The other commands

Each has its own page; this is the one-line version.

```bash
workstats timesheet --month last             # suggested hours per day and client
workstats timesheet --month last --export toggl > toggl.csv
workstats branch                             # the effort behind the current branch
workstats pr --number 123 --format markdown  # a block for a pull request description
workstats insights --section focus,leverage  # focus, patterns and leverage, last 28 days
workstats digest                             # this week against the week before
workstats now                                # a short line for a prompt or status bar
workstats calendar                           # a year grid of human time per day
workstats export --output laptop.json        # this machine's evidence, to merge elsewhere
workstats merge laptop.json desktop.json     # one report from several machines
workstats record --provider cursor --session s1 --kind prompt --branch feat/ACME-1
```

- **`timesheet`**: one entry per day per [engagement](timesheet.md#engagements),
  rounded, with manual entries, overrides and period locks
  ([the ledger](timesheet.md#the-ledger-manual-entries-overrides-and-locks)),
  vendor CSV exports and opt-in [descriptions](timesheet.md#descriptions).
  See [Timesheet](timesheet.md).
- **`branch` and `pr`**: the human time, agent time, tokens, list value and
  commits behind one branch or pull request, or one row per branch with `--all`;
  `pr --number N` finds the branch through the sessions that mentioned it.
  `--describe commits,sessions` adds commit subjects and session titles. See
  [Branches and pull requests](branches.md#the-branch-and-pr-commands).
- **`insights` and `digest`**: focus, patterns (when you work), leverage and
  models over a window (default: the last 28 days), and a weekly summary
  compared with the week before that also shows progress against your goals. See
  [Insights and digest](insights.md).
- **`now`**: today and the week so far, cheap enough to run on every prompt
  redraw because a fresh snapshot is printed without scanning anything. Format
  it with `--template` (or `now.template` in the config); `--no-wait` prints the
  last result at once and refreshes in the background, and `--quiet-errors`
  keeps a prompt clean when something fails. See
  [`now` in the configuration](configuration.md#now).
- **`calendar`**: the heatmap in the terminal; the same grid is in the HTML and
  Markdown reports and the explorer. See [Calendar heatmap](calendar.md).
- **`export` and `merge`**: write a content-free bundle of this machine's
  evidence and fold bundles from several machines into one report, or into any
  report with `--import`. See [Merging machines](merge.md).
- **`record --branch NAME`** stores the branch the work was on in the event log,
  so tools that are not read natively still group by `branch`, `issue` and
  `feature`. See [Add any tool or API](#add-any-tool-or-api).

## Explore it interactively

```bash
workstats ui                               # explore the current directory
workstats ui --dir ~/projects --since 2026-07
```

`workstats ui` builds exactly the report the printed dashboard builds and then
opens it as a drill-down explorer instead of printing it. It takes the same
report flags, but they go **after** the subcommand — `workstats ui --dir .`, not
`workstats --dir . ui`.

Drill down with `Enter`, back out with `Esc`:

```text
overview → repository → month, day or week → file area → commit → changed file → diff
```

Two levels behave less literally than they look, on purpose. A **commit** lists
every path it touched, not just the file area you drilled through, because a
commit is one atomic change and showing half of it misleads — so its file count
can differ from the per-area count above it. A **changed file** shows that
path's whole history in the repository rather than only the period in the
breadcrumb, because "when else did this change?" is the next question. Pressing
`Enter` there opens [the diff](privacy.md#the-diff-viewer-is-the-one-place-file-contents-are-read).

| Key | Does |
| --- | --- |
| `↑` `↓` / `k` `j` | move the selection |
| `PgUp` `PgDn` / `Ctrl-b` `Ctrl-f` / `Space` | move by a screen |
| `Home` `End` / `g` `G` | first / last row |
| `Enter` / `→` / `l` | descend into the selected row |
| `Esc` | close an overlay, then clear the filter, then go up one level |
| `Backspace` / `←` / `h` | go up one level |
| `/` | filter the current level as you type |
| `s` | fuzzy search repositories, files, and commits |
| `1`–`9` | sort by that column, numbered in `?` for the level on screen; press again to reverse |
| `[` `]` / `o` | previous / next sort column; reverse the order |
| `p` | cycle the period through month, day and ISO week |
| `w` / `v` / `d` | save the current view / open saved views / delete the highlighted one in that list |
| `?` | show or hide the key map and this level's sort keys |
| `q` / `Ctrl-C` | quit |

Search is fuzzy and ranked, over repository names, every changed file path, and
commits. A commit is matched by its short SHA and a summary derived from the
files it touched (`7 files · source, test`) — commit *messages* are not read
here either, so the explorer stays inside the same boundary as the report.

Saved views are bookmarks: a drill-down path, the period grain, the sort, and
the filter. They live next to the config file as
`~/.config/workstats/views.json` (`WORKSTATS_VIEWS` overrides the path) and
never in the cache. A saved view holds no measured data and never stores the
diff level, so restoring one cannot read a file's contents unasked. At most 64
are kept; an unreadable file reads as an empty bookmark list rather than an
error.

The explorer needs an interactive terminal. When stdout is redirected or piped,
or under `TERM=dumb`, `workstats ui` says so and exits rather than emitting
escape codes, and `workstats ui --format json|csv|markdown|html` is refused before any
scanning — use `workstats --format json` for machine-readable output.

## Add any tool or API

`workstats record` appends one content-free signal to the platform event log.
It intentionally has no prompt, response, token, or API-key argument.

```bash
# A foreground prompt from an editor or internal CLI
workstats record \
  --provider cursor \
  --session issue-184 \
  --model sonnet-4.5 \
  --kind prompt

# An exact interval measured by an API wrapper
workstats record \
  --provider openai-api \
  --session nightly-refactor \
  --model model-x \
  --role subagent \
  --started-at 2026-08-15T09:30:00Z \
  --completed-at 2026-08-15T09:31:12Z
```

The default event log is always loaded, including alongside `--events`, which
*adds* logs rather than replacing the default one. Use `--output FILE` to choose
a log or `--output -` to emit JSONL to stdout. Existing logs can be added
directly:

```bash
workstats --events ./activity.jsonl
workstats --events ./team-export/ --provider openai-api,internal-agent
workstats --events ./activity.jsonl --no-default-events   # this log only
```

Paths are deduplicated by canonical path, so naming the default log explicitly
cannot double-count it, and `--no-default-events` leaves it out entirely.

The [v1 JSON Schema](../schema/workstats-events-v1.schema.json) is deliberately
small and forward-compatible; unknown structural fields are ignored:

```json
{"timestamp":"2026-08-15T09:30:00Z","provider":"openai-api","session_id":"task-42","cwd":"/workspace/project","model":"model-x","event":"prompt","role":"foreground"}
```

`event` is `prompt` or `activity`; `role` is `foreground` or `subagent`. An
optional `branch` (`record --branch NAME`) names the Git branch the work was on.
Optional RFC 3339 `started_at` and `completed_at` fields provide an exact agent
interval. On Windows, JSON paths use normal JSON escaping—using `workstats
record` handles that automatically. Records containing common payload fields
such as `content`, `prompt`, `response`, `input`, `output`, or `api_key` are
rejected instead of indexed, and counted on their own line — `Privacy: N
record(s) carrying prompt or response text were skipped, as designed.` — rather
than as malformed input.

## Move or filter histories

Provider choices are names, not a closed enum. Values can be repeated or
comma-separated, and aliases such as `claude-code`, `gemini-cli`, and
`github-copilot` are normalized.

```bash
workstats --history gemini=/mnt/archive/gemini
workstats --history codex=D:\agent-history\sessions
workstats --provider gemini --exclude-provider internal-agent
```

`--history PROVIDER=PATH` supports the native adapters shown by `workstats
sources`. For any other provider name, use `--events` or `workstats record`.
