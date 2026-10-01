//! Branch names a provider recorded: they reach the report's `branch` grouping, and
//! `workstats record --branch` writes one into the open events format.

use std::collections::BTreeMap;
use std::fs;

use serde_json::Value;
use tempfile::tempdir;

mod common;
use common::*;

/// Report arguments that read only `history` for `provider`, grouped by branch.
fn branch_report(provider: &str, history: &str, extra: &[&str], config: &str) -> Value {
    let mut arguments = vec![
        "--no-git",
        "--no-cache",
        "--no-progress",
        "--no-default-events",
        "--no-update-check",
        "--provider",
        provider,
        "--config",
        config,
        "--month",
        "2026-03",
        "--format",
        "json",
        "--group-by",
        "branch",
    ];
    arguments.extend(extra);
    arguments.push("--history");
    arguments.push(history);
    json_stdout(&run(&arguments))
}

/// Human signals (prompts and session edges) per branch key in a `--group-by branch`
/// report; what matters to these tests is which branches appear and that each has some.
fn prompts_by_branch(report: &Value) -> BTreeMap<String, u64> {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["key"]["branch"].as_str().unwrap().to_string(),
                row["human_signal_count"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_claude_session_that_changes_branch_is_split_at_the_change() {
    let directory = tempdir().unwrap();
    let work = directory.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let history = directory.path().join("claude");
    fs::create_dir_all(history.join("project")).unwrap();
    let line = |kind: &str, time: &str, branch: &str| {
        serde_json::json!({
            "type": kind, "timestamp": format!("2026-03-02T{time}Z"), "cwd": work,
            "sessionId": "c1", "gitBranch": branch,
            "message": {"model": "claude-x", "content": "go"}
        })
        .to_string()
    };
    fs::write(
        history.join("project/session.jsonl"),
        [
            line("user", "09:00:00", "main"),
            line("assistant", "09:01:00", "main"),
            line("user", "09:05:00", "feat/branch-capture"),
            line("assistant", "09:06:00", "feat/branch-capture"),
        ]
        .join("\n"),
    )
    .unwrap();
    let config = directory.path().join("missing.json");
    let history = format!("claude={}", history.display());
    let report = branch_report("claude", &history, &[], config.to_str().unwrap());
    let branches = prompts_by_branch(&report);
    assert_eq!(
        vec!["feat/branch-capture", "main"],
        branches.keys().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(branches.values().all(|count| *count > 0));
}

#[test]
fn a_recorded_branch_reaches_the_report_and_a_hostile_one_is_refused() {
    let directory = tempdir().unwrap();
    let events = directory.path().join("events.jsonl");
    let record = |branch: &str, time: &str| {
        run(&[
            "record",
            "--provider",
            "cursor",
            "--session",
            "task-one",
            "--cwd",
            directory.path().to_str().unwrap(),
            "--kind",
            "prompt",
            "--timestamp",
            time,
            "--branch",
            branch,
            "--output",
            events.to_str().unwrap(),
        ])
    };
    assert!(record("main", "2026-03-02T09:00:00Z").status.success());
    assert!(
        record("feat/recorded", "2026-03-02T09:10:00Z")
            .status
            .success()
    );

    let refused = record("bad\u{1b}[2Jname", "2026-03-02T09:20:00Z");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--branch"));
    let refused = record(&"b".repeat(257), "2026-03-02T09:20:00Z");
    assert!(!refused.status.success());
    // Neither refused call wrote a line.
    assert_eq!(2, fs::read_to_string(&events).unwrap().lines().count());

    // A record without --branch has no `branch` key at all.
    let plain = run(&[
        "record",
        "--provider",
        "cursor",
        "--session",
        "task-one",
        "--cwd",
        directory.path().to_str().unwrap(),
        "--output",
        "-",
    ]);
    let line: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(line.get("branch").is_none());

    let config = directory.path().join("missing.json");
    let report = branch_report(
        "cursor",
        &format!("events={}", events.display()),
        &["--events", events.to_str().unwrap()],
        config.to_str().unwrap(),
    );
    let branches = prompts_by_branch(&report);
    assert_eq!(
        vec!["feat/recorded", "main"],
        branches.keys().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(branches.values().all(|count| *count > 0));
}
