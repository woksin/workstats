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
        // --events adds to the log `workstats record` writes rather than
        // replacing it, so without this the developer's own recorded sessions
        // reach these counts on a machine that has ever run `workstats record`.
        "--no-default-events",
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
