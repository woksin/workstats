# Insights and digest

How your time is shaped, and what the agents did with it, with
`workstats insights` and `workstats digest`.

Both commands are computed from the data a normal report is built from. They
read nothing new: no extra files, no extra Git calls. The figures are the
report's figures, cut differently, so they agree with it over the same window.

```console
$ workstats insights --section focus,leverage

  workstats insights — the last 28 days (since 2026-08-05)

Focus

  Work blocks                  41 (9 under 30 minutes)
  Longest block                4h 35m (2026-08-19)
  Longest single-repo stretch  3h 10m (workstats)
  Average block                1h 52m
  Context switches             37 (0.9 per block, 0.5 per human hour)
  ...
Leverage

  Human work                          76h 20m
  Agent wall clock                    52h 05m
  Agent wall per human hour           0.7×
  Tokens per commit                   1.2M
  List value (priced models)          $912.40
  List value per commit               $8.90
  Sessions without commits            14 of 62 (23%)
```

## `workstats insights`

```
workstats insights [REPORT FLAGS] [--section focus,leverage,heatmap,models]
```

It takes the same flags as a report: window, `--repo`, `--provider`,
`--no-git`, `--format` and the rest. **With no window flag the window is the
last 28 days**, today included, and the output says so. `--compare` is not
available here; use `digest`. Grouping flags (`--group-by`, `--by-repo`) have no
effect, because the sections are not grouped rows.

`--section` picks what is shown (default: all four). `--format` is `table`,
`json`, `markdown` or `html`; CSV is refused, because the output is not one flat
table. `--top` limits the per-day and per-model tables.

### Focus (`--section focus`)

Focus is about how your human time is broken up. It is computed from the same
work blocks the report counts: runs of prompts, session edges and commits with
no silence longer than `--human-idle`.

| Figure | Meaning |
|---|---|
| Work blocks | How many blocks, and how many are shorter than 30 minutes |
| Longest block | Its length and the day it started |
| Longest single-repo stretch | The longest run of consecutive time in one repository inside a block |
| Average block | Human time divided by blocks |
| Context switches | Changes of repository between consecutive pieces of one block, per block and per human hour. When `engagements` are configured, a change of engagement inside one repository counts too |

Two choices to know about:

- **A block belongs to the day it starts on**, whole. A block that runs past
  midnight is one block on one day, not two fragments, so the per-day rows add
  up to the totals.
- **Switches never cross a block boundary.** Coming back from lunch to a
  different repository is a new block, not a context switch.

The per-day table has one row per day with work (the latest `--top`, default 30).

### Heatmap (`--section heatmap`)

A 7×24 matrix of local weekday by hour of day, for human time and for agent wall
clock (overlapping agents counted once, as in the report). Each interval is split
at the boundaries of your local clock, so a daylight-saving change cannot put
time in the wrong hour: the skipped hour gets nothing and the repeated hour gets
both passes. JSON carries the full matrix in seconds (`patterns.human_seconds`,
`patterns.agent_wall_seconds`; rows Monday first).

It also reports **late-night** and **weekend** time, with the number of days
that had any:

- A night is named by the date its evening began, so work from 23:30 to 00:30 is
  one night, not two.
- Weekend days are days with human time on a weekend weekday.

Both are configurable in the config file:

```json
{
  "insights": {
    "night": ["22:00", "06:00"],
    "weekend": ["sat", "sun"]
  }
}
```

`night` is a start and an end as `HH:MM` and may run past midnight (the
default) or not (`["01:00", "03:00"]`). `weekend` is up to seven weekday names
(`mon` to `sun`). Anything else is refused with a message naming the key.

### Leverage (`--section leverage`)

What the agents produced for the human time. Every ratio is `n/a` (JSON `null`)
when the figure it divides by is zero, never `0`.

- **Agent wall clock per human hour** and **parallel agent work per human
  hour**: how many agent hours ran for each hour of you. Parallel work counts
  overlapping agents separately; wall clock counts them once.
- **Tokens** and **list value** per commit, per 100 changed lines (additions plus
  deletions) and per human hour. Commits are your own commits in the window;
  agent-authored commits are not counted. Without Git history (`--no-git`) the
  per-commit and per-line figures are `n/a`.
- **Sessions without commits**: the share of foreground sessions that had no
  commit in the same repository within the idle window, among sessions in
  repositories that produced commits. This is the report's own
  `foreground_sessions_without_commits`. It covers reading, review and
  uncommitted work, so it is a prompt for a question, not a score.

**List value is not a bill.** It is the tokens priced at the published list rates
the tool carries (or your `model_rates` overrides), as `allocate` does. Models
with no rate are named in a warning and left out of the value; their tokens are
still counted. Wherever a list value appears, a warning says when the built-in
rate table is older than 90 days.

### Providers and models (`--section models`)

One table per provider and one per `provider / model`: foreground sessions,
subagent sessions, sessions that had a commit, agent wall clock, tokens, list
value, and human time.

- A session is counted under every model it used, so model rows can add up to
  more than their provider's.
- Human time is attributed through the provider and model of the nearest prompt,
  session edge or commit, as the report's `--group-by provider,model` does.
  `git` is the commits themselves.
- The per-model list value is the same number `allocate` computes for the same
  tokens.

## `workstats digest`

```
workstats digest [REPORT FLAGS]
```

A short, shareable summary of one period. **With no window flag it is last week
(`--week last`) compared with the week before (`--compare previous`).** Pass any
window and the comparison with the period before it comes with it. An open-ended
window (`--since` alone) has nothing to compare, and the digest says so.

It has, in order:

1. **Comparison**: the same block as `--compare` on a plain report.
2. **Top repositories** and **Top features**: human time, share, agent wall clock
   and commits. A feature is the issue key when the branch names one, otherwise
   the branch; `—` is work with no known branch. Rows past `--top` (at most 10 by
   default) are counted as omitted, not dropped silently.
3. **Focus** and **Leverage**: the headline figures from above.
4. **Goals**, when goals are configured and not disabled with `--no-goals`.
5. **Warnings**, including the stale-rates one.

It is a document, so `--format markdown` is ready to paste into a PR, a wiki or
a status update (`#123` and `@name` in repository or branch names are defused),
`--format html` is one self-contained page, and `--format json` carries the same
sections as data.

Branch and feature names can carry client or ticket names. A digest that lists
features shares them; see [privacy](privacy.md).

## JSON

`insights --format json`:

```json
{
  "window": {"since": "…", "until": null, "label": "the last 28 days (since 2026-08-05)", "defaulted": true},
  "note": "…",
  "focus": {"aggregate": {"block_count": 41, "…": "…"}, "days": [{"date": "2026-08-19", "…": "…"}]},
  "patterns": {"weekdays": ["mon", "…"], "human_seconds": [[0, "… 24 hours …"]], "agent_wall_seconds": ["…"], "night": {}, "weekend": {}},
  "leverage": {"agent_wall_per_human_hour": 0.7, "tokens_per_commit": null, "…": "…"},
  "models": {"providers": [], "models": [], "rates_as_of": "…", "rate_overrides": []},
  "warnings": []
}
```

Only the requested sections are present. `digest --format json` has `window`,
`comparison` (absent for an open-ended window), `top_repos`, `top_features`,
`focus`, `leverage`, `goals` (only when present) and `warnings`.

## Estimates, not a stopwatch

Human time is an estimate from prompts, session edges and commits, and every
figure derived from it inherits that. Agent figures come from local histories,
which can be pruned or cover different tools in different windows. The days are
your local days, taken from the host's timezone.
