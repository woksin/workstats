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
══════════════════════════════════════════════════════════════════════════════
  Estimated human work    10h 24m
  Active work days         2
  Work blocks              6  (24 foreground session edges + 184 prompts + 31 commits)
  Git lines                +8,421 / -2,107

AI activity  (context only — these are not human hours)
  Agent wall clock         11h 18m  (overlap removed)
  Parallel agent work      37h 06m  (3.3× concurrency)
  Sessions                 74  (12 foreground, 62 subagents)

By repo
  Work area                         Human  Days   Avg/day  Commits   AI wall
  ───────────────────────────────────────────────────────────────────────────
  api                               5h 50m     2    2h 55m       18    6h 20m
  web                               3h 39m     2    1h 49m       11    4h 14m
  cli                               0h 55m     1    0h 55m        2    0h 44m
```

## The useful distinction

Agent runtime is not automatically human work. Lines changed are not time. A
session left open overnight is not automatically an eight-hour day. Prompts and
commits provide direct evidence; foreground session boundaries add bounded
setup and review evidence without treating autonomous output as attendance.

`workstats` keeps those ideas separate:

| Signal | What it answers | How it is treated |
|---|---|---|
| **Human-work estimate** | “How much time was plausibly spent developing or supervising?” | Prompts, foreground session boundaries, and authored commits form non-overlapping involvement blocks with setup/review credit. |
| **Git output** | “What changed?” | Commits, files, additions, deletions, and ignored generated/vendor lines. |
| **Agent wall clock** | “How long was any agent active?” | Overlapping agent intervals count once. |
| **Parallel agent work** | “How much automation ran?” | Concurrent sessions are summed, so this can exceed wall time. |

That makes the dashboard useful without pretending it is a stopwatch, an
attendance system, or a universal productivity score.

## Bring your whole AI stack

```text
  ● claude       Claude Code            built-in
  ● codex        OpenAI Codex           built-in
  ● gemini       Google Gemini CLI      built-in
  ● copilot      GitHub Copilot CLI     best-effort
  ● opencode     OpenCode               best-effort
  ● events       any CLI / IDE / API    stable open JSONL
```

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
<tr><td width="145"><b>macOS</b></td><td>

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
locked release build, preserve existing commands unless `--force` / `-Force` is
explicit, and install the backward-compatible `gitstats` alias too.

```bash
# macOS / Linux
./install.sh

# Windows PowerShell
./install.ps1
```

## Start here

```bash
workstats sources                          # supported + detected AI histories
workstats                                  # dashboard for the current repository
workstats --dir ~/projects                 # discover repositories below a directory
workstats --group-by month,repo            # recent work by month and repo
workstats --period day --group-by root     # daily trend by source root
workstats --since 2026-07 --until 2026-08  # inclusive local calendar bounds
workstats --provider codex,gemini --group-by model
workstats --exclude-provider copilot
workstats --repo-exact my-project             # infer its checkout from matching AI sessions
workstats --show-agent-work                # provider/model detail
```

Your Git author defaults to `git config --global user.email`, falling back to
`user.name`. Override it with `--author REGEX` or `WORKSTATS_AUTHOR`.

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

The default event log is loaded automatically. Use `--output FILE` to choose a
log or `--output -` to emit JSONL to stdout. Existing logs can be added directly:

```bash
workstats --events ./activity.jsonl
workstats --events ./team-export/ --provider openai-api,internal-agent
```

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
rejected instead of indexed.

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
or `NO_COLOR` when you want explicit control.

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

1. Foreground human prompts and authored Git commits provide direct evidence of
   involvement without reading prompt or response text.
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

`--gap-cap` controls AI wall-clock estimation only. The human estimate can still
miss meetings, thinking away from a recorded session, deleted history, and work
performed on another machine. Treat it as a realistic local heuristic, never
payroll data.

## Privacy boundary

`workstats` is local-only:

- no network API calls;
- no telemetry or update checks;
- no prompt or response bodies in reports or the cache;
- Codex and OpenCode SQLite databases are opened read-only;
- known credential files such as `auth.json`, `secrets.json`, `.env`, and key
  stores are never discovery targets;
- malformed and oversized transcript records degrade safely;
- CSV cells are neutralized against spreadsheet formula injection.

The cache contains the structural fields needed for reports: timestamps,
working directories, session identifiers, model names, roles, and derived
intervals. JSON/CSV output can contain repository names and paths—review a
report before sharing it.

See [SECURITY.md](SECURITY.md) for private vulnerability reporting.

## Inputs and index

| Source | Default location |
|---|---|
| Codex sessions | `~/.codex/sessions` |
| Codex metadata | `~/.codex/state_5.sqlite` |
| Claude Code projects | `~/.claude/projects` |
| Gemini CLI sessions | `~/.gemini/tmp/*/chats` |
| GitHub Copilot CLI sessions | `~/.copilot/session-state/*/events.jsonl` |
| OpenCode sessions | `~/.local/share/opencode/opencode.db` |
| Workstats Events | platform data directory (shown by `workstats sources`) |
| Git repositories | current directory or `--dir PATH` |

All inputs are optional (`--no-git`, `--no-ai`, `--provider`, and
`--exclude-provider`). Missing default histories are silently skipped; explicit
missing `--history` and `--events` paths appear in diagnostics. The structural
index defaults to:

| Platform | Cache | Config | Event log |
|---|---|---|---|
| macOS | `~/.cache/workstats/index.sqlite3` | `~/.config/workstats/config.json` | `~/Library/Application Support/workstats/events.jsonl` |
| Linux | `$XDG_CACHE_HOME/workstats/index.sqlite3` or `~/.cache/workstats/index.sqlite3` | `$XDG_CONFIG_HOME/workstats/config.json` or `~/.config/workstats/config.json` | `$XDG_DATA_HOME/workstats/events.jsonl` or `~/.local/share/workstats/events.jsonl` |
| Windows | `%LOCALAPPDATA%\workstats\cache\index.sqlite3` | `%APPDATA%\workstats\config.json` | `%LOCALAPPDATA%\workstats\events.jsonl` |

```bash
workstats --rebuild-cache       # rebuild every indexed entry
workstats --no-cache            # one uncached invocation
workstats --cache /safe/path.db # choose another index
```

`WORKSTATS_CACHE`, `WORKSTATS_CONFIG`, `WORKSTATS_EVENTS`, and `WORKSTATS_GIT`
provide explicit overrides.

Copilot CLI and OpenCode are marked best-effort because their vendors may evolve
the internal event/database schema. The readers check tables and fields before
querying, stay read-only, and degrade to diagnostics instead of guessing.

## Grouping and filtering

Dimensions can be composed: `root`, `repo`, `cwd`, `provider`, `model`, `day`,
and `month`.

```bash
workstats --group-by provider,model
workstats --group-by month,repo --top 0
workstats --repo service --path 'src/**' --path-exclude '**/*.generated.*'
```

`--repo PATTERN` is a broad case-insensitive substring filter.
`--repo-exact NAME` avoids mixing names such as `api` and `api-tools`.
When either filter matches retained AI sessions, `workstats` also scans their
locally available Git checkouts—even when those checkouts are outside `--dir`.
This keeps Git and AI results aligned without assuming a particular projects
folder; `--dir` remains the primary Git discovery root.

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

A `v<version>` tag matching `Cargo.toml` triggers the release workflow. It builds
six native artifacts, executes every runnable binary, generates SHA-256
checksums, and publishes a GitHub release without repository secrets.

## License

[MIT](LICENSE) © 2026 woksin
