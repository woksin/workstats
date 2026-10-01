//! Presenting a computed timesheet: one `Document` that the terminal,
//! Markdown and HTML outputs all draw from, and the JSON form. Totals are
//! computed here, from the entries that are displayed, so a table, a CSV and a
//! JSON file can never disagree about what adds up to what.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Result, bail};
use chrono::{Datelike, Duration, Local, NaiveDate};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::compute::{Computation, round_to_cents};
use super::model::{Adjustment, EntryStatus, TimesheetEntry, TimesheetWindow, TotalsBy};
use super::presets::status_name;
use crate::document::{Block, Column, Document, Table};
use crate::output::safe_text;

/// Stated at the top of every output.
pub(crate) const HEADER: &str =
    "SUGGESTED HOURS — estimates for review before submitting, not a stopwatch";

/// What the filters left out of the display, so a smaller table is never
/// mistaken for less work.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Hidden {
    pub(crate) entries: usize,
    pub(crate) seconds: u64,
    pub(crate) reasons: Vec<String>,
}

pub(crate) struct View<'a> {
    pub(crate) computation: &'a Computation,
    pub(crate) show_evidence: bool,
    pub(crate) show_description: bool,
    pub(crate) totals_by: TotalsBy,
    /// No window flag was given, so `--month current` was assumed.
    pub(crate) default_window: bool,
    pub(crate) hidden: &'a Hidden,
    /// Warnings from the run itself, listed after the computation's own.
    pub(crate) extra_warnings: &'a [String],
}

impl View<'_> {
    fn entries(&self) -> &[TimesheetEntry] {
        &self.computation.timesheet.entries
    }

    /// Every warning, in the order a reader should meet them.
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.computation
            .timesheet
            .warnings
            .iter()
            .chain(self.extra_warnings)
            .cloned()
            .collect()
    }
}

/// Descriptions come from commit subjects, session titles or a summarizer, and
/// all of them are opt-in. Wired here, where the description column is built,
/// so the reader that fills it can land without the rest of the timesheet
/// changing.
// P8 (descriptions) replaces the body: fill `entry.description` for each entry.
pub(crate) fn describe_entries(
    options: &super::TimesheetOptions,
    _entries: &mut [TimesheetEntry],
) -> Result<()> {
    if !options.describe.is_empty() || options.summarize_with.is_some() || options.digest {
        bail!(
            "--describe, --summarize-with and --digest are not yet implemented; descriptions arrive in a later change"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- figures

/// `2h 30m`, with seconds only when there are some.
pub(crate) fn duration_text(seconds: u64) -> String {
    let (hours, minutes, rest) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if rest == 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{hours}h {minutes:02}m {rest:02}s")
    }
}

fn decimal_hours(seconds: u64) -> String {
    format!("{:.2}", seconds as f64 / 3600.0)
}

/// Money per currency, summed in whole cents so a total is exactly the sum of
/// its displayed amounts. Nothing is converted between currencies.
#[derive(Clone, Debug, Default)]
struct Money(BTreeMap<String, i64>);

impl Money {
    fn add(&mut self, entry: &TimesheetEntry) {
        if let (Some(amount), Some(currency)) = (entry.amount, &entry.currency) {
            *self.0.entry(currency.clone()).or_default() += (amount * 100.0).round() as i64;
        }
    }

    fn text(&self) -> String {
        self.0
            .iter()
            .map(|(currency, cents)| format!("{} {currency}", cents_text(*cents)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(currency, cents)| {
                    (
                        currency.clone(),
                        json!(round_to_cents(*cents as f64 / 100.0)),
                    )
                })
                .collect::<Map<_, _>>(),
        )
    }
}

fn cents_text(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", cents.abs() / 100, cents.abs() % 100)
}

#[derive(Clone, Debug, Default)]
struct Totals {
    seconds: u64,
    money: Money,
}

impl Totals {
    fn of<'a>(entries: impl IntoIterator<Item = &'a TimesheetEntry>) -> Self {
        let mut totals = Self::default();
        for entry in entries {
            totals.seconds += entry.final_seconds;
            totals.money.add(entry);
        }
        totals
    }

    fn json(&self) -> Value {
        json!({
            "seconds": self.seconds,
            "hours": round_to_cents(self.seconds as f64 / 3600.0),
            "amounts": self.money.json(),
        })
    }
}

fn period_key(date: NaiveDate, by: TotalsBy) -> String {
    match by {
        TotalsBy::Day => date.to_string(),
        TotalsBy::Week => {
            let week = date.iso_week();
            format!("{}-W{:02}", week.year(), week.week())
        }
    }
}

fn period_noun(by: TotalsBy) -> &'static str {
    match by {
        TotalsBy::Day => "day",
        TotalsBy::Week => "week",
    }
}

fn window_text(window: &TimesheetWindow, default_window: bool) -> String {
    let day = |moment: chrono::DateTime<chrono::Utc>| {
        moment.with_timezone(&Local).format("%Y-%m-%d").to_string()
    };
    let mut text = match (window.since, window.until) {
        (Some(since), Some(until)) => format!(
            "{} to {} (local days)",
            day(since),
            // The window is half-open: its last day is the one before `until`.
            day(until - Duration::microseconds(1))
        ),
        (Some(since), None) => format!("from {} (local days)", day(since)),
        (None, Some(until)) => format!(
            "until {} (local days)",
            day(until - Duration::microseconds(1))
        ),
        (None, None) => "all history".to_string(),
    };
    if default_window {
        text.push_str(" — no window given, so --month current");
    }
    text
}

// --------------------------------------------------------------- document

fn adjustment_text(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::Capped => "capped to the daily cap",
        Adjustment::RaisedToMinimum => "raised to the minimum entry",
        Adjustment::Balanced => "adjusted so the day balances",
    }
}

fn notes_cell(entry: &TimesheetEntry) -> String {
    let mut parts: Vec<String> = Vec::new();
    match entry.status {
        EntryStatus::Suggested => {}
        other => parts.push(status_name(other).to_string()),
    }
    parts.extend(
        entry
            .adjustments
            .iter()
            .map(|a| adjustment_text(*a).to_string()),
    );
    if let Some(drift) = entry.lock_drift_seconds.filter(|drift| *drift != 0) {
        parts.push(format!("drift {drift:+}s since locked"));
    }
    parts.extend(entry.notes.iter().cloned());
    parts.join("; ")
}

/// The one document every presentation is drawn from.
pub(crate) fn document(view: &View<'_>) -> Document {
    let computation = view.computation;
    let timesheet = &computation.timesheet;
    let mut blocks = Vec::new();

    blocks.push(Block::Facts(facts(view)));

    blocks.push(Block::Section("Entries".to_string()));
    if view.hidden.entries > 0 {
        blocks.push(Block::Paragraph(format!(
            "Not shown: {} entr{} ({}) left out by {}. Totals below are of the entries shown.",
            view.hidden.entries,
            if view.hidden.entries == 1 { "y" } else { "ies" },
            duration_text(view.hidden.seconds),
            view.hidden.reasons.join(", ")
        )));
    }
    if view.entries().is_empty() {
        blocks.push(Block::Paragraph("No entries in this window.".to_string()));
    } else {
        blocks.push(Block::Table(entries_table(view)));
    }

    if !view.entries().is_empty() {
        blocks.push(Block::Section("Totals by engagement".to_string()));
        blocks.push(Block::Table(engagement_table(view)));
    }

    if !timesheet.drift.is_empty() {
        blocks.push(Block::Section("DRIFT since locked".to_string()));
        blocks.push(Block::Paragraph(
            "The locked periods above show what was submitted. The current computation disagrees on these days; the cause is the most likely one, not a proof."
                .to_string(),
        ));
        blocks.push(Block::Table(drift_table(view)));
    }

    if !timesheet.dropped.is_empty() {
        let total: f64 = timesheet
            .dropped
            .iter()
            .map(|entry| entry.raw_seconds)
            .sum();
        blocks.push(Block::Section("Below rounding".to_string()));
        blocks.push(Block::Paragraph(format!(
            "{} entr{} removed by rounding, the minimum or the cap, {} of raw time in all; they are not in any total.",
            timesheet.dropped.len(),
            if timesheet.dropped.len() == 1 { "y was" } else { "ies were" },
            duration_text(total.round() as u64)
        )));
        blocks.push(Block::List(
            timesheet
                .dropped
                .iter()
                .map(|entry| {
                    format!(
                        "{} {}{}: {}",
                        entry.date,
                        entry.engagement,
                        entry
                            .detail
                            .as_ref()
                            .map(|d| format!(" / {d}"))
                            .unwrap_or_default(),
                        duration_text(entry.raw_seconds.round() as u64)
                    )
                })
                .collect(),
        ));
    }

    if !timesheet.cross_check.is_empty() {
        blocks.push(Block::Section("Split rules compared".to_string()));
        blocks.push(Block::Paragraph(
            "Raw time per engagement under each rule, all engagements, before rounding. Every rule distributes the same total; only who it is attributed to changes."
                .to_string(),
        ));
        blocks.push(Block::Table(cross_check_table(view)));
    }

    let warnings = view.warnings();
    if !warnings.is_empty() {
        blocks.push(Block::Section("Warnings".to_string()));
        blocks.push(Block::List(warnings));
    }

    Document {
        title: HEADER.to_string(),
        blocks,
    }
}

fn facts(view: &View<'_>) -> Vec<(String, String)> {
    let timesheet = &view.computation.timesheet;
    let reconciliation = &view.computation.reconciliation;
    let raw = reconciliation.raw_seconds.round() as u64;
    let report = reconciliation.report_seconds.round() as u64;
    let mut reconcile = if reconciliation.consistent {
        format!(
            "raw estimate {} = the report's human time {} (matches)",
            duration_text(raw),
            duration_text(report)
        )
    } else {
        format!(
            "MISMATCH: raw estimate {:.3}s vs the report's human time {:.3}s",
            reconciliation.raw_seconds, reconciliation.report_seconds
        )
    };
    if reconciliation.unassigned_seconds > 0.0 {
        let _ = write!(
            reconcile,
            "; {} of it matches no engagement",
            duration_text(reconciliation.unassigned_seconds.round() as u64)
        );
    }
    let mut facts = vec![
        (
            "Window".to_string(),
            window_text(&timesheet.window, view.default_window),
        ),
        (
            "Rounding".to_string(),
            timesheet.methodology.rounding.clone(),
        ),
        (
            "Attribution".to_string(),
            timesheet.methodology.split_rule.clone(),
        ),
        ("Reconciliation".to_string(), reconcile),
    ];
    if !timesheet.applied_locks.is_empty() {
        facts.push((
            "Locked".to_string(),
            format!(
                "{}: the entries of these periods are the snapshots, not a recomputation",
                timesheet.applied_locks.join(", ")
            ),
        ));
    }
    facts
}

/// `+0h 15m` or `-1h 00m`: a difference, always signed.
fn signed_duration(seconds: i64) -> String {
    format!(
        "{}{}",
        if seconds < 0 { "-" } else { "+" },
        duration_text(seconds.unsigned_abs())
    )
}

fn drift_table(view: &View<'_>) -> Table {
    let rows = &view.computation.timesheet.drift;
    Table::new(
        vec![
            Column::text("Date"),
            Column::text("Engagement"),
            Column::number("Locked"),
            Column::number("Current"),
            Column::number("Difference"),
            Column::text("Likely cause"),
        ],
        rows.iter()
            .map(|row| {
                vec![
                    row.date.to_string(),
                    match &row.detail {
                        Some(detail) => format!("{} / {detail}", row.engagement),
                        None => row.engagement.clone(),
                    },
                    duration_text(row.locked_seconds),
                    duration_text(row.current_seconds),
                    signed_duration(row.difference_seconds),
                    row.cause.clone(),
                ]
            })
            .collect(),
    )
}

fn entries_table(view: &View<'_>) -> Table {
    let entries = view.entries();
    let show_detail = entries.iter().any(|entry| entry.detail.is_some());
    let mut columns = vec![Column::text("Date"), Column::text("Engagement")];
    if show_detail {
        columns.push(Column::text("Detail"));
    }
    columns.extend([
        Column::number("Time"),
        Column::number("Hours"),
        Column::text("Billable"),
        Column::number("Amount"),
    ]);
    if view.show_evidence {
        columns.extend([
            Column::number("Prompts"),
            Column::number("Commits"),
            Column::number("Sessions"),
        ]);
    }
    columns.push(Column::text("Notes"));
    if view.show_description {
        columns.push(Column::text("Description"));
    }
    let width = columns.len();
    // Cells of a row that is not an entry: a label and the figures.
    let summary = |period: &str, noun: &str, totals: &Totals| {
        let mut cells = vec![String::new(); width];
        cells[0] = period.to_string();
        cells[1] = format!("{noun} total");
        let first = if show_detail { 3 } else { 2 };
        cells[first] = duration_text(totals.seconds);
        cells[first + 1] = decimal_hours(totals.seconds);
        cells[first + 3] = totals.money.text();
        cells
    };

    let mut rows = Vec::new();
    let mut current: Option<(String, Vec<&TimesheetEntry>)> = None;
    let flush = |current: &mut Option<(String, Vec<&TimesheetEntry>)>,
                 rows: &mut Vec<Vec<String>>| {
        if let Some((period, group)) = current.take() {
            rows.push(summary(
                &period,
                period_noun(view.totals_by),
                &Totals::of(group.iter().copied()),
            ));
        }
    };
    for entry in entries {
        let period = period_key(entry.date, view.totals_by);
        if current.as_ref().is_some_and(|(key, _)| *key != period) {
            flush(&mut current, &mut rows);
        }
        current
            .get_or_insert_with(|| (period, Vec::new()))
            .1
            .push(entry);
        let mut cells = vec![entry.date.to_string(), entry.label.clone()];
        if show_detail {
            cells.push(entry.detail.clone().unwrap_or_default());
        }
        cells.extend([
            duration_text(entry.final_seconds),
            decimal_hours(entry.final_seconds),
            if entry.billable { "yes" } else { "no" }.to_string(),
            entry
                .amount
                .zip(entry.currency.as_ref())
                .map(|(amount, currency)| format!("{amount:.2} {currency}"))
                .unwrap_or_default(),
        ]);
        if view.show_evidence {
            cells.extend([
                entry.evidence.prompts.to_string(),
                entry.evidence.commits.to_string(),
                entry.evidence.sessions.to_string(),
            ]);
        }
        cells.push(notes_cell(entry));
        if view.show_description {
            cells.push(entry.description.clone().unwrap_or_default());
        }
        rows.push(cells);
    }
    flush(&mut current, &mut rows);

    let total = Totals::of(entries);
    let mut closing = summary("", "period", &total);
    closing[1] = "Period total".to_string();
    let mut table = Table::new(columns, rows);
    table.total = Some(closing);
    table
}

fn engagement_table(view: &View<'_>) -> Table {
    // Engagement order: as first met, which is date then key; sorted by label
    // for a stable reading instead.
    let mut by_engagement: BTreeMap<(bool, String), (String, Totals, bool)> = BTreeMap::new();
    for entry in view.entries() {
        let slot = by_engagement
            .entry((
                entry.engagement == crate::engagement::UNASSIGNED,
                entry.engagement.clone(),
            ))
            .or_insert_with(|| (entry.label.clone(), Totals::default(), entry.billable));
        slot.1.seconds += entry.final_seconds;
        slot.1.money.add(entry);
    }
    let rows = by_engagement
        .values()
        .map(|(label, totals, billable)| {
            vec![
                label.clone(),
                duration_text(totals.seconds),
                decimal_hours(totals.seconds),
                if *billable { "yes" } else { "no" }.to_string(),
                totals.money.text(),
            ]
        })
        .collect();
    let total = Totals::of(view.entries());
    let mut table = Table::new(
        vec![
            Column::text("Engagement"),
            Column::number("Time"),
            Column::number("Hours"),
            Column::text("Billable"),
            Column::number("Amount"),
        ],
        rows,
    );
    table.total = Some(vec![
        "Total".to_string(),
        duration_text(total.seconds),
        decimal_hours(total.seconds),
        String::new(),
        total.money.text(),
    ]);
    table
}

fn cross_check_table(view: &View<'_>) -> Table {
    let rows = &view.computation.timesheet.cross_check;
    let seconds = |value: f64| duration_text(value.round() as u64);
    let mut table = Table::new(
        vec![
            Column::text("Engagement"),
            Column::number("nearest"),
            Column::number("signals"),
            Column::number("agent"),
        ],
        rows.iter()
            .map(|row| {
                vec![
                    row.engagement.clone(),
                    seconds(row.nearest_seconds),
                    seconds(row.signals_seconds),
                    seconds(row.agent_seconds),
                ]
            })
            .collect(),
    );
    table.total = Some(vec![
        "Total".to_string(),
        seconds(rows.iter().map(|row| row.nearest_seconds).sum()),
        seconds(rows.iter().map(|row| row.signals_seconds).sum()),
        seconds(rows.iter().map(|row| row.agent_seconds).sum()),
    ]);
    table
}

// ------------------------------------------------------------------- text

/// The terminal view of a document: aligned columns, figures right-aligned,
/// every cell passed through the same control-character filter as every other
/// output.
pub(crate) fn render_text(document: &Document) -> String {
    let mut output = format!("{}\n", safe_text(&document.title));
    for block in &document.blocks {
        output.push('\n');
        match block {
            Block::Section(title) => {
                let title = safe_text(title);
                let _ = writeln!(output, "{title}\n{}", "-".repeat(title.chars().count()));
            }
            Block::Paragraph(text) => {
                let _ = writeln!(output, "{}", safe_text(text));
            }
            Block::List(items) => {
                for item in items {
                    let _ = writeln!(output, "  - {}", safe_text(item));
                }
            }
            Block::Facts(facts) => {
                let width = facts
                    .iter()
                    .map(|(label, _)| label.chars().count())
                    .max()
                    .unwrap_or(0);
                for (label, value) in facts {
                    let _ = writeln!(output, "{:<width$}  {}", safe_text(label), safe_text(value));
                }
            }
            Block::Table(table) => text_table(&mut output, table),
        }
    }
    output
}

fn text_table(output: &mut String, table: &Table) {
    let clean =
        |cells: &[String]| -> Vec<String> { cells.iter().map(|cell| safe_text(cell)).collect() };
    let header: Vec<String> = table.columns.iter().map(|c| safe_text(&c.label)).collect();
    let rows: Vec<Vec<String>> = table.rows.iter().map(|row| clean(row)).collect();
    let total = table.total.as_ref().map(|row| clean(row));
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows.iter().chain(total.iter()) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut text = String::new();
        for (index, cell) in cells.iter().enumerate() {
            if index > 0 {
                text.push_str("  ");
            }
            let width = widths[index];
            if table.columns[index].numeric {
                let _ = write!(text, "{cell:>width$}");
            } else {
                let _ = write!(text, "{cell:<width$}");
            }
        }
        text.trim_end().to_string()
    };
    let _ = writeln!(output, "{}", line(&header));
    let _ = writeln!(
        output,
        "{}",
        "-".repeat(widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1))
    );
    for row in &rows {
        let _ = writeln!(output, "{}", line(row));
    }
    if let Some(total) = total {
        let _ = writeln!(
            output,
            "{}",
            "-".repeat(widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1))
        );
        let _ = writeln!(output, "{}", line(&total));
    }
}

// ------------------------------------------------------------------- json

/// The JSON form: the computed timesheet, plus the reconciliation and the
/// totals of the displayed entries. Evidence and descriptions are left out
/// unless they were asked for.
pub(crate) fn json(view: &View<'_>) -> Result<Value> {
    let computation = view.computation;
    let mut value = serde_json::to_value(&computation.timesheet)?;
    let object = value
        .as_object_mut()
        .expect("a timesheet serializes to an object");
    object.insert("status".to_string(), json!("suggested"));
    object.insert("header".to_string(), json!(HEADER));
    object.insert(
        "reconciliation".to_string(),
        serde_json::to_value(&computation.reconciliation)?,
    );
    object.insert("warnings".to_string(), json!(view.warnings()));
    object.insert(
        "window".to_string(),
        json!({
            "since": computation.timesheet.window.since,
            "until": computation.timesheet.window.until,
            "default": view.default_window,
        }),
    );
    if let Some(Value::Array(entries)) = object.get_mut("entries") {
        for entry in entries {
            if let Some(entry) = entry.as_object_mut() {
                if !view.show_evidence {
                    entry.remove("evidence");
                }
                if !view.show_description {
                    entry.remove("description");
                }
            }
        }
    }

    let entries = view.entries();
    let mut by_engagement: BTreeMap<String, (String, Totals)> = BTreeMap::new();
    let mut by_period: BTreeMap<String, Totals> = BTreeMap::new();
    for entry in entries {
        let slot = by_engagement
            .entry(entry.engagement.clone())
            .or_insert_with(|| (entry.label.clone(), Totals::default()));
        slot.1.seconds += entry.final_seconds;
        slot.1.money.add(entry);
        let period = by_period
            .entry(period_key(entry.date, view.totals_by))
            .or_default();
        period.seconds += entry.final_seconds;
        period.money.add(entry);
    }
    let mut totals = Totals::of(entries).json();
    let total_object = totals.as_object_mut().expect("an object");
    total_object.insert(
        "by_engagement".to_string(),
        Value::Array(
            by_engagement
                .into_iter()
                .map(|(engagement, (label, totals))| {
                    let mut value = totals.json();
                    let object = value.as_object_mut().expect("an object");
                    object.insert("engagement".to_string(), json!(engagement));
                    object.insert("label".to_string(), json!(label));
                    value
                })
                .collect(),
        ),
    );
    total_object.insert("by".to_string(), json!(period_noun(view.totals_by)));
    total_object.insert(
        "periods".to_string(),
        Value::Array(
            by_period
                .into_iter()
                .map(|(period, totals)| {
                    let mut value = totals.json();
                    value
                        .as_object_mut()
                        .expect("an object")
                        .insert("period".to_string(), json!(period));
                    value
                })
                .collect(),
        ),
    );
    object.insert("totals".to_string(), totals);
    object.insert("hidden".to_string(), serde_json::to_value(view.hidden)?);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::timesheet::compute::Reconciliation;
    use crate::timesheet::model::{
        DroppedEntry, Evidence, Timesheet, TimesheetMethodology, TimesheetSettings,
    };

    fn entry(
        date: (i32, u32, u32),
        engagement: &str,
        seconds: u64,
        amount: Option<f64>,
    ) -> TimesheetEntry {
        TimesheetEntry {
            date: NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap(),
            engagement: engagement.to_string(),
            detail: None,
            label: engagement.to_string(),
            client: None,
            billable: amount.is_some(),
            raw_seconds: seconds as f64,
            estimated_seconds: seconds,
            manual_seconds: 0,
            override_seconds: None,
            final_seconds: seconds,
            first_start: None,
            last_end: None,
            evidence: Evidence::default(),
            rate: None,
            currency: amount.map(|_| "NOK".to_string()),
            amount,
            notes: Vec::new(),
            description: None,
            status: EntryStatus::Suggested,
            adjustments: Vec::new(),
            lock_drift_seconds: None,
        }
    }

    fn computation(entries: Vec<TimesheetEntry>) -> Computation {
        let raw: f64 = entries.iter().map(|entry| entry.raw_seconds).sum();
        Computation {
            timesheet: Timesheet {
                window: TimesheetWindow {
                    since: Some(chrono::Utc.with_ymd_and_hms(2026, 7, 31, 22, 0, 0).unwrap()),
                    until: None,
                },
                settings: TimesheetSettings::default(),
                entries,
                dropped: vec![DroppedEntry {
                    date: NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
                    engagement: "tiny".to_string(),
                    detail: None,
                    raw_seconds: 120.0,
                }],
                cross_check: Vec::new(),
                warnings: vec!["be careful".to_string()],
                methodology: TimesheetMethodology {
                    status: "suggested",
                    split_rule: "once".to_string(),
                    rounding: "nearest to 15m".to_string(),
                },
                drift: Vec::new(),
                applied_locks: Vec::new(),
            },
            reconciliation: Reconciliation {
                raw_seconds: raw,
                report_seconds: raw,
                difference_seconds: 0.0,
                consistent: true,
                unassigned_seconds: 0.0,
            },
        }
    }

    fn view<'a>(computation: &'a Computation, hidden: &'a Hidden, by: TotalsBy) -> View<'a> {
        View {
            computation,
            show_evidence: true,
            show_description: false,
            totals_by: by,
            default_window: true,
            hidden,
            extra_warnings: &[],
        }
    }

    fn sample() -> Computation {
        computation(vec![
            entry((2026, 8, 3), "acme", 5400, Some(2175.0)),
            entry((2026, 8, 3), "internal", 1800, None),
            entry((2026, 8, 4), "acme", 900, Some(362.5)),
            entry((2026, 8, 10), "acme", 3600, Some(1450.0)),
        ])
    }

    #[test]
    fn every_output_leads_with_the_header_and_states_the_default_window() {
        let computation = sample();
        let hidden = Hidden::default();
        let view = view(&computation, &hidden, TotalsBy::Day);
        let document = document(&view);
        assert_eq!(HEADER, document.title);
        let text = render_text(&document);
        assert!(text.starts_with(HEADER));
        assert!(
            text.contains("no window given, so --month current"),
            "{text}"
        );
        assert!(text.contains("matches"));
        let json = json(&view).unwrap();
        assert_eq!("suggested", json["status"]);
        assert_eq!(HEADER, json["header"]);
        assert_eq!(true, json["window"]["default"]);
    }

    #[test]
    fn totals_are_sums_of_the_displayed_entries_including_money() {
        let computation = sample();
        let hidden = Hidden::default();
        let view = view(&computation, &hidden, TotalsBy::Day);
        let json = json(&view).unwrap();
        assert_eq!(11700, json["totals"]["seconds"]);
        assert_eq!(3987.5, json["totals"]["amounts"]["NOK"]);
        let periods = json["totals"]["periods"].as_array().unwrap();
        assert_eq!(3, periods.len());
        let by_day: u64 = periods.iter().map(|p| p["seconds"].as_u64().unwrap()).sum();
        assert_eq!(11700, by_day);
        let engagements = json["totals"]["by_engagement"].as_array().unwrap();
        let money: f64 = engagements
            .iter()
            .filter_map(|e| e["amounts"]["NOK"].as_f64())
            .sum();
        assert_eq!(3987.5, money);
    }

    #[test]
    fn week_totals_group_by_iso_week() {
        let computation = sample();
        let hidden = Hidden::default();
        let json = json(&view(&computation, &hidden, TotalsBy::Week)).unwrap();
        let periods = json["totals"]["periods"].as_array().unwrap();
        assert_eq!(
            vec!["2026-W32", "2026-W33"],
            periods
                .iter()
                .map(|p| p["period"].as_str().unwrap())
                .collect::<Vec<_>>()
        );
        assert_eq!("week", json["totals"]["by"]);
    }

    #[test]
    fn the_table_has_subtotals_and_a_period_total_that_add_up() {
        let computation = sample();
        let hidden = Hidden::default();
        let view = view(&computation, &hidden, TotalsBy::Day);
        let document = document(&view);
        let Some(Block::Table(table)) = document
            .blocks
            .iter()
            .find(|block| matches!(block, Block::Table(_)))
        else {
            panic!("an entries table");
        };
        let subtotals: Vec<_> = table
            .rows
            .iter()
            .filter(|row| row[1].ends_with("day total"))
            .collect();
        assert_eq!(3, subtotals.len());
        let total = table.total.as_ref().unwrap();
        assert_eq!("Period total", total[1]);
        assert_eq!("3h 15m", total[2]);
        assert_eq!("3987.50 NOK", total[5]);
    }

    #[test]
    fn markdown_and_html_come_from_the_same_document() {
        let computation = sample();
        let hidden = Hidden::default();
        let document = document(&view(&computation, &hidden, TotalsBy::Day));
        let markdown = crate::document::render_markdown(&document);
        assert!(markdown.starts_with("# SUGGESTED HOURS"));
        assert!(markdown.contains("| acme |"));
        let html = crate::document::render_html(&document);
        assert!(html.contains("<h1>SUGGESTED HOURS"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn hidden_entries_are_stated_and_dropped_ones_listed() {
        let computation = sample();
        let hidden = Hidden {
            entries: 2,
            seconds: 7200,
            reasons: vec!["--billable-only".to_string()],
        };
        let text = render_text(&document(&view(&computation, &hidden, TotalsBy::Day)));
        assert!(
            text.contains("Not shown: 2 entries (2h 00m) left out by --billable-only"),
            "{text}"
        );
        assert!(text.contains("Below rounding"));
        assert!(text.contains("2026-08-03 tiny: 0h 02m"), "{text}");
        assert!(text.contains("be careful"));
    }

    #[test]
    fn evidence_and_descriptions_are_left_out_of_json_unless_asked() {
        let mut computation = sample();
        computation.timesheet.entries[0].description = Some("did things".into());
        let hidden = Hidden::default();
        let mut v = view(&computation, &hidden, TotalsBy::Day);
        v.show_evidence = false;
        let value = json(&v).unwrap();
        assert!(value["entries"][0].get("evidence").is_none());
        assert!(value["entries"][0].get("description").is_none());
        v.show_evidence = true;
        v.show_description = true;
        let value = json(&v).unwrap();
        assert!(value["entries"][0].get("evidence").is_some());
        assert_eq!("did things", value["entries"][0]["description"]);
    }

    #[test]
    fn hostile_text_cannot_reach_the_terminal() {
        let mut computation = sample();
        computation.timesheet.entries[0].label = "evil\u{1b}[2J\u{202e}".to_string();
        let hidden = Hidden::default();
        let text = render_text(&document(&view(&computation, &hidden, TotalsBy::Day)));
        assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}'));
    }

    #[test]
    fn descriptions_are_refused_until_they_exist() {
        let options = super::super::TimesheetOptions {
            describe: vec!["commits".to_string()],
            ..Default::default()
        };
        assert!(describe_entries(&options, &mut []).is_err());
        assert!(describe_entries(&Default::default(), &mut []).is_ok());
    }

    #[test]
    fn money_text_is_exact_in_cents() {
        let mut money = Money::default();
        for amount in [0.1, 0.2, 0.3] {
            money.add(&entry((2026, 8, 3), "a", 1, Some(amount)));
        }
        assert_eq!("0.60 NOK", money.text());
        assert_eq!("-1.05", cents_text(-105));
    }
}
