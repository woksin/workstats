//! Branch attribution and issue keys, end to end: real repositories, real Git
//! history and the real binary.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

mod common;
use common::*;

/// Runs `git` in `repo` as of `when`, so reflog entries land where the test
/// says they do.
fn git_at(repo: &str, when: &str, arguments: &[&str]) {
    let output = Command::new("git")
        .args(["-C", repo])
        .args(arguments)
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The commit count of each row of a report grouped by `dimensions`, keyed by
/// the row's values joined with `|`.
fn commits_by(report: &Value, dimensions: &[&str]) -> BTreeMap<String, u64> {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let key = dimensions
                .iter()
                .map(|dimension| row["key"][dimension].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
                .join("|");
            (key, row["commit_count"].as_u64().unwrap())
        })
        .collect()
}

/// `main` with one commit, a feature branch with another, and `main` again
/// with a third.
fn repository_with_feature(temporary: &Path, feature: &str) -> String {
    let path = repository_on_main(temporary, "project");
    commit_on(&path, "a.txt", "a\n", "2026-03-02");
    git_at(
        &path,
        "2026-03-02T13:00:00Z",
        &["checkout", "-q", "-b", feature],
    );
    commit_on(&path, "b.txt", "b\n", "2026-03-03");
    git_at(&path, "2026-03-03T13:00:00Z", &["checkout", "-q", "main"]);
    commit_on(&path, "c.txt", "c\n", "2026-03-04");
    path
}

fn arguments<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut arguments = vec!["--author", "fixture@example.com", "--month", "2026-03"];
    arguments.extend(extra);
    arguments
}

#[test]
fn commits_group_by_branch_issue_and_feature() {
    let temporary = tempdir().unwrap();
    let path = repository_with_feature(temporary.path(), "feature/ACME-7-login");

    let by_branch = git_report(&path, &arguments(&["--group-by", "branch"]));
    let expected: BTreeMap<String, u64> = [
        ("feature/ACME-7-login".to_string(), 1),
        ("main".to_string(), 2),
    ]
    .into();
    assert_eq!(expected, commits_by(&by_branch, &["branch"]));

    let by_issue = git_report(&path, &arguments(&["--group-by", "issue"]));
    let expected: BTreeMap<String, u64> = [("ACME-7".to_string(), 1), ("—".to_string(), 2)].into();
    assert_eq!(expected, commits_by(&by_issue, &["issue"]));

    // The integration branch's feature is its own name, not a dash.
    let by_feature = git_report(&path, &arguments(&["--group-by", "feature"]));
    let expected: BTreeMap<String, u64> =
        [("ACME-7".to_string(), 1), ("main".to_string(), 2)].into();
    assert_eq!(expected, commits_by(&by_feature, &["feature"]));
}

#[test]
fn a_branch_without_an_issue_is_its_slug_and_grouping_adds_up() {
    let temporary = tempdir().unwrap();
    let path = repository_with_feature(temporary.path(), "feature/login-page");

    let by_feature = git_report(&path, &arguments(&["--group-by", "feature"]));
    let expected: BTreeMap<String, u64> =
        [("login-page".to_string(), 1), ("main".to_string(), 2)].into();
    assert_eq!(expected, commits_by(&by_feature, &["feature"]));

    // Grouping never changes what is counted.
    let plain = git_report(&path, &arguments(&[]));
    let total: u64 = commits_by(&by_feature, &["feature"]).values().sum();
    assert_eq!(plain["summary"]["commit_count"].as_u64().unwrap(), total);
}

#[test]
fn configured_issue_rules_and_the_integration_branch_apply() {
    let temporary = tempdir().unwrap();
    let path = repository_with_feature(temporary.path(), "acme-12-thing");
    let config = temporary.path().join("config.json");
    fs::write(
        &config,
        r#"{"issues": {"projects": ["acme"]}, "branches": {"integration": ["trunk", "main"]}}"#,
    )
    .unwrap();
    let config = config.to_str().unwrap();

    let by_issue = git_report(
        &path,
        &arguments(&["--group-by", "issue", "--config", config]),
    );
    let expected: BTreeMap<String, u64> = [("ACME-12".to_string(), 1), ("—".to_string(), 2)].into();
    assert_eq!(expected, commits_by(&by_issue, &["issue"]));

    // Without `projects`, a lower-case key is not guessed at.
    let by_issue = git_report(&path, &arguments(&["--group-by", "issue"]));
    let expected: BTreeMap<String, u64> = [("—".to_string(), 3)].into();
    assert_eq!(expected, commits_by(&by_issue, &["issue"]));
}

#[test]
fn a_configured_integration_branch_takes_over_the_fallback() {
    let temporary = tempdir().unwrap();
    let path = repository_with_feature(temporary.path(), "next");
    let config = temporary.path().join("config.json");
    fs::write(&config, r#"{"branches": {"integration": ["next"]}}"#).unwrap();
    let config = config.to_str().unwrap();

    // `next` is the integration branch now. `c` is the only commit not on it
    // (Unique to `main`); `a` predates the switch to `next`, so the reflog
    // says it was made on `main`; `b` was made while `next` was out, which is
    // the integration branch itself.
    let by_branch = git_report(
        &path,
        &arguments(&["--group-by", "branch", "--config", config]),
    );
    let expected: BTreeMap<String, u64> = [("main".to_string(), 2), ("next".to_string(), 1)].into();
    assert_eq!(expected, commits_by(&by_branch, &["branch"]));
}

#[test]
fn an_invalid_issue_pattern_stops_the_run_and_names_the_key() {
    let temporary = tempdir().unwrap();
    let path = repository_with_feature(temporary.path(), "feature/x");
    let config = temporary.path().join("config.json");
    fs::write(&config, r#"{"issues": {"patterns": ["(?P<key>ok)", "("]}}"#).unwrap();

    let output = run(&[
        "--dir",
        &path,
        "--no-ai",
        "--no-cache",
        "--no-progress",
        "--format",
        "json",
        "--config",
        config.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("patterns"), "{error}");
    assert!(error.contains("[1]"), "{error}");
}

/// A report over one Pi session in March 2026 whose working directory is
/// `project`, with Git read as well.
fn session_report(directory: &Path, project: &Path, extra: &[&str]) -> Value {
    let history = directory.join("pi-sessions");
    pi_session(&history, "s1", project, "claude-opus-5", 10);
    let history = format!("pi={}", history.display());
    let config = directory.join("missing-config.json");
    let mut arguments = vec![
        "--dir",
        project.to_str().unwrap(),
        "--author",
        "fixture@example.com",
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
    json_stdout(&run(&arguments))
}

#[test]
fn a_session_without_a_recorded_branch_follows_its_checkout() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "project");
    commit_on(&path, "a.txt", "a\n", "2026-02-27");
    // The checkout moved to the feature branch the day before the session
    // (2026-03-02) and is still there.
    git_at(
        &path,
        "2026-03-01T10:00:00Z",
        &["checkout", "-q", "-b", "feature/ACME-7-login"],
    );

    let report = session_report(
        temporary.path(),
        Path::new(&path),
        &["--group-by", "branch,issue,feature"],
    );
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(1, rows.len(), "{rows:?}");
    assert_eq!("feature/ACME-7-login", rows[0]["key"]["branch"]);
    assert_eq!("ACME-7", rows[0]["key"]["issue"]);
    assert_eq!("ACME-7", rows[0]["key"]["feature"]);
    assert_eq!(1, rows[0]["session_count"]);
}

#[test]
fn a_session_before_a_switch_is_on_the_branch_the_switch_left() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "project");
    commit_on(&path, "a.txt", "a\n", "2026-02-27");
    // The session ran on 2026-03-02, before the checkout moved on.
    git_at(
        &path,
        "2026-03-10T10:00:00Z",
        &["checkout", "-q", "-b", "feature/later"],
    );

    let report = session_report(
        temporary.path(),
        Path::new(&path),
        &["--group-by", "branch"],
    );
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(1, rows.len(), "{rows:?}");
    assert_eq!("main", rows[0]["key"]["branch"]);
}

/// A `git` that appends its arguments to `log` and then runs the real one, so
/// a test can see which Git questions a run asked. `WORKSTATS_GIT` points the
/// program at it.
#[cfg(unix)]
fn logging_git(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = directory.join("git-calls.log");
    let script = directory.join("logging-git.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nexec git \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

#[cfg(unix)]
#[test]
fn a_plain_report_asks_git_for_no_branches_but_a_branch_grouping_does() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "project");
    commit_on(&path, "a.txt", "a\n", "2026-02-27");
    git_at(
        &path,
        "2026-03-01T10:00:00Z",
        &["checkout", "-q", "-b", "feature/ACME-7-login"],
    );
    commit_on(&path, "b.txt", "b\n", "2026-03-02");
    let (script, log) = logging_git(temporary.path());
    let script = script.to_str().unwrap().to_string();

    let calls = |extra: &[&str]| -> String {
        let _ = fs::remove_file(&log);
        let mut arguments = vec!["--dir", path.as_str()];
        arguments.extend(extra);
        let mut command = Command::new(binary());
        command.env("WORKSTATS_GIT", &script);
        let history = temporary.path().join("pi-sessions");
        pi_session(&history, "s1", Path::new(&path), "claude-opus-5", 10);
        let history = format!("pi={}", history.display());
        let config = temporary.path().join("missing-config.json");
        command.args(arguments).args([
            "--author",
            "fixture@example.com",
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
        ]);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&log).unwrap_or_default()
    };

    // The questions only the branch enrichment asks.
    let asks_for_branches = |log: &str| {
        log.contains("for-each-ref") || log.contains(" -g ") || log.contains("--source")
    };
    let plain = calls(&[]);
    assert!(plain.contains("log"), "Git was read at all: {plain}");
    assert!(!asks_for_branches(&plain), "{plain}");
    let by_repo = calls(&["--group-by", "repo,day"]);
    assert!(!asks_for_branches(&by_repo), "{by_repo}");
    for grouping in ["branch", "issue", "feature"] {
        let grouped = calls(&["--group-by", grouping]);
        assert!(asks_for_branches(&grouped), "{grouping}: {grouped}");
    }
}

#[test]
fn a_detached_head_recorded_by_the_provider_leaves_the_branch_to_the_checkout() {
    let temporary = tempdir().unwrap();
    let path = repository_on_main(temporary.path(), "project");
    commit_on(&path, "a.txt", "a\n", "2026-02-27");
    git_at(
        &path,
        "2026-03-01T10:00:00Z",
        &["checkout", "-q", "-b", "feature/ACME-7-login"],
    );
    let history = temporary.path().join("claude");
    fs::create_dir_all(history.join("project")).unwrap();
    let line = |kind: &str, time: &str| {
        serde_json::json!({
            "type": kind, "timestamp": format!("2026-03-02T{time}Z"), "cwd": path,
            "sessionId": "c1", "gitBranch": "HEAD",
            "message": {"model": "claude-x", "content": "go"}
        })
        .to_string()
    };
    fs::write(
        history.join("project/session.jsonl"),
        [line("user", "09:00:00"), line("assistant", "09:01:00")].join("\n"),
    )
    .unwrap();
    let history = format!("claude={}", history.display());
    let config = temporary.path().join("missing-config.json");
    let report = json_stdout(&run(&[
        "--dir",
        &path,
        "--author",
        "fixture@example.com",
        "--no-cache",
        "--no-progress",
        "--no-default-events",
        "--no-update-check",
        "--provider",
        "claude",
        "--history",
        &history,
        "--config",
        config.to_str().unwrap(),
        "--month",
        "2026-03",
        "--format",
        "json",
        "--group-by",
        "branch",
    ]));
    let rows = report["rows"].as_array().unwrap();
    let branches: Vec<&str> = rows
        .iter()
        .map(|row| row["key"]["branch"].as_str().unwrap())
        .collect();
    assert!(!branches.contains(&"HEAD"), "{branches:?}");
    assert!(branches.contains(&"feature/ACME-7-login"), "{branches:?}");
}
