<div align="center">

<img src="assets/banner.svg" alt="workstats — human work, Git output, and agent activity" width="900">

<br>

[![CI](https://github.com/woksin/workstats/actions/workflows/ci.yml/badge.svg)](https://github.com/woksin/workstats/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/woksin/workstats?color=86efac)](https://github.com/woksin/workstats/releases/latest)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-67e8f9)](docs/install.md#platforms)
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
`--co-authors`](docs/how-it-works.md#agent-authored-commits). Agent-authored commits are shown
because they are real output, and shown *apart* because none of them is evidence
that anyone was at the keyboard: they add no human time, no work blocks, and no
active human days.

That is the printed report. [`workstats ui`](docs/usage.md#explore-it-interactively) opens
the same numbers as a drill-down explorer — repository, month, file area,
commit, changed file, diff — with search, filtering, and saved views.

## The useful distinction

Agent runtime is not automatically human work. Lines changed are not time. A
session left open overnight is not automatically an eight-hour day. Prompts and
commits provide direct evidence; foreground session boundaries add bounded
setup and review evidence without treating autonomous output as attendance.

`workstats` keeps those ideas separate:

| Signal | What it answers | How it is treated |
| --- | --- | --- |
| **Human-work estimate** | “How much time was plausibly spent developing or supervising?” | Prompts, foreground session boundaries, and authored commits form non-overlapping involvement blocks with setup/review credit. |
| **Git output** | “What changed?” | Commits, files, additions, deletions, and ignored generated/vendor lines — all of them authored by the identity `--author` names. |
| **Agent-authored Git output** | “What landed that I did not type?” | Commits a coding agent authored, matched by Git identity in a second pass and reported in their own columns and their own section. **Never human time**: no work blocks, no setup/review credit, no active *human* days, and never added into the Git-output figures above. A `Co-authored-by:` trailer is the other case — it describes a commit you already wrote, so it is a share of your commits and never an addition to them. |
| **Agent wall clock** | “How long was any agent active?” | Overlapping agent intervals count once. |
| **Parallel agent work** | “How much automation ran?” | Concurrent sessions are summed, so this can exceed wall time. |
| **AI tokens** | “How many tokens did agents use?” | Input, output, cache-read, and cache-creation counts read from local transcripts; not an intervaled/deduplicated metric like wall clock, so grouped totals just sum. |
| **Work composition** | “Where did the output land?” | Changed lines bucketed into file areas — `source`, `test`, `docs`, `config`, `assets`, `other` by default, and [whatever else you configure](docs/configuration.md#make-the-areas-your-own) — from the file path alone. |
| **Change shapes** | “What did the work look like?” | Each commit described by its dominant file area and its addition/deletion balance. Never read from the commit message. |

That makes the dashboard useful without pretending it is a stopwatch, an
attendance system, or a universal productivity score.

## Bring your whole AI stack

```text
  ● claude          Claude Code                    built-in
  ● codex           OpenAI Codex                   built-in
  ● gemini          Google Gemini CLI              built-in
  ● pi              Pi Coding Agent                built-in
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

No Rust toolchain is required for the prebuilt binaries, and every release
includes a `SHA256SUMS` file. The full instructions, including Windows `PATH`
setup, the macOS quarantine note and building from source, are in
[Install and update](docs/install.md).

```bash
# Homebrew (macOS, Linux)
brew install woksin/workstats/workstats

# macOS, Apple silicon (Intel: replace arm64 with x86_64)
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-macos-arm64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# Linux x86_64 (ARM64: replace x86_64 with arm64)
curl -fsSL https://github.com/woksin/workstats/releases/latest/download/workstats-linux-x86_64.tar.gz | tar xz
install -m 0755 workstats ~/.local/bin/workstats

# From source
cargo install --git https://github.com/woksin/workstats --locked
```

```powershell
# Windows
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\workstats\bin" | Out-Null
Invoke-WebRequest https://github.com/woksin/workstats/releases/latest/download/workstats-windows-x86_64.exe `
  -OutFile "$env:LOCALAPPDATA\workstats\bin\workstats.exe"
```

Homebrew installs by the fully-qualified name, which taps `woksin/workstats` and
trusts that one formula; the reasoning is in
[Prebuilt binaries](docs/install.md#prebuilt-binaries). Supported platforms are
macOS, Linux and Windows; see [Platforms](docs/install.md#platforms).

> [!NOTE]
> The macOS binaries are currently unsigned. Downloads made by a browser may
> need `xattr -d com.apple.quarantine workstats`.

Later, `workstats update` upgrades in place; see
[Updating](docs/install.md#updating).

## Start here

```bash
workstats sources                          # supported + detected AI histories
workstats                                  # dashboard for the current directory
workstats ui                               # explore the same report interactively
workstats --dir ~/projects                 # discover repositories below a directory
workstats --group-by month,repo            # recent work by month and repo
workstats --period day --group-by root     # daily trend by source root
workstats --period week                    # ISO-week trend (2026-W09)
workstats --since 2026-07 --until 2026-08  # inclusive local calendar bounds
workstats --month 2026-07                  # filter to one calendar month
workstats --week last                      # filter to the previous ISO week
workstats --year last                      # filter to the previous calendar year
workstats --provider codex,gemini --group-by model
workstats --exclude-provider copilot
workstats --repo-exact my-project          # infer its checkout from matching AI sessions
workstats --agent-commits                  # also read commits a coding agent authored
workstats --raw                            # provider/model detail (alias --show-agent-work)
workstats --explain-human-time             # auditable signal and work-block ledger
workstats classify src/main.rs             # which file area a path lands in, and why
```

And the commands built on the same numbers:

```bash
workstats timesheet --month last           # suggested hours per day and client, rounded
workstats branch                           # the effort behind the current branch
workstats pr --number 123 --format markdown   # a block to paste into a pull request
workstats insights                         # focus, patterns and leverage, last 28 days
workstats digest                           # this week against the week before
workstats now                              # a short line for a prompt or status bar
workstats calendar                         # a year grid of human time per day
workstats export --output laptop.json      # this machine's evidence, for another to merge
workstats merge laptop.json desktop.json   # one report from several machines
workstats --group-by branch,issue          # what the work was for
```

These are the everyday commands. [Usage](docs/usage.md) covers what decides the
scope of a run (Git scope versus AI scope, branches, authors, nested
repositories) and every flag in detail.

## A short tour

- **Group and filter.** Compose `root`, `repo`, `cwd`, `provider`, `model`,
  `day`, `week` and `month`, and narrow by date, provider or repository. See
  [Grouping and filtering](docs/usage.md#grouping-and-filtering).
- **Compare windows.** `--compare previous` (or a month, ISO week or year) adds
  headline deltas against another window. See
  [Comparing two windows](docs/usage.md#comparing-two-windows).
- **Explore interactively.** `workstats ui` opens the report as a drill-down
  explorer from repository down to the diff, with search and saved views. See
  [Explore it interactively](docs/usage.md#explore-it-interactively).
- **Feed it to other tools.** `--format json|csv|markdown|html` for pipes, PRs
  and self-contained pages. See
  [Reports made for pipes](docs/usage.md#reports-made-for-pipes).
- **Add any tool or API.** `workstats record` and `--events` take a small open
  JSONL format. See [Add any tool or API](docs/usage.md#add-any-tool-or-api).
- **Timesheets.** `workstats timesheet` turns the human time into one rounded
  entry per day per client, with manual entries, overrides, period locks and
  CSV exports for Toggl, Harvest and Clockify. See [Timesheet](docs/timesheet.md).
- **Branches and pull requests.** Group by `branch`, `issue` or `feature`, or
  ask for the effort behind one branch or pull request with `workstats branch`
  and `workstats pr`. See [Branches and pull requests](docs/branches.md).
- **Insights and a weekly digest.** Focus, when you work, and what the agents
  did with your time, plus a week-over-week summary with your goals. See
  [Insights and digest](docs/insights.md).
- **In your prompt.** `workstats now` prints today and the week so far from a
  cached snapshot, cheaply enough for every redraw. See
  [Usage](docs/usage.md#the-other-commands) and
  [`now` in the configuration](docs/configuration.md#now).
- **A year at a glance.** `workstats calendar` and the HTML and Markdown
  reports draw a heatmap of human time per day. See
  [Calendar heatmap](docs/calendar.md).
- **Several machines.** `workstats export` writes a content-free bundle and
  `workstats merge` (or `--import`) folds bundles into one report. See
  [Merging machines](docs/merge.md).
- **Agent-authored commits.** Shown apart from your own and never counted as
  human time. See [Agent-authored commits](docs/how-it-works.md#agent-authored-commits).
- **Split the bill.** `workstats allocate` shares subscription cost across
  projects. See [Splitting the bill](docs/allocate.md).
- **Stay private.** Nothing is uploaded, prompt and response text is never
  read, and the diff viewer is the one place file contents are shown. See
  [Privacy boundary](docs/privacy.md).

## Documentation

| Guide | What is in it |
| --- | --- |
| [Install and update](docs/install.md) | Prebuilt binaries, building from source, `workstats update`, supported platforms |
| [Usage](docs/usage.md) | Scope and authors, grouping and filtering, windows and `--compare`, output formats, the interactive explorer, `record` and `--events`, and a guide to the newer commands |
| [Configuration](docs/configuration.md) | The config file and its keys, `defaults`, inputs, the index, file-area categories, and the blocks for engagements, issues, branches, timesheet, goals, insights and `now` |
| [Timesheet](docs/timesheet.md) | Engagements, rounding, the ledger of manual entries and locks, vendor CSV exports, descriptions |
| [Branches and pull requests](docs/branches.md) | How work is tied to a branch, issue and feature, and `workstats branch` and `pr` |
| [Insights and digest](docs/insights.md) | Focus, patterns, leverage, models, and the weekly digest |
| [Calendar heatmap](docs/calendar.md) | The year grid in the terminal, HTML, Markdown and the explorer |
| [Merging machines](docs/merge.md) | `workstats export`, `merge` and `--import` |
| [How it works](docs/how-it-works.md) | The human-time estimate, work composition, why it is fast, agent-authored commits, Copilot activity |
| [Splitting the bill](docs/allocate.md) | `workstats allocate`: rates, currency, and missing history |
| [Privacy boundary](docs/privacy.md) | What is read, what never is, and the diff viewer |
| [Development and releases](docs/development.md) | Checks to run and how releases are cut |

See the [changelog](CHANGELOG.md) for what changed in each release and
[SECURITY.md](SECURITY.md) for private vulnerability reporting.

## License

[MIT](LICENSE) © 2026 woksin
