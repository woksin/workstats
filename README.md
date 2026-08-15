<div align="center">

<img src="assets/banner.svg" alt="workstats — human work, Git output, and agent activity" width="900">

<br>

[![CI](https://github.com/woksin/workstats/actions/workflows/ci.yml/badge.svg)](https://github.com/woksin/workstats/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/woksin/workstats?color=86efac)](https://github.com/woksin/workstats/releases/latest)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-67e8f9)](#platforms)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-b7410e?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-86efac)](LICENSE)

**See where the work happened—without sending your work anywhere.**

`workstats` turns local Git history and retained Codex/Claude Code metadata into
an honest view of focused human work, code output, and parallel agent activity.

</div>

---

```text
  ⠼ Loading Codex activity  0.8s

WORKSTATS  human work across local projects
══════════════════════════════════════════════════════════════════════════════
  Estimated hands-on work  6h 42m
  Active work days         2
  Work blocks              9  (184 prompts + 31 commits observed)
  Git lines                +8,421 / -2,107

AI activity  (context only — these are not human hours)
  Agent wall clock         11h 18m  (overlap removed)
  Parallel agent work      37h 06m  (3.3× concurrency)
  Sessions                 74  (12 foreground, 62 subagents)

By repo
  Work area                         Human  Days   Avg/day  Commits   AI wall
  ───────────────────────────────────────────────────────────────────────────
  studio/api                        3h 51m     2    1h 55m       18    6h 20m
  studio/web                        2h 17m     2    1h 08m       11    4h 14m
  lab/cli                           0h 34m     1    0h 34m        2    0h 44m
```

## The useful distinction

Agent runtime is not human work. Lines changed are not time. A session left open
overnight is not an eight-hour day.

`workstats` keeps those ideas separate:

| Signal | What it answers | How it is treated |
|---|---|---|
| **Hands-on estimate** | “How much focused work is evidenced here?” | Foreground prompts and authored commits form non-overlapping work blocks. |
| **Git output** | “What changed?” | Commits, files, additions, deletions, and ignored generated/vendor lines. |
| **Agent wall clock** | “How long was any agent active?” | Overlapping agent intervals count once. |
| **Parallel agent work** | “How much automation ran?” | Concurrent sessions are summed, so this can exceed wall time. |

That makes the dashboard useful without pretending it is a stopwatch, an
attendance system, or a universal productivity score.

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
workstats                                  # the dashboard
workstats --group-by repo                  # one row per repository
workstats --group-by month,repo            # recent work by month and repo
workstats --period day --group-by root     # daily trend by source root
workstats --since 2026-07 --until 2026-08  # inclusive local calendar bounds
workstats --provider codex --group-by model
workstats --repo-exact my-project
workstats --show-agent-work                # provider/model detail
```

Your Git author defaults to `git config --global user.email`, falling back to
`user.name`. Override it with `--author REGEX` or `WORKSTATS_AUTHOR`.

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

| Implementation | Time | Peak memory |
|---|---:|---:|
| Original Python implementation | 24.7 s | 270 MiB |
| Rust, index disabled | 2.7 s | 172 MiB |
| Rust, warm index | **1.05 s** | **54 MiB** |

Different histories and disks will vary; the architectural win is that normal
runs parse only what changed.

## How the estimate works

1. Foreground human prompts from Codex and Claude Code become activity signals.
   Tool results, compact summaries, sidechains, and subagent prompts do not.
2. Authored Git commits add evidence for work performed away from an AI chat.
3. Signals no more than 15 minutes apart form a work block. The time between
   them is attributed to the nearest signal's work area.
4. An isolated signal receives five minutes for preparation and review, clamped
   to its local calendar day.
5. All human intervals form one global union. Ten concurrent agents cannot make
   a ten-hour human hour.

Tune the assumptions when your workflow needs it:

```bash
workstats --human-idle 20m --isolated-credit 8m --gap-cap 10m
```

The estimate intentionally misses meetings, reading, thinking away from a
session, uncommitted manual work, deleted history, and work performed on another
machine. Treat it as local evidence, never payroll data.

## Privacy boundary

`workstats` is local-only:

- no network API calls;
- no telemetry or update checks;
- no prompt or response bodies in reports or the cache;
- Codex's optional SQLite metadata database is opened read-only;
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
| Git repositories | `~/src/repos` or `--dir PATH` |

All inputs are optional (`--no-git`, `--no-ai`, `--no-codex`,
`--no-claude`). The transcript index defaults to:

| Platform | Cache | Config |
|---|---|---|
| macOS / Linux | `$XDG_CACHE_HOME/workstats/index.sqlite3` or `~/.cache/workstats/index.sqlite3` | `$XDG_CONFIG_HOME/workstats/config.json` or `~/.config/workstats/config.json` |
| Windows | `%LOCALAPPDATA%\workstats\cache\index.sqlite3` | `%APPDATA%\workstats\config.json` |

```bash
workstats --rebuild-cache       # rebuild every indexed entry
workstats --no-cache            # one uncached invocation
workstats --cache /safe/path.db # choose another index
```

`WORKSTATS_CACHE`, `WORKSTATS_CONFIG`, and `WORKSTATS_GIT` provide explicit
overrides.

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

The original Python implementation remains under `src/workstats` as a
behavioral oracle. It is not needed to build or run the Rust CLI.

A `v<version>` tag matching `Cargo.toml` triggers the release workflow. It builds
six native artifacts, executes every runnable binary, generates SHA-256
checksums, and publishes a GitHub release without repository secrets.

## License

[MIT](LICENSE) © 2026 woksin
