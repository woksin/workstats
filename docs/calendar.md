# Calendar heatmap

A year of human time at a glance: one square per day, one column per ISO week, Monday at the top.

```bash
workstats calendar                      # the last 365 days, ending today
workstats calendar --year 2026
workstats calendar --month 2026-03 --repo api
workstats --format html > report.html   # the page includes it
workstats --format markdown --daily     # so does Markdown, when you ask for per-day figures
workstats ui                            # press c
```

```text
2026                      (illustrative)
    Jan  Feb   Mar
Mon ·▒·█▓·░··▒···
    ·░·▒▒·········
Wed ··▓▓█·░··▒····
    ...
```

## What it shows

Each square is the **human time** of one local day, the same human-time figure the report adds up per day (`workstats --daily`). Agent wall-clock time is not drawn. Days follow the host's local time zone, like every other day in the tool.

Shades are `·` (nothing) and four levels `░ ▒ ▓ █`. Levels 1 to 4 are the quartiles of the days that had any human time, taken once over the whole window so two years in one window are comparable. The legend under the grid names the hours each shade starts at, for the shades that some day actually has. Identical days all get the darkest shade; one active day is that shade too. A day with no work is not the same as a day outside the window: the latter is left blank, and days that have not happened yet are not drawn.

## Layout

Columns are ISO weeks. There is one grid per **ISO week-year**, which is the calendar year except for the few days around 1 January that belong to week 1, 52 or 53 of their neighbour (29 December 2025 is in week 1 of 2026). That keeps every grid to at most 53 columns and never cuts a week in two. Months are named over the column that holds their first day.

A window that starts mid-year starts its grid at that week. A window longer than 3,660 days draws the last 3,660 days and says so in the legend.

## Where it appears

| Surface | When |
| --- | --- |
| `workstats calendar` | Always. `--format markdown` and `--format html` work; `json` and `csv` are refused, since the per-day figures are `workstats --daily --format json`. With no window flag it covers the last 365 days. |
| HTML report | Automatically when the window is 28 days or longer, or has no start or no end. A shorter window is a list, not a calendar. |
| Markdown report | The same rule, but Markdown only has per-day figures with `--daily`. |
| `workstats ui` | `c` opens it over the report window; `↑ ↓` change year; `c` or `Esc` closes it. |
| Table and JSON reports | Not drawn. `--daily` adds the figures to JSON. |

The HTML calendar is an inline `<svg>` of `<rect>`s, each with a `<title>` such as `2026-08-12 · 6h 15m` that browsers show on hover. It has no script and no link, its colours are CSS classes (with a dark-mode set), and it works under the page's `Content-Security-Policy` of `default-src 'none'; style-src 'unsafe-inline'`. The Markdown form is a fenced code block of ` ░▒▓█`.
