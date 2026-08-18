<div align="center">

<img src="assets/banner.svg" alt="workstats — human work, Git output, and agent activity" width="900">

<br>

[![CI](https://github.com/woksin/workstats/actions/workflows/ci.yml/badge.svg)](https://github.com/woksin/workstats/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/woksin/workstats?color=86efac)](https://github.com/woksin/workstats/releases/latest)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-67e8f9)](#platforms)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-b7410e?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-86efac)](LICENSE)

**See where the work happened—without sending your work anywhere.**

`workstats` turns local Git history and retained AI-tool metadata into an honest
view of focused human work, code output, and parallel agent activity. It
auto-detects supported histories and gives every other CLI, IDE, script, and API
wrapper a tiny open event format—without uploading anything.

</div>

---

```text
  ⠼ Discovering local AI activity  0.8s

WORKSTATS  human involvement across local projects
══════════════════════════════════════════════════════════════════════════════════════════════
  Estimated human work  10h 24m
  Active work days      2
  Average / active day  5h 12m
  Work blocks          6  (24 foreground session edges + 184 prompts + 31 commits)
  Git commits             31
  Git lines               +8,421 / -2,107
  Agent-authored          14 commits  +6,204 / -1,118  (output only — no human time)
  Co-authored by AI       9 of the 31 commits above
  Observed                2026-08-01 → 2026-08-18

AI activity  (context only — these are not human hours)
  Agent wall clock      11h 18m  (any agent active, overlap removed)
  Parallel agent work   37h 06m  (3.3× concurrency)
  Sessions              74  (12 foreground, 62 subagents)
  Tokens                18.4M  (2.1M in, 640.3k out, 15.7M cached)
  Committed output      9 of 12 foreground sessions in repos with visible commits
                        3 left no commit — reading, review, or uncommitted work

Work composition  (changed Git lines by file area)
  Area         Files       Added     Removed   Share
  ──────────────────────────────────────────────────
  source         214      +5,900      -1,500     70%
  test            63      +1,700        -400     20%
  docs            18        +520        -140      6%
  config          11        +301         -67      4%
  Test lines per source line  0.28

Change shapes  (from diff composition only — commit messages are never read)
  Shape        Commits   Share
  ────────────────────────────
  new code          14     45%
  revision           9     29%
  tests              5     16%
  docs               3     10%

By repo  (human involvement first; AI wall clock shown as context)
  Work area                                  Human  Days   Avg/day  Commits   AI wall Agent work    Tokens
  ──────────────────────────────────────────────────────────────────────────────────────────────────────────
  api                                       5h 50m     2    2h 55m       18    6h 20m    21h 04m      9.8M
  web                                       3h 39m     2    1h 49m       11    4h 14m    12h 30m      6.1M
  cli                                       0h 55m     1    0h 55m        2    0h 44m     3h 32m      2.5M

Agent-authored Git output  (landed code you did not type — no human time, no work blocks)
  Work area                                Commits       Added     Removed
  ────────────────────────────────────────────────────────────────────────
  api                                            9      +4,102        -712
  web                                            5      +2,102        -406
```

The two agent lines in the summary and the block at the end appear only when a
run asks for them, with [`--agent-commits` and
`--co-authors`](#agent-authored-commits). Agent-authored commits are shown
because they are real output, and shown *apart* because none of them is evidence
that anyone was at the keyboard: they add no human time, no work blocks, and no
active human days.

That is the printed report. [`workstats ui`](#explore-it-interactively) opens
the same numbers as a drill-down explorer — repository, month, file area,
commit, changed file, diff — with search, filtering, and saved views.

## The useful distinction

Agent runtime is not automatically human work. Lines changed are not time. A
session left open overnight is not automatically an eight-hour day. Prompts and
commits provide direct evidence; foreground session boundaries add bounded
setup and review evidence without treating autonomous output as attendance.

`workstats` keeps those ideas separate:

| Signal | What it answers | How it is treated |
|---|---|---|
| **Human-work estimate** | “How much time was plausibly spent developing or supervising?” | Prompts, foreground session boundaries, and authored commits form non-overlapping involvement blocks with setup/review credit. |
| **Git output** | “What changed?” | Commits, files, additions, deletions, and ignored generated/vendor lines — all of them authored by the identity `--author` names. |
| **Agent-authored Git output** | “What landed that I did not type?” | Commits a coding agent authored, matched by Git identity in a second pass and reported in their own columns and their own section. **Never human time**: no work blocks, no setup/review credit, no active *human* days, and never added into the Git-output figures above. A `Co-authored-by:` trailer is the other case — it describes a commit you already wrote, so it is a share of your commits and never an addition to them. |
| **Agent wall clock** | “How long was any agent active?” | Overlapping agent intervals count once. |
| **Parallel agent work** | “How much automation ran?” | Concurrent sessions are summed, so this can exceed wall time. |
| **AI tokens** | “How many tokens did agents use?” | Input, output, cache-read, and cache-creation counts read from local transcripts; not an intervaled/deduplicated metric like wall clock, so grouped totals just sum. |
| **Work composition** | “Where did the output land?” | Changed lines bucketed into file areas — `source`, `test`, `docs`, `config`, `assets`, `other` by default, and [whatever else you configure](#make-the-areas-your-own) — from the file path alone. |
| **Change shapes** | “What did the work look like?” | Each commit described by its dominant file area and its addition/deletion balance. Never read from the commit message. |

That makes the dashboard useful without pretending it is a stopwatch, an
attendance system, or a universal productivity score.

## Bring your whole AI stack

```text
  ● claude          Claude Code                    built-in
  ● codex           OpenAI Codex                   built-in
  ● gemini          Google Gemini CLI              built-in
  ● copilot         GitHub Copilot CLI             best-effort
  ● copilot-vscode  GitHub Copilot Chat (VS Code)  best-effort
  ● opencode        OpenCode                       best-effort
  ● events          any CLI / IDE / API            stable open JSONL
```

GitHub Copilot is covered on two surfaces, the CLI and Copilot Chat in VS Code,
because those are the two that leave a timestamped local record. Inline
completions leave none, so nothing counts them.

Run `workstats sources` to see what is detected on the current machine. Native
adapters read structural fields from documented or inspectable local histories.
The open event bridge covers tools such as editor agents, internal assistants,
SDK calls, and proprietary workflows without making `workstats` depend on every
vendor's private database schema.

There is no credential discovery and no attempt to sign in to providers. A
history adapter is enabled only when its local source exists; everything remains
optional.

## Install

### Prebuilt binaries

No Rust toolchain is required. Every release includes a `SHA256SUMS` file.

<table>
<tr><td width="145"><b>Homebrew</b><br><sub>macOS · Linux</sub></td><td>

```bash
brew install woksin/workstats/workstats
```

The fully-qualified name taps `woksin/workstats` and trusts that one formula as
it installs it. Homebrew 5.1.15 and newer refuse to load a formula from a
non-official tap until it is trusted, so installing by the short name instead
takes `brew tap woksin/workstats && brew trust woksin/workstats` first, which
trusts the whole tap rather than a single formula.

</td></tr>
<tr><td><b>macOS</b></td><td>

```bash
# Apple silicon
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-macos-arm64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# Intel: replace arm64 with x86_64
```

</td></tr>
<tr><td><b>Linux</b></td><td>

```bash
# x86_64
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-linux-x86_64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# ARM64: replace x86_64 with arm64
```

</td></tr>
<tr><td><b>Windows</b></td><td>

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\workstats\bin" | Out-Null
Invoke-WebRequest https://github.com/woksin/workstats/releases/latest/download/workstats-windows-x86_64.exe `
  -OutFile "$env:LOCALAPPDATA\workstats\bin\workstats.exe"
```

Add that directory to your user `PATH`. A 32-bit
`workstats-windows-x86.exe` is also published.

</td></tr>
</table>

> [!NOTE]
> The macOS binaries are currently unsigned. Downloads made by a browser may
> need `xattr -d com.apple.quarantine workstats`; downloads piped through
> `curl` normally do not receive the quarantine attribute.

### Build from source

```bash
cargo install --git https://github.com/woksin/workstats --locked
```

Or clone the repository and use the platform installer. These compile the
locked release build and preserve existing commands unless `--force` /
`-Force` is explicit.

```bash
# macOS / Linux
./install.sh

# Windows PowerShell
./install.ps1
```

## Updating

`workstats` never phones home on a normal run. Checking for or installing a
new version only ever happens when you ask for it:

```bash
workstats update            # check, download, verify, and install a newer release
workstats update --check    # only report whether a newer version exists
```

`workstats update` fetches the latest release from GitHub, verifies the
downloaded binary against the release's published `SHA256SUMS`, and replaces
the running executable in place. It refuses to run if no prebuilt binary is
published for your platform.

If you'd like a passive reminder instead of running the command yourself, opt
into a throttled (at most once every 24 hours) background check that prints a
one-line footer on normal runs when a newer version is known:

```bash
workstats --check-updates                  # opt in for this run
WORKSTATS_CHECK_UPDATES=1 workstats         # opt in via environment
```

```json
{"check_updates": true}
```

in the [config file](#inputs-and-index) opts in permanently. `--no-update-check`
or `WORKSTATS_NO_UPDATE_CHECK=1` suppresses both the background check and the
footer for a single run, regardless of how it was enabled. Every check is a
plain HTTPS request to GitHub's public release API—no other data leaves the
machine, and nothing is ever installed without `workstats update`.

## Start here

```bash
workstats sources                          # supported + detected AI histories
workstats                                  # dashboard for the current directory
workstats ui                               # explore the same report interactively
workstats --dir ~/projects                 # discover repositories below a directory
workstats --group-by month,repo            # recent work by month and repo
workstats --period day --group-by root     # daily trend by source root
workstats --since 2026-07 --until 2026-08  # inclusive local calendar bounds
workstats --month 2026-07                  # filter to one calendar month
workstats --year last                      # filter to the previous calendar year
workstats --provider codex,gemini --group-by model
workstats --exclude-provider copilot
workstats --repo-exact my-project          # infer its checkout from matching AI sessions
workstats --agent-commits                  # also read commits a coding agent authored
workstats --raw                            # provider/model detail (alias --show-agent-work)
workstats classify src/main.rs             # which file area a path lands in, and why
```

**Git scope and AI scope are not the same thing.** The bare command scans Git
history under the current directory — `--dir PATH` or `WORKSTATS_DIR` moves that
root, `--depth N` (default 4) bounds how far below it repositories are
discovered. Retained AI history is always machine-wide, because that is how the
tools store it: a run in one checkout still sees sessions from everywhere unless
you narrow it with `--repo`, `--repo-exact`, `--since`/`--until`,
`--month`/`--year`, or `--provider`. That asymmetry is deliberate and it is why
*committed output* counts only sessions in repositories Git actually scanned.

`--dir` and `WORKSTATS_DIR` must name a directory that exists. A path that does
not is an error naming which of the two was wrong, not an all-zero report.

Your Git author defaults to `git config --global user.email`, falling back to
`user.name`. Override it with `--author REGEX` or `WORKSTATS_AUTHOR`. That one
identity is the whole of what a report describes by default; commits a coding
agent authored are read only when you ask, and even then are reported apart from
your own and never as human time — see
[Agent-authored commits](#agent-authored-commits).

### Explore it interactively

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
overview → repository → month or day → file area → commit → changed file → diff
```

Two levels behave less literally than they look, on purpose. A **commit** lists
every path it touched, not just the file area you drilled through, because a
commit is one atomic change and showing half of it misleads — so its file count
can differ from the per-area count above it. A **changed file** shows that
path's whole history in the repository rather than only the period in the
breadcrumb, because "when else did this change?" is the next question. Pressing
`Enter` there opens [the diff](#the-diff-viewer-is-the-one-place-file-contents-are-read).

| Key | Does |
|---|---|
| `↑` `↓` / `k` `j`, `PgUp` `PgDn`, `Home` `End` | move the selection |
| `Enter` / `→` / `l` | descend into the selected row |
| `Esc` | close an overlay, then clear the filter, then go up one level |
| `Backspace` / `←` / `h` | go up one level |
| `/` | filter the current level as you type |
| `s` | fuzzy search repositories, files, and commits |
| `1`–`9` | sort by that column; press again to reverse |
| `[` `]` / `o` | previous / next sort column; reverse the order |
| `p` | switch the period between month and day |
| `w` / `v` / `d` | save the current view / open saved views / delete the highlighted one |
| `?` | show or hide the key map |
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
escape codes, and `workstats ui --format json|csv` is refused before any
scanning — use `workstats --format json` for machine-readable output.

### Add any tool or API

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

The [v1 JSON Schema](schema/workstats-events-v1.schema.json) is deliberately
small and forward-compatible; unknown structural fields are ignored:

```json
{"timestamp":"2026-08-15T09:30:00Z","provider":"openai-api","session_id":"task-42","cwd":"/workspace/project","model":"model-x","event":"prompt","role":"foreground"}
```

`event` is `prompt` or `activity`; `role` is `foreground` or `subagent`.
Optional RFC 3339 `started_at` and `completed_at` fields provide an exact agent
interval. On Windows, JSON paths use normal JSON escaping—using `workstats
record` handles that automatically. Records containing common payload fields
such as `content`, `prompt`, `response`, `input`, `output`, or `api_key` are
rejected instead of indexed, and counted on their own line — `Privacy: N
record(s) carrying prompt or response text were skipped, as designed.` — rather
than as malformed input.

### Move or filter histories

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

### Reports made for pipes

```bash
workstats --format json > workstats.json
workstats --group-by month,repo --format csv > workstats.csv
```

The animated status line lives on stderr and appears only in a real terminal.
It disables itself when redirected, in CI, or under `TERM=dumb`, so stdout stays
machine-readable. Use `--no-progress`, `WORKSTATS_NO_PROGRESS=1`, `--no-color`,
or `NO_COLOR` when you want explicit control. `workstats ui` is interactive
only — it writes no machine-readable output and refuses `--format json|csv`
rather than pretending otherwise.

CSV columns for the file areas follow the
[category registry](#make-the-areas-your-own), so read them by header name.

## Why it is fast

Transcript files can be enormous, so the hot path is deliberately boring:

1. Stream JSONL records instead of loading histories into memory.
2. Deserialize only structural fields; skip prompt and response bodies.
3. Parse changed files in parallel.
4. Cache derived structural metadata in SQLite.
5. Use file timestamps and observed time ranges to prune warm queries.

An illustrative one-day report over roughly 6.8 GB of retained transcripts on
an Apple silicon laptop:

| Mode | Time | Peak memory |
|---|---:|---:|
| Index disabled | 2.7 s | 172 MiB |
| Warm index | **1.05 s** | **54 MiB** |

Different histories and disks will vary; the architectural win is that normal
runs parse only what changed.

## How the estimate works

1. Foreground human prompts and commits **you** authored provide direct evidence
   of involvement without reading prompt or response text. A commit a coding
   agent authored provides none, and there is no route by which one can become
   human time — see [Agent-authored commits](#agent-authored-commits).
2. The start and end of each foreground session add bounded setup/follow-up
   evidence. Internal assistant and tool events do not keep a human block alive;
   meta messages, sidechains, and subagent sessions do not add human time.
3. Signals no more than one hour apart form a work block. The intervening time
   counts because development often continues through reading, testing, review,
   and agent execution.
4. Each block receives 30 minutes total for setup and follow-up review, split
   around its first and last signal and clamped to local calendar boundaries.
5. All human intervals form one global union. Ten concurrent agents cannot make
   ten simultaneous human hours.

Tune the assumptions when your workflow needs it:

```bash
workstats --human-idle 90m --review-credit 45m  # more generous
workstats --human-idle 30m --review-credit 10m  # more conservative
```

`--gap-cap` controls AI wall-clock estimation only. Durations are written as
`30s`, `5m`, or `1h` and are accepted up to `8784h` (366 days); anything larger
is rejected with a message naming the flag and the value. The human estimate can
still miss meetings, thinking away from a recorded session, deleted history, and
work performed on another machine. Treat it as a realistic local heuristic,
never payroll data.

## What was worked on

Time answers *how much*. Two path-derived breakdowns answer *where* and *what
kind*, without opening a single file or reading a single commit message.

**Work composition** buckets every changed line by the area its file belongs to.
The areas are a registry, listed here in match order — **the first category
whose rules match a path wins**:

| Area | Matched by |
|---|---|
| `test` | A `tests/`, `spec/`, `__tests__/`, `fixtures/`, or `benches/` directory; a sibling test project such as `Arc.Core.Specs/` or `Fundamentals.Tests/`; a BDD behaviour folder such as `for_Subject/when_something/given/`; or a name such as `user_spec.rb`, `Button.test.tsx`, `UserServiceTest.java`, `when_binding.cs`. |
| `docs` | `.md`, `.rst`, `.adoc`, a `docs/` directory, or a `README`/`LICENSE`/`CHANGELOG`-style name. |
| `config` | Manifests, CI, and tooling — `.toml`, `.yml`, `.json`, `Dockerfile`, `Makefile`, `.github/**`. |
| `assets` | Images, fonts, media, and other binaries. |
| `source` | Known source extensions — `.rs`, `.go`, `.ts`, `.py`, `.sql`, `.css`, and friends. |
| `other` | Anything left unclassified, kept visible instead of forced into a bucket. |

A file is classified once, by path, in that order — so `tests/fixtures/data.json`
is a test rather than config, and `docs/architecture.md` is docs rather than
source. This measures **churn, not codebase size**: it is the volume of work
that landed in each area, not how much test or source code the repository
currently contains.

**Change shapes** describe each commit by the area holding at least 60% of its
changed lines, and — for code-like areas such as `source` — by its
addition/deletion balance:

| Shape | Diff |
|---|---|
| `new code` | Code-dominant, deletions under a quarter of additions. |
| `revision` | Code-dominant, additions and deletions comparable. |
| `removal` | Code-dominant, deletions more than double additions. |
| `tests` / `docs` / `config` / `assets` / *your own area* | Dominated by that area, which names the shape. |
| `mixed` | No area reached 60%, or the dominant area was `other`. |

These name the *shape of the diff*, never the author's intent. A commit
message that says "refactor" has no bearing on the label, because the message
is never read. Note that a feature is not a countable unit here; source-area
additions and `new code` commits are the closest honest proxy.

**Committed output** compares foreground AI sessions against authored commits:
a session counts as having produced output when a commit lands in the same
repository within one `--human-idle` window of it. Only sessions in
repositories that Git actually scanned are counted at all — AI history spans
the whole machine while `--dir` usually does not, and an unscanned repository
says nothing either way. The remainder genuinely covers reading, review, and
uncommitted work, which local structure cannot tell apart.

Both breakdowns appear in the dashboard, per row in JSON, and as
`{area}_files` / `{area}_additions` / `{area}_deletions` columns in CSV. They
respect `--path`, `--path-exclude`, and the generated/vendor ignores, so
narrowing the scope narrows the breakdown too:

```bash
workstats --format json | jq '.summary.composition'
workstats --group-by month --format csv > areas-by-month.csv
workstats --path 'src/**'                  # composition of one subtree
```

> [!IMPORTANT]
> Those CSV columns are **derived from the category registry**, so they depend
> on the config: a new area adds three columns and a renamed one renames them.
> Their order follows match order, which by default is
> `test_*`, `docs_*`, `config_*`, `assets_*`, `source_*`, `other_*`. Read CSV
> by header name rather than by position, and expect a machine consuming
> reports from several people to see different columns if they configure
> different areas.

### Make the areas your own

The six built-ins are defaults, not a closed set. The `categories` block in the
[config file](#inputs-and-index) adds rules to a built-in area, and any name the
built-ins do not know creates a **new** area that appears everywhere the others
do — the dashboard, JSON `composition`, CSV columns, change shapes, and the
explorer:

```json
{
  "categories": {
    "test":     {"directory_prefixes": ["it_"]},
    "ai":       {"directories": [".ai", ".claude"], "names": ["CLAUDE.md", "AGENTS.md"]},
    "planning": {"directories": ["planning", "rfcs"], "name_suffixes": ["-plan.md"]},
    "corpus":   {"directories": ["corpus"], "extensions": ["jsonl"]}
  },
  "category_mode": "extend"
}
```

`category_mode` decides what happens to a name the built-ins already know:

- **`"extend"`** (the default) adds your rules to that area's built-in rules.
  `"test": {"directory_prefixes": ["it_"]}` keeps every existing test rule and
  adds one.
- **`"replace"`** discards that area's built-in rules and uses only yours. It
  changes the rules, not what kind of work the area is: an area stays code-like
  unless the block says `"code_like"` explicitly.

Either way, a name the built-ins do not know is a new area, and new areas are
matched **before** the built-ins (among themselves in name order). That is what
makes `.claude/settings.json` land in `ai` rather than `config`.

Every rule set is a list of plain strings — no regular expressions:

| Rule | Matches |
|---|---|
| `directories` | An exact path component above the file name (`"corpus"`, `".claude"`). |
| `directory_prefixes` / `directory_suffixes` | A path component starting or ending with it (`"for_"`, `".specs"`). |
| `extensions` | The final extension, written `".rs"` or `"rs"`. |
| `names` | The whole file name (`"CLAUDE.md"`). |
| `name_prefixes` / `name_suffixes` / `name_contains` | Part of the file name (`"when_"`, `".test."`). |
| `stems` | The file name without its final extension (`"readme"`). |
| `stem_suffixes` | The end of that stem (`"_test"`). |
| `cased_stem_suffixes` | The same, matched against the **original** casing, so `UserTest.cs` is a test and `Latest.cs` is not. |
| `globs` | A glob over the whole path, matched case-sensitively (`"docs/**/*.png"`). |
| `code_like` | `true` opts the area into the `new code` / `revision` / `removal` shapes instead of being named directly. |

Everything except `cased_stem_suffixes` and `globs` is case-insensitive, so
`"CLAUDE.md"` and `"claude.md"` behave the same. Within one area the rule kinds
are tried most specific first — globs, then directories, then file names, then
the extension — which only decides *which rule* is reported as the reason, never
which area wins.

The registry is bounded the way the source-root rules are: at most 32
categories, 128 rules per category, 128 bytes per rule (256 for a glob), no
empty strings and no control characters. A category name must be lowercase
`[a-z][a-z0-9_-]*`, at most 32 characters; `ignored` is reserved, because
`ignored_additions` is already a CSV column of its own. Breaking one of those
bounds, or writing a `category_mode` other than `extend`/`replace`, stops the
run with a message naming the problem rather than quietly reporting different
numbers. A misspelled *rule key* is a JSON error like any other malformation:
the whole config is ignored for that run and the reason is printed as a
`Warning:` line under the report, so a typo is visible instead of silently
partial.

`workstats classify` answers "why did this file land there?" without running a
report:

```bash
$ workstats classify src/main.rs docs/design.md .claude/settings.json
PATH                                                 CATEGORY   RULE               MATCHED
src/main.rs                                          source     extension          rs
docs/design.md                                       docs       directory          docs
.claude/settings.json                                ai         directory          .claude

Categories in match order: ai, test, docs, config, assets, source, other
```

It reads the same config the report does (`--config PATH` to point elsewhere)
and supports `--format json` and `--format csv`.

## Privacy boundary

`workstats` is local-only by default:

- no network calls and no telemetry, unless you explicitly run `workstats
  update` or opt into `--check-updates` (see [Updating](#updating));
- no credential discovery and no attempt to sign in to providers;
- no prompt or response bodies in reports or the cache;
- Git is read for commit metadata only — the commit id, the author date, and
  `--numstat`'s per-path line counts. The second pass
  [`--agent-commits`](#agent-authored-commits) runs is the same read with a
  different `--author`, over the history already on disk, and it makes no network
  call. `--co-authors` widens that read by exactly the *values* of
  `Co-authored-by:` trailers, asked for by name; no other part of a commit
  message is ever requested, and commit messages are never read to classify
  anything;
- no file contents in reports or the cache either — the explorer's diff viewer
  is the single place a tracked file is ever read, and what it reads is
  display-only ([details below](#the-diff-viewer-is-the-one-place-file-contents-are-read));
- a VS Code chat session is read for timestamps, model ids, and how long each
  turn took; the parser names no message field, so prompt and response bodies
  are never deserialized;
- Copilot's `~/.copilot/session-store.db` is read for `sessions(id, cwd,
  repository, branch, host_type)` and nothing else — that database also holds a
  `turns` table of full prompt and response bodies and a `search_index` FTS5
  index over them, and `workstats` queries neither;
- Codex, Copilot, and OpenCode SQLite databases are opened read-only;
- known credential locations such as `auth.json`, `secrets.json`, `.env`,
  `~/.config/github-copilot/`, and key stores are never discovery targets;
- malformed and oversized transcript records degrade safely;
- CSV cells are neutralized against spreadsheet formula injection.

The cache contains the structural fields needed for reports: timestamps,
working directories, session identifiers, model names, roles, derived
intervals, and token usage counts. JSON/CSV output can contain repository
names and paths—review a report before sharing it.

### The diff viewer is the one place file contents are read

Every number `workstats` reports is derived from paths, timestamps, and line
counts. The explorer's deepest level is the one exception, and it is
deliberately narrow: when you press `Enter` on a changed file in `workstats
ui`, it runs Git in that repository and shows you the patch.

That patch is **display-only**, and specifically:

- it is **never written to the cache**;
- it is **never written into a report** — not the dashboard, not `--format
  json`, not `--format csv`;
- it is **never stored in a saved view**; a saved view is a drill-down path and
  a sort, it cannot even name the diff level, so restoring one never reopens a
  file;
- it is **never sent anywhere** — reading a diff makes no network call, exactly
  like every other part of a normal run;
- it exists **only in memory while it is on screen**, and is dropped the moment
  you navigate away.

Nothing else changes. Reports, the cache, and the event format still contain no
prompt bodies, no response bodies, and no file contents; `workstats` without
`ui` never opens a tracked file at all. The viewer reads only what you are
already entitled to read — it shells out to your own `git`, in your own
checkout, and only for a commit id it has validated as a plain hexadecimal
object name, with the file path passed after `--` so it cannot be read as a
flag.

Two practical limits: a diff is truncated at roughly 2 MiB, 20,000 lines, or
2,000 characters per line and says so in the footer; and control and
direction-override characters in the patch are replaced before it is drawn, so
a file cannot repaint or reorder your terminal. Binary files come back as
Git's own `Binary files … differ` line — no bytes are ever emitted.

See [SECURITY.md](SECURITY.md) for private vulnerability reporting.

## Inputs and index

| Source | Default location |
|---|---|
| Codex sessions | `~/.codex/sessions` |
| Codex metadata | `~/.codex/state_5.sqlite` |
| Claude Code projects | `~/.claude/projects` |
| Gemini CLI sessions | `~/.gemini/tmp/*/chats` |
| GitHub Copilot CLI sessions | `~/.copilot/session-state/*/events.jsonl` |
| GitHub Copilot CLI metadata | `~/.copilot/session-store.db` |
| GitHub Copilot Chat sessions | `~/Library/Application Support/Code/User/workspaceStorage` (macOS) |
| OpenCode sessions | `~/.local/share/opencode/opencode.db` |
| Workstats Events | platform data directory (shown by `workstats sources`) |
| Git repositories | current directory, `--dir PATH`, or `WORKSTATS_DIR` |

VS Code keeps that chat directory under `%APPDATA%\Code\User\workspaceStorage`
on Windows and `~/.config/Code/User/workspaceStorage` on Linux. `Code -
Insiders` and `VSCodium` are read alongside `Code` when they exist; any other
install is reachable with `--history copilot-vscode=PATH`.

All inputs are optional (`--no-git`, `--no-ai`, `--provider`, and
`--exclude-provider`). Missing default histories are silently skipped; a
mistyped `--history` or `--events` path is reported as a `Warning:` line under
the report (the first five, with a count of the rest; `--format json` carries
them all) rather than producing a clean-looking report with data quietly
missing. The structural index defaults to:

| Platform | Cache | Config | Event log |
|---|---|---|---|
| macOS | `~/.cache/workstats/index.sqlite3` | `~/.config/workstats/config.json` | `~/Library/Application Support/workstats/events.jsonl` |
| Linux | `$XDG_CACHE_HOME/workstats/index.sqlite3` or `~/.cache/workstats/index.sqlite3` | `$XDG_CONFIG_HOME/workstats/config.json` or `~/.config/workstats/config.json` | `$XDG_DATA_HOME/workstats/events.jsonl` or `~/.local/share/workstats/events.jsonl` |
| Windows | `%LOCALAPPDATA%\workstats\cache\index.sqlite3` | `%APPDATA%\workstats\config.json` | `%LOCALAPPDATA%\workstats\events.jsonl` |

```bash
workstats --rebuild-cache       # rebuild every indexed entry
workstats --no-cache            # one uncached invocation
workstats --cache /safe/path.db # choose another index
workstats --config ./team.json  # read source roots and categories from elsewhere
```

The config file holds `source_roots`, `categories`, `category_mode`, and
`check_updates`. `workstats ui`'s saved views are kept beside it as
`views.json` — configuration, never cache — so `--rebuild-cache` and
`--no-cache` leave them alone.

`WORKSTATS_CACHE`, `WORKSTATS_CONFIG`, `WORKSTATS_EVENTS`, `WORKSTATS_VIEWS`,
`WORKSTATS_DIR`, and `WORKSTATS_GIT` provide explicit overrides. When
[update checks](#updating) are opted into, the last known result is cached next
to the index as `update-check.json` (`WORKSTATS_UPDATE_CACHE` overrides its
path); it contains only a version string and a timestamp.

Copilot and OpenCode are marked best-effort because their vendors may evolve the
internal event/database/document schema. The readers check tables and fields
before querying, stay read-only, and degrade to diagnostics instead of guessing:
a chat session in a format newer than the parser knows is reported and skipped
rather than guessed at.

A Copilot CLI session whose event log never recorded a working directory takes
one from `session-store.db`, so it is attributed to the repository it actually
ran in instead of to the transcript directory. A directory the event log did
record always wins, and a `repository` in the store that disagrees with the
directory is reported as a diagnostic naming both rather than silently
preferred.

Token usage (input, output, cache-read, cache-creation) is read from Claude
Code and Codex transcripts directly and from Copilot's end-of-session summary.
Gemini CLI and OpenCode token counts are best-effort and may read as zero if
the locally installed version doesn't expose per-turn usage fields; this never
affects the time-based metrics.

## Grouping and filtering

Dimensions can be composed: `root`, `repo`, `cwd`, `provider`, `model`, `day`,
and `month`. The default is `repo`.

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

`--month` and `--year` narrow the window a report covers, exactly as `--since`
and `--until` do. `--month 2026-07` and `--year 2026` name one outright; both
also accept `current` (or `this`) and `last` (or `previous`), resolved against
the local calendar. They cannot be combined with each other or with
`--since`/`--until`.

They are filters, not groupings. `--group-by` and `--period` decide how the rows
*inside* that window are split, so the two compose rather than compete.

```bash
workstats --month last                     # the previous calendar month
workstats --month 2026-07 --group-by repo  # July, one row per repository
workstats --year 2026 --period month       # 2026, with a month column per row
```

`--depth N` (default 4) bounds Git repository discovery below the scan root.
`--no-ignore` includes the generated and vendor paths — `node_modules/`,
`dist/`, `build/`, lockfiles, and the rest — that are otherwise counted
separately as ignored lines.

`--repo PATTERN` is a broad case-insensitive substring filter, matched against
the same three labels on both sides — the repository name, the working
directory, and the source root — so a pattern naming a source root selects
commits as well as AI sessions. `--repo-exact NAME` avoids mixing names such as
`api` and `api-tools`. When either filter matches retained AI sessions,
`workstats` also scans their locally available Git checkouts—even when those
checkouts are outside `--dir`. This keeps Git and AI results aligned without
assuming a particular projects folder; `--dir` remains the primary Git
discovery root.

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

### Agent-authored commits

Once a branch has been fetched, work a coding agent pushed is ordinary local Git
history — and `--author` does not see it, because the agent is the author. Two
opt-in flags read it, with no network access of any kind:

```bash
workstats --agent-commits                             # the built-in agent identities
workstats --agent-commits='<bot@example\.com>'        # just this one instead
workstats --co-authors                                # flag your own AI-assisted commits
```

`--agent-commits` runs a **second `git log` pass** over the same repositories,
asking for the agent's commits instead of yours. It is a second pass rather than
a wider `--author` on the first for one reason: these commits must never reach
the collection the human estimate is built from. They are landed output and zero
evidence that anyone was present, so they contribute **no human time, no work
blocks, no setup/review credit, and no active human days** — only their own
commit and line counts, plus the calendar day they landed on. A repository whose
history is nothing but agent commits reports `Estimated human work  0h 00m`, and
that is the correct answer.

The built-in identities are matched on the **tail of the e-mail address**, never
on the number in front of it:

| Identity | Matched by |
|---|---|
| GitHub Copilot coding agent | `+Copilot@users.noreply.github.com>` |
| Copilot on github.com | `<copilot@github.com>` |
| Claude | `+claude[bot]@users.noreply.github.com>` and `<noreply@anthropic.com>` |

GitHub has issued more than one numeric id for the same Copilot account —
`198982749+Copilot@…` and `223556219+Copilot@…` both occur in real history — so
anything keyed on the number finds part of an agent's work and silently misses
the rest. Matching the address also covers every display name that account
commits under; `Copilot` and `copilot-swe-agent[bot]` share one address.
Automation that is not an AI agent is deliberately absent: `github-actions` and
`dependabot` push far more commits than Copilot does, and counting a version bump
as agent output would say something false about both.

`--agent-commits=REGEX` **replaces** the built-in identities rather than adding
to them — one pattern can only honestly mean "just this one". The `=` is
required, so that a bare `--agent-commits` can mean "the built-in ones" without
swallowing whatever follows it. The value is handed to `git log --author` raw,
the same contract `--author` has, so it is a *basic* regular expression: `+`,
`?`, `(`, `)` and `|` are literals there, and a backslash is what promotes them
to operators.

`--co-authors` is the opposite case and a separate decision. It reads the
`Co-authored-by:` trailers on **your own** commits so a commit you wrote with an
agent can be described as such. It never adds a commit: `Co-authored by AI  9 of
the 31 commits above` is a share of what was already counted, and the human
estimate is byte-identical with and without the flag. Trailers naming Copilot
Autofix are counted separately from assisted development, because code scanning
and writing code with an assistant are different activities. Only the trailer
*values* are requested from Git, so no other part of a commit message is ever
read.

Agent output stays out of the figures `--author` promises are yours. It has its
own summary line, its own report section, its own `agent_commit_count` /
`agent_additions` / `agent_deletions` / `ai_assisted_commit_count` /
`autofix_assisted_commit_count` fields in JSON and columns in CSV, and its own
`git-agent` row under `--group-by provider`. It is not folded into `commit_count`,
`additions`, `deletions`, [work composition](#what-was-worked-on), or change
shapes — those describe the work you authored.

### Copilot activity that never reaches your clone

Two things Copilot does on github.com leave no trace in a clone: pull requests
the coding agent opened, and code reviews it left. The default refspec fetches
`refs/heads/*` only, so `refs/pull/*` is never on disk, and a review suggestion
the author declined leaves no artifact at all.

[`contrib/copilot-github-sync.sh`](contrib/copilot-github-sync.sh) covers them:

```bash
contrib/copilot-github-sync.sh --since 2026-01-01 ~/src/api ~/src/web
contrib/copilot-github-sync.sh --dry-run ~/src/api          # print, record nothing
contrib/copilot-github-sync.sh --jsonl backfill.jsonl ~/src/api   # fast first backfill
```

Each argument is a local clone; the slug comes from its `origin` remote and the
clone's own path becomes the event's `cwd`, so the events land on the same report
row as the rest of that repository's work. Every event is written with
`--role subagent`, which is the same lever the built-in adapters use: it
contributes to AI wall clock and session counts and exactly zero to the human
estimate.

**It is a script, on purpose, and not a flag.** Reading either of those means
calling the GitHub API, and an HTTP client inside the binary would end two of the
guarantees in [Privacy boundary](#privacy-boundary) at once: `workstats` makes no
network calls, and it performs no credential discovery — it never reads a
keyring, a token file, or `GH_TOKEN`. The second is the expensive one. From then
on the tool would have to be audited for how it holds a token, how it keeps one
out of `--format json`, and what a poisoned cache could do with one. So the
network call is made outside, by your own authenticated `gh`, with your own
credentials, only when you run it. What crosses back in is the content-free
record [`workstats record`](#add-any-tool-or-api) already accepts — a provider, an
identifier, a directory, a model name, and timestamps. No titles, no bodies, no
review text; the events-v1 schema rejects records carrying any of those.

It needs `gh` on `PATH` and authenticated (`gh auth status`), and says which of
the two is missing rather than failing mid-run. A clone it cannot read — no
GitHub remote, or a repository this account cannot search — is named on stderr
and skipped; the remaining clones are still done and the run exits non-zero, so
a scheduled partial sync does not look like a clean one. A squash-merged agent
pull request is visible both ways — as an agent-authored commit and as an event
recorded here — so use one or the other per repository.

## Platforms

CI builds and tests every change on **macOS, Linux, and Windows**. Releases ship:

- macOS: Apple silicon and Intel;
- Linux: x86_64 and ARM64;
- Windows: x86_64 and x86.

Rust 1.88 or newer is supported. Git is discovered from standard locations and
`PATH`; set `WORKSTATS_GIT` to an absolute executable path when needed.

## Development and releases

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

The repository, installers, tests, and release artifacts are Rust-only.

Versions are not set by hand. A merged pull request labelled `major`, `minor`,
or `patch` decides the next one; a merge carrying none of those labels
releases nothing. [`cratis/release-action`](https://github.com/Cratis/release-action)
works out the version and cuts the GitHub release, then the release workflow
builds six native artifacts from it, executes every runnable binary, generates
SHA-256 checksums, and attaches them—no repository secrets required for the
binaries. The `HOMEBREW_TAP_DEPLOY_KEY` secret then publishes the formula to
[`woksin/homebrew-workstats`](https://github.com/woksin/homebrew-workstats) —
the tap the Homebrew instructions above install from — and the job reads the
formula back afterwards to confirm it names the version just released. Without
that secret the step is skipped with a warning annotation, or fails the release
outright if this README advertises a tap that is not configured.

## License

[MIT](LICENSE) © 2026 woksin
