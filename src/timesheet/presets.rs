//! The CSV layouts: one static table per vendor, each pinned by a golden test.
//!
//! Vendors change their import templates now and then, so every layout is a
//! plain list of columns here and nowhere else; the docs tell the reader to
//! check the current template. Every cell goes through the same two filters
//! as the report's CSV: control characters are replaced, and a cell a
//! spreadsheet would read as a formula is defused.

use std::io::Write;

use anyhow::Result;
use chrono::{Duration, Local, NaiveDateTime, NaiveTime};

use super::model::{Adjustment, EntryStatus, ExportPreset, TimesheetEntry};
use crate::engagement::Engagements;
use crate::output::safe_text;

/// Who the hours are for, from `timesheet.person` in the config.
#[derive(Clone, Debug, Default)]
pub(crate) struct Person {
    pub(crate) email: String,
    pub(crate) first_name: String,
    pub(crate) last_name: String,
}

/// An entry with the vendor-facing names resolved.
pub(crate) struct Row<'a> {
    entry: &'a TimesheetEntry,
    /// `export.project`, else the engagement's label.
    project: String,
    /// `export.client`, else the engagement's client.
    client: String,
    task: String,
    tags: String,
    person: &'a Person,
}

struct Column {
    header: &'static str,
    /// Left out under `--no-evidence`.
    evidence: bool,
    value: fn(&Row<'_>) -> String,
}

const fn column(header: &'static str, value: fn(&Row<'_>) -> String) -> Column {
    Column {
        header,
        evidence: false,
        value,
    }
}

const fn evidence(header: &'static str, value: fn(&Row<'_>) -> String) -> Column {
    Column {
        header,
        evidence: true,
        value,
    }
}

/// Where an entry with no known start (one typed by hand) is placed in vendors
/// that require a time of day.
const DEFAULT_START: NaiveTime = NaiveTime::from_hms_opt(9, 0, 0).unwrap();

static TOGGL: [Column; 9] = [
    column("Email", |row| row.person.email.clone()),
    column("Start date", |row| row.entry.date.to_string()),
    column("Start time", |row| {
        start(row.entry).format("%H:%M:%S").to_string()
    }),
    column("Duration", |row| clock(row.entry.final_seconds)),
    column("Project", |row| row.project.clone()),
    column("Client", |row| row.client.clone()),
    column("Description", description_or_detail),
    column("Billable", |row| yes_no(row.entry.billable)),
    column("Tags", |row| row.tags.clone()),
];

static HARVEST: [Column; 8] = [
    column("Date", |row| row.entry.date.to_string()),
    column("Client", |row| row.client.clone()),
    column("Project", |row| row.project.clone()),
    column("Task", |row| row.task.clone()),
    column("Notes", description_or_detail),
    column("Hours", |row| hours(row.entry.final_seconds)),
    column("First Name", |row| row.person.first_name.clone()),
    column("Last Name", |row| row.person.last_name.clone()),
];

// ISO dates: Clockify reads the date in the workspace's own format, which
// cannot be known from here, so the portable spelling is the safest.
static CLOCKIFY: [Column; 12] = [
    column("Project", |row| row.project.clone()),
    column("Client", |row| row.client.clone()),
    column("Description", description_or_detail),
    column("Task", |row| row.task.clone()),
    column("Email", |row| row.person.email.clone()),
    column("Tags", |row| row.tags.clone()),
    column("Billable", |row| yes_no(row.entry.billable)),
    column("Start Date", |row| row.entry.date.to_string()),
    column("Start Time", |row| {
        start(row.entry).format("%H:%M:%S").to_string()
    }),
    column("End Date", |row| end(row.entry).date().to_string()),
    column("End Time", |row| {
        end(row.entry).format("%H:%M:%S").to_string()
    }),
    column("Duration (h)", |row| clock(row.entry.final_seconds)),
];

static GENERIC: [Column; 22] = [
    column("date", |row| row.entry.date.to_string()),
    column("engagement", |row| row.entry.engagement.clone()),
    column("label", |row| row.entry.label.clone()),
    column("client", |row| row.entry.client.clone().unwrap_or_default()),
    column("detail", |row| row.entry.detail.clone().unwrap_or_default()),
    column("billable", |row| yes_no(row.entry.billable)),
    column("hours", |row| hours(row.entry.final_seconds)),
    column("duration", |row| clock(row.entry.final_seconds)),
    column("raw_hours", |row| {
        format!("{:.4}", row.entry.raw_seconds / 3600.0)
    }),
    column("estimated_hours", |row| hours(row.entry.estimated_seconds)),
    column("manual_hours", |row| hours(row.entry.manual_seconds)),
    column("override_hours", |row| {
        row.entry.override_seconds.map(hours).unwrap_or_default()
    }),
    column("rate", |row| {
        row.entry
            .rate
            .map(|rate| rate.to_string())
            .unwrap_or_default()
    }),
    column("currency", |row| {
        row.entry.currency.clone().unwrap_or_default()
    }),
    column("amount", |row| {
        row.entry
            .amount
            .map(|amount| format!("{amount:.2}"))
            .unwrap_or_default()
    }),
    evidence("prompts", |row| row.entry.evidence.prompts.to_string()),
    evidence("commits", |row| row.entry.evidence.commits.to_string()),
    evidence("sessions", |row| row.entry.evidence.sessions.to_string()),
    column("status", |row| status_name(row.entry.status).to_string()),
    column("adjustments", |row| {
        row.entry
            .adjustments
            .iter()
            .map(|adjustment| adjustment_name(*adjustment))
            .collect::<Vec<_>>()
            .join(";")
    }),
    column("notes", |row| row.entry.notes.join("; ")),
    column("description", |row| {
        row.entry.description.clone().unwrap_or_default()
    }),
];

fn columns(preset: ExportPreset, with_evidence: bool) -> Vec<&'static Column> {
    let all: &'static [Column] = match preset {
        ExportPreset::Toggl => &TOGGL,
        ExportPreset::Harvest => &HARVEST,
        ExportPreset::Clockify => &CLOCKIFY,
        ExportPreset::Generic => &GENERIC,
    };
    all.iter()
        .filter(|column| with_evidence || !column.evidence)
        .collect()
}

pub(crate) fn status_name(status: EntryStatus) -> &'static str {
    match status {
        EntryStatus::Suggested => "suggested",
        EntryStatus::Overridden => "overridden",
        EntryStatus::Manual => "manual",
        EntryStatus::Locked => "locked",
    }
}

pub(crate) fn adjustment_name(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::Capped => "capped",
        Adjustment::RaisedToMinimum => "raised_to_minimum",
        Adjustment::Balanced => "balanced",
    }
}

fn yes_no(value: bool) -> String {
    if value { "Yes" } else { "No" }.to_string()
}

fn description_or_detail(row: &Row<'_>) -> String {
    row.entry
        .description
        .clone()
        .or_else(|| row.entry.detail.clone())
        .unwrap_or_default()
}

/// `HH:MM:SS`, hours unbounded.
fn clock(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

fn hours(seconds: u64) -> String {
    format!("{:.2}", seconds as f64 / 3600.0)
}

/// The local start of an entry: the first moment of its time, or 09:00 when it
/// has none (a manual entry).
fn start(entry: &TimesheetEntry) -> NaiveDateTime {
    let time = entry
        .first_start
        .map_or(DEFAULT_START, |first| first.with_timezone(&Local).time());
    entry.date.and_time(time)
}

/// Start plus the final duration, which can differ from the last moment of
/// activity: the hours are rounded and the day's time is not contiguous.
fn end(entry: &TimesheetEntry) -> NaiveDateTime {
    start(entry) + Duration::seconds(entry.final_seconds as i64)
}

/// The same rule as the report's CSV: a leading `= + - @` is read as a
/// formula by a spreadsheet, unless the whole cell is a number.
fn neutralize_formula(value: String) -> String {
    if value.starts_with(['=', '+', '-', '@']) && value.parse::<f64>().is_err() {
        format!("'{value}")
    } else {
        value
    }
}

fn cell(value: &str) -> String {
    neutralize_formula(safe_text(value))
}

/// Resolves the vendor-facing names of an entry from its engagement's
/// `export` block, falling back to the label and client.
fn row<'a>(entry: &'a TimesheetEntry, engagements: &Engagements, person: &'a Person) -> Row<'a> {
    let export = engagements
        .get(&entry.engagement)
        .map(|engagement| &engagement.export);
    Row {
        entry,
        project: export
            .and_then(|export| export.project.clone())
            .unwrap_or_else(|| entry.label.clone()),
        client: export
            .and_then(|export| export.client.clone())
            .or_else(|| entry.client.clone())
            .unwrap_or_default(),
        task: export
            .and_then(|export| export.task.clone())
            .unwrap_or_default(),
        tags: export
            .map(|export| export.tags.join(", "))
            .unwrap_or_default(),
        person,
    }
}

/// Writes the entries as CSV in the given layout, header first.
pub(crate) fn write_csv<W: Write>(
    output: W,
    entries: &[TimesheetEntry],
    preset: ExportPreset,
    engagements: &Engagements,
    person: &Person,
    with_evidence: bool,
) -> Result<()> {
    let columns = columns(preset, with_evidence);
    let mut writer = csv::Writer::from_writer(output);
    writer.write_record(columns.iter().map(|column| column.header))?;
    for entry in entries {
        let row = row(entry, engagements, person);
        writer.write_record(columns.iter().map(|column| cell(&(column.value)(&row))))?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use chrono::{NaiveDate, TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::timesheet::model::Evidence;

    fn engagements() -> Engagements {
        Engagements::compile(
            Some(&json!({
                "acme": {
                    "label": "ACME – Platform", "client": "ACME AS", "rate": 1450, "currency": "NOK",
                    "paths": ["/acme"],
                    "export": {"project": "Platform", "client": "ACME", "task": "Development", "tags": ["dev", "billable"]}
                },
                "internal": {"label": "Internal", "billable": false, "fallback": true},
            })),
            &BTreeMap::new(),
            Path::new("/"),
        )
        .unwrap()
    }

    fn person() -> Person {
        Person {
            email: "ada@example.com".into(),
            first_name: "Ada".into(),
            last_name: "L".into(),
        }
    }

    /// A local clock time, so the golden start times hold in any timezone.
    fn local(hour: u32, minute: u32) -> chrono::DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 8, 12, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn entry(
        engagement: &str,
        label: &str,
        client: Option<&str>,
        billable: bool,
    ) -> TimesheetEntry {
        TimesheetEntry {
            date: NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(),
            engagement: engagement.into(),
            detail: None,
            label: label.into(),
            client: client.map(str::to_string),
            billable,
            raw_seconds: 8000.0,
            estimated_seconds: 8100,
            manual_seconds: 0,
            override_seconds: None,
            final_seconds: 8100,
            first_start: Some(local(9, 5)),
            last_end: Some(local(11, 30)),
            evidence: Evidence {
                prompts: 12,
                commits: 3,
                sessions: 2,
                blocks: 1,
                ..Evidence::default()
            },
            rate: billable.then_some(1450.0),
            currency: billable.then(|| "NOK".to_string()),
            amount: billable.then_some(3262.5),
            notes: Vec::new(),
            description: None,
            status: EntryStatus::Suggested,
            adjustments: Vec::new(),
            lock_drift_seconds: None,
        }
    }

    fn sample() -> Vec<TimesheetEntry> {
        let mut internal = entry("internal", "Internal", None, false);
        internal.final_seconds = 900;
        internal.estimated_seconds = 900;
        internal.raw_seconds = 700.0;
        internal.first_start = Some(local(23, 30));
        internal.adjustments = vec![Adjustment::Capped];
        internal.notes = vec!["half a day".into()];
        internal.evidence = Evidence::default();
        vec![
            entry("acme", "ACME – Platform", Some("ACME AS"), true),
            internal,
        ]
    }

    fn render(preset: ExportPreset, evidence: bool) -> String {
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &sample(),
            preset,
            &engagements(),
            &person(),
            evidence,
        )
        .unwrap();
        String::from_utf8(buffer).unwrap().replace("\r\n", "\n")
    }

    #[test]
    fn toggl_golden() {
        assert_eq!(
            "Email,Start date,Start time,Duration,Project,Client,Description,Billable,Tags\n\
ada@example.com,2026-08-12,09:05:00,02:15:00,Platform,ACME,,Yes,\"dev, billable\"\n\
ada@example.com,2026-08-12,23:30:00,00:15:00,Internal,,,No,\n",
            render(ExportPreset::Toggl, true)
        );
    }

    #[test]
    fn harvest_golden() {
        assert_eq!(
            "Date,Client,Project,Task,Notes,Hours,First Name,Last Name\n\
2026-08-12,ACME,Platform,Development,,2.25,Ada,L\n\
2026-08-12,,Internal,,,0.25,Ada,L\n",
            render(ExportPreset::Harvest, true)
        );
    }

    #[test]
    fn clockify_golden() {
        assert_eq!(
            "Project,Client,Description,Task,Email,Tags,Billable,Start Date,Start Time,End Date,End Time,Duration (h)\n\
Platform,ACME,,Development,ada@example.com,\"dev, billable\",Yes,2026-08-12,09:05:00,2026-08-12,11:20:00,02:15:00\n\
Internal,,,,ada@example.com,,No,2026-08-12,23:30:00,2026-08-12,23:45:00,00:15:00\n",
            render(ExportPreset::Clockify, true)
        );
    }

    #[test]
    fn generic_golden() {
        assert_eq!(
            "date,engagement,label,client,detail,billable,hours,duration,raw_hours,estimated_hours,manual_hours,override_hours,rate,currency,amount,prompts,commits,sessions,status,adjustments,notes,description\n\
2026-08-12,acme,ACME – Platform,ACME AS,,Yes,2.25,02:15:00,2.2222,2.25,0.00,,1450,NOK,3262.50,12,3,2,suggested,,,\n\
2026-08-12,internal,Internal,,,No,0.25,00:15:00,0.1944,0.25,0.00,,,,,0,0,0,suggested,capped,half a day,\n",
            render(ExportPreset::Generic, true)
        );
    }

    #[test]
    fn no_evidence_leaves_the_evidence_columns_out() {
        let text = render(ExportPreset::Generic, false);
        let header = text.lines().next().unwrap();
        assert!(!header.contains("prompts") && !header.contains("sessions"));
        assert!(header.contains("amount,status"));
        assert_eq!(
            header.split(',').count(),
            text.lines().nth(1).unwrap().split(',').count()
        );
    }

    #[test]
    fn a_description_replaces_the_detail_in_the_vendor_columns() {
        let mut entries = sample();
        entries[0].detail = Some("ACME-12".into());
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &entries,
            ExportPreset::Harvest,
            &engagements(),
            &person(),
            true,
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&buffer).contains("Development,ACME-12,2.25"));
        entries[0].description = Some("Reviewed the API".into());
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &entries,
            ExportPreset::Harvest,
            &engagements(),
            &person(),
            true,
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&buffer).contains("Development,Reviewed the API,2.25"));
    }

    #[test]
    fn a_manual_entry_with_no_start_begins_at_nine() {
        let mut entries = sample();
        entries[0].first_start = None;
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &entries,
            ExportPreset::Toggl,
            &engagements(),
            &person(),
            true,
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&buffer).contains("2026-08-12,09:00:00,02:15:00"));
    }

    #[test]
    fn hostile_cells_are_neutralised_and_cannot_break_the_row() {
        let mut entries = sample();
        entries[0].label = "=HYPERLINK(\"http://x\")".into();
        entries[0].client = Some("+cmd".into());
        entries[0].detail = Some("line\nbreak\u{202e}".into());
        entries[0].notes = vec!["@sum".into()];
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &entries,
            ExportPreset::Generic,
            &engagements(),
            &person(),
            true,
        )
        .unwrap();
        let text = String::from_utf8(buffer).unwrap();
        // One header, two rows: the newline in a cell did not add a line.
        assert_eq!(3, text.lines().count());
        assert!(text.contains("'=HYPERLINK"), "{text}");
        assert!(text.contains("'+cmd"), "{text}");
        assert!(text.contains("'@sum"), "{text}");
        assert!(!text.contains('\u{202e}'));
        // A negative amount-like number is left alone: it is not a formula.
        assert_eq!("-1.5", neutralize_formula("-1.5".into()));
    }

    #[test]
    fn the_exported_hours_and_the_durations_always_agree() {
        let mut entries = sample();
        entries[0].final_seconds = 3 * 3600 + 20 * 60;
        let mut buffer = Vec::new();
        write_csv(
            &mut buffer,
            &entries,
            ExportPreset::Generic,
            &engagements(),
            &person(),
            false,
        )
        .unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.contains("3.33,03:20:00"), "{text}");
    }
}
