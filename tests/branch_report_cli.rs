//! `workstats branch` and `workstats pr`: the effort behind one branch, from a
//! temp repository and a Claude history that worked on two branches at once.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use tempfile::{TempDir, tempdir};

mod common;
use common::*;

/// A repository with `main` and a feature branch of two commits, and a Claude
/// history with two overlapping sessions: one on the feature branch, one on
/// `main`.
struct Fixture {
    _directory: TempDir,
    repo: PathBuf,
    history: String,
    config: PathBuf,
}

const FEATURE: &str = "feat/ACME-1";

fn line(kind: &str, time: &str, session: &str, branch: &str, cwd: &str, output: u64) -> String {
    let mut message = serde_json::json!({"model": "claude-opus-4-8", "content": "go"});
    if kind == "assistant" {
        message["usage"] = serde_json::json!({"input_tokens": 10, "output_tokens": output});
    }
    serde_json::json!({
        "type": kind, "timestamp": format!("2026-03-02T{time}Z"), "cwd": cwd,
        "sessionId": session, "gitBranch": branch, "message": message
    })
    .to_string()
}

fn fixture(feature: &str, pull_request: Option<u64>) -> Fixture {
    let directory = tempdir().unwrap();
    let repo = repository_on_main(directory.path(), "repo");
    commit_on(&repo, "base.txt", "one\n", "2026-03-01");
    assert!(
        git(&["-C", &repo, "checkout", "-q", "-b", feature])
            .status
            .success()
    );
    commit_on(&repo, "a.txt", "a\nb\nc\n", "2026-03-02");
    commit_on(&repo, "b.txt", "x\n", "2026-03-03");

    let history = directory.path().join("claude");
    fs::create_dir_all(history.join("project")).unwrap();
    let mut feature_lines = vec![
        line("user", "11:00:00", "feat1", feature, &repo, 0),
        line("assistant", "11:02:00", "feat1", feature, &repo, 1000),
        line("user", "11:10:00", "feat1", feature, &repo, 0),
        line("assistant", "11:12:00", "feat1", feature, &repo, 1000),
        line("user", "11:20:00", "feat1", feature, &repo, 0),
        line("assistant", "11:25:00", "feat1", feature, &repo, 1000),
    ];
    if let Some(number) = pull_request {
        feature_lines.push(
            serde_json::json!({
                "type": "pr-link", "timestamp": "2026-03-02T11:13:00Z", "sessionId": "feat1",
                "prNumber": number, "prRepository": "acme/api",
                "prUrl": format!("https://example.invalid/acme/api/pull/{number}")
            })
            .to_string(),
        );
    }
    fs::write(
        history.join("project/feature.jsonl"),
        feature_lines.join("\n"),
    )
    .unwrap();
    // Work on main at the same time, in the same checkout.
    let main_lines = [
        line("user", "11:05:00", "main1", "main", &repo, 0),
        line("assistant", "11:06:00", "main1", "main", &repo, 5000),
        line("user", "11:15:00", "main1", "main", &repo, 0),
        line("assistant", "11:17:00", "main1", "main", &repo, 5000),
    ];
    fs::write(history.join("project/main.jsonl"), main_lines.join("\n")).unwrap();

    let config = directory.path().join("missing.json");
    Fixture {
        history: format!("claude={}", history.display()),
        _directory: directory,
        repo: PathBuf::from(repo),
        config,
    }
}

impl Fixture {
    fn command(&self, subcommand: &[&str], extra: &[&str]) -> std::process::Output {
        let mut arguments: Vec<&str> = subcommand.to_vec();
        arguments.extend([
            "--dir",
            self.repo.to_str().unwrap(),
            "--author",
            "fixture@example.com",
            "--provider",
            "claude",
            "--history",
            &self.history,
            "--config",
            self.config.to_str().unwrap(),
            "--no-cache",
            "--no-progress",
            "--no-default-events",
            "--no-update-check",
        ]);
        arguments.extend(extra);
        run(&arguments)
    }

    fn json(&self, subcommand: &[&str], extra: &[&str]) -> Value {
        let mut all = vec!["--format", "json"];
        all.extend(extra);
        json_stdout(&self.command(subcommand, &all))
    }
}

fn entry<'a>(report: &'a Value, branch: &str) -> &'a Value {
    report["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["branch"] == branch)
        .unwrap_or_else(|| panic!("no row for {branch}: {report}"))
}

#[test]
fn human_time_is_the_branch_pieces_and_parallel_main_work_is_excluded() {
    let fixture = fixture(FEATURE, None);
    let report = fixture.json(&["branch", FEATURE], &[]);
    let branch = entry(&report, FEATURE);

    let human = branch["human_seconds"].as_f64().unwrap();
    assert!(human > 0.0);
    // Never the window's total: the main-branch work in the same checkout, at
    // the same hours, is somebody else's piece of the timeline.
    let window_total = report["window_human_seconds"].as_f64().unwrap();
    assert!(human < window_total, "{human} !< {window_total}");

    // The same pieces the ordinary report groups by branch.
    let grouped = json_stdout(&fixture.command(
        &[],
        &[
            "--format",
            "json",
            "--group-by",
            "branch",
            "--month",
            "2026-03",
        ],
    ));
    let row = grouped["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"]["branch"] == FEATURE)
        .unwrap();
    assert_eq!(
        row["human_estimated_seconds"].as_f64().unwrap(),
        human,
        "the branch figure is the report's branch row"
    );

    // Only the feature session's tokens: 3 x (10 + 1000), not main's 10,000.
    assert_eq!(1, branch["sessions"]);
    assert_eq!(3030, branch["tokens"]["total"]);
    assert_eq!(
        vec!["Claude Code"],
        branch["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert!(branch["list_value_usd"].as_f64().unwrap() > 0.0);
    assert!(branch["agent_wall_seconds"].as_f64().unwrap() > 0.0);

    // The two commits on the branch, not the one on main.
    assert_eq!(2, branch["commits"]["own"]);
    assert_eq!(4, branch["commits"]["additions"]);
    assert_eq!("main", branch["base"]);
    assert_eq!("fork point", branch["window"]["since_source"]);
    assert_eq!("estimate", report["status"]);
}

#[test]
fn the_current_branch_is_the_default_and_table_is_the_default_format() {
    let fixture = fixture(FEATURE, None);
    let output = fixture.command(&["branch"], &[]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Branch effort (estimated)"), "{text}");
    assert!(text.contains(FEATURE), "{text}");
    assert!(text.contains("Human time"), "{text}");
}

#[test]
fn all_prints_a_row_per_branch_and_the_rows_add_up_to_no_more_than_the_window() {
    let fixture = fixture(FEATURE, None);
    let report = fixture.json(&["branch"], &["--all"]);
    let rows = report["branches"].as_array().unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["branch"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&FEATURE) && names.contains(&"main"),
        "{names:?}"
    );
    let sum: f64 = rows
        .iter()
        .map(|row| row["human_seconds"].as_f64().unwrap())
        .sum();
    assert!(sum <= report["window_human_seconds"].as_f64().unwrap() + 1e-6);
    // Each row is the single-branch report.
    let single = fixture.json(&["branch", FEATURE], &[]);
    assert_eq!(
        entry(&single, FEATURE)["human_seconds"],
        entry(&report, FEATURE)["human_seconds"]
    );
    // Main's own sessions are on main's row only.
    assert_eq!(10_020, entry(&report, "main")["tokens"]["total"]);
}

#[test]
fn markdown_defuses_issue_references_in_branch_names() {
    let fixture = fixture("fix/#123", None);
    let output = fixture.command(&["pr", "fix/#123"], &[]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("**Effort (estimated):** \\~"), "{text}");
    assert!(text.contains("2 commits"), "{text}");
    assert!(text.contains("1 session (Claude Code)"), "{text}");
    assert!(
        !text.contains("#123"),
        "an issue reference would link: {text}"
    );
    assert!(text.contains("#\u{200B}123"), "{text}");

    // The branch command's own Markdown goes through the same escape.
    let branch = fixture.command(&["branch", "fix/#123"], &["--format", "markdown"]);
    let text = String::from_utf8_lossy(&branch.stdout);
    assert!(!text.contains("#123"), "{text}");
}

#[test]
fn pr_number_is_resolved_through_the_sessions_that_linked_it() {
    let fixture = fixture(FEATURE, Some(42));
    let report = fixture.json(&["pr"], &["--number", "42"]);
    assert_eq!(42, report["pull_request"]);
    let branches = report["branches"].as_array().unwrap();
    assert_eq!(1, branches.len());
    assert_eq!(FEATURE, branches[0]["branch"]);
    assert_eq!(2, branches[0]["commits"]["own"]);
    assert!(branches[0]["human_seconds"].as_f64().unwrap() > 0.0);

    let by_name = fixture.json(&["pr", FEATURE], &[]);
    assert_eq!(
        entry(&by_name, FEATURE)["human_seconds"],
        branches[0]["human_seconds"]
    );

    let stderr = failure(
        &[
            "pr".to_string(),
            "--dir".to_string(),
            fixture.repo.to_str().unwrap().to_string(),
            "--author".to_string(),
            "fixture@example.com".to_string(),
            "--provider".to_string(),
            "claude".to_string(),
            "--history".to_string(),
            fixture.history.clone(),
            "--config".to_string(),
            fixture.config.to_str().unwrap().to_string(),
            "--no-cache".to_string(),
            "--no-default-events".to_string(),
        ],
        &["--number", "7"],
    );
    assert!(
        stderr.contains("no session in this repository linked pull request #7"),
        "{stderr}"
    );
}

#[test]
fn describe_adds_commit_subjects_and_session_titles_only_when_asked() {
    let fixture = fixture(FEATURE, None);
    // A tool-generated title on the feature session, and a prompt-bearing
    // record that must never be read as a title.
    let path = fixture.history.trim_start_matches("claude=").to_string() + "/project/feature.jsonl";
    let mut text = fs::read_to_string(&path).unwrap();
    text.push('\n');
    text.push_str(
        &serde_json::json!({"type": "ai-title", "sessionId": "feat1", "aiTitle": "Add the ACME widget"})
            .to_string(),
    );
    text.push('\n');
    text.push_str(
        &serde_json::json!({"type": "last-prompt", "sessionId": "feat1", "lastPrompt": "SECRET PROMPT TEXT"})
            .to_string(),
    );
    fs::write(&path, text).unwrap();

    let plain = fixture.json(&["branch", FEATURE], &[]);
    assert!(entry(&plain, FEATURE).get("description").is_none());

    let commits = fixture.json(&["branch", FEATURE], &["--describe", "commits"]);
    let description = entry(&commits, FEATURE)["description"].as_str().unwrap();
    assert_eq!(description, "a.txt; b.txt", "{commits}");

    let both = fixture.json(&["pr", FEATURE], &["--describe", "commits,sessions"]);
    let description = entry(&both, FEATURE)["description"].as_str().unwrap();
    assert_eq!(description, "Add the ACME widget; a.txt; b.txt", "{both}");
    assert!(!both.to_string().contains("SECRET PROMPT TEXT"));

    let refused = fixture.command(&["branch", FEATURE], &["--describe", "prompts"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unknown --describe source"));
}

#[test]
fn the_default_markdown_pr_block_carries_the_description() {
    let fixture = fixture(FEATURE, None);
    let path = fixture
        ._directory
        .path()
        .join("claude/project/feature.jsonl");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push('\n');
    text.push_str(
        &serde_json::json!({"type": "ai-title", "sessionId": "feat1", "aiTitle": "Add the *ACME* widget"})
            .to_string(),
    );
    text.push('\n');
    text.push_str(
        &serde_json::json!({"type": "last-prompt", "sessionId": "feat1", "lastPrompt": "SECRET PROMPT TEXT"})
            .to_string(),
    );
    fs::write(&path, text).unwrap();

    // No --format: the Markdown block is what `pr` prints.
    let plain = fixture.command(&["pr", FEATURE], &[]);
    let plain = String::from_utf8_lossy(&stdout_of(&plain)).into_owned();
    assert!(!plain.contains("Description"), "{plain}");

    let described = fixture.command(&["pr", FEATURE], &["--describe", "commits,sessions"]);
    let text = String::from_utf8_lossy(&stdout_of(&described)).into_owned();
    assert!(text.contains("**Effort (estimated):**"), "{text}");
    assert!(
        text.contains("**Description:** Add the \\*ACME\\* widget; a.txt; b.txt"),
        "{text}"
    );
    assert!(!text.contains("SECRET PROMPT TEXT"), "{text}");
}

/// The stdout of a command that must have succeeded.
fn stdout_of(output: &std::process::Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout.clone()
}

#[test]
fn a_missing_branch_and_a_base_branch_without_a_window_are_refused() {
    let fixture = fixture(FEATURE, None);
    let output = fixture.command(&["branch", "no-such-branch"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no local branch named"));

    let output = fixture.command(&["branch", "main"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("fork point"));
    // A window the user states is enough.
    let output = fixture.command(
        &["branch", "main"],
        &["--month", "2026-03", "--format", "json"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
