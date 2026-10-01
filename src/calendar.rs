//! The calendar heatmap: human time per day as a grid of ISO weeks.
//!
//! One computation feeds every surface. [`build`] turns `Report.daily` into a
//! [`Heatmap`], and the HTML page, the Markdown report, the explorer's overlay
//! and `workstats calendar` only draw it, so a day cannot be darker in one of
//! them than in another.
//!
//! Columns are ISO weeks, Monday first, seven rows. There is one grid per ISO
//! week-year, which is the calendar year except for the few days around
//! 1 January that belong to week 1, 52 or 53 of their neighbour: that keeps
//! every grid to at most 53 columns, and a week is never cut in two. Shades are
//! 0 for a day with no human time and 1 to 4 for the quartiles of the days that
//! had some, computed once over the whole window so two grids compare.
//!
//! Days are the host's local days, as everywhere else in the tool.

use std::collections::BTreeMap;
use std::io::{self, Write};

use anyhow::{Result, bail};
use chrono::{Datelike, Duration, Local, NaiveDate};
use clap::Args;

use crate::cli::{OutputFormat, ReportArguments, ReportWindow};
use crate::document::{Block, Document, HeatCell, HeatGrid, Heatmap, render_html, render_markdown};
use crate::model::{DayFigures, Report};
use crate::report::{Purpose, collect};
use crate::timesheet::render::render_text;

/// The longest span drawn, so a window opened with `--since 0001-01` cannot ask
/// for a million cells. Longer windows lose their oldest days, and the legend
/// says so.
const MAX_DAYS: i64 = 3660;
/// Below this a window is a few weeks of a report, not a calendar.
const MIN_DOCUMENT_DAYS: i64 = 28;
/// What `workstats calendar` shows when no window flag is given.
const DEFAULT_DAYS: i64 = 365;

#[derive(Debug, Args)]
pub(crate) struct CalendarArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

/// `workstats calendar`: the grid in the terminal. With no window flag it
/// covers the last 365 days, ending today, so it reads as a rolling year rather
/// than a mostly empty current one in January.
pub(crate) fn run(arguments: CalendarArguments) -> Result<()> {
    let CalendarArguments { mut report } = arguments;
    let explicit_format = report.output_format;
    if let Some(format @ (OutputFormat::Json | OutputFormat::Csv)) = explicit_format {
        bail!(
            "`workstats calendar` draws a grid and has no {} form; use `workstats --daily --format json` for the per-day figures",
            format.name()
        );
    }
    report.daily = true;
    let default_window = !has_window(&report);
    if default_window {
        let today = Local::now().date_naive();
        report.since = Some((today - Duration::days(DEFAULT_DAYS - 1)).to_string());
        report.until = Some(today.to_string());
    }
    let collected = collect(report, Purpose::Query)?;
    // A configured default format may be one a grid cannot take; the grid then
    // falls back to the terminal rather than failing a command that was not
    // given the flag.
    let format = explicit_format
        .or_else(|| {
            let name = collected.report.inputs.config_defaults.get("format")?;
            clap::ValueEnum::from_str(name, true).ok()
        })
        .filter(|format| matches!(format, OutputFormat::Markdown | OutputFormat::Html))
        .unwrap_or(OutputFormat::Table);

    let mut blocks = Vec::new();
    if default_window {
        blocks.push(Block::Paragraph(format!(
            "The last {DEFAULT_DAYS} days; choose another window with --year, --month, --week, --since or --until."
        )));
    }
    match build(&collected.report) {
        Some(heatmap) => blocks.push(Block::Heatmap(heatmap)),
        None => blocks.push(Block::Paragraph(
            "No activity in this window, so there is no calendar to draw.".to_string(),
        )),
    }
    let document = Document {
        title: "WORKSTATS calendar".to_string(),
        blocks,
    };
    let text = match format {
        OutputFormat::Markdown => render_markdown(&document),
        OutputFormat::Html => render_html(&document),
        _ => render_text(&document),
    };
    write!(io::stdout().lock(), "{text}")?;
    Ok(())
}

fn has_window(report: &ReportArguments) -> bool {
    report.month.is_some()
        || report.year.is_some()
        || report.week.is_some()
        || report.since.is_some()
        || report.until.is_some()
}

/// The heatmap for a report, or `None` when the report carries no per-day
/// figures or has no days to draw. The explorer and `workstats calendar` use
/// this.
pub(crate) fn build(report: &Report) -> Option<Heatmap> {
    build_from(
        report.daily.as_deref()?,
        report.window,
        Local::now().date_naive(),
    )
}

/// The heatmap a document (the HTML page, or Markdown under `--daily`) includes
/// on its own: only for a window of 28 days or more, or one with an open end,
/// because a fortnight is a list, not a calendar.
pub(crate) fn for_document(report: &Report) -> Option<Heatmap> {
    if !document_wants(report.window) {
        return None;
    }
    build(report)
}

fn document_wants(window: ReportWindow) -> bool {
    match window {
        (Some(since), Some(until)) => (until - since).num_days() >= MIN_DOCUMENT_DAYS,
        _ => true,
    }
}

/// The window decides which days are drawn; the per-day figures only shade
/// them, because `daily` leaves out days with nothing on them. An open end of
/// the window falls back to the first or last day that has figures.
fn build_from(daily: &[DayFigures], window: ReportWindow, today: NaiveDate) -> Option<Heatmap> {
    let first = daily.first().map(|day| day.date.min(today));
    let last = daily.last().map(|day| day.date);
    let start = match window.0 {
        Some(since) => since.with_timezone(&Local).date_naive(),
        None => first?,
    };
    // `until` is exclusive, so the last day is the one a nanosecond earlier.
    let end = match window.1 {
        Some(until) => (until - Duration::nanoseconds(1))
            .with_timezone(&Local)
            .date_naive(),
        None => last?,
    };
    // Days that have not happened yet are not days with no work.
    let end = end.min(today.max(last.unwrap_or(today)));
    if end < start {
        return None;
    }
    // An old start with nothing after it is not information; only a window that
    // is still too long after that loses days, and says so.
    let mut start = start;
    if (end - start).num_days() > MAX_DAYS
        && let Some(first) = first
    {
        start = start.max(first);
    }
    let mut truncated = None;
    if (end - start).num_days() > MAX_DAYS {
        start = end - Duration::days(MAX_DAYS);
        truncated = Some(start);
    }
    Some(heatmap(daily, start, end, truncated))
}

/// The grids for `start..=end`, shaded from `daily`.
fn heatmap(
    daily: &[DayFigures],
    start: NaiveDate,
    end: NaiveDate,
    truncated: Option<NaiveDate>,
) -> Heatmap {
    let seconds: BTreeMap<NaiveDate, f64> = daily
        .iter()
        .map(|day| (day.date, day.human_seconds))
        .collect();
    let mut active: Vec<f64> = seconds
        .range(start..=end)
        .map(|(_, value)| *value)
        .filter(|value| *value > 0.0)
        .collect();
    active.sort_by(f64::total_cmp);
    let cutoffs = cutoffs(&active);

    let mut builders: Vec<GridBuilder> = Vec::new();
    for date in start.iter_days().take_while(|date| *date <= end) {
        let week = date.iso_week();
        if builders.last().is_none_or(|grid| grid.year != week.year()) {
            builders.push(GridBuilder::new(week.year(), week.week()));
        }
        let value = seconds.get(&date).copied().unwrap_or(0.0);
        if let Some(grid) = builders.last_mut() {
            grid.push(date, value, level(value, &cutoffs));
        }
    }
    let mut legend = legend(&active, &cutoffs);
    if let Some(from) = truncated {
        legend.push_str(&format!(
            " Only the last {MAX_DAYS} days are drawn; days before {from} are not."
        ));
    }
    Heatmap {
        grids: builders.into_iter().map(GridBuilder::finish).collect(),
        legend,
    }
}

/// Where level 2, 3 and 4 begin: the lower edges of the second, third and
/// fourth quarter of the non-zero days, in order. Ties go up, so a window of
/// identical days is all the darkest shade rather than all the lightest.
fn cutoffs(sorted: &[f64]) -> [f64; 3] {
    let edge = |quarter: usize| {
        sorted
            .get(sorted.len() * quarter / 4)
            .copied()
            .unwrap_or(f64::INFINITY)
    };
    [edge(1), edge(2), edge(3)]
}

/// 0 for a day without human time, otherwise 1 plus the quartile edges it has
/// reached.
fn level(seconds: f64, cutoffs: &[f64; 3]) -> u8 {
    if seconds <= 0.0 {
        return 0;
    }
    1 + cutoffs.iter().filter(|edge| seconds >= **edge).count() as u8
}

/// What each shade stands for, for the shades some day actually has: with few
/// active days several quartile edges coincide and a range such as "31m to 31m"
/// would describe no day at all.
fn legend(active: &[f64], cutoffs: &[f64; 3]) -> String {
    if active.is_empty() {
        return "No human time in this window.".to_string();
    }
    let [a, b, c] = cutoffs.map(duration_label);
    let ranges = [
        format!("under {a}"),
        format!("{a} to {b}"),
        format!("{b} to {c}"),
        format!("{c} and more"),
    ];
    let mut used = [false; 4];
    for seconds in active {
        used[usize::from(level(*seconds, cutoffs)) - 1] = true;
    }
    let shades = ['░', '▒', '▓', '█'];
    let parts: Vec<String> = (0..4)
        .filter(|index| used[*index])
        .map(|index| format!("{} {}", shades[index], ranges[index]))
        .collect();
    format!(
        "Human time per day, over {} active {}. Shades are the quartiles of those days: {}.",
        active.len(),
        if active.len() == 1 { "day" } else { "days" },
        parts.join(" · ")
    )
}

/// `6h 15m`, `45m`, `2h`; under a minute is `<1m` rather than a misleading zero.
fn duration_label(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "0m".to_string();
    }
    if seconds < 30.0 {
        return "<1m".to_string();
    }
    let minutes = (seconds / 60.0).round() as u64;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

struct GridBuilder {
    year: i32,
    first_week: u32,
    cells: Vec<(NaiveDate, HeatCell)>,
}

impl GridBuilder {
    fn new(year: i32, first_week: u32) -> Self {
        Self {
            year,
            first_week,
            cells: Vec::new(),
        }
    }

    fn push(&mut self, date: NaiveDate, seconds: f64, level: u8) {
        let title = if seconds > 0.0 {
            format!("{date} · {}", duration_label(seconds))
        } else {
            format!("{date} · no activity")
        };
        self.cells.push((
            date,
            HeatCell {
                // Weeks only increase inside one ISO year.
                column: (date.iso_week().week() - self.first_week) as usize,
                row: date.weekday().num_days_from_monday() as usize,
                level,
                title,
            },
        ));
    }

    fn finish(self) -> HeatGrid {
        let columns = self.cells.last().map_or(0, |(_, cell)| cell.column + 1);
        // A month is named over the column that holds its first day; the
        // leftmost column is named too, so a window that starts mid-month is
        // not unlabelled.
        let mut months: Vec<(usize, String)> = self
            .cells
            .iter()
            .filter(|(date, _)| date.day() == 1)
            .map(|(date, cell)| (cell.column, date.format("%b").to_string()))
            .collect();
        if months.first().is_none_or(|(column, _)| *column != 0)
            && let Some((date, _)) = self.cells.first()
        {
            months.insert(0, (0, date.format("%b").to_string()));
        }
        HeatGrid {
            label: self.year.to_string(),
            columns,
            cells: self.cells.into_iter().map(|(_, cell)| cell).collect(),
            months,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveTime, TimeZone, Utc};

    use super::*;
    use crate::document::{heatmap_lines, render_html, render_markdown};

    fn day(year: i32, month: u32, date: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, date).expect("a valid date")
    }

    fn figures(date: NaiveDate, human_seconds: f64) -> DayFigures {
        DayFigures {
            date,
            human_seconds,
            agent_wall_seconds: 0.0,
            prompts: 0,
            commits: 0,
            sessions: 0,
        }
    }

    fn cell(grid: &HeatGrid, column: usize, row: usize) -> Option<&HeatCell> {
        grid.cells
            .iter()
            .find(|cell| cell.column == column && cell.row == row)
    }

    #[test]
    fn levels_are_zero_and_the_quartiles_of_the_active_days() {
        let sorted: Vec<f64> = (1..=8).map(|n| f64::from(n) * 600.0).collect();
        let edges = cutoffs(&sorted);
        assert_eq!([1800.0, 3000.0, 4200.0], edges);
        let levels: Vec<u8> = sorted.iter().map(|value| level(*value, &edges)).collect();
        assert_eq!(vec![1, 1, 2, 2, 3, 3, 4, 4], levels);
        assert_eq!(0, level(0.0, &edges));
    }

    #[test]
    fn identical_days_are_all_the_darkest_shade_and_one_day_is_not_zero() {
        let edges = cutoffs(&[600.0, 600.0, 600.0]);
        assert_eq!(4, level(600.0, &edges));
        assert_eq!(4, level(1.0, &cutoffs(&[1.0])));
        // No active day: nothing to shade, and nothing divides by zero.
        assert_eq!(0, level(0.0, &cutoffs(&[])));
    }

    #[test]
    fn durations_read_the_way_the_tables_read() {
        assert_eq!("6h 15m", duration_label(6.0 * 3600.0 + 15.0 * 60.0));
        assert_eq!("2h", duration_label(7200.0));
        assert_eq!("45m", duration_label(2700.0));
        assert_eq!("<1m", duration_label(10.0));
        assert_eq!("0m", duration_label(0.0));
    }

    #[test]
    fn weeks_are_columns_and_monday_is_the_first_row() {
        // 2026-08-12 is a Wednesday in ISO week 33; 2026-08-10 is its Monday.
        let daily = vec![figures(day(2026, 8, 12), 3600.0)];
        let map = heatmap(&daily, day(2026, 8, 10), day(2026, 8, 23), None);
        assert_eq!(1, map.grids.len());
        let grid = &map.grids[0];
        assert_eq!("2026", grid.label);
        assert_eq!(2, grid.columns);
        let busy = cell(grid, 0, 2).expect("Wednesday of the first week");
        assert_eq!(4, busy.level);
        assert_eq!("2026-08-12 · 1h", busy.title);
        assert_eq!(0, cell(grid, 0, 0).unwrap().level);
        assert_eq!("2026-08-10 · no activity", cell(grid, 0, 0).unwrap().title);
        // The next Monday starts the second column.
        assert_eq!("2026-08-17 · no activity", cell(grid, 1, 0).unwrap().title);
    }

    /// 2025-12-29 is a Monday and belongs to ISO week 1 of 2026, 2026 has a 53rd
    /// week that ends on 2027-01-03, and 2027-01-04 begins 2027's week 1.
    #[test]
    fn iso_weeks_cross_the_year_boundary_without_cutting_a_week() {
        let daily = vec![figures(day(2025, 12, 31), 1200.0)];
        let map = heatmap(&daily, day(2025, 12, 22), day(2026, 1, 11), None);
        assert_eq!(
            vec!["2025", "2026"],
            map.grids
                .iter()
                .map(|grid| grid.label.as_str())
                .collect::<Vec<_>>()
        );
        let (old, new) = (&map.grids[0], &map.grids[1]);
        // The week of 22 December is the 52nd of 2025, and stays whole.
        assert_eq!(1, old.columns);
        assert_eq!(7, old.cells.len());
        // 29 December starts 2026's first column, and 1 January sits in it.
        assert_eq!("2025-12-29 · no activity", cell(new, 0, 0).unwrap().title);
        assert_eq!("2025-12-31 · 20m", cell(new, 0, 2).unwrap().title);
        assert_eq!("2026-01-01 · no activity", cell(new, 0, 3).unwrap().title);
        assert_eq!("2026-01-05 · no activity", cell(new, 1, 0).unwrap().title);
        assert_eq!(2, new.columns);

        // The 53-week year, and the Friday that ends it.
        let long = heatmap(&[], day(2026, 1, 1), day(2027, 1, 3), None);
        assert_eq!(1, long.grids.len());
        assert_eq!(53, long.grids[0].columns);
        let next = heatmap(&[], day(2026, 12, 28), day(2027, 1, 4), None);
        assert_eq!(2, next.grids.len());
        assert_eq!(
            "2027-01-04 · no activity",
            cell(&next.grids[1], 0, 0).unwrap().title
        );
    }

    #[test]
    fn no_grid_has_more_than_53_columns() {
        for year in 2000..2040 {
            let map = heatmap(&[], day(year, 1, 1), day(year, 12, 31), None);
            for grid in &map.grids {
                assert!(grid.columns <= 53, "{year}: {}", grid.columns);
            }
        }
    }

    #[test]
    fn a_window_that_starts_mid_year_does_not_leave_empty_columns() {
        let map = heatmap(&[], day(2026, 6, 10), day(2026, 6, 30), None);
        assert_eq!(1, map.grids.len());
        // Wednesday 10 June is its own week's column 0.
        assert_eq!(0, map.grids[0].cells[0].column);
        assert_eq!(2, map.grids[0].cells[0].row);
        assert_eq!("Jun", map.grids[0].months[0].1);
    }

    #[test]
    fn months_are_named_over_the_column_of_their_first_day() {
        let map = heatmap(&[], day(2026, 1, 1), day(2026, 4, 30), None);
        let names: Vec<&str> = map.grids[0]
            .months
            .iter()
            .map(|(_, name)| name.as_str())
            .collect();
        assert_eq!(vec!["Jan", "Feb", "Mar", "Apr"], names);
        // 1 February 2026 is a Sunday of ISO week 5, the fifth column.
        assert_eq!(4, map.grids[0].months[1].0);
    }

    #[test]
    fn the_text_grid_has_a_ruler_and_seven_weekdays() {
        let daily = vec![figures(day(2026, 8, 12), 3600.0)];
        let map = heatmap(&daily, day(2026, 8, 10), day(2026, 8, 16), None);
        let lines = heatmap_lines(&map.grids[0], '·');
        assert_eq!(8, lines.len());
        assert!(lines[0].trim_start().starts_with("Aug"), "{lines:?}");
        assert_eq!("Mon ·", lines[1]);
        assert_eq!("Wed █", lines[3]);
        assert_eq!("Sun ·", lines[7]);
    }

    fn window(since: Option<NaiveDate>, until: Option<NaiveDate>) -> ReportWindow {
        let midnight = |date: NaiveDate| {
            Local
                .from_local_datetime(&date.and_time(NaiveTime::MIN))
                .earliest()
                .expect("local midnight exists")
                .with_timezone(&Utc)
        };
        (since.map(midnight), until.map(midnight))
    }

    #[test]
    fn nothing_to_anchor_a_grid_on_is_no_heatmap() {
        // Unbounded and empty: no first or last day to start from.
        assert!(build_from(&[], window(None, None), day(2026, 8, 20)).is_none());
        // Bounded and empty: the window is the calendar, all of it quiet.
        let map = build_from(
            &[],
            window(Some(day(2026, 8, 1)), Some(day(2026, 9, 1))),
            day(2026, 12, 1),
        )
        .unwrap();
        assert!(map.legend.starts_with("No human time"), "{}", map.legend);
        assert_eq!(31, map.grids.iter().map(|g| g.cells.len()).sum::<usize>());
    }

    #[test]
    fn the_window_decides_the_days_not_the_activity() {
        let daily = vec![figures(day(2026, 8, 12), 600.0)];
        // Half-open: 1 August up to 1 September is the 31 days of August.
        let map = build_from(
            &daily,
            window(Some(day(2026, 8, 1)), Some(day(2026, 9, 1))),
            day(2026, 12, 1),
        )
        .unwrap();
        assert_eq!(31, map.grids.iter().map(|g| g.cells.len()).sum::<usize>());
        // Unbounded: from the first to the last active day.
        let daily = vec![
            figures(day(2026, 8, 12), 600.0),
            figures(day(2026, 8, 14), 600.0),
        ];
        let map = build_from(&daily, window(None, None), day(2026, 12, 1)).unwrap();
        assert_eq!(3, map.grids[0].cells.len());
    }

    #[test]
    fn days_that_have_not_happened_are_not_drawn_as_empty() {
        let daily = vec![figures(day(2026, 8, 3), 600.0)];
        let map = build_from(
            &daily,
            window(Some(day(2026, 8, 1)), Some(day(2027, 1, 1))),
            day(2026, 8, 10),
        )
        .unwrap();
        assert_eq!(10, map.grids.iter().map(|g| g.cells.len()).sum::<usize>());
    }

    #[test]
    fn a_short_window_is_not_a_document_calendar_but_a_long_or_open_one_is() {
        assert!(!document_wants(window(
            Some(day(2026, 8, 10)),
            Some(day(2026, 8, 17))
        )));
        assert!(!document_wants(window(
            Some(day(2026, 8, 1)),
            Some(day(2026, 8, 28))
        )));
        assert!(document_wants(window(
            Some(day(2026, 8, 1)),
            Some(day(2026, 8, 29))
        )));
        assert!(document_wants(window(None, None)));
        assert!(document_wants(window(Some(day(2026, 8, 1)), None)));
    }

    #[test]
    fn an_absurdly_long_window_is_cut_and_says_so() {
        let long = window(Some(day(1700, 1, 1)), Some(day(2026, 9, 1)));
        // The empty prefix is dropped first, so nothing is lost here.
        let daily = vec![figures(day(2026, 8, 12), 600.0)];
        let map = build_from(&daily, long, day(2026, 12, 1)).unwrap();
        assert!(!map.legend.contains("Only the last"), "{}", map.legend);
        let daily = vec![
            figures(day(1700, 1, 1), 600.0),
            figures(day(2026, 8, 12), 600.0),
        ];
        let map = build_from(&daily, long, day(2026, 12, 1)).unwrap();
        assert!(
            map.legend.contains("Only the last 3660 days"),
            "{}",
            map.legend
        );
    }

    fn hostile_map() -> Heatmap {
        let hostile = "<script>alert(1)</script>&\"x\"|`a`";
        Heatmap {
            grids: vec![HeatGrid {
                label: hostile.to_string(),
                columns: 2,
                cells: vec![HeatCell {
                    column: 0,
                    row: 0,
                    level: 9,
                    title: format!("{hostile}\u{202e}\n"),
                }],
                months: vec![(0, format!("{hostile}```"))],
            }],
            legend: hostile.to_string(),
        }
    }

    fn document(heatmap: Heatmap) -> Document {
        Document {
            title: "t".to_string(),
            blocks: vec![Block::Heatmap(heatmap)],
        }
    }

    #[test]
    fn the_svg_carries_no_script_link_or_foreign_markup() {
        let html = render_html(&document(hostile_map()));
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("href"), "{html}");
        assert!(!html.contains('\u{202e}'));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;&amp;&quot;x&quot;"));
        // A level outside the palette is the darkest shade, not a new class.
        assert!(html.contains("class=\"l4\""));
        for forbidden in [
            "http://",
            "https://",
            "<link",
            "<img",
            "url(",
            "onload",
            "<foreignObject",
        ] {
            assert!(!html.contains(forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn a_real_heatmap_svg_is_well_formed() {
        let daily = vec![
            figures(day(2026, 8, 12), 3600.0),
            figures(day(2026, 8, 13), 600.0),
        ];
        let map = heatmap(&daily, day(2026, 7, 1), day(2026, 9, 30), None);
        let cells: usize = map.grids.iter().map(|grid| grid.cells.len()).sum();
        let html = render_html(&document(map));
        let svg = &html[html.find("<svg").unwrap()..html.find("</svg>").unwrap() + 6];
        assert_eq!(cells, svg.matches("<rect ").count());
        assert_eq!(cells, svg.matches("</rect>").count());
        assert_eq!(cells, svg.matches("<title>").count());
        assert_eq!(cells, svg.matches("</title>").count());
        assert_eq!(
            svg.matches("<text ").count(),
            svg.matches("</text>").count()
        );
        assert_eq!(1, svg.matches("<svg ").count());
        assert!(svg.contains("<title>2026-08-12 · 1h</title>"));
        // Every attribute is quoted and every opening quote has a closing one.
        assert_eq!(0, svg.matches('"').count() % 2);
        assert!(!html.contains("<script"));
        assert!(html.contains("content=\"default-src 'none'; style-src 'unsafe-inline'\""));
        // Colours come from classes defined in the page, dark mode included.
        assert!(html.contains(".l4{fill:var(--l4)}"));
        assert!(html.contains("prefers-color-scheme:dark"));
    }

    #[test]
    fn the_markdown_grid_cannot_leave_its_fence() {
        let markdown = render_markdown(&document(hostile_map()));
        let fenced: Vec<&str> = markdown
            .lines()
            .skip_while(|line| *line != "```text")
            .skip(1)
            .take_while(|line| *line != "```")
            .collect();
        assert_eq!(8, fenced.len(), "{markdown}");
        for line in fenced {
            assert!(
                line.chars()
                    .all(|c| c.is_ascii_alphabetic() || " ░▒▓█".contains(c)),
                "{line:?}"
            );
        }
        assert_eq!(2, markdown.matches("```").count());
        // Every `<` outside the fence is backslash-escaped, as elsewhere.
        assert!(!markdown.replace("\\<", "").contains('<'), "{markdown}");
    }

    #[test]
    fn the_legend_names_only_the_shades_some_day_has() {
        let one = heatmap(
            &[figures(day(2026, 8, 12), 1860.0)],
            day(2026, 8, 10),
            day(2026, 8, 16),
            None,
        );
        assert!(
            one.legend.ends_with("those days: █ 31m and more."),
            "{}",
            one.legend
        );
        let many: Vec<DayFigures> = (1..=8)
            .map(|n| figures(day(2026, 8, n), f64::from(n) * 600.0))
            .collect();
        let map = heatmap(&many, day(2026, 8, 1), day(2026, 8, 8), None);
        assert!(
            map.legend
                .contains("░ under 30m · ▒ 30m to 50m · ▓ 50m to 1h 10m · █ 1h 10m and more."),
            "{}",
            map.legend
        );
    }

    #[test]
    fn the_markdown_grid_uses_the_shade_glyphs() {
        let daily = vec![figures(day(2026, 8, 12), 3600.0)];
        let map = heatmap(&daily, day(2026, 8, 10), day(2026, 8, 16), None);
        let markdown = render_markdown(&document(map));
        assert!(markdown.contains("### 2026\n\n```text\n"));
        assert!(markdown.contains("Wed █\n"), "{markdown}");
        assert!(markdown.contains("Human time per day, over 1 active day."));
    }
}
