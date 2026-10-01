//! The foundation the batch of new commands stands on: the new grouping
//! dimensions, the optional per-day figures, and the command surface.

use std::fs;
use std::path::Path;

use serde_json::Value;
use tempfile::tempdir;

mod common;
use common::*;

/// A report over one Pi session in March 2026, with nothing else read.
fn pi_report(directory: &Path, extra: &[&str]) -> Value {
    let project = directory.join("project");
    fs::create_dir_all(&project).unwrap();
    let history = directory.join("pi-sessions");
    pi_session(&history, "s1", &project, "claude-opus-5", 10);
    let history = format!("pi={}", history.display());
    let config = directory.join("missing-config.json");
    let mut arguments = vec![
        "--no-git",
        "--no-cache",
        "--no-progress",
        "--no-default-events",
        "--no-update-check",
        "--provider",
        "pi",
        "--history",
        history.as_str(),
        "--config",
        config.to_str().unwrap(),
        "--month",
        "2026-03",
        "--format",
        "json",
    ];
    arguments.extend(extra);
    let output = run(&arguments);
    json_stdout(&output)
}

#[test]
fn the_attribution_dimensions_group_and_fall_back_to_their_placeholders() {
    let directory = tempdir().unwrap();
    let report = pi_report(
        directory.path(),
        &["--group-by", "branch,issue,feature,engagement"],
    );
    let rows = report["rows"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        // No provider recorded a branch and nothing is configured.
        assert_eq!("—", row["key"]["branch"]);
        assert_eq!("—", row["key"]["issue"]);
        assert_eq!("—", row["key"]["feature"]);
        assert_eq!("(unassigned)", row["key"]["engagement"]);
    }
}

#[test]
fn a_report_carries_daily_figures_only_when_asked() {
    let directory = tempdir().unwrap();
    let plain = pi_report(directory.path(), &[]);
    assert!(plain.get("daily").is_none());
    assert!(plain.get("goals").is_none());

    let daily = pi_report(directory.path(), &["--daily"]);
    let days = daily["daily"].as_array().expect("--daily adds the figures");
    assert_eq!(1, days.len());
    assert_eq!("2026-03-02", days[0]["date"]);
    assert_eq!(1, days[0]["prompts"]);
    // Everything else in the report is what it was without the flag.
    let mut without = daily.clone();
    without.as_object_mut().unwrap().remove("daily");
    assert_eq!(plain, without);
}

#[test]
fn every_new_command_is_wired_into_the_command_line() {
    for command in [
        &["timesheet"][..],
        &["timesheet", "add"],
        &["timesheet", "set"],
        &["timesheet", "unset"],
        &["timesheet", "rm"],
        &["timesheet", "entries"],
        &["timesheet", "lock"],
        &["timesheet", "unlock"],
        &["timesheet", "locks"],
        &["branch"],
        &["pr"],
        &["insights"],
        &["digest"],
        &["now"],
        &["export"],
        &["merge"],
        &["calendar"],
    ] {
        let mut arguments = command.to_vec();
        arguments.push("--help");
        let output = run(&arguments);
        assert!(
            output.status.success(),
            "{command:?} --help: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let help = run(&["--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in ["--import", "--daily", "--no-goals"] {
        assert!(help.contains(flag), "{flag} is missing from --help");
    }
}
