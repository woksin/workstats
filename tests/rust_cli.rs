use std::fs;
use std::path::Path;
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

/// Writes `body` to `file` and commits it to `repo` as `author`.
///
/// The committer is always the fixture identity; only the *author* varies,
/// because `--author` and `--agent-commits` both filter on authorship. Each
/// `message` becomes its own paragraph, which is how a `Co-authored-by:`
/// trailer is attached to a commit.
fn commit_as(repo: &str, file: &str, body: &str, author: &str, message: &[&str]) {
    let target = Path::new(repo).join(file);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, body).unwrap();
    assert!(git(&["-C", repo, "add", "."]).status.success());
    let author = format!("--author={author}");
    let mut arguments = vec![
        "-C",
        repo,
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.com",
        "commit",
        "-q",
        author.as_str(),
    ];
    for part in message {
        arguments.push("-m");
        arguments.push(part);
    }
    let output = git(&arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    assert!(help.contains("--explain-human-time"));
    assert!(help.contains("--explain-repository-attribution"));

    let overlapping = run(&[
        "--no-ai",
        "--no-git",
        "--human-idle",
        "30m",
        "--review-credit",
        "31m",
    ]);
    assert!(!overlapping.status.success());
    let error = String::from_utf8_lossy(&overlapping.stderr);
    assert!(error.contains("--review-credit"), "{error}");
    assert!(error.contains("--human-idle"), "{error}");

    let csv_explanation = run(&[
        "--no-ai",
        "--no-git",
        "--format",
        "csv",
        "--explain-human-time",
    ]);
    assert!(!csv_explanation.status.success());
    assert!(
        String::from_utf8_lossy(&csv_explanation.stderr)
            .contains("not available with --format csv")
    );

    let csv_repositories = run(&[
        "--no-ai",
        "--no-git",
        "--format",
        "csv",
        "--explain-repository-attribution",
    ]);
    assert!(!csv_repositories.status.success());
    assert!(
        String::from_utf8_lossy(&csv_repositories.stderr)
            .contains("explanation flags are not available with --format csv")
    );
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
    assert_eq!(
        "signal-blocks-v1",
        report["methodology"]["human_time_algorithm_version"]
    );
    assert!(report["methodology"]["human_time_timezone_basis"].is_string());
    assert!(report["methodology"]["human_time_boundary_basis"].is_string());
    assert!(report.get("human_time_explanation").is_none());
    assert!(output.stderr.is_empty());
}

#[test]
fn human_time_explanation_is_opt_in_structured_and_reconciled() {
    let directory = tempdir().unwrap();
    let output = run(&[
        "--dir",
        directory.path().to_str().unwrap(),
        "--no-ai",
        "--no-git",
        "--no-progress",
        "--format",
        "json",
        "--explain-human-time",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let explanation = &report["human_time_explanation"];
    assert_eq!("signal-blocks-v1", explanation["algorithm_version"]);
    assert_eq!(0, explanation["input_signals"]["count"]);
    assert_eq!(0, explanation["effective_signals"]["count"]);
    assert_eq!(0, explanation["blocks"].as_array().unwrap().len());
    assert_eq!(
        report["summary"]["human_estimated_seconds"],
        explanation["total_seconds"]
    );

    let table = run(&[
        "--dir",
        directory.path().to_str().unwrap(),
        "--no-ai",
        "--no-git",
        "--no-progress",
        "--explain-human-time",
    ]);
    assert!(table.status.success());
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(table.contains("Human-time calculation ledger"), "{table}");
    assert!(
        table.contains("prompt text, session IDs, paths, and commit hashes are never included")
    );
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

/// Pi records delegated work in its own session file, so a run that reads them has to
/// keep a subagent's activity out of the human estimate while still reporting it as agent
/// work. This exercises the whole path — discovery, `--history` override, parsing,
/// grouping — rather than the parser alone.
#[test]
fn pi_subagent_work_is_reported_as_agent_activity_and_never_as_human_time() {
    let directory = tempdir().unwrap();
    let project = directory.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let history = directory.path().join("pi-sessions/--encoded--");
    fs::create_dir_all(&history).unwrap();

    let usage = serde_json::json!({
        "input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40,
        "cacheWrite1h": 40, "totalTokens": 100,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let foreground = history.join("2026-01-01T00-00-00-000Z_foreground.jsonl");
    let lines = [
        serde_json::json!({"type": "session", "version": 3, "id": "foreground",
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": project}),
        serde_json::json!({"type": "message", "id": "a", "parentId": null,
            "timestamp": "2026-01-01T00:00:10.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "do the thing"}]}}),
        serde_json::json!({"type": "message", "id": "b", "parentId": "a",
            "timestamp": "2026-01-01T00:01:10.000Z",
            "message": {"role": "assistant", "model": "pi-test", "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "done"}]}}),
    ];
    fs::write(
        &foreground,
        lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let child = history.join("2026-01-01T00-02-00-000Z_child.jsonl");
    let child_lines = [
        serde_json::json!({"type": "session", "version": 3, "id": "child",
            "timestamp": "2026-01-01T00:02:00.000Z", "cwd": project,
            "parentSession": foreground}),
        // The delegating agent wrote this prompt, not a person.
        serde_json::json!({"type": "message", "id": "a", "parentId": null,
            "timestamp": "2026-01-01T00:02:10.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "You are investigating X."}]}}),
        serde_json::json!({"type": "message", "id": "b", "parentId": "a",
            "timestamp": "2026-01-01T00:03:10.000Z",
            "message": {"role": "assistant", "model": "pi-test", "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "reported"}]}}),
    ];
    fs::write(
        &child,
        child_lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let report: Value = serde_json::from_slice(
        &run(&[
            "--no-git",
            "--provider",
            "pi",
            "--history",
            &format!("pi={}", directory.path().join("pi-sessions").display()),
            "--format",
            "json",
        ])
        .stdout,
    )
    .unwrap();

    let summary = &report["summary"];
    assert_eq!(2, summary["session_count"]);
    assert_eq!(1, summary["foreground_session_count"]);
    assert_eq!(1, summary["subagent_session_count"]);
    // One typed prompt, from the foreground session only.
    assert_eq!(1, summary["prompt_signal_count"]);
    // Both sessions ran a minute of agent time, and they do not overlap.
    assert_eq!(120.0, summary["agent_wall_seconds"]);
    assert_eq!(200, summary["total_tokens"]);
    assert_eq!(200, summary["provider_tokens"]["pi"]);
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
    assert!(ids.contains(&"copilot-vscode"));
    assert!(ids.contains(&"opencode"));
    assert!(ids.contains(&"pi"));
    assert!(ids.contains(&"events"));

    let directory = tempdir().unwrap();
    let events = directory.path().join("events.jsonl");
    let recorded = run(&[
        "record",
        "--provider",
        "cursor",
        "--session",
        "task-one",
        "--model",
        "MODEL_SECRET",
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
    let second = run(&[
        "record",
        "--provider",
        "zed",
        "--session",
        "task-two",
        "--model",
        "SECOND_MODEL_SECRET",
        "--cwd",
        directory.path().to_str().unwrap(),
        "--kind",
        "prompt",
        "--timestamp",
        "2026-01-01T00:00:00Z",
        "--output",
        events.to_str().unwrap(),
    ]);
    assert!(second.status.success());

    let report = run(&[
        "--no-git",
        "--provider",
        "cursor,zed",
        "--events",
        events.to_str().unwrap(),
        // --events adds to the log `workstats record` writes rather than
        // replacing it, so without this the developer's own recorded sessions
        // reach these counts on a machine that has ever run `workstats record`.
        "--no-default-events",
        "--no-cache",
        "--no-progress",
        "--format",
        "json",
        "--explain-human-time",
    ]);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(2, report["summary"]["session_count"]);
    assert_eq!(2, report["summary"]["prompt_signal_count"]);
    let explanation = &report["human_time_explanation"];
    assert_eq!(4, explanation["input_signals"]["count"]);
    assert_eq!(1, explanation["effective_signals"]["count"]);
    assert_eq!(
        "prompt",
        explanation["effective_signals"]["signals"][0]["kind"]
    );
    assert_eq!(
        3,
        explanation["same_timestamp_deduplication"]["discarded_signal_count"]
    );
    assert_eq!(1, explanation["blocks"].as_array().unwrap().len());
    assert_eq!(
        report["summary"]["human_estimated_seconds"],
        explanation["total_seconds"]
    );
    let serialized = serde_json::to_string(explanation).unwrap();
    assert!(!serialized.contains("task-one"));
    assert!(!serialized.contains("task-two"));
    assert!(!serialized.contains("MODEL_SECRET"));
    assert!(!serialized.contains("SECOND_MODEL_SECRET"));
    assert!(!serialized.contains(&directory.path().to_string_lossy().into_owned()));

    let table = run(&[
        "--no-git",
        "--provider",
        "cursor,zed",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
        "--no-cache",
        "--no-progress",
        "--explain-human-time",
    ]);
    assert!(table.status.success());
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(table.contains("Equal-priority collisions: first input signal wins."));
    let decisions = table
        .split("Shared-timestamp decisions")
        .nth(1)
        .and_then(|tail| tail.split("  Blocks").next())
        .unwrap();
    assert!(decisions.contains("signal:"), "{decisions}");
    assert!(decisions.contains("cursor"), "{decisions}");
    assert!(decisions.contains("zed"), "{decisions}");
    assert!(decisions.contains("kept"), "{decisions}");
    assert!(decisions.contains("discarded"), "{decisions}");
}

#[test]
fn git_output_is_reported_by_file_area_in_json_and_csv() {
    let temporary = tempdir().unwrap();
    let project = temporary.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(project.join("tests")).unwrap();
    assert!(git(&["init", project.to_str().unwrap()]).status.success());
    fs::write(project.join("src/lib.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    fs::write(project.join("tests/lib_test.rs"), "check\n").unwrap();
    fs::write(project.join("README.md"), "docs\n").unwrap();
    let path = project.to_str().unwrap();
    assert!(git(&["-C", path, "add", "."]).status.success());
    assert!(
        git(&[
            "-C",
            path,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-m",
            "areas",
        ])
        .status
        .success()
    );

    let arguments = |format: &str| {
        vec![
            "--dir".to_string(),
            path.to_string(),
            "--author".to_string(),
            "fixture@example.com".to_string(),
            "--no-ai".to_string(),
            "--no-cache".to_string(),
            "--no-progress".to_string(),
            "--format".to_string(),
            format.to_string(),
        ]
    };
    let json = run(&arguments("json")
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>());
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report: Value = serde_json::from_slice(&json.stdout).unwrap();
    let composition = report["summary"]["composition"].as_array().unwrap();
    let area = |name: &str| {
        composition
            .iter()
            .find(|entry| entry["category"] == name)
            .unwrap_or_else(|| panic!("missing {name} in {composition:?}"))
    };
    assert_eq!(4, area("source")["additions"]);
    assert_eq!(1, area("test")["additions"]);
    assert_eq!(1, area("docs")["additions"]);
    assert_eq!(
        "new code",
        report["summary"]["change_shapes"][0]["shape"]
            .as_str()
            .unwrap()
    );

    let csv = run(&arguments("csv")
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>());
    assert!(csv.status.success());
    let csv = String::from_utf8_lossy(&csv.stdout);
    let mut lines = csv.lines();
    let header: Vec<_> = lines.next().unwrap().split(',').collect();
    let row: Vec<_> = lines.next().unwrap().split(',').collect();
    let cell = |name: &str| {
        let index = header
            .iter()
            .position(|field| *field == name)
            .unwrap_or_else(|| panic!("missing column {name} in {header:?}"));
        row[index]
    };
    assert_eq!("4", cell("source_additions"));
    assert_eq!("1", cell("test_files"));
    assert_eq!("1", cell("docs_additions"));
    // An area the row never touched still reports a numeric zero.
    assert_eq!("0", cell("assets_additions"));
}

#[test]
fn worktrees_of_one_repository_are_combined_into_one_stat_row() {
    let temporary = tempdir().unwrap();
    let primary = temporary.path().join("product-primary");
    let worktree = temporary.path().join("feature-checkout");
    assert!(
        git(&["init", "-q", primary.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        git(&[
            "-C",
            primary.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://github.com/acme/product.git",
        ])
        .status
        .success()
    );
    commit_as(
        primary.to_str().unwrap(),
        "src/lib.rs",
        "primary\n",
        "Fixture <fixture@example.com>",
        &["primary"],
    );
    assert!(
        git(&[
            "-C",
            primary.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ])
        .status
        .success()
    );
    commit_as(
        worktree.to_str().unwrap(),
        "tests/lib.rs",
        "worktree\n",
        "Fixture <fixture@example.com>",
        &["worktree"],
    );

    let output = run(&[
        "--dir",
        temporary.path().to_str().unwrap(),
        "--author",
        "fixture@example.com",
        "--no-ai",
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
    let rows = report["rows"].as_array().unwrap();

    assert_eq!(2, report["summary"]["commit_count"]);
    assert_eq!(1, rows.len(), "worktree checkouts became separate rows");
    assert_eq!("product", rows[0]["key"]["repo"]);
    assert_eq!(2, rows[0]["commit_count"]);
}

#[test]
fn configured_project_alias_combines_distinct_git_repositories() {
    let temporary = tempdir().unwrap();
    let api = temporary.path().join("api");
    let web = temporary.path().join("web");
    for (checkout, remote, file) in [
        (&api, "https://github.com/acme/api.git", "api.rs"),
        (&web, "git@github.com:acme/web.git", "web.rs"),
    ] {
        assert!(
            git(&["init", "-q", checkout.to_str().unwrap()])
                .status
                .success()
        );
        assert!(
            git(&[
                "-C",
                checkout.to_str().unwrap(),
                "remote",
                "add",
                "origin",
                remote,
            ])
            .status
            .success()
        );
        commit_as(
            checkout.to_str().unwrap(),
            file,
            "one\n",
            "Fixture <fixture@example.com>",
            &["fixture"],
        );
    }
    let config = temporary.path().join("config.json");
    fs::write(
        &config,
        r#"{"project_aliases":{"acme":{"label":"Acme Product","remotes":["https://github.com/acme/api.git","https://github.com/acme/web.git"]}}}"#,
    )
    .unwrap();
    let base = [
        "--dir",
        temporary.path().to_str().unwrap(),
        "--author",
        "fixture@example.com",
        "--config",
        config.to_str().unwrap(),
        "--no-ai",
        "--no-progress",
        "--format",
        "json",
    ];

    let output = run(&base);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(1, report["rows"].as_array().unwrap().len());
    assert_eq!("Acme Product", report["rows"][0]["key"]["repo"]);
    assert_eq!(2, report["rows"][0]["commit_count"]);

    let mut by_cwd = base.to_vec();
    by_cwd.splice(0..0, ["--group-by", "cwd"]);
    let output = run(&by_cwd);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(2, report["rows"].as_array().unwrap().len());
}

#[test]
fn repository_history_recovers_a_deleted_foreground_worktree() {
    let temporary = tempdir().unwrap();
    let primary = temporary.path().join("primary");
    let worktree = temporary.path().join("foreground-worktree");
    let unrelated = temporary.path().join("unrelated");
    fs::create_dir_all(&unrelated).unwrap();
    assert!(
        git(&["init", "-q", primary.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        git(&[
            "-C",
            primary.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://github.com/acme/product.git",
        ])
        .status
        .success()
    );
    commit_as(
        primary.to_str().unwrap(),
        "README.md",
        "one\n",
        "Fixture <fixture@example.com>",
        &["fixture"],
    );
    assert!(
        git(&[
            "-C",
            primary.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "foreground",
            worktree.to_str().unwrap(),
        ])
        .status
        .success()
    );
    let events = temporary.path().join("events.jsonl");
    assert!(
        run(&[
            "record",
            "--provider",
            "fixture",
            "--session",
            "foreground",
            "--cwd",
            worktree.to_str().unwrap(),
            "--kind",
            "prompt",
            "--timestamp",
            "2026-01-01T00:00:00Z",
            "--output",
            events.to_str().unwrap(),
        ])
        .status
        .success()
    );
    let cache = temporary.path().join("cache.sqlite3");
    let seed = run(&[
        "--dir",
        temporary.path().to_str().unwrap(),
        "--author",
        "fixture@example.com",
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
        "--cache",
        cache.to_str().unwrap(),
        "--no-progress",
        "--format",
        "json",
    ]);
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    assert!(
        git(&[
            "-C",
            primary.to_str().unwrap(),
            "worktree",
            "remove",
            "--force",
            worktree.to_str().unwrap(),
        ])
        .status
        .success()
    );

    let recovered = run(&[
        "--dir",
        unrelated.to_str().unwrap(),
        "--no-git",
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
        "--cache",
        cache.to_str().unwrap(),
        "--no-progress",
        "--format",
        "json",
        "--explain-repository-attribution",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let report: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!("product", report["rows"][0]["key"]["repo"]);
    assert_eq!(1, report["repository_attribution"]["history_hits"]);
    let explanation = serde_json::to_string(&report["repository_attribution"]).unwrap();
    assert!(!explanation.contains(temporary.path().to_str().unwrap()));

    let unresolved = run(&[
        "--dir",
        unrelated.to_str().unwrap(),
        "--no-git",
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
        "--no-cache",
        "--no-progress",
        "--format",
        "json",
    ]);
    let report: Value = serde_json::from_slice(&unresolved.stdout).unwrap();
    assert_eq!("foreground-worktree", report["rows"][0]["key"]["repo"]);
}

#[test]
fn a_filter_matching_only_a_nested_session_directory_still_finds_the_commits() {
    // `--repo api` matches the session's own working directory, which is deep
    // inside the checkout. The repository is described by its root, which does
    // not contain "api" — so re-applying the filter to the inferred root used
    // to return the session with none of its commits.
    let temporary = tempdir().unwrap();
    let project = temporary.path().join("widget");
    let nested = project.join("packages/api");
    let unrelated = temporary.path().join("elsewhere");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    assert!(git(&["init", project.to_str().unwrap()]).status.success());
    fs::write(nested.join("service.rs"), "one\ntwo\n").unwrap();
    let path = project.to_str().unwrap();
    assert!(git(&["-C", path, "add", "."]).status.success());
    assert!(
        git(&[
            "-C",
            path,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-m",
            "service",
        ])
        .status
        .success()
    );

    let events = temporary.path().join("events.jsonl");
    assert!(
        run(&[
            "record",
            "--provider",
            "fixture",
            "--session",
            "nested",
            "--cwd",
            nested.to_str().unwrap(),
            "--kind",
            "prompt",
            "--timestamp",
            "2026-01-01T00:00:00Z",
            "--output",
            events.to_str().unwrap(),
        ])
        .status
        .success()
    );

    let output = run(&[
        "--dir",
        unrelated.to_str().unwrap(),
        "--author",
        "fixture@example.com",
        "--repo",
        "api",
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
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
    assert_eq!(1, report["summary"]["session_count"]);
    assert_eq!(1, report["summary"]["commit_count"], "commits were dropped");
    assert_eq!(2, report["summary"]["additions"]);
}

#[test]
fn ai_session_infers_its_git_checkout_outside_the_scan_directory_without_a_repo_filter() {
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
        "--provider",
        "fixture",
        "--events",
        events.to_str().unwrap(),
        "--no-default-events",
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
    assert_eq!(1, report["summary"]["session_count"]);
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

/// The invariant the whole agent-authorship feature rests on, driven end to end
/// through the real binary and a real `git log` rather than through the model.
///
/// A coding agent's commits are landed output and *zero* evidence that anybody
/// was at the keyboard. The unit tests pin that on `GitCommit::human_signal`;
/// what only a fixture can check is that the second `git log` pass finds the
/// agent by the identity it actually commits under, keeps its work out of the
/// figures `--author` promises are the developer's, and reports the split at
/// the JSON boundary other tools read.
#[test]
fn agent_authored_commits_are_reported_as_output_and_never_as_human_time() {
    let temporary = tempdir().unwrap();
    let project = temporary.path().join("api");
    fs::create_dir_all(&project).unwrap();
    assert!(git(&["init", project.to_str().unwrap()]).status.success());
    let path = project.to_str().unwrap();

    // The numeric prefix is the point of the fixture: GitHub has issued more
    // than one for the same Copilot account, so an identity keyed on the number
    // finds some of an agent's work and silently misses the rest. Only the
    // address suffix is matched, and this address carries a prefix that is not
    // in any list in the source.
    const AGENT: &str = "Copilot <5551212+Copilot@users.noreply.github.com>";
    const HUMAN: &str = "Fixture <fixture@example.com>";

    commit_as(path, "src/lib.rs", "one\ntwo\n", HUMAN, &["mine"]);
    commit_as(
        path,
        "src/lib.rs",
        "one\ntwo\nthree\n",
        HUMAN,
        &[
            "mine, with help",
            "Co-authored-by: Copilot <5551212+Copilot@users.noreply.github.com>",
        ],
    );
    commit_as(
        path,
        "src/agent.rs",
        "a\nb\nc\nd\ne\nf\n",
        AGENT,
        &["Initial plan"],
    );
    commit_as(
        path,
        "src/agent.rs",
        "a\nb\nc\nd\ne\nf\ng\nh\n",
        AGENT,
        &["Address review feedback"],
    );

    let report = |author: &str, extra: &[&str]| -> Value {
        let mut arguments = vec![
            "--dir",
            path,
            "--author",
            author,
            "--no-ai",
            "--no-cache",
            "--no-progress",
            "--format",
            "json",
        ];
        arguments.extend_from_slice(extra);
        let output = run(&arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let seconds = |report: &Value, field: &str| -> f64 {
        report["summary"][field]
            .as_f64()
            .unwrap_or_else(|| panic!("{field} is not a number in {:?}", report["summary"]))
    };

    // Nothing happens until the run asks for it, and asking for it must not
    // change a single number the report already produced.
    let quiet = report("fixture@example.com", &[]);
    assert_eq!(2, quiet["summary"]["commit_count"]);
    assert_eq!(3, quiet["summary"]["additions"]);
    assert_eq!(0, quiet["summary"]["agent_commit_count"]);
    assert_eq!(0, quiet["summary"]["ai_assisted_commit_count"]);

    let both = report("fixture@example.com", &["--agent-commits", "--co-authors"]);
    assert_eq!(
        2, both["summary"]["commit_count"],
        "the agent's commits are not the developer's"
    );
    assert_eq!(3, both["summary"]["additions"], "nor are the agent's lines");
    assert_eq!(
        seconds(&quiet, "human_estimated_seconds"),
        seconds(&both, "human_estimated_seconds"),
        "the second pass moved the estimate"
    );
    assert_eq!(
        quiet["summary"]["work_block_count"],
        both["summary"]["work_block_count"]
    );
    assert_eq!(
        quiet["summary"]["human_active_days"],
        both["summary"]["human_active_days"]
    );

    // It is still output, and still visible.
    assert_eq!(2, both["summary"]["agent_commit_count"]);
    assert_eq!(8, both["summary"]["agent_additions"]);
    assert_eq!(0, both["summary"]["agent_deletions"]);
    assert_eq!(2, both["rows"][0]["agent_commit_count"]);
    assert!(
        !both["inputs"]["agent_authors"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a report is only reproducible if it says which identities it matched"
    );
    // A trailer describes a commit already counted above; it never adds one.
    assert_eq!(1, both["summary"]["ai_assisted_commit_count"]);
    assert_eq!(0, both["summary"]["autofix_assisted_commit_count"]);

    // The failure mode this feature exists to prevent: a history in which the
    // configured author wrote nothing at all and an agent wrote everything.
    // Real machines hold repositories exactly like this.
    let agent_only = report("nobody@example.com", &["--agent-commits"]);
    assert_eq!(0.0, seconds(&agent_only, "human_estimated_seconds"));
    assert_eq!(0, agent_only["summary"]["work_block_count"]);
    assert_eq!(0, agent_only["summary"]["human_active_days"]);
    assert_eq!(0, agent_only["summary"]["human_signal_count"]);
    assert_eq!(0, agent_only["summary"]["commit_signal_count"]);
    assert_eq!(0, agent_only["summary"]["commit_count"]);
    assert_eq!(0, agent_only["summary"]["additions"]);
    assert_eq!(2, agent_only["summary"]["agent_commit_count"]);
    assert_eq!(8, agent_only["summary"]["agent_additions"]);
    // Calendar coverage is the one thing it does contribute: the day an agent
    // landed code is a day this repository saw work, just not a human's.
    assert_eq!(1, agent_only["summary"]["active_days"]);
    assert_eq!(2, agent_only["rows"][0]["agent_commit_count"]);
    assert_eq!(0.0, agent_only["rows"][0]["human_estimated_seconds"]);

    // `--agent-commits=REGEX` replaces the built-in identities rather than
    // joining them, so a pattern naming nobody finds nobody — the author scan
    // is untouched either way.
    let narrowed = report(
        "fixture@example.com",
        &["--agent-commits=someone-else@example.com"],
    );
    assert_eq!(0, narrowed["summary"]["agent_commit_count"]);
    assert_eq!(2, narrowed["summary"]["commit_count"]);
}

/// Writes one pi session under `history` that ran `model` in `cwd` and produced
/// `output` output tokens, so an allocation fixture can be described in the
/// terms allocation actually splits on.
fn pi_session(history: &Path, name: &str, cwd: &Path, model: &str, output: u64) {
    pi_session_on(history, name, cwd, model, output, "2026-03-02");
}

/// The same session on another day, for tests that need history in two windows.
fn pi_session_on(history: &Path, name: &str, cwd: &Path, model: &str, output: u64, day: &str) {
    let directory = history.join(format!("--{name}--"));
    fs::create_dir_all(&directory).unwrap();
    let usage = serde_json::json!({
        "input": 1, "output": output, "cacheRead": 0, "cacheWrite": 0,
        "totalTokens": output + 1,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let lines = [
        serde_json::json!({"type": "session", "version": 3, "id": name,
            "timestamp": format!("{day}T00:00:00.000Z"), "cwd": cwd}),
        serde_json::json!({"type": "message", "id": "a", "parentId": null,
            "timestamp": format!("{day}T00:00:10.000Z"),
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}),
        serde_json::json!({"type": "message", "id": "b", "parentId": "a",
            "timestamp": format!("{day}T00:01:10.000Z"),
            "message": {"role": "assistant", "model": model, "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "done"}]}}),
    ];
    fs::write(
        directory.join(format!("{day}T00-00-00-000Z_{name}.jsonl")),
        lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
}

/// The whole point of `allocate`: a project's claim on a plan is its share of
/// *that vendor's* pool, weighted by how many plans the vendor holds — not its
/// share of all tokens everywhere.
#[test]
fn allocate_splits_each_vendor_pool_and_weighs_it_by_plans_held() {
    let directory = tempdir().unwrap();
    let ada = directory.path().join("ada");
    let other = directory.path().join("other");
    fs::create_dir_all(&ada).unwrap();
    fs::create_dir_all(&other).unwrap();
    let history = directory.path().join("pi-sessions");

    // Ada is 60% of the Claude pool but only 25% of the OpenAI pool.
    pi_session(&history, "a1", &ada, "claude-opus-5", 60);
    pi_session(&history, "a2", &other, "claude-opus-5", 40);
    pi_session(&history, "a3", &ada, "gpt-5.6-sol", 25);
    pi_session(&history, "a4", &other, "gpt-5.6-sol", 75);

    let allocation: Value = serde_json::from_slice(
        &run(&[
            "allocate",
            "-p",
            "ada",
            "--sub",
            "claude=2",
            "--sub",
            "codex=4",
            "--price",
            "200",
            "--month",
            "2026-03",
            "--no-git",
            "--provider",
            "pi",
            "--history",
            &format!("pi={}", history.display()),
            "--format",
            "json",
        ])
        .stdout,
    )
    .unwrap();

    let period = |family: &str| -> Value {
        allocation["periods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["family"] == family)
            .unwrap()
            .clone()
    };
    assert_eq!(0.6, period("claude")["share"]);
    assert_eq!(240.0, period("claude")["amount"]);
    assert_eq!(0.25, period("openai")["share"]);
    assert_eq!(200.0, period("openai")["amount"]);
    assert_eq!(440.0, allocation["attributable"]);
    assert_eq!(1200.0, allocation["billed"]);

    // Pooling every token together would read 42.5%; plans held make it 36.7%.
    let effective = allocation["effective_share"].as_f64().unwrap();
    assert!((effective - 440.0 / 1200.0).abs() < 1e-9, "got {effective}");

    // The per-model evidence is what makes the share auditable.
    let models = allocation["models"].as_array().unwrap();
    assert!(
        models.iter().any(|model| model["model"] == "claude-opus-5"),
        "expected a per-model breakdown, got {models:?}"
    );
}

/// A month whose history has been pruned is not a month of no work, and
/// reporting it as a zero silently bills the user for their client's usage.
#[test]
fn allocate_refuses_to_read_pruned_history_as_an_absence_of_work() {
    let directory = tempdir().unwrap();
    let ada = directory.path().join("ada");
    fs::create_dir_all(&ada).unwrap();
    let history = directory.path().join("pi-sessions");
    // Claude only: the OpenAI plans have no surviving history this month.
    pi_session(&history, "a1", &ada, "claude-opus-5", 60);

    let allocation: Value = serde_json::from_slice(
        &run(&[
            "allocate",
            "-p",
            "ada",
            "--sub",
            "claude=2",
            "--sub",
            "codex=4",
            "--month",
            "2026-03",
            "--no-git",
            "--provider",
            "pi",
            "--history",
            &format!("pi={}", history.display()),
            "--format",
            "json",
        ])
        .stdout,
    )
    .unwrap();

    assert_eq!(1200.0, allocation["billed"]);
    // The $800 of OpenAI plans leaves the denominator rather than diluting it.
    assert_eq!(400.0, allocation["documented"]);
    assert_eq!(1.0, allocation["effective_share"]);
    let warnings = allocation["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("no openai history")),
        "the gap must be named, got {warnings:?}"
    );
}

/// A report over `path` with `arguments` appended, parsed. The shared flags
/// keep every Git test away from AI history, the cache and the terminal.
fn git_report(path: &str, arguments: &[&str]) -> Value {
    report_with_env(path, arguments, &[])
}

fn report_with_env(path: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Value {
    let mut command = Command::new(binary());
    command
        .args([
            "--dir",
            path,
            "--no-ai",
            "--no-cache",
            "--no-progress",
            "--format",
            "json",
        ])
        .args(arguments)
        .env_remove("WORKSTATS_AUTHOR")
        .envs(environment.iter().copied());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A repository whose first branch is called `main`, whatever the machine's
/// `init.defaultBranch` says.
fn repository_on_main(temporary: &Path, name: &str) -> String {
    let project = temporary.join(name);
    fs::create_dir_all(&project).unwrap();
    let path = project.to_str().unwrap().to_string();
    assert!(git(&["init", "-q", "-b", "main", &path]).status.success());
    path
}

#[test]
fn commits_on_other_local_branches_are_counted_once() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "branches");
    const ME: &str = "Fixture <fixture@example.com>";

    commit_as(&path, "src/lib.rs", "one\n", ME, &["on main"]);
    assert!(
        git(&["-C", &path, "checkout", "-q", "-b", "side"])
            .status
            .success()
    );
    commit_as(&path, "src/side.rs", "two\nthree\n", ME, &["on side"]);
    // Back on `main`: HEAD no longer reaches the side branch's commit.
    assert!(
        git(&["-C", &path, "checkout", "-q", "main"])
            .status
            .success()
    );
    let head_only = git(&["-C", &path, "rev-list", "--count", "HEAD"]);
    assert_eq!("1", String::from_utf8_lossy(&head_only.stdout).trim());

    let report = git_report(&path, &["--author", "fixture@example.com"]);
    assert_eq!(2, report["summary"]["commit_count"]);
    assert_eq!(3, report["summary"]["additions"]);

    // A detached HEAD on a commit no branch names still counts, and a commit
    // reachable from both HEAD and a branch is not counted twice.
    assert!(
        git(&["-C", &path, "checkout", "-q", "--detach", "side"])
            .status
            .success()
    );
    commit_as(&path, "src/loose.rs", "four\n", ME, &["detached"]);
    let report = git_report(&path, &["--author", "fixture@example.com"]);
    assert_eq!(3, report["summary"]["commit_count"]);
}

/// Git is asked for `--numstat` only for the commits inside the window. A
/// wrapper around the real Git, named through `WORKSTATS_GIT`, records what
/// reaches the diff phase on its standard input.
#[cfg(unix)]
#[test]
fn only_in_window_commits_are_diffed() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "history");
    commit_on(&path, "src/jan.rs", "1\n", "2026-01-10");
    commit_on(&path, "src/feb.rs", "1\n2\n", "2026-02-10");
    commit_on(&path, "src/mar.rs", "1\n2\n3\n", "2026-03-10");
    // A later commit and a rebase-style one: authored in February, committed in
    // April. It belongs to February, so it must be diffed for February.
    fs::write(Path::new(&path).join("src/rebased.rs"), "1\n").unwrap();
    assert!(git(&["-C", &path, "add", "."]).status.success());
    let output = Command::new("git")
        .args([
            "-C",
            &path,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "-m",
            "rebased",
            "--author=Fixture <fixture@example.com>",
        ])
        .env("GIT_AUTHOR_DATE", "2026-02-20T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-04-01T12:00:00Z")
        .output()
        .unwrap();
    assert!(output.status.success());
    let shas = |extra: &[&str]| -> Vec<String> {
        let mut arguments = vec!["-C", path.as_str(), "log", "--all", "--format=%H %s"];
        arguments.extend(extra);
        String::from_utf8(git(&arguments).stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let all = shas(&[]);
    let sha_of = |subject: &str| {
        all.iter()
            .find_map(|line| line.strip_suffix(&format!(" {subject}")))
            .unwrap()
            .to_string()
    };

    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let log = temporary.path().join("stdin.log");
    let wrapper = temporary.path().join("git-wrapper");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *--stdin*) tee -a \"{log}\" | exec \"{real}\" \"$@\" ;;\n  *) exec \"{real}\" \"$@\" ;;\nesac\n",
            log = log.display(),
            real = real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let february = report_with_env(
        &path,
        &["--author", "fixture@example.com", "--month", "2026-02"],
        &[("WORKSTATS_GIT", wrapper.to_str().unwrap())],
    );
    // Feb and the rebased commit, counted with their lines.
    assert_eq!(2, february["summary"]["commit_count"]);
    assert_eq!(3, february["summary"]["additions"]);

    let diffed: std::collections::BTreeSet<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        std::collections::BTreeSet::from([sha_of("src/feb.rs"), sha_of("rebased")]),
        diffed,
        "only the commits authored in February may reach the diff phase"
    );
    // The January and March commits were listed and never diffed.
    for other in ["src/jan.rs", "src/mar.rs"] {
        assert!(!diffed.contains(&sha_of(other)), "{other}");
    }
}

#[test]
fn merge_commits_are_not_counted_and_their_branches_commits_are() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "merged");
    commit_on(&path, "src/base.rs", "1\n", "2026-03-01");
    assert!(
        git(&["-C", &path, "checkout", "-q", "-b", "side"])
            .status
            .success()
    );
    commit_on(&path, "src/side.rs", "1\n2\n", "2026-03-02");
    assert!(
        git(&["-C", &path, "checkout", "-q", "main"])
            .status
            .success()
    );
    commit_on(&path, "src/main.rs", "1\n2\n3\n", "2026-03-03");
    let merge = Command::new("git")
        .args([
            "-C",
            &path,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "merge side",
            "side",
        ])
        .env("GIT_AUTHOR_DATE", "2026-03-04T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-04T12:00:00Z")
        .output()
        .unwrap();
    assert!(merge.status.success());

    let report = git_report(
        &path,
        &["--author", "fixture@example.com", "--month", "2026-03"],
    );
    assert_eq!(3, report["summary"]["commit_count"]);
    assert_eq!(6, report["summary"]["additions"]);
}

#[test]
fn the_window_is_the_author_date_not_the_committer_date() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "dates");
    const ME: &str = "Fixture <fixture@example.com>";

    // (file, author date, committer date)
    let commits = [
        // Authored before the window, committed inside it: a rebase or a
        // cherry-pick. Not March work.
        ("before.rs", "2026-02-10T10:00:00Z", "2026-03-15T10:00:00Z"),
        // Authored and committed inside the window.
        ("both.rs", "2026-03-12T10:00:00Z", "2026-03-12T10:00:00Z"),
        // Authored inside the window, amended after it: still March work.
        ("amended.rs", "2026-03-10T10:00:00Z", "2026-05-20T10:00:00Z"),
    ];
    for (file, authored, committed) in commits {
        fs::write(Path::new(&path).join(file), "line\n").unwrap();
        assert!(git(&["-C", &path, "add", "."]).status.success());
        let output = Command::new("git")
            .args([
                "-C",
                &path,
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "-q",
                "-m",
                file,
                &format!("--author={ME}"),
            ])
            .env("GIT_AUTHOR_DATE", authored)
            .env("GIT_COMMITTER_DATE", committed)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let march = git_report(
        &path,
        &["--author", "fixture@example.com", "--month", "2026-03"],
    );
    assert_eq!(
        2, march["summary"]["commit_count"],
        "the amended commit belongs to March and the rebased one does not"
    );
    assert_eq!(2, march["summary"]["additions"]);

    // The same two bounds spelled as a range behave the same way.
    let range = git_report(
        &path,
        &[
            "--author",
            "fixture@example.com",
            "--since",
            "2026-03-01",
            "--until",
            "2026-03-31",
        ],
    );
    assert_eq!(2, range["summary"]["commit_count"]);

    let everything = git_report(&path, &["--author", "fixture@example.com"]);
    assert_eq!(3, everything["summary"]["commit_count"]);
}

/// Dates a whole day clear of any Monday boundary, so the expected ISO weeks
/// hold in whatever timezone the suite runs in: 27 December 2025 is in the last
/// week of 2025, 1 and 2 January 2026 are in the first week of 2026.
#[test]
fn period_week_groups_by_iso_week_across_a_year_boundary() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "weeks");
    for (file, authored) in [
        ("december.rs", "2025-12-27T12:00:00Z"),
        ("january-a.rs", "2026-01-01T12:00:00Z"),
        ("january-b.rs", "2026-01-02T12:00:00Z"),
    ] {
        fs::write(Path::new(&path).join(file), "line\n").unwrap();
        assert!(git(&["-C", &path, "add", "."]).status.success());
        let output = Command::new("git")
            .args([
                "-C",
                &path,
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "-q",
                "-m",
                file,
            ])
            .env("GIT_AUTHOR_DATE", authored)
            .env("GIT_COMMITTER_DATE", authored)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let author = ["--author", "fixture@example.com"];

    let weekly = git_report(&path, &[author.as_slice(), &["--period", "week"]].concat());
    let weeks: Vec<(&str, u64)> = weekly["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["key"]["week"].as_str().unwrap(),
                row["commit_count"].as_u64().unwrap(),
            )
        })
        .collect();
    // Newest first, like every calendar grouping.
    assert_eq!(vec![("2026-W01", 2), ("2025-W52", 1)], weeks);

    let first_week = git_report(
        &path,
        &[author.as_slice(), &["--week", "2026-W01"]].concat(),
    );
    assert_eq!(2, first_week["summary"]["commit_count"]);
    let last_week = git_report(
        &path,
        &[author.as_slice(), &["--week", "2025-W52"]].concat(),
    );
    assert_eq!(1, last_week["summary"]["commit_count"]);

    // The same window, asked for with --group-by, renders in the table and CSV.
    let csv = run(&[
        "--dir",
        &path,
        "--author",
        "fixture@example.com",
        "--no-ai",
        "--no-cache",
        "--no-progress",
        "--group-by",
        "week",
        "--format",
        "csv",
    ]);
    assert!(csv.status.success());
    let csv = String::from_utf8_lossy(&csv.stdout);
    assert!(
        csv.lines()
            .next()
            .unwrap()
            .split(',')
            .any(|name| name == "week")
    );
    assert!(csv.contains("2026-W01"), "{csv}");
    assert!(csv.contains("2025-W52"), "{csv}");

    let table = run(&[
        "--dir",
        &path,
        "--author",
        "fixture@example.com",
        "--no-ai",
        "--no-cache",
        "--no-progress",
        "--period",
        "week",
    ]);
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains("2026-W01"));

    // A week that does not exist is refused, and so is mixing calendars.
    let missing = run(&["--no-ai", "--no-git", "--week", "2025-W53"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--week"));
    let mixed = run(&["--no-ai", "--no-git", "--group-by", "week,month"]);
    assert_eq!(Some(2), mixed.status.code());
    let conflicting = run(&[
        "--no-ai", "--no-git", "--week", "2026-W01", "--month", "2026-01",
    ]);
    assert_eq!(Some(2), conflicting.status.code());
}

#[test]
fn several_author_identities_are_one_developer() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "identities");
    commit_as(&path, "a.rs", "1\n", "A <a@example.com>", &["a"]);
    commit_as(&path, "b.rs", "1\n", "B <b@example.com>", &["b"]);
    commit_as(&path, "c.rs", "1\n", "C <c@example.com>", &["c"]);
    let count = |report: &Value| report["summary"]["commit_count"].as_u64().unwrap();

    let flags = git_report(&path, &["-a", "a@example.com", "--author", "b@example.com"]);
    assert_eq!(2, count(&flags));
    assert_eq!(
        vec!["a@example.com", "b@example.com"],
        flags["inputs"]["authors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!("a@example.com, b@example.com", flags["inputs"]["author"]);

    // Config < environment < flags.
    let config = temporary.path().join("config.json");
    fs::write(
        &config,
        r#"{"authors": ["a@example.com", "c@example.com"]}"#,
    )
    .unwrap();
    let config = config.to_str().unwrap();
    let from_config = git_report(&path, &["--config", config]);
    assert_eq!(2, count(&from_config));
    assert_eq!(
        "a@example.com, c@example.com",
        from_config["inputs"]["author"]
    );

    let from_environment = report_with_env(
        &path,
        &["--config", config],
        &[("WORKSTATS_AUTHOR", "b@example.com")],
    );
    assert_eq!(1, count(&from_environment));
    assert_eq!(
        1,
        from_environment["inputs"]["authors"]
            .as_array()
            .unwrap()
            .len()
    );

    let from_flag = report_with_env(
        &path,
        &["--config", config, "--author", "c@example.com"],
        &[("WORKSTATS_AUTHOR", "b@example.com")],
    );
    assert_eq!(1, count(&from_flag));
    assert_eq!("c@example.com", from_flag["inputs"]["author"]);
}

fn allocate_with_config(directory: &Path, config: &str, format: &str) -> Output {
    let ada = directory.join("ada");
    let other = directory.join("other");
    fs::create_dir_all(&ada).unwrap();
    fs::create_dir_all(&other).unwrap();
    let history = directory.join("pi-sessions");
    // A model the built-in table has never heard of, ten times the tokens.
    pi_session(&history, "a1", &ada, "acme-coder-1", 10);
    pi_session(&history, "a2", &other, "claude-opus-5", 90);
    let config_file = directory.join("config.json");
    fs::write(&config_file, config).unwrap();
    run(&[
        "allocate",
        "-p",
        "ada",
        "--sub",
        "claude=1",
        "--basis",
        "value",
        "--month",
        "2026-03",
        "--no-git",
        "--provider",
        "pi",
        "--history",
        &format!("pi={}", history.display()),
        "--config",
        config_file.to_str().unwrap(),
        "--format",
        format,
    ])
}

/// A model the table lacks is "unpriced" and drops out of the value basis;
/// a `model_rates` entry prices it, and the output says whose rate was used.
#[test]
fn allocate_prices_unknown_models_from_config_rate_overrides() {
    let directory = tempdir().unwrap();
    let config = r#"{"model_rates": {"acme-coder": {
        "input": 0, "cache_write": 0, "cache_read": 0, "output": 25, "family": "claude"}}}"#;
    let output = allocate_with_config(directory.path(), config, "json");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let allocation: Value = serde_json::from_slice(&output.stdout).unwrap();

    // Ada's 10 output tokens at $25 against 90 of Opus at $25: a 10% claim
    // (give or take the one input token the fixture adds). Unpriced, Ada would
    // have had no value at all.
    let share = allocation["effective_share"].as_f64().unwrap();
    assert!((share - 0.1).abs() < 1e-3, "got {share}");
    assert_eq!(
        serde_json::json!(["acme-coder"]),
        allocation["rate_overrides"]
    );
    let acme = allocation["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "acme-coder-1")
        .unwrap();
    assert_eq!("override", acme["rate_source"]);
    assert_eq!(true, acme["priced"]);

    let table = allocate_with_config(directory.path(), config, "table");
    let text = String::from_utf8_lossy(&table.stdout);
    assert!(
        text.contains("overridden by model_rates: acme-coder"),
        "the override must be visible beside the rates-as-of line, got {text}"
    );
}

/// One misspelt field used to make serde discard the whole config with a
/// warning, silently losing the rest of it (authors, defaults, aliases).
#[test]
fn a_misspelt_model_rates_field_is_a_hard_error_naming_the_key() {
    let directory = tempdir().unwrap();
    for (config, expected) in [
        (
            r#"{"authors": ["me@example.com"], "model_rates": {"acme-coder": {
                "inptu": 1, "cache_write": 1, "cache_read": 1, "output": 2}}}"#,
            "model_rates.acme-coder",
        ),
        (
            r#"{"defaults": {"format": "json"}, "model_rates": {"acme-coder": {
                "input": "fast", "cache_write": 1, "cache_read": 1, "output": 2}}}"#,
            "model_rates.acme-coder",
        ),
        (r#"{"model_rates": ["acme-coder"]}"#, "model_rates"),
    ] {
        let output = run_with_defaults(directory.path(), config, &[]);
        assert_eq!(Some(2), output.status.code(), "{config}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{config}: {stderr}");
        assert!(!stderr.contains("ignoring config"), "{config}: {stderr}");
    }
    let output = run_with_defaults(
        directory.path(),
        r#"{"model_rates": {"acme-coder": {"inptu": 1}}}"#,
        &[],
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("inptu"));
}

/// A bad override is refused by name rather than silently priced at nonsense.
#[test]
fn allocate_refuses_invalid_rate_overrides_naming_the_key() {
    let directory = tempdir().unwrap();
    let output = allocate_with_config(
        directory.path(),
        r#"{"model_rates": {"acme-coder": {
            "input": 1, "cache_write": 1, "cache_read": 1, "output": -3}}}"#,
        "json",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("model_rates")
            && stderr.contains("acme-coder")
            && stderr.contains("output"),
        "got {stderr}"
    );

    let output = allocate_with_config(
        directory.path(),
        r#"{"model_rates": {"acme-coder": {
            "input": 1, "cache_write": 1, "cache_read": 1, "output": 3, "family": "bedrock"}}}"#,
        "json",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("acme-coder") && stderr.contains("bedrock"),
        "got {stderr}"
    );
}

/// Runs a report against a config file holding `config`, with the
/// environment variables that would otherwise leak into precedence removed.
fn run_with_defaults(directory: &Path, config: &str, arguments: &[&str]) -> Output {
    let config_file = directory.join("config.json");
    fs::write(&config_file, config).unwrap();
    Command::new(binary())
        .args([
            "--no-ai",
            "--no-git",
            "--no-cache",
            "--no-progress",
            "--config",
            config_file.to_str().unwrap(),
        ])
        .args(arguments)
        .env_remove("WORKSTATS_DIR")
        .env_remove("WORKSTATS_AUTHOR")
        .output()
        .unwrap()
}

fn json_stdout(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `defaults` fills in flags that were not given, and a flag given with the
/// very value the built-in default has still wins over the config.
#[test]
fn config_defaults_fill_unset_flags_and_never_override_explicit_ones() {
    let directory = tempdir().unwrap();
    let config = r#"{"defaults": {"format": "json", "human_idle": "45m", "review_credit": "10m"}}"#;

    let report = json_stdout(&run_with_defaults(directory.path(), config, &[]));
    assert_eq!("45m", report["inputs"]["human_idle"]);
    assert_eq!("10m", report["inputs"]["review_credit"]);
    assert_eq!(
        serde_json::json!({"format": "json", "human_idle": "45m", "review_credit": "10m"}),
        report["inputs"]["config_defaults"]
    );

    // --human-idle 1h is the built-in value; it must still beat the config.
    let report = json_stdout(&run_with_defaults(
        directory.path(),
        config,
        &["--human-idle", "1h", "--format", "json"],
    ));
    assert_eq!("1h", report["inputs"]["human_idle"]);
    assert_eq!("10m", report["inputs"]["review_credit"]);
    assert_eq!(
        serde_json::json!({"review_credit": "10m"}),
        report["inputs"]["config_defaults"]
    );

    // --format table is the built-in value too: the output must not be JSON.
    let table = run_with_defaults(directory.path(), config, &["--format", "table"]);
    assert!(table.status.success());
    assert!(serde_json::from_slice::<Value>(&table.stdout).is_err());
}

#[test]
fn config_default_dir_sits_between_the_environment_and_the_working_directory() {
    let directory = tempdir().unwrap();
    let configured = tempdir().unwrap();
    let environment = tempdir().unwrap();
    let flagged = tempdir().unwrap();
    let config = format!(
        r#"{{"defaults": {{"format": "json", "dir": {:?}}}}}"#,
        configured.path().to_str().unwrap()
    );

    let root = |output: &Output| json_stdout(output)["inputs"]["git_root"].clone();
    assert_eq!(
        configured.path().to_str().unwrap(),
        root(&run_with_defaults(directory.path(), &config, &[]))
            .as_str()
            .unwrap()
    );

    let config_file = directory.path().join("config.json");
    let with_environment = Command::new(binary())
        .args([
            "--no-ai",
            "--no-git",
            "--no-cache",
            "--no-progress",
            "--config",
        ])
        .arg(&config_file)
        .env("WORKSTATS_DIR", environment.path())
        .output()
        .unwrap();
    assert_eq!(
        environment.path().to_str().unwrap(),
        root(&with_environment).as_str().unwrap()
    );

    let with_flag = Command::new(binary())
        .args([
            "--no-ai",
            "--no-git",
            "--no-cache",
            "--no-progress",
            "--config",
        ])
        .arg(&config_file)
        .arg("--dir")
        .arg(flagged.path())
        .env("WORKSTATS_DIR", environment.path())
        .output()
        .unwrap();
    assert_eq!(
        flagged.path().to_str().unwrap(),
        root(&with_flag).as_str().unwrap()
    );
}

#[test]
fn config_default_dir_is_recorded_only_when_it_was_the_source() {
    let directory = tempdir().unwrap();
    let configured = tempdir().unwrap();
    let flagged = tempdir().unwrap();
    let config = format!(
        r#"{{"defaults": {{"format": "json", "dir": {:?}}}}}"#,
        configured.path().to_str().unwrap()
    );

    let used = json_stdout(&run_with_defaults(directory.path(), &config, &[]));
    assert_eq!(
        configured.path().to_str().unwrap(),
        used["inputs"]["config_defaults"]["dir"].as_str().unwrap()
    );
    let flag = flagged.path().to_str().unwrap();
    let overridden = json_stdout(&run_with_defaults(
        directory.path(),
        &config,
        &["--dir", flag],
    ));
    assert!(overridden["inputs"]["config_defaults"].get("dir").is_none());
}

#[test]
fn config_defaults_that_applied_are_listed_in_every_human_readable_output() {
    let directory = tempdir().unwrap();
    let config = r#"{"defaults": {"providers": ["claude", "codex"], "group_by": "repo,month"}}"#;
    let note = "Config defaults: group_by=repo,month; providers=claude,codex";

    let table = run_with_defaults(directory.path(), config, &[]);
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains(note));
    let markdown = run_with_defaults(directory.path(), config, &["--format", "markdown"]);
    assert!(
        String::from_utf8_lossy(&markdown.stdout)
            .contains("Config defaults: group\\_by=repo,month"),
        "{}",
        String::from_utf8_lossy(&markdown.stdout)
    );
    let html = run_with_defaults(directory.path(), config, &["--format", "html"]);
    assert!(String::from_utf8_lossy(&html.stdout).contains(note));
    // `allocate` reads its own `--config`, and a run with its own flags for
    // everything else still lists what the config supplied.
    for format in ["table", "markdown", "html"] {
        let allocation = allocate_with_config(
            directory.path(),
            r#"{"defaults": {"human_idle": "45m"}}"#,
            format,
        );
        assert!(
            allocation.status.success(),
            "{}",
            String::from_utf8_lossy(&allocation.stderr)
        );
        let text = String::from_utf8_lossy(&allocation.stdout);
        assert!(
            text.contains("Config defaults: human_idle=45m")
                || text.contains("Config defaults: human\\_idle=45m"),
            "{format}: {text}"
        );
    }

    // Nothing applied, nothing said: flags given on the command line win.
    let typed = run_with_defaults(
        directory.path(),
        config,
        &["--provider", "pi", "--group-by", "repo"],
    );
    assert!(!String::from_utf8_lossy(&typed.stdout).contains("Config defaults"));
}

#[test]
fn refusals_blame_the_config_when_the_format_came_from_it() {
    let directory = tempdir().unwrap();
    let window = ["--month", "2026-03", "--compare", "previous"];
    let config = r#"{"defaults": {"format": "csv"}}"#.to_string();
    let output = run_with_defaults(directory.path(), &config, &window);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("defaults.format \"csv\" (from config)"),
        "{error}"
    );
    assert!(
        error.contains("pass --format table or --format json"),
        "{error}"
    );
    assert!(!error.contains("with --format csv"), "{error}");

    // Given on the command line, the flag is what to change.
    let typed = run_with_defaults(
        directory.path(),
        &config,
        &[
            "--format", "csv", window[0], window[1], window[2], window[3],
        ],
    );
    let error = String::from_utf8_lossy(&typed.stderr);
    assert!(error.contains("with --format csv"), "{error}");
    for format in ["csv", "markdown", "html"] {
        let config = format!(r#"{{"defaults": {{"format": "{format}"}}}}"#);
        let output = run_with_defaults(directory.path(), &config, &["--explain-human-time"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(&format!("defaults.format \"{format}\" (from config)")),
            "{error}"
        );
        assert!(
            error.contains("pass --format table or --format json"),
            "{error}"
        );
    }
}

#[test]
fn a_config_calendar_grouping_gives_way_to_an_explicit_period() {
    let directory = tempdir().unwrap();
    let config = r#"{"defaults": {"format": "json", "group_by": "repo,month"}}"#;
    let report = json_stdout(&run_with_defaults(
        directory.path(),
        config,
        &["--period", "week"],
    ));
    assert_eq!(serde_json::json!(["repo", "week"]), report["group_by"]);
    assert_eq!(
        serde_json::json!({"format": "json", "group_by": "repo"}),
        report["inputs"]["config_defaults"]
    );
    // Without the flag the config's own grouping stands.
    let report = json_stdout(&run_with_defaults(directory.path(), config, &[]));
    assert_eq!(serde_json::json!(["repo", "month"]), report["group_by"]);
}

#[test]
fn config_defaults_refuse_unknown_keys_and_bad_values_naming_them() {
    let directory = tempdir().unwrap();
    for (config, expected) in [
        (r#"{"defaults": {"depht": 2}}"#, "depht"),
        (r#"{"defaults": {"gap_cap": "soon"}}"#, "defaults.gap_cap"),
        (r#"{"defaults": {"format": "xml"}}"#, "defaults.format"),
        (r#"{"defaults": {"depth": "deep"}}"#, "defaults.depth"),
        (
            r#"{"defaults": {"dir": "/nonexistent/workstats"}}"#,
            "defaults.dir",
        ),
    ] {
        let output = run_with_defaults(directory.path(), config, &[]);
        assert_eq!(Some(2), output.status.code(), "{config}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{config}: {stderr}");
    }
}

/// A one-commit repository to render the report from, plus the flags that
/// select it alone: no AI history, no cache, no progress line.
fn document_fixture(temporary: &Path) -> (String, Vec<String>) {
    let project = temporary.join("project");
    fs::create_dir_all(&project).unwrap();
    let path = project.to_str().unwrap().to_string();
    assert!(git(&["init", "-q", &path]).status.success());
    commit_as(
        &path,
        "src/lib.rs",
        "one\ntwo\nthree\n",
        "Fixture <fixture@example.com>",
        &["areas"],
    );
    let arguments = [
        "--dir",
        &path,
        "--author",
        "fixture@example.com",
        "--no-ai",
        "--no-cache",
        "--no-progress",
    ]
    .map(str::to_string)
    .to_vec();
    (path, arguments)
}

fn run_with_format(base: &[String], format: &str) -> Output {
    let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
    arguments.extend(["--format", format]);
    run(&arguments)
}

#[test]
fn markdown_report_mirrors_the_table_sections_and_figures() {
    let temporary = tempdir().unwrap();
    let (_, base) = document_fixture(temporary.path());
    let table = run_with_format(&base, "table");
    let markdown = run_with_format(&base, "markdown");
    assert!(markdown.status.success());
    // Nothing but the document on either stream.
    assert!(markdown.stderr.is_empty());
    let table = String::from_utf8_lossy(&table.stdout);
    let markdown = String::from_utf8(markdown.stdout).unwrap();

    assert!(markdown.starts_with("# WORKSTATS\n"), "{markdown}");
    for heading in [
        "## Summary",
        "## Work composition",
        "## By repo",
        "## Notes",
    ] {
        assert!(markdown.contains(heading), "missing {heading}\n{markdown}");
    }
    assert!(markdown.contains("| Measure | Value |\n| --- | --- |"));
    assert!(
        markdown
            .contains("| Work area | Human | Days | Avg/day | Commits | AI wall | Agent work |")
    );
    assert!(markdown.contains("| --- | ---: | ---: | ---: | ---: | ---: | ---: |"));
    // The same formatted figure appears in both views.
    let human = markdown
        .lines()
        .find_map(|line| line.strip_prefix("| Estimated human work | "))
        .unwrap()
        .trim_end_matches(" |");
    assert!(
        table.contains(&format!("Estimated human work  {human}")),
        "{table}"
    );
    assert!(markdown.contains("| Git lines | +3 / -0 |"));
    // No update notice, which the table may print after the report.
    assert!(!markdown.contains("is available"));
}

#[test]
fn html_report_is_one_self_contained_static_page() {
    let temporary = tempdir().unwrap();
    let (_, base) = document_fixture(temporary.path());
    let output = run_with_format(&base, "html");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let html = String::from_utf8(output.stdout).unwrap();

    assert!(html.starts_with("<!doctype html>"), "{html}");
    assert!(html.ends_with("</html>\n"));
    assert!(html.contains("<style>"));
    assert!(html.contains("prefers-color-scheme"));
    assert!(html.contains("<h2>Summary</h2>"));
    assert!(html.contains("<td class=\"num\">"));
    // Offline by construction: no script, nothing referenced, nothing fetched.
    for forbidden in [
        "http://", "https://", "<script", "<link", "<img", "<iframe", "@import", "url(", " src=",
        " href=",
    ] {
        assert!(!html.contains(forbidden), "found {forbidden}\n{html}");
    }
}

/// The working directory of an event is an arbitrary string, which makes it the
/// cross-platform way to put hostile text in a row label — a directory called
/// `<script>` cannot exist on Windows, and `record --model` refuses the
/// characters that matter here.
#[test]
fn hostile_row_labels_are_escaped_in_markdown_and_html() {
    let directory = tempdir().unwrap();
    let events = directory.path().join("events.jsonl");
    let hostile = "/x/<script>alert(1)</script>&\"x\"|`y`";
    let line = serde_json::json!({
        "timestamp": "2026-01-01T00:00:00+00:00",
        "provider": "cursor",
        "session_id": "one",
        "cwd": hostile,
        "model": "abc",
        "event": "prompt",
        "role": "foreground",
    });
    fs::write(&events, format!("{line}\n")).unwrap();
    let render = |format: &str| {
        let output = run(&[
            "--no-git",
            "--provider",
            "cursor",
            "--events",
            events.to_str().unwrap(),
            "--no-default-events",
            "--no-cache",
            "--no-progress",
            "--group-by",
            "cwd",
            "--format",
            format,
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let html = render("html");
    assert!(!html.contains("<script"), "{html}");
    assert!(html.contains("/x/&lt;script&gt;alert(1)&lt;/script&gt;&amp;&quot;x&quot;|`y`"));

    let markdown = render("markdown");
    assert!(
        markdown.contains(r#"| /x/\<script\>alert(1)\</script\>\&"x"\|\`y\` |"#),
        "{markdown}"
    );
    // The row stayed one row: the pipe did not add a column.
    let row = markdown
        .lines()
        .find(|line| line.contains("alert(1)"))
        .unwrap();
    assert_eq!(8, row.matches('|').count() - row.matches("\\|").count());
}

#[test]
fn allocate_renders_markdown_and_html() {
    let directory = tempdir().unwrap();
    let ada = directory.path().join("ada");
    fs::create_dir_all(&ada).unwrap();
    let history = directory.path().join("pi-sessions");
    pi_session(&history, "a1", &ada, "claude-opus-5", 60);
    let render = |format: &str| {
        let output = run(&[
            "allocate",
            "-p",
            "ada",
            "--sub",
            "claude=2",
            "--price",
            "200",
            "--month",
            "2026-03",
            "--no-git",
            "--no-cache",
            "--no-progress",
            "--provider",
            "pi",
            "--history",
            &format!("pi={}", history.display()),
            "--format",
            format,
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let markdown = render("markdown");
    assert!(markdown.starts_with("# Allocation\n"), "{markdown}");
    assert!(markdown.contains("Project: ada"));
    assert!(
        markdown
            .contains("| Month | Family | Subs | Plan/mo | Project | Pool | Share | Owed | Note |")
    );
    assert!(markdown.contains("| **Attributable** |"));
    assert!(markdown.contains("## Cross-check"));

    let html = render("html");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<tfoot>"));
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("http://") && !html.contains("https://"));
}

#[test]
fn markdown_and_html_are_refused_where_they_have_no_meaning() {
    let temporary = tempdir().unwrap();
    let (_, base) = document_fixture(temporary.path());
    for format in ["markdown", "html"] {
        let sources = run(&["sources", "--format", format]);
        assert!(!sources.status.success());
        assert!(
            String::from_utf8_lossy(&sources.stderr)
                .contains("not available for `workstats sources`")
        );
        let classify = run(&["classify", "src/lib.rs", "--format", format]);
        assert!(!classify.status.success());

        let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
        arguments.extend(["--explain-human-time", "--format", format]);
        let explained = run(&arguments);
        assert!(!explained.status.success());
        assert!(
            String::from_utf8_lossy(&explained.stderr)
                .contains(&format!("not available with --format {format}"))
        );
        let ui = run(&["ui", "--format", format]);
        assert!(!ui.status.success());
    }
}

/// Commits `body` to `file` as the fixture developer at noon UTC on `day`, a
/// whole half-day clear of any local midnight so the month it lands in does not
/// depend on the timezone the suite runs in.
fn commit_on(repo: &str, file: &str, body: &str, day: &str) {
    fs::create_dir_all(Path::new(repo).join(file).parent().unwrap()).unwrap();
    fs::write(Path::new(repo).join(file), body).unwrap();
    assert!(git(&["-C", repo, "add", "."]).status.success());
    let date = format!("{day}T12:00:00Z");
    let output = Command::new("git")
        .args([
            "-C",
            repo,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "-m",
            file,
            "--author=Fixture <fixture@example.com>",
        ])
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// February has two commits (two source lines, one test line), March three
/// (eight source lines, three test lines), and January none. Each month has one
/// Pi session in the repository, so AI figures exist on both sides.
fn compare_fixture(temporary: &Path) -> (String, Vec<String>) {
    let path = repository_on_main(temporary, "compared");
    commit_on(&path, "src/a.rs", "1\n2\n", "2026-02-10");
    commit_on(&path, "tests/a.rs", "1\n", "2026-02-11");
    commit_on(&path, "src/b.rs", "1\n2\n3\n", "2026-03-10");
    commit_on(&path, "src/c.rs", "1\n2\n3\n4\n5\n", "2026-03-11");
    commit_on(&path, "tests/b.rs", "1\n2\n3\n", "2026-03-12");
    let history = temporary.join("pi-sessions");
    pi_session_on(
        &history,
        "feb",
        Path::new(&path),
        "claude-opus-5",
        10,
        "2026-02-10",
    );
    pi_session_on(
        &history,
        "mar",
        Path::new(&path),
        "claude-opus-5",
        10,
        "2026-03-10",
    );
    let arguments = [
        "--dir",
        &path,
        "--author",
        "fixture@example.com",
        "--no-cache",
        "--no-progress",
        "--provider",
        "pi",
        "--history",
        &format!("pi={}", history.display()),
        "--format",
        "json",
    ]
    .map(str::to_string)
    .to_vec();
    (path, arguments)
}

fn report_json(base: &[String], extra: &[&str]) -> Value {
    let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
    arguments.extend(extra);
    let output = run(&arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(base: &[String], extra: &[&str]) -> String {
    let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
    arguments.extend(extra);
    let output = run(&arguments);
    assert!(!output.status.success(), "expected {extra:?} to be refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn compare_previous_puts_each_side_where_a_standalone_run_would() {
    let temporary = tempdir().unwrap();
    let (_, base) = compare_fixture(temporary.path());

    let march = report_json(&base, &["--month", "2026-03"]);
    let february = report_json(&base, &["--month", "2026-02"]);
    let compared = report_json(&base, &["--month", "2026-03", "--compare", "previous"]);

    // The report itself is the selected window's, as if --compare were absent,
    // and the flag adds only a `comparison` block.
    assert!(march.get("comparison").is_none());
    let mut without = compared.clone();
    without.as_object_mut().unwrap().remove("comparison");
    assert_eq!(march["summary"], without["summary"]);
    assert_eq!(march["rows"], without["rows"]);
    assert_eq!(
        march["inputs"]["git_scan_roots"],
        without["inputs"]["git_scan_roots"]
    );

    let comparison = &compared["comparison"];
    assert_eq!("previous", comparison["basis"]);
    assert_eq!("2026-03", comparison["current"]["label"]);
    assert_eq!("2026-02", comparison["previous"]["label"]);
    assert!(
        comparison["note"]
            .as_str()
            .unwrap()
            .contains("not stopwatch times")
    );

    // Both sides match the windows run on their own, figure for figure.
    for (side, standalone) in [("current", &march), ("previous", &february)] {
        let figures = &comparison[side]["figures"];
        let summary = &standalone["summary"];
        for key in [
            "commit_count",
            "additions",
            "deletions",
            "session_count",
            "human_estimated_seconds",
            "human_active_days",
            "prompt_signal_count",
            "parallel_agent_seconds",
        ] {
            assert_eq!(summary[key], figures[key], "{side} {key}");
        }
    }
    assert_eq!(3, comparison["current"]["figures"]["commit_count"]);
    assert_eq!(2, comparison["previous"]["figures"]["commit_count"]);
    assert_eq!(1, comparison["current"]["figures"]["session_count"]);
    assert_eq!(1, comparison["previous"]["figures"]["session_count"]);

    let delta = &comparison["delta"];
    assert_eq!(1.0, delta["commit_count"]["change"]);
    assert_eq!(50.0, delta["commit_count"]["percent"]);
    assert_eq!(8.0, delta["additions"]["change"]);
    // Composition is compared per area in percentage points: source was 2 of 3
    // changed lines in February and 8 of 11 in March.
    let source = delta["composition"]
        .as_array()
        .unwrap()
        .iter()
        .find(|area| area["category"] == "source")
        .unwrap();
    // Shares are reported to three decimals (0.727 and 0.667), so the change is
    // exactly six points rather than 6.06.
    assert_eq!(6.0, source["change_points"].as_f64().unwrap(), "{source}");
}

#[test]
fn compare_scans_the_same_checkouts_as_the_standalone_runs() {
    let temporary = tempdir().unwrap();
    let (_, base) = compare_fixture(temporary.path());
    // A checkout outside --dir that only a January session points at, with a
    // March commit: a standalone run scans it whatever the session's date, so
    // --compare must not stop counting that commit.
    let other = repository_on_main(temporary.path(), "other");
    commit_on(&other, "src/o.rs", "1\n2\n", "2026-03-10");
    pi_session_on(
        &temporary.path().join("pi-sessions"),
        "jan",
        Path::new(&other),
        "claude-opus-5",
        10,
        "2026-01-05",
    );

    for (month, compared_with) in [("2026-03", "previous"), ("2026-02", "2026-03")] {
        let standalone = report_json(&base, &["--month", month]);
        let compared = report_json(&base, &["--month", month, "--compare", compared_with]);
        let roots = standalone["inputs"]["git_scan_roots"].as_array().unwrap();
        assert!(
            roots
                .iter()
                .any(|root| root.as_str().unwrap().ends_with("other")),
            "{month}: {roots:?}"
        );
        assert_eq!(standalone["summary"], compared["summary"], "{month}");
        assert_eq!(standalone["rows"], compared["rows"], "{month}");
        assert_eq!(
            standalone["inputs"]["git_scan_roots"], compared["inputs"]["git_scan_roots"],
            "{month}"
        );
        // The other side is the window a standalone run of it would print.
        let side = if compared_with == "previous" {
            "2026-02"
        } else {
            "2026-03"
        };
        let other_window = report_json(&base, &["--month", side]);
        for key in ["commit_count", "additions", "deletions", "session_count"] {
            assert_eq!(
                other_window["summary"][key], compared["comparison"]["previous"]["figures"][key],
                "{month} vs {side}: {key}"
            );
        }
    }
    let march = report_json(&base, &["--month", "2026-03"]);
    assert_eq!(4, march["summary"]["commit_count"]);
}

#[test]
fn compare_shows_not_available_rather_than_infinity_when_the_earlier_window_is_empty() {
    let temporary = tempdir().unwrap();
    let (_, base) = compare_fixture(temporary.path());

    // January holds nothing, so February grew from zero.
    let compared = report_json(&base, &["--month", "2026-02", "--compare", "previous"]);
    let comparison = &compared["comparison"];
    assert_eq!("2026-01", comparison["previous"]["label"]);
    assert_eq!(0, comparison["previous"]["figures"]["commit_count"]);
    assert_eq!(2.0, comparison["delta"]["commit_count"]["change"]);
    assert!(comparison["delta"]["commit_count"]["percent"].is_null());
    assert!(comparison["delta"]["human_estimated_seconds"]["percent"].is_null());

    // Shares have nothing to be a share of in an empty window.
    let area = &comparison["delta"]["composition"][0];
    assert!(area["previous"].is_null() && area["change_points"].is_null());

    let table = run(&[
        "--dir",
        &base[1],
        "--author",
        "fixture@example.com",
        "--no-ai",
        "--no-cache",
        "--no-progress",
        "--month",
        "2026-02",
        "--compare",
        "previous",
    ]);
    assert!(table.status.success());
    let text = String::from_utf8_lossy(&table.stdout);
    assert!(text.contains("Comparison"), "{text}");
    assert!(text.contains("(n/a)"), "{text}");
    assert!(text.contains("estimates"), "{text}");
    assert!(!text.contains("inf"), "{text}");
}

#[test]
fn compare_accepts_a_named_baseline_and_ranges_and_renders_documents() {
    let temporary = tempdir().unwrap();
    let (_, base) = compare_fixture(temporary.path());

    let named = report_json(&base, &["--month", "2026-03", "--compare", "2026-02"]);
    assert_eq!("2026-02", named["comparison"]["basis"]);
    assert_eq!(
        2,
        named["comparison"]["previous"]["figures"]["commit_count"]
    );

    // A named baseline may lie after the selected window.
    let later = report_json(&base, &["--month", "2026-02", "--compare", "2026-03"]);
    assert_eq!(
        3,
        later["comparison"]["previous"]["figures"]["commit_count"]
    );
    assert_eq!(2, later["comparison"]["current"]["figures"]["commit_count"]);

    // A range is compared with the same number of days before it.
    let range = report_json(
        &base,
        &[
            "--since",
            "2026-03-01",
            "--until",
            "2026-03-31",
            "--compare",
            "previous",
        ],
    );
    assert_eq!("2026-02", range["comparison"]["previous"]["label"]);

    for (format, marker) in [
        ("markdown", "## Comparison"),
        ("html", "<h2>Comparison</h2>"),
    ] {
        let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
        arguments.extend(["--month", "2026-03", "--compare", "previous"]);
        let position = arguments.iter().position(|item| *item == "json").unwrap();
        arguments[position] = format;
        let output = run(&arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(marker), "{format}: {text}");
        assert!(text.contains("Estimated human work"), "{format}: {text}");
        assert!(text.contains("+1 (+50%)"), "{format}: {text}");
    }
}

#[test]
fn compare_refuses_what_it_cannot_compare_or_show() {
    let temporary = tempdir().unwrap();
    let (_, base) = compare_fixture(temporary.path());

    for window in [
        vec!["--compare", "previous"],
        vec!["--since", "2026-03", "--compare", "previous"],
    ] {
        let error = failure(&base, &window);
        assert!(error.contains("bounded window"), "{error}");
    }
    let error = failure(&base, &["--month", "2026-03", "--compare", "2026-03"]);
    assert!(error.contains("overlaps"), "{error}");
    let error = failure(&base, &["--month", "2026-03", "--compare", "sometime"]);
    assert!(error.contains("--compare"), "{error}");

    let mut csv = base.clone();
    let position = csv.iter().position(|item| item == "json").unwrap();
    csv[position] = "csv".to_string();
    let error = failure(&csv, &["--month", "2026-03", "--compare", "previous"]);
    assert!(error.contains("--format csv"), "{error}");

    let error = failure(
        &base,
        &["ui", "--month", "2026-03", "--compare", "previous"],
    );
    assert!(error.contains("workstats ui"), "{error}");
    let error = failure(
        &["allocate", "--sub", "claude=1"]
            .map(str::to_string)
            .into_iter()
            .chain(base.iter().cloned())
            .collect::<Vec<_>>(),
        &["--month", "2026-03", "--compare", "previous"],
    );
    assert!(error.contains("workstats allocate"), "{error}");
}
