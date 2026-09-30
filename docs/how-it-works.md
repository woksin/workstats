# How it works

How the human-work estimate is built, how output is classified, why runs are fast, and how agent-authored commits and Copilot activity are handled.

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
5. At a shared timestamp, one signal is retained: prompt over commit over
   foreground-session edge; equal-priority ties keep the first input signal.
6. All human intervals form one global, non-overlapping timeline. Ten concurrent
   agents cannot make ten simultaneous human hours.

The stable calculation identifier is `signal-blocks-v1`. A gap must be
**strictly greater than** the idle threshold to start a new block. Report windows
are half-open (`since <= signal < until`); ledger timestamps are UTC, while the
outer review-credit edges clamp to host-local calendar midnights. JSON reports
always state this algorithm and boundary basis.

To audit one run rather than only read its assumptions:

```bash
workstats --explain-human-time
workstats --format json --explain-human-time
```

The opt-in ledger shows scoped input signals, effective signals after timestamp
deduplication, every block boundary and duration, actual versus requested
credit, clipping, and a reconciling total. Block durations retain microsecond
precision; the ledger exposes the explicit adjustment used to round the final
summary total to milliseconds. It contains structural metadata only, never
prompt text.

Tune the assumptions when your workflow needs it:

```bash
workstats --human-idle 90m --review-credit 45m  # more generous
workstats --human-idle 30m --review-credit 10m  # more conservative
```

Review credit may not exceed the idle threshold; enforcing that invariant keeps
separate blocks from overlapping and being counted twice.

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
| --- | --- |
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
| --- | --- |
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
| --- | ---: | ---: |
| Index disabled | 2.7 s | 172 MiB |
| Warm index | **1.05 s** | **54 MiB** |

Different histories and disks will vary; the architectural win is that normal
runs parse only what changed.

## Agent-authored commits

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
| --- | --- |
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

## Copilot activity that never reaches your clone

Two things Copilot does on github.com leave no trace in a clone: pull requests
the coding agent opened, and code reviews it left. The default refspec fetches
`refs/heads/*` only, so `refs/pull/*` is never on disk, and a review suggestion
the author declined leaves no artifact at all.

[`contrib/copilot-github-sync.sh`](../contrib/copilot-github-sync.sh) covers them:

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
guarantees in [Privacy boundary](privacy.md#privacy-boundary) at once: `workstats` makes no
network calls, and it performs no credential discovery — it never reads a
keyring, a token file, or `GH_TOKEN`. The second is the expensive one. From then
on the tool would have to be audited for how it holds a token, how it keeps one
out of `--format json`, and what a poisoned cache could do with one. So the
network call is made outside, by your own authenticated `gh`, with your own
credentials, only when you run it. What crosses back in is the content-free
record [`workstats record`](usage.md#add-any-tool-or-api) already accepts — a provider, an
identifier, a directory, a model name, and timestamps. No titles, no bodies, no
review text; the events-v1 schema rejects records carrying any of those.

It needs `gh` on `PATH` and authenticated (`gh auth status`), and says which of
the two is missing rather than failing mid-run. A clone it cannot read — no
GitHub remote, or a repository this account cannot search — is named on stderr
and skipped; the remaining clones are still done and the run exits non-zero, so
a scheduled partial sync does not look like a clean one. A squash-merged agent
pull request is visible both ways — as an agent-authored commit and as an event
recorded here — so use one or the other per repository.
