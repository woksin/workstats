//! `workstats insights` and `workstats digest`, end to end over fixture
//! histories. The computations are tested on synthetic timelines beside the
//! code; these tests pin what the commands add around them: the default
//! windows, the section filter, the formats and agreement with the report.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::tempdir;

mod common;
use common::*;

/// One Pi session on 2026-03-02 with a prompt and an assistant reply at each of
/// `times` (UTC, `HH:MM`), in `cwd`, so there is a known amount of human and
/// agent time to look for.
fn pi_history(directory: &Path, cwd: &Path, times: &[&str]) -> PathBuf {
    let history = directory.join("pi-sessions");
    let session = history.join("--s1--");
    fs::create_dir_all(&session).unwrap();
    let usage = serde_json::json!({
        "input": 1000, "output": 5000, "cacheRead": 2000, "cacheWrite": 100,
        "totalTokens": 8100,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let mut lines = vec![
        serde_json::json!({"type": "session", "version": 3, "id": "s1",
        "timestamp": "2026-03-02T08:00:00.000Z", "cwd": cwd}),
    ];
    for (index, time) in times.iter().enumerate() {
        lines.push(
            serde_json::json!({"type": "message", "id": format!("u{index}"),
            "parentId": null, "timestamp": format!("2026-03-02T{time}:00.000Z"),
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}),
        );
        lines.push(
            serde_json::json!({"type": "message", "id": format!("a{index}"),
            "parentId": null, "timestamp": format!("2026-03-02T{time}:40.000Z"),
            "message": {"role": "assistant", "model": "claude-opus-5", "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "done"}]}}),
        );
    }
    fs::write(
        session.join("2026-03-02T08-00-00-000Z_s1.jsonl"),
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    history
}

struct Fixture {
    _directory: tempfile::TempDir,
    base: Vec<String>,
}

fn fixture() -> Fixture {
    let directory = tempdir().unwrap();
    let project = directory.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let history = pi_history(
        directory.path(),
        &project,
        &["08:00", "08:20", "08:40", "09:10", "13:00", "13:05"],
    );
    let config = directory.path().join("missing-config.json");
    let base = [
        "--no-git",
        "--no-cache",
        "--no-progress",
        "--no-default-events",
        "--no-update-check",
        "--provider",
        "pi",
        "--history",
        &format!("pi={}", history.display()),
        "--config",
        config.to_str().unwrap(),
    ]
    .map(String::from)
    .to_vec();
    Fixture {
        _directory: directory,
        base,
    }
}

fn command(name: &str, fixture: &Fixture, extra: &[&str]) -> Vec<String> {
    let mut arguments = vec![name.to_string()];
    arguments.extend(fixture.base.iter().cloned());
    arguments.extend(extra.iter().map(ToString::to_string));
    arguments
}

fn run_strings(arguments: &[String]) -> std::process::Output {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run(&arguments)
}

fn json_of(name: &str, fixture: &Fixture, extra: &[&str]) -> Value {
    let mut arguments = extra.to_vec();
    arguments.extend(["--format", "json"]);
    json_stdout(&run_strings(&command(name, fixture, &arguments)))
}

fn text_of(name: &str, fixture: &Fixture, extra: &[&str]) -> String {
    let output = run_strings(&command(name, fixture, extra));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn stderr_of(name: &str, fixture: &Fixture, extra: &[&str]) -> String {
    let output = run_strings(&command(name, fixture, extra));
    assert!(!output.status.success(), "expected {extra:?} to be refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const MARCH: &[&str] = &["--since", "2026-03-01", "--until", "2026-03-31"];

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("{value} is not a number"))
}

#[test]
fn insights_agree_with_the_report_over_the_same_window() {
    let fixture = fixture();
    let insights = json_of("insights", &fixture, MARCH);
    let mut report_arguments = fixture.base.clone();
    report_arguments.extend(MARCH.iter().map(ToString::to_string));
    report_arguments.extend(["--format".to_string(), "json".to_string()]);
    let report = json_stdout(&run_strings(&report_arguments));
    let summary = &report["summary"];

    let human = number(&summary["human_estimated_seconds"]);
    assert!(human > 0.0);
    assert_eq!(
        human,
        number(&insights["focus"]["aggregate"]["human_seconds"])
    );
    assert_eq!(human, number(&insights["leverage"]["human_seconds"]));
    assert_eq!(
        number(&summary["agent_wall_seconds"]),
        number(&insights["leverage"]["agent_wall_seconds"])
    );
    assert_eq!(
        number(&summary["parallel_agent_seconds"]),
        number(&insights["leverage"]["parallel_agent_seconds"])
    );
    assert_eq!(
        summary["total_tokens"], insights["leverage"]["tokens"],
        "tokens"
    );
    assert_eq!(
        summary["foreground_sessions_with_commits"],
        insights["leverage"]["foreground_sessions_with_commits"]
    );
    assert_eq!(
        summary["foreground_sessions_without_commits"],
        insights["leverage"]["foreground_sessions_without_commits"]
    );
    // Two work blocks: the morning and the 13:00 pair.
    assert_eq!(2, insights["focus"]["aggregate"]["block_count"]);
    // The heatmap holds exactly the human time, whatever the local zone.
    let matrix_total: f64 = insights["patterns"]["human_seconds"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap())
        .map(number)
        .sum();
    assert!(
        (matrix_total - human).abs() < 0.01,
        "{matrix_total} against {human}"
    );
    // No Git was read, so ratios per commit have nothing to divide by.
    assert_eq!(Value::Null, insights["leverage"]["tokens_per_commit"]);
    assert_eq!(Value::Null, insights["leverage"]["list_value_per_commit"]);
    assert!(insights["leverage"]["tokens_per_human_hour"].is_number());
    let models = insights["models"]["models"].as_array().unwrap();
    assert_eq!(1, models.len());
    assert_eq!("claude-opus-5", models[0]["model"]);
    assert_eq!(
        insights["leverage"]["list_value_usd"],
        models[0]["list_value_usd"]
    );
}

#[test]
fn insights_state_and_apply_their_default_window() {
    let fixture = fixture();
    let insights = json_of("insights", &fixture, &[]);
    assert_eq!(true, insights["window"]["defaulted"]);
    assert!(
        insights["window"]["label"]
            .as_str()
            .unwrap()
            .contains("last 28 days"),
        "{}",
        insights["window"]
    );
    assert!(insights["window"]["since"].is_string());
    assert_eq!(Value::Null, insights["window"]["until"]);
    // The fixture is in 2026-03, nowhere near the last 28 days of any run that
    // matters, so the window being applied means nothing was found.
    assert_eq!(0, insights["focus"]["aggregate"]["block_count"]);

    let table = text_of("insights", &fixture, &[]);
    assert!(table.contains("the last 28 days"), "{table}");

    let explicit = json_of("insights", &fixture, MARCH);
    assert_eq!(false, explicit["window"]["defaulted"]);
}

#[test]
fn the_section_flag_limits_what_is_computed_into_the_output() {
    let fixture = fixture();
    let only = json_of(
        "insights",
        &fixture,
        &[MARCH, &["--section", "focus"]].concat(),
    );
    assert!(only.get("focus").is_some());
    for absent in ["patterns", "leverage", "models"] {
        assert!(only.get(absent).is_none(), "{absent} should be absent");
    }
    let two = json_of(
        "insights",
        &fixture,
        &[MARCH, &["--section", "heatmap,models"]].concat(),
    );
    assert!(two.get("patterns").is_some());
    assert!(two.get("models").is_some());
    assert!(two.get("focus").is_none());
    assert!(two.get("leverage").is_none());

    let table = text_of(
        "insights",
        &fixture,
        &[MARCH, &["--section", "leverage"]].concat(),
    );
    assert!(table.contains("Leverage"));
    assert!(!table.contains("Heatmap"));
    assert!(!table.contains("Focus"));

    let refused = stderr_of("insights", &fixture, &["--section", "nonsense"]);
    assert!(refused.contains("nonsense"), "{refused}");
}

#[test]
fn csv_and_a_compare_flag_are_refused_for_insights() {
    let fixture = fixture();
    let csv = stderr_of("insights", &fixture, &["--format", "csv"]);
    assert!(csv.contains("csv"), "{csv}");
    let compare = stderr_of(
        "insights",
        &fixture,
        &["--month", "2026-03", "--compare", "previous"],
    );
    assert!(compare.contains("digest"), "{compare}");
    let csv = stderr_of("digest", &fixture, &["--format", "csv"]);
    assert!(csv.contains("csv"), "{csv}");
}

#[test]
fn a_configured_csv_default_is_refused_with_its_origin() {
    let fixture = fixture();
    let directory = tempdir().unwrap();
    let config = directory.path().join("config.json");
    fs::write(&config, r#"{"defaults": {"format": "csv"}}"#).unwrap();
    let mut arguments = command("insights", &fixture, &[]);
    let position = arguments
        .iter()
        .position(|argument| argument == "--config")
        .unwrap();
    arguments[position + 1] = config.to_str().unwrap().to_string();
    let output = run_strings(&arguments);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("defaults.format"), "{stderr}");
}

#[test]
fn a_bad_insights_config_is_refused_naming_the_key() {
    let fixture = fixture();
    let directory = tempdir().unwrap();
    let config = directory.path().join("config.json");
    fs::write(&config, r#"{"insights": {"night": ["22:00"]}}"#).unwrap();
    let mut arguments = command("insights", &fixture, MARCH);
    let position = arguments
        .iter()
        .position(|argument| argument == "--config")
        .unwrap();
    arguments[position + 1] = config.to_str().unwrap().to_string();
    let output = run_strings(&arguments);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("insights.night"), "{stderr}");
}

#[test]
fn the_night_and_weekend_windows_come_from_the_config() {
    let fixture = fixture();
    let directory = tempdir().unwrap();
    let config = directory.path().join("config.json");
    // All day is "night" and every weekday is "weekend", so every human second
    // lands in both, whatever the local zone.
    fs::write(
        &config,
        r#"{"insights": {"night": ["00:00", "23:59"], "weekend": ["mon","tue","wed","thu","fri","sat","sun"]}}"#,
    )
    .unwrap();
    let mut arguments = command(
        "insights",
        &fixture,
        &[MARCH, &["--format", "json"]].concat(),
    );
    let position = arguments
        .iter()
        .position(|argument| argument == "--config")
        .unwrap();
    arguments[position + 1] = config.to_str().unwrap().to_string();
    let insights = json_stdout(&run_strings(&arguments));
    let patterns = &insights["patterns"];
    let human = number(&insights["focus"]["aggregate"]["human_seconds"]);
    assert_eq!(human, number(&patterns["weekend"]["human_seconds"]));
    assert_eq!("00:00", patterns["night"]["from"]);
    assert_eq!("23:59", patterns["night"]["to"]);
    // Only the last minute of the day is outside the window.
    assert!(number(&patterns["night"]["human_seconds"]) >= human - 120.0);
    assert_eq!(7, patterns["weekend"]["weekdays"].as_array().unwrap().len());
}

#[test]
fn a_digest_defaults_to_last_week_compared_with_the_week_before() {
    let fixture = fixture();
    let digest = json_of("digest", &fixture, &[]);
    assert_eq!(true, digest["window"]["defaulted"]);
    let comparison = &digest["comparison"];
    assert_eq!("previous", comparison["basis"]);
    let since = comparison["current"]["since"].as_str().unwrap();
    let until = comparison["current"]["until"].as_str().unwrap();
    let span = chrono::DateTime::parse_from_rfc3339(until).unwrap()
        - chrono::DateTime::parse_from_rfc3339(since).unwrap();
    // A week, give or take the hour a daylight-saving change adds or removes.
    assert!(
        (167..=169).contains(&span.num_hours()),
        "{} hours",
        span.num_hours()
    );
    let previous_until = comparison["previous"]["until"].as_str().unwrap();
    assert_eq!(
        since, previous_until,
        "the baseline ends where the week starts"
    );
    for key in ["top_repos", "top_features", "focus", "leverage", "warnings"] {
        assert!(digest.get(key).is_some(), "{key} missing");
    }
    assert!(digest.get("goals").is_none(), "no goals are configured");
}

#[test]
fn a_digest_of_an_open_ended_window_says_it_has_no_comparison() {
    let fixture = fixture();
    let digest = json_of("digest", &fixture, &["--since", "2026-03-01"]);
    assert!(digest.get("comparison").is_none());
    let table = text_of("digest", &fixture, &["--since", "2026-03-01"]);
    assert!(table.contains("No comparison"), "{table}");
}

#[test]
fn a_digest_names_its_repositories_and_renders_in_every_format() {
    let fixture = fixture();
    let month = ["--month", "2026-03"];
    let digest = json_of("digest", &fixture, &month);
    let repos = digest["top_repos"]["rows"].as_array().unwrap();
    assert_eq!(1, repos.len());
    assert_eq!("project", repos[0]["name"]);
    assert_eq!(1.0, number(&repos[0]["share_of_human"]));
    assert_eq!(0, digest["top_repos"]["omitted"]);
    assert_eq!(
        number(&digest["leverage"]["human_seconds"]),
        number(&repos[0]["human_seconds"])
    );
    // The month before has nothing in it, so everything is growth from nothing.
    assert_eq!(
        0.0,
        number(&digest["comparison"]["previous"]["figures"]["human_estimated_seconds"])
    );

    let markdown = text_of(
        "digest",
        &fixture,
        &[&month[..], &["--format", "markdown"]].concat(),
    );
    for heading in [
        "# workstats digest",
        "## Comparison",
        "## Top repositories",
        "## Top features",
        "## Focus",
        "## Leverage",
    ] {
        assert!(
            markdown.contains(heading),
            "{heading} missing from\n{markdown}"
        );
    }
    let html = text_of(
        "digest",
        &fixture,
        &[&month[..], &["--format", "html"]].concat(),
    );
    assert!(html.contains("<table"));
    assert!(!html.contains("<script"));
    let table = text_of("digest", &fixture, &month);
    assert!(table.contains("Top repositories"));
    assert!(table.contains("project"));
}

#[test]
fn a_digest_ranks_features_by_the_branch_the_session_recorded() {
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
            line("user", "09:00:00", "feat-login"),
            line("assistant", "09:01:00", "feat-login"),
            line("user", "13:00:00", "feat-search"),
            line("assistant", "13:01:00", "feat-search"),
            line("user", "13:20:00", "feat-search"),
            line("assistant", "13:21:00", "feat-search"),
        ]
        .join("\n"),
    )
    .unwrap();
    let config = directory.path().join("missing.json");
    let output = run(&[
        "digest",
        "--no-git",
        "--no-cache",
        "--no-progress",
        "--no-default-events",
        "--no-update-check",
        "--provider",
        "claude",
        "--config",
        config.to_str().unwrap(),
        "--month",
        "2026-03",
        "--format",
        "json",
        "--history",
        &format!("claude={}", history.display()),
    ]);
    let digest = json_stdout(&output);
    let names: Vec<&str> = digest["top_features"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(2, names.len(), "{names:?}");
    assert!(names.iter().all(|name| *name != "—"), "{names:?}");
    // The afternoon branch has more human time.
    assert!(names[0].contains("search"), "{names:?}");
}
