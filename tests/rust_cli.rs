use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_workstats")
}

fn run(arguments: &[&str]) -> Output {
    Command::new(binary()).args(arguments).output().unwrap()
}

#[test]
fn native_cli_reports_version_and_rejects_conflicting_calendar_dimensions() {
    let version = run(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("workstats 0.4.0"));

    let invalid = run(&["--no-ai", "--no-git", "--group-by", "day,month"]);
    assert_eq!(Some(2), invalid.status.code());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("alternative calendar groupings"));
}

#[test]
fn missing_inputs_still_produce_a_complete_json_report() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing");
    let output = run(&[
        "--dir",
        directory.path().to_str().unwrap(),
        "--codex-dir",
        missing.to_str().unwrap(),
        "--claude-dir",
        missing.to_str().unwrap(),
        "--no-cache",
        "--no-progress",
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.get("methodology").is_some());
    assert!(report.get("diagnostics").is_some());
    assert!(output.stderr.is_empty());
}

#[test]
fn cache_hits_then_invalidates_a_changed_transcript() {
    let directory = tempdir().unwrap();
    let project = directory.path().join("claude/project");
    fs::create_dir_all(&project).unwrap();
    let transcript = project.join("session.jsonl");
    let first = format!(
        "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{}\",\"sessionId\":\"s\",\"message\":{{\"content\":\"hello\"}}}}\n",
        project.display()
    );
    fs::write(&transcript, &first).unwrap();
    let cache = directory.path().join("cache/index.sqlite3");
    let missing = directory.path().join("missing");
    let arguments = [
        "--no-git",
        "--provider",
        "claude",
        "--claude-dir",
        project.parent().unwrap().to_str().unwrap(),
        "--codex-dir",
        missing.to_str().unwrap(),
        "--cache",
        cache.to_str().unwrap(),
        "--format",
        "json",
    ];

    let cold: Value = serde_json::from_slice(&run(&arguments).stdout).unwrap();
    assert_eq!(1, cold["diagnostics"]["cache_misses"]);
    assert_eq!(1, cold["diagnostics"]["cache_writes"]);
    let warm: Value = serde_json::from_slice(&run(&arguments).stdout).unwrap();
    assert_eq!(1, warm["diagnostics"]["cache_hits"]);
    assert_eq!(0, warm["diagnostics"]["cache_misses"]);

    let second = format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T00:01:00Z\",\"cwd\":\"{}\",\"sessionId\":\"s\",\"message\":{{\"model\":\"claude-test\"}}}}\n",
        project.display()
    );
    fs::write(&transcript, format!("{first}{second}")).unwrap();
    let changed: Value = serde_json::from_slice(&run(&arguments).stdout).unwrap();
    assert_eq!(1, changed["diagnostics"]["cache_misses"]);
    assert_eq!(1, changed["diagnostics"]["cache_writes"]);
    assert_eq!(60.0, changed["summary"]["agent_wall_seconds"]);
}
