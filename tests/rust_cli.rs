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

fn git(arguments: &[&str]) -> Output {
    Command::new("git").args(arguments).output().unwrap()
}

#[test]
fn native_cli_reports_version_and_rejects_conflicting_calendar_dimensions() {
    let version = run(&["--version"]);
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout)
            .contains(&format!("workstats {}", env!("CARGO_PKG_VERSION")))
    );

    let invalid = run(&["--no-ai", "--no-git", "--group-by", "day,month"]);
    assert_eq!(Some(2), invalid.status.code());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("alternative calendar groupings"));

    let help = run(&["--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--review-credit"));
    assert!(help.contains("--isolated-credit"));
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
    assert_eq!(
        3600.0,
        report["methodology"]["human_idle_threshold_seconds"]
    );
    assert_eq!(1800.0, report["methodology"]["review_credit_seconds"]);
    assert!(output.stderr.is_empty());
}

#[test]
fn cache_hits_then_invalidates_a_changed_transcript() {
    let directory = tempdir().unwrap();
    let project = directory.path().join("claude/project");
    fs::create_dir_all(&project).unwrap();
    let transcript = project.join("session.jsonl");
    let first = serde_json::json!({
        "type": "user",
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": project,
        "sessionId": "s",
        "message": {"content": "hello"}
    })
    .to_string()
        + "\n";
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

    let second = serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-01-01T00:01:00Z",
        "cwd": project,
        "sessionId": "s",
        "message": {"model": "claude-test"}
    })
    .to_string()
        + "\n";
    fs::write(&transcript, format!("{first}{second}")).unwrap();
    let changed: Value = serde_json::from_slice(&run(&arguments).stdout).unwrap();
    assert_eq!(1, changed["diagnostics"]["cache_misses"]);
    assert_eq!(1, changed["diagnostics"]["cache_writes"]);
    assert_eq!(60.0, changed["summary"]["agent_wall_seconds"]);
}

#[test]
fn sources_and_open_event_recording_form_a_complete_integration_path() {
    let sources = run(&["sources", "--format", "json"]);
    assert!(sources.status.success());
    let inventory: Value = serde_json::from_slice(&sources.stdout).unwrap();
    let ids: Vec<_> = inventory
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(ids.contains(&"gemini"));
    assert!(ids.contains(&"copilot"));
    assert!(ids.contains(&"opencode"));
    assert!(ids.contains(&"events"));

    let directory = tempdir().unwrap();
    let events = directory.path().join("events.jsonl");
    let recorded = run(&[
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
        "2026-01-01T00:00:00Z",
        "--output",
        events.to_str().unwrap(),
    ]);
    assert!(recorded.status.success());
    let line: Value = serde_json::from_slice(&fs::read(&events).unwrap()).unwrap();
    assert_eq!("cursor", line["provider"]);
    assert!(line.get("content").is_none());

    let report = run(&[
        "--no-git",
        "--provider",
        "cursor",
        "--events",
        events.to_str().unwrap(),
        "--no-cache",
        "--no-progress",
        "--format",
        "json",
    ]);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(1, report["summary"]["session_count"]);
    assert_eq!(1, report["summary"]["prompt_signal_count"]);
}

#[test]
fn filtered_ai_session_infers_its_git_checkout_outside_the_scan_directory() {
    let temporary = tempdir().unwrap();
    let project = temporary.path().join("project");
    let unrelated = temporary.path().join("unrelated");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    assert!(git(&["init", project.to_str().unwrap()]).status.success());
    fs::write(project.join("README.md"), "fixture\n").unwrap();
    assert!(
        git(&["-C", project.to_str().unwrap(), "add", "README.md"])
            .status
            .success()
    );
    assert!(
        git(&[
            "-C",
            project.to_str().unwrap(),
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-m",
            "fixture",
        ])
        .status
        .success()
    );

    let events = temporary.path().join("events.jsonl");
    let recorded = run(&[
        "record",
        "--provider",
        "fixture",
        "--session",
        "outside-scan-root",
        "--cwd",
        project.to_str().unwrap(),
        "--kind",
        "prompt",
        "--timestamp",
        "2026-01-01T00:00:00Z",
        "--output",
        events.to_str().unwrap(),
    ]);
    assert!(recorded.status.success());

    let output = run(&[
        "--dir",
        unrelated.to_str().unwrap(),
        "--author",
        "fixture@example.com",
        "--repo-exact",
        "project",
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
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
    assert_eq!(1, report["summary"]["commit_count"]);
    let expected_root = project
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        report["inputs"]["git_scan_roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root.as_str() == Some(&expected_root))
    );
}
