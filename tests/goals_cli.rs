//! Goals in an ordinary report: the section, the JSON, the warnings, and the
//! partial-period flags, over one Pi session on a fixed March 2026 day.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::*;

/// A Pi session at noon UTC on Wednesday 2026-03-04, a half-day clear of any
/// local midnight, so it lands in ISO week 2026-W10 in every timezone.
fn session(history: &Path, cwd: &Path, model: &str, output: u64) {
    let directory = history.join("--goals-fixture--");
    fs::create_dir_all(&directory).unwrap();
    let usage = json!({
        "input": 1, "output": output, "cacheRead": 0, "cacheWrite": 0,
        "totalTokens": output + 1,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let lines = [
        json!({"type": "session", "version": 3, "id": "g1",
            "timestamp": "2026-03-04T12:00:00.000Z", "cwd": cwd}),
        json!({"type": "message", "id": "a", "parentId": null,
            "timestamp": "2026-03-04T12:00:10.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}),
        json!({"type": "message", "id": "b", "parentId": "a",
            "timestamp": "2026-03-04T12:01:10.000Z",
            "message": {"role": "assistant", "model": model, "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "done"}]}}),
    ];
    fs::write(
        directory.join("g1.jsonl"),
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(goals: &Value) -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().to_path_buf();
        fs::create_dir_all(root.join("project")).unwrap();
        session(
            &root.join("pi-sessions"),
            &root.join("project"),
            "claude-opus-5",
            1_000_000,
        );
        fs::write(
            root.join("config.json"),
            json!({"goals": goals}).to_string(),
        )
        .unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn report(&self, extra: &[&str]) -> Output {
        let history = format!("pi={}", self.root.join("pi-sessions").display());
        let config = self.root.join("config.json");
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
        ];
        arguments.extend(extra);
        run(&arguments)
    }

    fn json(&self, extra: &[&str]) -> Value {
        let mut arguments = vec!["--format", "json"];
        arguments.extend(extra);
        json_stdout(&self.report(&arguments))
    }
}

#[test]
fn a_week_report_carries_the_weekly_hours_and_a_cap_warning() {
    let fixture = Fixture::new(&json!({
        "weekly_hours": 30, "max_weekly_hours": 40, "daily_hours": 6,
        "list_value_caps": [{"pool": "claude", "period": "week", "usd": 0.5}]
    }));
    let report = fixture.json(&["--week", "2026-W10"]);
    let goals = &report["goals"];
    assert_eq!(30.0, goals["weekly_hours"]);
    let weeks = goals["weeks"].as_array().unwrap();
    assert_eq!(1, weeks.len());
    assert_eq!("2026-W10", weeks[0]["week"]);
    assert!(weeks[0]["hours"].as_f64().unwrap() > 0.0);
    assert_eq!(false, weeks[0]["partial"]);
    assert_eq!(false, weeks[0]["in_progress"]);

    let caps = goals["caps"].as_array().unwrap();
    assert_eq!(1, caps.len());
    assert_eq!("claude", caps[0]["pool"]);
    assert_eq!("2026-W10", caps[0]["label"]);
    assert_eq!("reached", caps[0]["status"]);
    assert!(caps[0]["value_usd"].as_f64().unwrap() > 0.5);

    let warnings = report["diagnostics"]["messages"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|message| message.as_str().unwrap().contains("goals: claude weekly")),
        "{warnings:?}"
    );
}

#[test]
fn a_window_that_cuts_a_period_marks_it_partial() {
    let fixture = Fixture::new(&json!({
        "list_value_caps": [{"pool": "claude", "period": "month", "usd": 100000}]
    }));
    // Three days of March: the month is cut at both ends.
    let cut = fixture.json(&["--since", "2026-03-03", "--until", "2026-03-05"]);
    let caps = cut["goals"]["caps"].as_array().unwrap();
    assert_eq!(1, caps.len());
    assert_eq!("2026-03", caps[0]["label"]);
    assert_eq!(true, caps[0]["partial"]);
    assert_eq!("ok", caps[0]["status"]);

    // The whole month is not.
    let whole = fixture.json(&["--month", "2026-03"]);
    assert_eq!(false, whole["goals"]["caps"][0]["partial"]);

    // A window across two months reports both.
    let two = fixture.json(&["--since", "2026-02-15", "--until", "2026-03-10"]);
    let labels: Vec<_> = two["goals"]["caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cap| cap["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(vec!["2026-02", "2026-03"], labels);
}

#[test]
fn no_goals_removes_the_section_and_the_warnings() {
    let fixture = Fixture::new(&json!({
        "weekly_hours": 1,
        "list_value_caps": [{"pool": "claude", "period": "month", "usd": 0.01}]
    }));
    let with = fixture.json(&["--month", "2026-03"]);
    assert!(with.get("goals").is_some());
    let without = fixture.json(&["--month", "2026-03", "--no-goals"]);
    assert!(without.get("goals").is_none());
    let messages = without["diagnostics"]["messages"].as_array().unwrap();
    assert!(
        !messages
            .iter()
            .any(|message| message.as_str().unwrap().contains("goals:")),
        "{messages:?}"
    );
}

#[test]
fn no_goal_configured_leaves_the_report_unchanged() {
    let fixture = Fixture::new(&json!({}));
    let report = fixture.json(&["--month", "2026-03"]);
    assert!(report.get("goals").is_none());
}

#[test]
fn the_table_and_the_documents_show_one_goals_section() {
    let fixture = Fixture::new(&json!({
        "weekly_hours": 30,
        "list_value_caps": [{"pool": "claude", "period": "week", "usd": 0.5}]
    }));
    let table = fixture.report(&["--week", "2026-W10"]);
    let text = String::from_utf8_lossy(&table.stdout).into_owned();
    assert_eq!(1, text.matches("\nGoals\n").count(), "{text}");
    assert!(text.contains("2026-W10:"), "{text}");
    assert!(text.contains("cap reached"), "{text}");

    let markdown = fixture.report(&["--week", "2026-W10", "--format", "markdown"]);
    let text = String::from_utf8_lossy(&markdown.stdout).into_owned();
    assert_eq!(1, text.matches("Goals").count(), "{text}");

    let html = fixture.report(&["--week", "2026-W10", "--format", "html"]);
    let text = String::from_utf8_lossy(&html.stdout).into_owned();
    assert!(text.contains(">Goals<"), "{text}");

    let hidden = fixture.report(&["--week", "2026-W10", "--no-goals"]);
    assert!(!String::from_utf8_lossy(&hidden.stdout).contains("\nGoals\n"));
}

#[test]
fn a_bad_goals_block_stops_the_run_with_the_entry_named() {
    let fixture = Fixture::new(&json!({
        "list_value_caps": [{"pool": "mistral", "period": "week", "usd": 5}]
    }));
    let output = fixture.report(&["--format", "json"]);
    assert_eq!(Some(2), output.status.code());
    let error = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(error.contains("invalid \"goals\" configuration"), "{error}");
    assert!(error.contains("list_value_caps[0].pool"), "{error}");
    // Turning goals off skips the check, like any other unused setting.
    assert!(
        fixture
            .report(&["--format", "json", "--no-goals"])
            .status
            .success()
    );
}
