//! `workstats timesheet` and engagement grouping, end to end: two projects on
//! two days become two engagements, and every output says the same thing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::*;

struct Fixture {
    _directory: tempfile::TempDir,
    config: PathBuf,
    history: String,
}

/// Two Pi sessions in March 2026: one in `acme`, one in `internal`, on
/// different days so their prompts never share a timestamp.
fn fixture(engagements: impl FnOnce(&Path, &Path) -> Value) -> Fixture {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let acme = root.join("acme");
    let internal = root.join("internal");
    fs::create_dir_all(&acme).unwrap();
    fs::create_dir_all(&internal).unwrap();
    let sessions = root.join("pi-sessions");
    pi_session_on(&sessions, "a1", &acme, "claude-opus-5", 10, "2026-03-02");
    pi_session_on(
        &sessions,
        "i1",
        &internal,
        "claude-opus-5",
        10,
        "2026-03-03",
    );
    let config = root.join("config.json");
    let engagements = engagements(&acme, &internal);
    fs::write(
        &config,
        json!({
            "engagements": engagements,
            "timesheet": {"person": {"email": "ada@example.com", "first_name": "Ada", "last_name": "L"}},
        })
        .to_string(),
    )
    .unwrap();
    Fixture {
        history: format!("pi={}", sessions.display()),
        _directory: directory,
        config,
    }
}

/// ACME bills by the hour, internal work does not.
fn standard(acme: &Path, internal: &Path) -> Value {
    json!({
        "acme": {
            "label": "ACME - Platform", "client": "ACME AS", "rate": 1000, "currency": "NOK",
            "paths": [acme],
            "export": {"project": "Platform", "task": "Development", "tags": ["dev"]}
        },
        "internal": {"label": "Internal", "billable": false, "paths": [internal]},
    })
}

impl Fixture {
    fn run(&self, subcommand: &[&str], extra: &[&str]) -> Output {
        let mut arguments: Vec<&str> = subcommand.to_vec();
        arguments.extend([
            "--no-git",
            "--no-cache",
            "--no-progress",
            "--no-default-events",
            "--no-update-check",
            "--provider",
            "pi",
            "--history",
            self.history.as_str(),
            "--config",
            self.config.to_str().unwrap(),
        ]);
        arguments.extend(extra);
        // Never the developer's own ledger: its entries would change the
        // expected figures.
        std::process::Command::new(binary())
            .args(&arguments)
            .env(
                "WORKSTATS_TIMESHEET",
                self._directory.path().join("timesheet.json"),
            )
            .output()
            .unwrap()
    }

    fn timesheet(&self, extra: &[&str]) -> Output {
        let mut arguments = vec!["--month", "2026-03"];
        arguments.extend(extra);
        self.run(&["timesheet"], &arguments)
    }
}

fn text(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    assert!(!output.status.success(), "expected a failure");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn engagements_group_an_ordinary_report_too() {
    let fixture = fixture(standard);
    let output = fixture.run(
        &[],
        &[
            "--month",
            "2026-03",
            "--group-by",
            "engagement",
            "--format",
            "json",
        ],
    );
    let report = json_stdout(&output);
    let mut engagements: Vec<String> = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["key"]["engagement"].as_str().unwrap().to_string())
        .collect();
    engagements.sort();
    assert_eq!(vec!["acme", "internal"], engagements);
    let by_engagement: f64 = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["human_estimated_seconds"].as_f64().unwrap())
        .sum();
    // Engagements partition the human timeline: they never add to it.
    assert!(
        (by_engagement
            - report["summary"]["human_estimated_seconds"]
                .as_f64()
                .unwrap())
        .abs()
            < 0.01
    );
}

#[test]
fn the_timesheet_json_reconciles_with_the_report_and_adds_up() {
    let fixture = fixture(standard);
    let value = json_stdout(&fixture.timesheet(&["--format", "json"]));
    assert_eq!("suggested", value["status"]);
    assert_eq!(
        "SUGGESTED HOURS — estimates for review before submitting, not a stopwatch",
        value["header"]
    );
    assert!(
        value["methodology"]["split_rule"]
            .as_str()
            .unwrap()
            .contains("attributed once")
    );
    assert_eq!(true, value["reconciliation"]["consistent"]);

    let report = json_stdout(&fixture.run(&[], &["--month", "2026-03", "--format", "json"]));
    let human = report["summary"]["human_estimated_seconds"]
        .as_f64()
        .unwrap();
    assert!((value["reconciliation"]["raw_seconds"].as_f64().unwrap() - human).abs() < 0.001);

    let entries = value["entries"].as_array().unwrap();
    let engagements: std::collections::BTreeSet<_> = entries
        .iter()
        .map(|entry| entry["engagement"].as_str().unwrap())
        .collect();
    assert_eq!(
        std::collections::BTreeSet::from(["acme", "internal"]),
        engagements
    );
    let mut total = 0;
    for entry in entries {
        let seconds = entry["final_seconds"].as_u64().unwrap();
        assert_eq!(0, seconds % 900, "whole increments: {entry}");
        total += seconds;
    }
    assert_eq!(total, value["totals"]["seconds"].as_u64().unwrap());
    // 1000/h on the billable engagement only, in its own currency.
    let acme = entries.iter().find(|e| e["engagement"] == "acme").unwrap();
    let hours = acme["final_seconds"].as_f64().unwrap() / 3600.0;
    assert!(
        (acme["amount"].as_f64().unwrap() - (hours * 1000.0 * 100.0).round() / 100.0).abs() < 0.005
    );
    assert_eq!("NOK", acme["currency"]);
    assert!(value["totals"]["amounts"]["NOK"].as_f64().unwrap() > 0.0);
    let internal = entries
        .iter()
        .find(|e| e["engagement"] == "internal")
        .unwrap();
    assert!(internal["amount"].is_null());
    assert!(!value["cross_check"].as_array().unwrap().is_empty());
}

#[test]
fn the_default_window_is_the_current_month_and_says_so() {
    let fixture = fixture(standard);
    let table = text(&fixture.run(&["timesheet"], &[]));
    assert!(
        table.starts_with(
            "SUGGESTED HOURS — estimates for review before submitting, not a stopwatch"
        )
    );
    assert!(
        table.contains("no window given, so --month current"),
        "{table}"
    );
    let value = json_stdout(&fixture.run(&["timesheet"], &["--format", "json"]));
    assert_eq!(true, value["window"]["default"]);
    let explicit = json_stdout(&fixture.timesheet(&["--format", "json"]));
    assert_eq!(false, explicit["window"]["default"]);
}

#[test]
fn the_table_shows_entries_totals_and_the_cross_check() {
    let fixture = fixture(standard);
    let table = text(&fixture.timesheet(&[]));
    for expected in [
        "ACME - Platform",
        "Internal",
        "day total",
        "Period total",
        "Totals by engagement",
        "Split rules compared",
        "Reconciliation",
        "matches",
    ] {
        assert!(table.contains(expected), "missing {expected:?}:\n{table}");
    }
    let weekly = text(&fixture.timesheet(&["--totals-by", "week"]));
    assert!(
        weekly.contains("week total") && weekly.contains("2026-W"),
        "{weekly}"
    );
}

#[test]
fn markdown_and_html_are_documents() {
    let fixture = fixture(standard);
    let markdown = text(&fixture.timesheet(&["--format", "markdown"]));
    assert!(markdown.starts_with("# SUGGESTED HOURS"), "{markdown}");
    assert!(markdown.contains("| Date | Engagement |"));
    let html = text(&fixture.timesheet(&["--format", "html"]));
    assert!(html.contains("<h1>SUGGESTED HOURS"));
    assert!(html.contains("default-src 'none'"));
    assert!(!html.contains("<script"));
}

#[test]
fn export_presets_write_their_columns() {
    let fixture = fixture(standard);
    for (preset, header) in [
        (
            "toggl",
            "Email,Start date,Start time,Duration,Project,Client,Description,Billable,Tags",
        ),
        (
            "harvest",
            "Date,Client,Project,Task,Notes,Hours,First Name,Last Name",
        ),
        (
            "clockify",
            "Project,Client,Description,Task,Email,Tags,Billable,Start Date,Start Time,End Date,End Time,Duration (h)",
        ),
    ] {
        let csv = text(&fixture.timesheet(&["--export", preset]));
        let mut lines = csv.lines();
        assert_eq!(header, lines.next().unwrap(), "{preset}");
        let rows: Vec<_> = lines.collect();
        assert_eq!(2, rows.len(), "{preset}: {csv}");
        assert!(
            rows.iter().any(|row| row.contains("Platform")),
            "{preset}: {csv}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Internal")),
            "{preset}: {csv}"
        );
    }
    let toggl = text(&fixture.timesheet(&["--export", "toggl"]));
    assert!(toggl.contains("ada@example.com"));
    assert!(
        toggl.contains("Platform,ACME AS"),
        "client falls back to the engagement's: {toggl}"
    );
    // Plain --format csv is the generic layout.
    let generic = text(&fixture.timesheet(&["--format", "csv"]));
    assert!(generic.starts_with("date,engagement,label,client,detail,billable,hours,duration,"));
    // The CSV hours add up to the JSON total.
    let json = json_stdout(&fixture.timesheet(&["--format", "json"]));
    let hours: f64 = generic
        .lines()
        .skip(1)
        .map(|row| row.split(',').nth(6).unwrap().parse::<f64>().unwrap())
        .sum();
    assert!((hours - json["totals"]["hours"].as_f64().unwrap()).abs() < 0.011);
}

#[test]
fn export_conflicts_with_an_explicit_other_format() {
    let fixture = fixture(standard);
    let message = stderr(&fixture.timesheet(&["--export", "harvest", "--format", "json"]));
    assert!(
        message.contains("--export") && message.contains("--format json"),
        "{message}"
    );
    // An explicit csv is fine.
    text(&fixture.timesheet(&["--export", "harvest", "--format", "csv"]));
}

#[test]
fn filters_narrow_the_display_and_say_what_they_left_out() {
    let fixture = fixture(standard);
    let value = json_stdout(&fixture.timesheet(&["--format", "json", "--billable-only"]));
    let entries = value["entries"].as_array().unwrap();
    assert!(entries.iter().all(|entry| entry["engagement"] == "acme"));
    assert_eq!(1, value["hidden"]["entries"]);
    assert!(value["hidden"]["seconds"].as_u64().unwrap() > 0);
    // The reconciliation still covers all the work, not the displayed part.
    assert_eq!(true, value["reconciliation"]["consistent"]);

    let value = json_stdout(&fixture.timesheet(&["--format", "json", "--engagement", "internal"]));
    assert!(
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["engagement"] == "internal")
    );

    let message = stderr(&fixture.timesheet(&["--engagement", "ghost"]));
    assert!(
        message.contains("ghost") && message.contains("acme"),
        "{message}"
    );
}

/// A path no session was in, so every session is unassigned.
fn nowhere() -> PathBuf {
    std::env::temp_dir().join("workstats-timesheet-nothing-here")
}

#[test]
fn work_matching_no_engagement_is_unassigned_and_can_be_hidden() {
    let fixture = fixture(|_, _| json!({"elsewhere": {"paths": [nowhere()]}}));
    let shown = json_stdout(&fixture.timesheet(&["--format", "json"]));
    assert!(
        shown["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["engagement"] == "(unassigned)")
    );
    assert_eq!(
        shown["reconciliation"]["unassigned_seconds"],
        shown["reconciliation"]["raw_seconds"]
    );
    let hidden = json_stdout(&fixture.timesheet(&["--format", "json", "--unassigned", "hide"]));
    assert!(hidden["entries"].as_array().unwrap().is_empty());
    assert_eq!(2, hidden["hidden"]["entries"]);
}

#[test]
fn a_bad_engagement_config_stops_the_run_naming_the_key() {
    let fixture = fixture(|_, _| json!({"acme": {"paths": ["/a"], "rate": 100}}));
    let message = stderr(&fixture.timesheet(&[]));
    assert!(message.contains("engagements.acme.currency"), "{message}");
    // A plain report is refused for the same reason: the config is one thing.
    let output = fixture.run(&[], &["--month", "2026-03", "--format", "json"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("engagements.acme"));

    let fixture = self::fixture(|_, _| json!({"a": {"paths": ["/a"]}, "b": {"paths": ["/a"]}}));
    let message = stderr(&fixture.timesheet(&[]));
    assert!(
        message.contains("\"a\"") && message.contains("\"b\""),
        "{message}"
    );
}

#[test]
fn rounding_and_the_cap_are_visible_in_the_result() {
    let fixture = fixture(standard);
    let up = json_stdout(&fixture.timesheet(&[
        "--format",
        "json",
        "--rounding",
        "up",
        "--increment",
        "1h",
    ]));
    for entry in up["entries"].as_array().unwrap() {
        assert_eq!(0, entry["final_seconds"].as_u64().unwrap() % 3600);
    }
    let capped = json_stdout(&fixture.timesheet(&[
        "--format",
        "json",
        "--increment",
        "30m",
        "--daily-cap",
        "30m",
    ]));
    for entry in capped["entries"].as_array().unwrap() {
        assert!(entry["final_seconds"].as_u64().unwrap() <= 1800);
    }
    let message = stderr(&fixture.timesheet(&["--increment", "20m", "--daily-cap", "1h30m"]));
    assert!(message.contains("multiple of the increment"), "{message}");
}

#[test]
fn a_hostile_engagement_label_cannot_break_the_csv() {
    let fixture = fixture(|acme, _| {
        json!({"acme": {
            "label": "=HYPERLINK(\"x\")", "client": "+evil", "paths": [acme],
            "export": {"project": "@sum"}
        }})
    });
    let csv = text(&fixture.timesheet(&["--export", "toggl"]));
    assert!(csv.contains("'@sum"), "{csv}");
    assert!(csv.contains("'+evil"), "{csv}");
    assert!(!csv.lines().any(|line| line.starts_with('=')));
    let generic = text(&fixture.timesheet(&["--format", "csv"]));
    assert!(generic.contains("'=HYPERLINK"), "{generic}");
}

#[test]
fn goal_warnings_are_not_reported_as_trouble_reading_history() {
    let fixture = fixture(standard);
    // A cap on weekly hours far below the fixture's, so a goal warning would
    // be raised if goals were evaluated at all.
    let mut config: Value = serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    config["goals"] = json!({"max_weekly_hours": 0.001});
    fs::write(&fixture.config, config.to_string()).unwrap();

    // The ordinary report does warn: the setup is real.
    let report = fixture.run(&[], &["--month", "2026-03", "--format", "json"]);
    let report = json_stdout(&report);
    assert!(
        report["diagnostics"]["messages"]
            .to_string()
            .contains("max_weekly_hours"),
        "{}",
        report["diagnostics"]
    );

    let output = fixture.timesheet(&[]);
    let printed = format!(
        "{}{}",
        text(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains("while reading history"), "{printed}");
    assert!(!printed.contains("max_weekly_hours"), "{printed}");
}
