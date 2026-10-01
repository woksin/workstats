//! The timesheet ledger end to end: manual entries, overrides and locks, with
//! every output saying the same thing. Dates of activity are read from the
//! timesheet itself rather than assumed, because the day a UTC timestamp falls
//! on depends on the timezone the suite runs in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::*;

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    ledger: PathBuf,
    sessions: PathBuf,
    acme: PathBuf,
    internal: PathBuf,
}

fn engagements(acme: &Path, internal: &Path) -> Value {
    json!({
        "acme": {
            "label": "ACME - Platform", "client": "ACME AS", "rate": 1000, "currency": "NOK",
            "paths": [acme]
        },
        "internal": {"label": "Internal", "billable": false, "paths": [internal]},
    })
}

/// Two Pi sessions in March 2026, one per engagement, on different days.
fn fixture() -> Fixture {
    let directory = tempdir().unwrap();
    let root = directory.path().to_path_buf();
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
    let fixture = Fixture {
        config: root.join("config.json"),
        ledger: root.join("timesheet.json"),
        sessions,
        acme,
        internal,
        directory,
    };
    fixture.write_config(&engagements(&fixture.acme, &fixture.internal));
    fixture
}

impl Fixture {
    fn write_config(&self, engagements: &Value) {
        fs::write(
            &self.config,
            json!({"engagements": engagements}).to_string(),
        )
        .unwrap();
    }

    /// The ledger actions read the config and ledger from the environment.
    fn command(&self) -> Command {
        let mut command = Command::new(binary());
        command
            .env("WORKSTATS_CONFIG", &self.config)
            .env("WORKSTATS_TIMESHEET", &self.ledger)
            .env(
                "WORKSTATS_CACHE",
                self.directory.path().join("index.sqlite3"),
            );
        command
    }

    fn action(&self, arguments: &[&str]) -> Output {
        let mut full = vec!["timesheet"];
        full.extend_from_slice(arguments);
        self.command().args(full).output().unwrap()
    }

    /// `timesheet` or `timesheet lock` with the flags that keep a run to the
    /// fixture's history.
    fn scan(&self, head: &[&str], extra: &[&str]) -> Output {
        let history = format!("pi={}", self.sessions.display());
        let mut arguments: Vec<&str> = vec!["timesheet"];
        arguments.extend_from_slice(head);
        arguments.extend([
            "--no-git",
            "--no-progress",
            "--no-default-events",
            "--no-update-check",
            "--provider",
            "pi",
            "--history",
            history.as_str(),
            "--config",
            self.config.to_str().unwrap(),
        ]);
        arguments.extend_from_slice(extra);
        self.command().args(arguments).output().unwrap()
    }

    fn sheet(&self, extra: &[&str]) -> Value {
        let mut arguments = vec!["--month", "2026-03", "--format", "json", "--no-cache"];
        arguments.extend_from_slice(extra);
        json_stdout(&self.scan(&[], &arguments))
    }

    fn lock(&self, extra: &[&str]) -> Output {
        let mut arguments = vec!["--no-cache"];
        arguments.extend_from_slice(extra);
        self.scan(&["lock", "2026-03"], &arguments)
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

fn failure(output: &Output) -> String {
    assert!(!output.status.success(), "expected a failure");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn entry<'a>(sheet: &'a Value, engagement: &str, date: Option<&str>) -> &'a Value {
    sheet["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["engagement"] == engagement && date.is_none_or(|date| entry["date"] == date)
        })
        .unwrap_or_else(|| panic!("no {engagement} {date:?} in {sheet}"))
}

fn seconds(entry: &Value) -> u64 {
    entry["final_seconds"].as_u64().unwrap()
}

fn total(sheet: &Value) -> u64 {
    sheet["totals"]["seconds"].as_u64().unwrap()
}

/// The day an engagement's activity landed on, whatever the timezone.
fn day_of(sheet: &Value, engagement: &str) -> String {
    entry(sheet, engagement, None)["date"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn add_set_unset_and_rm_change_the_timesheet_and_say_why() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    let acme_day = day_of(&base, "acme");
    let internal_day = day_of(&base, "internal");
    let acme_estimate = seconds(entry(&base, "acme", Some(&acme_day)));
    let internal_estimate = seconds(entry(&base, "internal", Some(&internal_day)));
    assert!(acme_estimate > 0 && internal_estimate > 0, "{base}");

    // Manual hours on top of an estimate, and on a day with no activity.
    text(&fixture.action(&["add", &acme_day, "acme", "1h", "Workshop"]));
    let added = text(&fixture.action(&[
        "add",
        "2026-03-20",
        "acme",
        "1h30m",
        "Steering meeting",
        "--start",
        "09:00",
    ]));
    assert!(
        added.contains("Added ") && added.contains("1h30m"),
        "{added}"
    );
    let sheet = fixture.sheet(&[]);
    let with_manual = entry(&sheet, "acme", Some(&acme_day));
    assert_eq!(acme_estimate + 3600, seconds(with_manual));
    assert_eq!(3600, with_manual["manual_seconds"]);
    assert_eq!(acme_estimate, with_manual["estimated_seconds"]);
    let quiet = entry(&sheet, "acme", Some("2026-03-20"));
    assert_eq!("manual", quiet["status"]);
    assert_eq!(5400, seconds(quiet));
    // Billing follows the engagement: 1.5h at 1000/h, in its currency.
    assert_eq!(1500.0, quiet["amount"].as_f64().unwrap());
    assert_eq!("NOK", quiet["currency"]);
    assert_eq!(
        total(&base) + 3600 + 5400,
        total(&sheet),
        "totals are the sum of the entries shown"
    );
    // Manual hours are not activity: the report's human time is unchanged.
    assert_eq!(true, sheet["reconciliation"]["consistent"]);
    assert_eq!(
        base["reconciliation"]["raw_seconds"],
        sheet["reconciliation"]["raw_seconds"]
    );

    // An override replaces the estimate; manual hours still add on top.
    text(&fixture.action(&["set", &acme_day, "acme", "2h", "after review"]));
    text(&fixture.action(&["set", &internal_day, "internal", "0", "was out"]));
    let sheet = fixture.sheet(&[]);
    let overridden = entry(&sheet, "acme", Some(&acme_day));
    assert_eq!("overridden", overridden["status"]);
    assert_eq!(7200, overridden["override_seconds"]);
    assert_eq!(7200 + 3600, seconds(overridden));
    let suppressed = entry(&sheet, "internal", Some(&internal_day));
    assert_eq!(0, seconds(suppressed));
    assert_eq!("overridden", suppressed["status"]);

    // The table says so in the notes column.
    let table = text(&fixture.scan(&[], &["--month", "2026-03", "--no-cache"]));
    assert!(
        table.contains("override: after review") && table.contains("Steering meeting"),
        "{table}"
    );

    // Everything undoes.
    text(&fixture.action(&["unset", &acme_day, "acme"]));
    text(&fixture.action(&["unset", &internal_day, "internal"]));
    let listed = text(&fixture.action(&["entries"]));
    let ids: Vec<String> = serde_json::from_slice::<Value>(&fs::read(&fixture.ledger).unwrap())
        .unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(2, ids.len());
    for id in &ids {
        assert!(listed.contains(id), "{listed}");
        text(&fixture.action(&["rm", id]));
    }
    let restored = fixture.sheet(&[]);
    assert_eq!(base["entries"], restored["entries"]);
    assert_eq!(total(&base), total(&restored));
}

#[test]
fn a_manual_entry_with_its_own_billing_is_a_row_of_its_own() {
    let fixture = fixture();
    text(&fixture.action(&[
        "add",
        "2026-03-20",
        "acme",
        "1h",
        "Pro bono",
        "--non-billable",
    ]));
    let sheet = fixture.sheet(&[]);
    let row = entry(&sheet, "acme", Some("2026-03-20"));
    assert_eq!(false, row["billable"]);
    assert!(row["amount"].is_null());
    assert_eq!("manual, non-billable", row["detail"]);
}

#[test]
fn entries_validate_before_anything_is_written() {
    let fixture = fixture();
    for (arguments, expected) in [
        (vec!["add", "2026-03-20", "ghost", "1h"], "ghost"),
        (vec!["add", "2026-03-20", "acme", "0"], "more than zero"),
        (vec!["add", "2026-03-20", "acme", "30h"], "24h"),
        (vec!["add", "someday", "acme", "1h"], "YYYY-MM-DD"),
        (vec!["set", "2026-03-20", "ghost", "1h"], "ghost"),
        (vec!["unset", "2026-03-20", "acme"], "no override"),
        (vec!["rm", "deadbeef"], "no manual entry"),
    ] {
        let message = failure(&fixture.action(&arguments));
        assert!(
            message.contains(expected),
            "{arguments:?} should mention {expected:?}: {message}"
        );
    }
    let message = failure(&fixture.action(&["add", "2026-03-20", "ghost", "1h"]));
    assert!(
        message.contains("acme") && message.contains("internal"),
        "the message lists the configured engagements: {message}"
    );
    assert!(!fixture.ledger.exists(), "nothing was written");
}

#[test]
fn dates_accept_words_and_weekdays() {
    use chrono::{Datelike, Duration, Local};
    let fixture = fixture();
    let today = Local::now().date_naive();
    text(&fixture.action(&["add", "yesterday", "acme", "1h"]));
    text(&fixture.action(&["add", "today", "acme", "30m"]));
    text(&fixture.action(&["add", "mon", "acme", "15m"]));
    let listed = text(&fixture.action(&["entries"]));
    let back = (today.weekday().num_days_from_monday()) as i64;
    for date in [
        today - Duration::days(1),
        today,
        today - Duration::days(back),
    ] {
        assert!(listed.contains(&date.to_string()), "{date} in\n{listed}");
    }
}

#[test]
fn a_corrupt_ledger_is_a_hard_error_everywhere_and_is_left_alone() {
    let fixture = fixture();
    fs::write(&fixture.ledger, "{ this is not a ledger").unwrap();
    for message in [
        failure(&fixture.scan(&[], &["--month", "2026-03", "--no-cache"])),
        failure(&fixture.action(&["add", "2026-03-20", "acme", "1h"])),
        failure(&fixture.action(&["entries"])),
        failure(&fixture.action(&["locks"])),
    ] {
        assert!(
            message.contains("never ignored") && message.contains(fixture.ledger.to_str().unwrap()),
            "{message}"
        );
    }
    assert_eq!(
        "{ this is not a ledger",
        fs::read_to_string(&fixture.ledger).unwrap()
    );
}

#[test]
fn a_ledger_from_a_newer_version_is_refused_rather_than_rewritten() {
    let fixture = fixture();
    fs::write(&fixture.ledger, r#"{"version": 2, "entries": []}"#).unwrap();
    let message = failure(&fixture.action(&["add", "2026-03-20", "acme", "1h"]));
    assert!(message.contains("version 2"), "{message}");
}

#[test]
fn locking_freezes_the_period_and_shows_the_snapshot_as_locked() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    let summary = text(&fixture.lock(&[]));
    assert!(
        summary.contains("Locked 2026-03") && summary.contains("2 entries"),
        "{summary}"
    );

    let locks = text(&fixture.action(&["locks"]));
    assert!(
        locks.contains("2026-03")
            && locks.contains("2 entries")
            && locks.contains("nearest to 15m"),
        "{locks}"
    );

    // The file has the documented shape.
    let stored: Value = serde_json::from_slice(&fs::read(&fixture.ledger).unwrap()).unwrap();
    let lock = &stored["locks"][0];
    assert_eq!("2026-03", lock["period"]);
    assert_eq!(2, lock["entries"].as_array().unwrap().len());
    assert_eq!("15m", lock["settings"]["increment"]);
    assert_eq!("1h", lock["settings"]["human_idle"]);
    assert!(
        lock["settings"]["engagements_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        lock["settings"]["ledger_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(total(&base), lock["totals"]["seconds"].as_u64().unwrap());

    let sheet = fixture.sheet(&[]);
    for engagement in ["acme", "internal"] {
        let shown = entry(&sheet, engagement, None);
        assert_eq!("locked", shown["status"]);
        assert_eq!(seconds(entry(&base, engagement, None)), seconds(shown));
    }
    assert_eq!(total(&base), total(&sheet));
    assert!(sheet["drift"].as_array().unwrap().is_empty());
    assert!(
        sheet["warnings"].as_array().unwrap().is_empty(),
        "{}",
        sheet["warnings"]
    );
    let table = text(&fixture.scan(&[], &["--month", "2026-03", "--no-cache"]));
    assert!(
        table.contains("Locked") && !table.contains("DRIFT"),
        "{table}"
    );
}

#[test]
fn a_lock_survives_a_cache_rebuild_and_no_cache_leaves_the_ledger_alone() {
    let fixture = fixture();
    text(&fixture.action(&["add", "2026-03-20", "acme", "1h"]));
    text(&fixture.lock(&[]));
    let before = fs::read(&fixture.ledger).unwrap();
    // Neither flag touches the ledger; the second run also uses the cache.
    text(&fixture.scan(&[], &["--month", "2026-03", "--rebuild-cache"]));
    text(&fixture.scan(&[], &["--month", "2026-03", "--no-cache"]));
    assert_eq!(before, fs::read(&fixture.ledger).unwrap());
    let sheet = fixture.sheet(&[]);
    assert_eq!(
        "locked",
        entry(&sheet, "acme", Some("2026-03-20"))["status"]
    );
}

#[test]
fn writes_into_a_locked_day_are_refused_unless_forced_and_show_as_drift() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    let acme_day = day_of(&base, "acme");
    text(&fixture.lock(&[]));

    for arguments in [
        vec!["add", &acme_day, "acme", "1h"],
        vec!["set", &acme_day, "acme", "2h"],
    ] {
        let message = failure(&fixture.action(&arguments));
        assert!(
            message.contains("locked period 2026-03") && message.contains("--force"),
            "{message}"
        );
    }
    assert_eq!(1, stored(&fixture)["locks"].as_array().unwrap().len());
    assert!(stored(&fixture)["entries"].as_array().unwrap().is_empty());

    let forced = text(&fixture.action(&["add", &acme_day, "acme", "1h", "--force"]));
    assert!(forced.contains("Forced into the locked period"), "{forced}");
    assert_eq!(
        1,
        stored(&fixture)["forced_writes"].as_array().unwrap().len()
    );

    let sheet = fixture.sheet(&[]);
    // The submitted figure is still what is shown...
    let shown = entry(&sheet, "acme", Some(&acme_day));
    assert_eq!("locked", shown["status"]);
    assert_eq!(
        seconds(entry(&base, "acme", Some(&acme_day))),
        seconds(shown)
    );
    // ...and the forced write is the drift.
    let drift = sheet["drift"].as_array().unwrap();
    assert_eq!(1, drift.len(), "{sheet}");
    assert_eq!("ledger edited after lock", drift[0]["cause"]);
    assert_eq!(3600, drift[0]["difference_seconds"]);
    assert_eq!(1, sheet["warnings"].as_array().unwrap().len());
    let table = text(&fixture.scan(&[], &["--month", "2026-03", "--no-cache"]));
    assert!(
        table.contains("DRIFT since locked") && table.contains("ledger edited after lock"),
        "{table}"
    );
}

fn stored(fixture: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(&fixture.ledger).unwrap()).unwrap()
}

#[test]
fn ignoring_locks_shows_the_live_computation() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    let acme_day = day_of(&base, "acme");
    text(&fixture.lock(&[]));
    text(&fixture.action(&["add", &acme_day, "acme", "1h", "--force"]));
    let live = fixture.sheet(&["--ignore-locks"]);
    let row = entry(&live, "acme", Some(&acme_day));
    assert_ne!("locked", row["status"]);
    assert_eq!(
        seconds(entry(&base, "acme", Some(&acme_day))) + 3600,
        seconds(row)
    );
    assert!(live["drift"].as_array().unwrap().is_empty());
}

#[test]
fn drift_names_a_changed_setting() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    text(&fixture.lock(&[]));
    let sheet = fixture.sheet(&["--rounding", "up", "--increment", "1h"]);
    let drift = sheet["drift"].as_array().unwrap();
    assert!(!drift.is_empty(), "{sheet}");
    for row in drift {
        let cause = row["cause"].as_str().unwrap();
        assert!(
            cause.starts_with("settings changed") && cause.contains("increment"),
            "{cause}"
        );
    }
    // What is shown is still what was locked.
    assert_eq!(total(&base), total(&sheet));
}

#[test]
fn drift_names_a_changed_engagement_configuration() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    text(&fixture.lock(&[]));
    // The internal work now bills to acme: same history, different attribution.
    fixture.write_config(&json!({
        "acme": {
            "label": "ACME - Platform", "client": "ACME AS", "rate": 1000, "currency": "NOK",
            "paths": [&fixture.acme, &fixture.internal]
        },
        "internal": {"label": "Internal", "billable": false,
                     "paths": [fixture.directory.path().join("nowhere")]},
    }));
    let sheet = fixture.sheet(&[]);
    let drift = sheet["drift"].as_array().unwrap();
    assert!(!drift.is_empty(), "{sheet}");
    assert!(
        drift
            .iter()
            .all(|row| row["cause"] == "engagement config changed"),
        "{sheet}"
    );
    assert_eq!(total(&base), total(&sheet), "the locked figures are shown");
}

#[test]
fn drift_names_new_history() {
    let fixture = fixture();
    text(&fixture.lock(&[]));
    // More work in March, after the lock.
    pi_session_on(
        &fixture.sessions,
        "a2",
        &fixture.acme,
        "claude-opus-5",
        10,
        "2026-03-16",
    );
    let sheet = fixture.sheet(&[]);
    let drift = sheet["drift"].as_array().unwrap();
    assert_eq!(1, drift.len(), "{sheet}");
    assert_eq!("new or pruned history", drift[0]["cause"]);
    assert_eq!(0, drift[0]["locked_seconds"]);
    assert!(drift[0]["current_seconds"].as_u64().unwrap() > 0);
}

#[test]
fn locking_twice_needs_force_and_overlaps_are_refused() {
    let fixture = fixture();
    text(&fixture.lock(&[]));
    let message = failure(&fixture.lock(&[]));
    assert!(
        message.contains("already locked") && message.contains("--force"),
        "{message}"
    );
    text(&fixture.lock(&["--force"]));
    assert_eq!(1, stored(&fixture)["locks"].as_array().unwrap().len());

    let message = failure(&fixture.scan(&["lock", "2026-W10"], &["--no-cache"]));
    assert!(message.contains("overlaps the lock 2026-03"), "{message}");

    // Window flags and filters do not combine with a period.
    let message = failure(&fixture.scan(&["lock", "2026-04"], &["--month", "2026-04"]));
    assert!(message.contains("PERIOD"), "{message}");
    let message = failure(&fixture.scan(&["lock", "2026-04"], &["--engagement", "acme"]));
    assert!(message.contains("--engagement"), "{message}");
    let message = failure(&fixture.scan(&["lock", "last"], &[]));
    assert!(message.contains("invalid period"), "{message}");
}

#[test]
fn unlocking_makes_the_days_live_and_writable_again() {
    let fixture = fixture();
    let base = fixture.sheet(&[]);
    let acme_day = day_of(&base, "acme");
    text(&fixture.lock(&[]));
    let message = failure(&fixture.action(&["unlock", "2026-04"]));
    assert!(message.contains("no lock for 2026-04"), "{message}");
    text(&fixture.action(&["unlock", "2026-03"]));
    assert!(text(&fixture.action(&["locks"])).contains("No locks"));
    text(&fixture.action(&["add", &acme_day, "acme", "1h"]));
    let sheet = fixture.sheet(&[]);
    assert_ne!("locked", entry(&sheet, "acme", Some(&acme_day))["status"]);
}

#[test]
fn a_week_and_a_range_can_be_locked_too() {
    let fixture = fixture();
    let sheet = fixture.sheet(&[]);
    let acme_day = day_of(&sheet, "acme");
    let summary = text(&fixture.scan(
        &["lock", &format!("{acme_day}..{acme_day}")],
        &["--no-cache"],
    ));
    assert!(
        summary.contains(&format!("{acme_day}..{acme_day}")),
        "{summary}"
    );
    // Only that day is frozen; the other engagement's day is still live.
    let sheet = fixture.sheet(&[]);
    assert_eq!("locked", entry(&sheet, "acme", Some(&acme_day))["status"]);
    let internal = entry(&sheet, "internal", None);
    if internal["date"] != acme_day.as_str() {
        assert_ne!("locked", internal["status"]);
    }
}

#[test]
fn locked_entries_reach_the_csv_with_their_status() {
    let fixture = fixture();
    text(&fixture.lock(&[]));
    let csv = text(&fixture.scan(
        &[],
        &["--month", "2026-03", "--no-cache", "--format", "csv"],
    ));
    let header = csv.lines().next().unwrap();
    let status = header.split(',').position(|name| name == "status").unwrap();
    for row in csv.lines().skip(1) {
        assert_eq!("locked", row.split(',').nth(status).unwrap(), "{row}");
    }
}

#[test]
fn entries_filter_by_window_and_mark_locked_days() {
    let fixture = fixture();
    text(&fixture.action(&["add", "2026-02-10", "acme", "1h", "February"]));
    text(&fixture.action(&["add", "2026-03-20", "acme", "1h", "March"]));
    text(&fixture.action(&["set", "2026-03-21", "internal", "0", "Out"]));
    text(&fixture.lock(&[]));
    let all = text(&fixture.action(&["entries"]));
    assert!(
        all.contains("February") && all.contains("March") && all.contains("Out"),
        "{all}"
    );
    assert!(all.contains("locked 2026-03"), "{all}");
    let march = text(&fixture.action(&["entries", "--month", "2026-03"]));
    assert!(
        march.contains("March") && !march.contains("February"),
        "{march}"
    );
    assert!(march.contains("not shown"), "{march}");
}
