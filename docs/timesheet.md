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
| `--describe commits\|sessions[=PROVIDERS]` | add a description column; see [Descriptions](#descriptions) |
| `--summarize-with CMD`, `--summarize-timeout DUR` | a command writes each entry's description |
| `--digest` | print what `--summarize-with` would be given, and run nothing |
| `--ignore-locks` | show the live computation for locked periods instead of their snapshots |
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

## The ledger: manual entries, overrides and locks

Estimates are not always the whole story: a meeting with no keyboard in it, a
day you were out, an hour the tool got wrong. The **ledger** is where those
live, in `timesheet.json` beside the config file (override the location with
`WORKSTATS_TIMESHEET`). It is your file: workstats changes it only when you run
one of these commands, and it is never the cache, so `--no-cache` and
`--rebuild-cache` leave it alone.

```console
$ workstats timesheet add yesterday acme 1h30m "Steering meeting" --start 09:00
Added 7f3a2c1e: 2026-08-11 acme 1h30m (Steering meeting)
$ workstats timesheet set mon acme 2h "Adjusted after review"
$ workstats timesheet set 2026-08-14 internal 0 "Out sick"
$ workstats timesheet entries --month 2026-08
$ workstats timesheet rm 7f3a2c1e
$ workstats timesheet unset mon acme
```

| Command | Does |
| --- | --- |
| `add DATE ENGAGEMENT DURATION [NOTE]` | add hours by hand on top of the estimate. `--start HH:MM` gives the CSV a start time; `--billable` / `--non-billable` overrides the engagement's billing for this entry |
| `set DATE ENGAGEMENT DURATION [NOTE]` | **override**: the entry for that day and engagement becomes this value, whatever was estimated. `0` suppresses the estimate |
| `unset DATE ENGAGEMENT` | remove an override; the estimate applies again |
| `rm ID` | remove a manual entry (ids come from `add` and `entries`) |
| `entries [--month …\|--week …\|--since …]` | list manual entries and overrides, marking those in locked periods; the output says how many are outside the window |
| `lock PERIOD`, `unlock PERIOD`, `locks` | freeze, release and list periods (below) |

**DATE** is `YYYY-MM-DD`, `today`, `yesterday`, or a weekday name (`mon` …
`sun`, or in full): the most recent such day, today included. **DURATION** is
written like the other timesheet durations (`90m`, `1h30m`); an entry is at most
24h, a note one line of at most 1,024 bytes. The engagement must exist in the
config; the error lists the configured keys. Everything is validated before
anything is written, and the ledger holds at most 10,000 entries and overrides
and 500 locks.

### How the ledger composes with the estimate

1. The estimate is computed and rounded as usual.
2. An **override** replaces the estimate for its day and engagement. With
   `--detail`, an engagement has several rows on a day; the override covers the
   engagement's day, so it sits on the first row and the others are zeroed.
3. **Manual entries** are added on top (`manual_seconds`), override or not. An
   entry whose billing differs from its engagement's (`--billable` /
   `--non-billable`) is shown as a row of its own, detail `manual, billable` or
   `manual, non-billable`, so it does not change how the estimate is billed.
4. The **daily cap** is enforced again, with manual hours and overrides counted
   first and never reduced. A cap already applied to estimates is not undone: an
   override that frees room does not give another estimate its capped increment
   back.
5. Hours, amounts and totals are computed last, from the entries as shown, as
   everywhere else.

Status says where a row's figure came from: `suggested` (estimate only),
`overridden`, `manual` (hours by hand with no activity behind them) or
`locked`. The notes column carries your notes and the entry ids. Manual hours
and overrides are not activity, so the reconciliation line, which compares raw
estimates with the report, is unaffected by them.

A ledger item for an engagement that is no longer in the config is still
counted, under its key, with a warning. An item outside the window is not
shown.

### Locks

`workstats timesheet lock 2026-08` freezes a period as submitted. `PERIOD` is
`YYYY-MM`, `YYYY-Www` (ISO week) or `A..B` (two dates, both inclusive); the
window comes from it, so window flags are refused, and so are `--engagement`,
`--billable-only` and `--export`: a lock freezes every entry. The settings
flags (`--increment`, `--rounding`, `--daily-cap` …) apply as for a report.

A lock stores, per entry, what was submitted (hours, billing, rate, currency,
amount, notes, and the description if one was requested), the totals, and the
settings that produced them: increment, rounding, split, minimum entry, daily
cap, `human_idle`, `review_credit`, `gap_cap`, plus fingerprints of the
engagement configuration and of the ledger's entries and overrides.

Viewing a locked period shows the snapshot's entries with status `locked`: what
was submitted stays what is displayed, in the table, the JSON and the CSV, even
if the history, the settings or the engagement rules have since changed. The
live computation is still made, and where it disagrees a **DRIFT** section
lists each day and engagement with the locked value, the current value, the
difference and the likely cause, in this order:

| Cause | When |
| --- | --- |
| `settings changed (…)` | the increment, rounding, split, cap, idle settings … differ from the lock's (the ones that changed are named) |
| `engagement config changed` | the engagement configuration's fingerprint differs |
| `ledger edited after lock` | the ledger changed, and holds a write to that day and engagement made after the lock (a forced write or a newer entry) |
| `new or pruned history` | none of the above: sessions or commits appeared, or the tool that held them pruned them |

The cause is the likeliest, not a proof. Drift in the selected window raises
one warning; no drift raises none. `--ignore-locks` shows the live computation
instead.

- A write into a locked day (`add`, `set`, `unset`, `rm`) is **refused** unless
  `--force`. A forced write is recorded in the ledger and shows up as drift,
  because the locked figures do not change.
- Locking a period already locked needs `--force` and replaces the snapshot
  with the current figures; this is how you accept drift. A lock may not
  overlap a lock of a different period: unlock one first.
- `unlock PERIOD` removes a lock; the days are computed live again. `locks`
  lists what is held: period, when it was locked, entry count, totals and
  settings.
- With a window that covers part of a lock, only the days in both are replaced.

### Storage and safety

The ledger is written atomically (a temporary file in the same directory, then
a rename). **A ledger that cannot be read is a hard error** naming the file,
for every command that would use it, and the file is left untouched:
ignoring it would silently change hours that may already have been submitted.
The same goes for a file written by a newer version. There is no locking
between processes. Like the saved views, the path ignores `--config`
(`WORKSTATS_CONFIG` still moves it, being where "beside the config" points), so
every command sees the same ledger; set `WORKSTATS_TIMESHEET` to keep a
separate ledger, for instance per client.

Back it up like any other record of submitted hours. What it contains is
described in [privacy](privacy.md).

## Descriptions

Entries have no description unless you ask. They are read when you run the
command, shown, and thrown away: never cached, never in a bundle. The
[privacy](privacy.md) page says exactly what is read.

```sh
workstats timesheet --describe commits                  # the subjects of your own commits
workstats timesheet --describe sessions                 # session titles (not Codex)
workstats timesheet --describe commits,sessions=claude+codex
workstats timesheet --describe commits --summarize-with 'my-summarizer' --summarize-timeout 90s
workstats timesheet --describe commits --digest         # print the digests, run nothing
```

* `commits` lists the subject lines of the commits that are yours (agent-authored
  commits are left out), 200 characters each.
* `sessions` lists titles Claude, Pi, OpenCode and Copilot gave their sessions.
  Codex is read only when you name it (`sessions=codex`).
* With both, titles come first, joined with `; ` and cut at 500 characters.
* `--summarize-with CMD` replaces that text: each entry's digest (JSON) goes to
  the command's standard input, and the first 500 characters of its output
  become the description. If it fails or times out you get a warning and no
  description for that entry.

The digest looks like this; the last two fields appear only for the matching
`--describe`:

```json
{"version": 1, "date": "2026-03-02", "engagement": "acme", "detail": "ABC-12",
 "hours": 3.5, "repos": ["acme"], "branches": ["feat/ABC-12-export"],
 "issues": ["ABC-12"], "counts": {"prompts": 12, "commits": 3, "sessions": 2},
 "commit_subjects": ["Add the invoice export"], "session_titles": ["Reconcile the invoices"]}
```

The description is a column in the table, Markdown and HTML output, and appears
in JSON and CSV only when requested. The vendor presets use it in place of the
detail for their Description (or Notes) column. Locked entries keep the
description they had when you locked the period, and are not described again.
Only the entries the output shows are described, so `--engagement` and
`--billable-only` also bound what is read and what a summarizer is run for.

See [privacy](privacy.md) for what the engagement configuration, the ledger and
the outputs contain.
