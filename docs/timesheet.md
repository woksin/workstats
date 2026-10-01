# Timesheet

Suggested hours per day and engagement, rounded the way a timesheet is, with the working shown.

`workstats timesheet` turns the human time `workstats` already measures into
entries you can review and submit: one line per day per client (an
*engagement*), in whole increments, with the evidence next to each. It is an
estimate for review, not a stopwatch, and every output says so:

```
SUGGESTED HOURS — estimates for review before submitting, not a stopwatch
```

```console
$ workstats timesheet --month 2026-08

Window          2026-08-01 to 2026-08-31 (local days)
Rounding        nearest to 15m
Attribution     each estimated moment is attributed once, to the engagement of the nearest prompt, commit or session edge; concurrent agents add no hours
Reconciliation  raw estimate 41h 02m 11s = the report's human time 41h 02m 11s (matches)

Date        Engagement         Time  Hours  Billable       Amount  Prompts  Commits  Sessions  Notes
2026-08-12  ACME – Platform  3h 30m   3.50  yes       5075.00 NOK       41        3         2
2026-08-12  Internal         1h 15m   1.25  no                          12        0         1
2026-08-12  day total        4h 45m   4.75            5075.00 NOK
…
```

With no window flag the report covers `--month current`, and the output says
so. Every report flag (`--month`, `--week`, `--since`, `--repo`, `--provider`,
`--human-idle` …) works as usual; flags that group rows (`--group-by`,
`--period`, `--compare`) are refused, because a timesheet always groups by day
and engagement.

## Engagements

An engagement says which client or contract work bills to. It sits beside
`project_aliases` and does not replace them: an alias says these repositories
are one product; an engagement says this work bills to this client. Declare
them in the config file:

```json
{
  "engagements": {
    "acme": {
      "label": "ACME – Platform", "client": "ACME AS",
      "billable": true, "rate": 1450, "currency": "NOK",
      "projects": ["cratis"],
      "remotes": ["git@github.com:acme/api.git"],
      "remote_globs": ["github.com/acme/*"],
      "paths": ["~/work/acme"],
      "branches": ["acme/*"],
      "issue_prefixes": ["ACME", "PLAT"],
      "export": {"project": "Platform", "client": "ACME", "task": "Development", "tags": ["dev"]}
    },
    "internal": {"label": "Internal", "billable": false, "paths": ["~/code"], "fallback": true}
  }
}
```

The same rules also give `--group-by engagement` to ordinary reports, so
`workstats --month 2026-08 --group-by engagement` and the timesheet agree.

**Matching.** Each interval, signal, token or commit is checked tier by tier,
and the first tier with a match wins:

1. `issue_prefixes`, against the issue the branch names (needs the
   `issues` rules; `ACME-123` matches prefix `ACME`, case-insensitively);
2. `branches` globs (`*` matches any characters, including `/`);
3. `projects` (a [project alias](configuration.md) key) and `remotes` (a git
   remote URL, normalised like an alias's, so SSH and HTTPS spellings match);
4. `remote_globs`, against the normalised `host/org/repo` (case-insensitive);
5. `paths`: the **longest** prefix of the working directory, across all
   engagements, so `~/work/acme` can sit inside `~/work`;
6. the one engagement with `"fallback": true`.

Anything left is `(unassigned)`: it is listed, summed and reconciled like any
other entry, never dropped. When two engagements' globs overlap, the one whose
key sorts first wins.

**Fields.** `label` defaults to the key; `billable` defaults to true when there
is a `rate` and false otherwise; a `rate` needs a three-letter `currency`.
`export` names the engagement in vendor CSVs; anything left out falls back to
the label and client.

**Validation.** The config is checked before any history is read, and an error
names the engagement and key at fault (`engagements.acme.currency: a rate needs
a currency …`). Refused: bad keys (`^[a-z][a-z0-9_-]{0,63}$`, at most 64
engagements); unknown fields; the same issue prefix, remote, project or
identical path in two engagements; a remote a project alias already absorbed
(reference the alias in `projects` instead); a `projects` entry that is not an
alias; a negative rate; two fallbacks; an engagement that can match nothing.

## Attribution: who gets an overlapping hour

The human timeline is already a partition: every moment of your time is one
piece labelled by the nearest prompt, commit or session edge, so hours per
engagement are sums of pieces and **concurrent agents add no hours**. `--split`
only changes how a work block that touches several engagements is shared; the
block's total, and so the period's, never changes.

| `--split` | A work block is shared… |
| --- | --- |
| `nearest` (default) | by the existing rule: each moment goes to the engagement of the nearest signal |
| `signals` | in proportion to the signals each engagement had in the block (prompts and commits count 1, session edges 0.5) |
| `agent` | in proportion to the agent time each engagement had inside the block, overlaps counted once (a block with no agent time falls back to `nearest`) |

All three are always computed, and the **Split rules compared** table shows
each engagement under each, so you can see how much the choice matters.

## Rounding

Everything is integer seconds; rounded values are multiples of `--increment`
(default `15m`). Durations are written `30s`, `15m`, `2h` or `1h30m`.

| `--rounding` | Rule |
| --- | --- |
| `nearest` (default) | half-up to the nearest increment |
| `up` / `down` | ceiling / floor |
| `balanced` | per day: the day's total is rounded, then the leftover increments go to the entries with the largest remainders (ties: the larger raw value, then the lower key). Every entry stays within one increment of its raw value and the day within half an increment of its raw total |

- `--min-entry DUR`: an entry rounded below this is raised to it (marked).
- `--drop-below DUR`: an entry whose raw time is below this is dropped. An entry
  that rounds to nothing is dropped too. Dropped entries are listed under
  **Below rounding** with their raw time, so nothing disappears silently.
- `--daily-cap DUR` (a multiple of the increment): while a day's estimates
  exceed the cap, one increment comes off the entry rounded up the most (ties:
  non-billable first, then the smaller raw value). Manual entries and overrides
  are never reduced; if they alone exceed the cap, you are warned. The cap
  applies after rounding, so everything stays in increments.

**Totals are always the sum of the displayed entries**: day, week, engagement,
period and amount are never rounded separately, so the table, the CSV and the
JSON add up.

**Reconciliation.** Before rounding, the raw entries (including unassigned)
must equal the report's `human_estimated_seconds` within a millisecond. The
output prints the line; a mismatch is a warning.

## Money

`amount = round2(final hours × rate)` for billable entries with a rate, summed
per currency. Nothing is converted between currencies; two currencies give two
totals.

## Options

| Flag | |
| --- | --- |
| `--detail issue\|feature\|branch\|repo` | break each engagement down further |
| `--engagement KEY` | only this engagement (repeatable; `unassigned` is a key) |
| `--billable-only` | only billable engagements |
| `--unassigned show\|hide` | list or hide work matching no engagement |
| `--totals-by day\|week` | subtotal by day (default) or ISO week |
| `--no-evidence` | leave prompts, commits and sessions out |
| `--format table\|json\|csv\|markdown\|html` | output format |
| `--export toggl\|harvest\|clockify\|generic` | a vendor CSV; implies `--format csv` and conflicts with any other explicit `--format` |

Filters apply after the whole computation: a day is rounded and capped over all
its work, so filtering never changes a figure that is still shown, and the
output states how many entries and hours the filters left out.

Settings can also live in the config:

```json
"timesheet": {
  "increment": "15m", "rounding": "nearest", "min_entry": "0m", "drop_below": "0m",
  "daily_cap": null, "split": "nearest", "unassigned": "show",
  "person": {"email": "me@example.com", "first_name": "Ada", "last_name": "L"}
}
```

Flag, then config, then the built-in default. `person` fills the email and
name columns of the vendor CSVs.

## Exports

`--export` writes one CSV in the layout a time tracker imports. Project,
client, task and tags come from the engagement's `export` block, falling back
to its label and client. Every cell has control characters replaced and a
leading `=`, `+`, `-` or `@` defused, like the report's CSV.

| Preset | Columns |
| --- | --- |
| `toggl` | Email, Start date, Start time, Duration, Project, Client, Description, Billable, Tags |
| `harvest` | Date, Client, Project, Task, Notes, Hours, First Name, Last Name |
| `clockify` | Project, Client, Description, Task, Email, Tags, Billable, Start Date, Start Time, End Date, End Time, Duration (h) |
| `generic` | date, engagement, label, client, detail, billable, hours, duration, raw_hours, estimated_hours, manual_hours, override_hours, rate, currency, amount, prompts, commits, sessions, status, adjustments, notes, description |

- Start time is the local time of the entry's first activity (09:00 for an
  entry with none); the end is that plus the rounded duration, so it is not the
  moment you stopped.
- Dates are ISO (`2026-08-12`). Clockify reads dates in the workspace's own
  format, so check its import settings.
- Vendors change their templates. The layouts are one table in
  `src/timesheet/presets.rs` with a test each; **check a first import against
  the vendor's current template** before relying on one.
- `--format csv` without `--export` is the `generic` layout. Warnings go to
  standard error so a pipe stays clean.

## What is not here yet

Descriptions (`--describe`, `--summarize-with`, `--digest`) are refused until
they land, and the ledger actions (`add`, `set`, `unset`, `rm`, `entries`,
`lock`, `unlock`, `locks`), `--ignore-locks` and the manual-entry columns
arrive with the ledger. See [privacy](privacy.md) for what the engagement
configuration and the outputs contain.
