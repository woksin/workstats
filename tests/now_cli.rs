//! `workstats now`: the snapshot shortcut, the template, errors, and the
//! detached refresh, observed through the binary.
//!
//! Every run reads one Pi session written relative to the real clock (the
//! command always reports today and this week), so the figures can be asserted
//! without a fixed date.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as Span, Utc};
use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::binary;

/// One Pi session whose prompt came `minutes_ago` minutes before now.
fn session_now(history: &Path, cwd: &Path, minutes_ago: i64) {
    let start: DateTime<Utc> = Utc::now() - Span::minutes(minutes_ago);
    let stamp = |seconds: i64| (start + Span::seconds(seconds)).to_rfc3339();
    let directory = history.join("--now-fixture--");
    fs::create_dir_all(&directory).unwrap();
    let usage = json!({
        "input": 1, "output": 1000, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1001,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let lines = [
        json!({"type": "session", "version": 3, "id": "now1", "timestamp": stamp(0), "cwd": cwd}),
        json!({"type": "message", "id": "a", "parentId": null, "timestamp": stamp(10),
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}),
        json!({"type": "message", "id": "b", "parentId": "a", "timestamp": stamp(70),
            "message": {"role": "assistant", "model": "claude-opus-5", "provider": "anthropic",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": "done"}]}}),
    ];
    fs::write(
        directory.join("now1.jsonl"),
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
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().to_path_buf();
        fs::create_dir_all(root.join("project")).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn history(&self) -> PathBuf {
        self.root.join("pi-sessions")
    }

    fn snapshot(&self) -> PathBuf {
        self.root.join("state").join("now.json")
    }

    fn write_session(&self, minutes_ago: i64) {
        session_now(&self.history(), &self.root.join("project"), minutes_ago);
    }

    fn command(&self, extra: &[&str]) -> Command {
        let mut command = Command::new(binary());
        command
            .arg("now")
            .args([
                "--no-git",
                "--no-cache",
                "--no-default-events",
                "--provider",
                "pi",
                "--history",
                &format!("pi={}", self.history().display()),
            ])
            .args(extra)
            .env("WORKSTATS_CONFIG", self.root.join("missing-config.json"))
            .env("WORKSTATS_NOW_CACHE", self.snapshot())
            .env_remove("WORKSTATS_AUTHOR");
        command
    }

    fn now(&self, extra: &[&str]) -> Output {
        self.command(extra).output().unwrap()
    }

    fn line(&self, extra: &[&str]) -> String {
        let output = self.now(extra);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn stored(&self) -> Value {
        serde_json::from_slice(&fs::read(self.snapshot()).unwrap()).unwrap()
    }
}

#[test]
fn a_fresh_snapshot_is_printed_without_reading_the_history() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let template = "{prompts}|{sessions}|{active}|{commits}";
    let first = fixture.line(&["--template", template]);
    assert_eq!("1|1|●|0", first);
    assert!(
        fixture.snapshot().is_file(),
        "the first run leaves a snapshot"
    );

    // Take the history away: only a recompute could notice.
    fs::remove_dir_all(fixture.history()).unwrap();
    assert_eq!(first, fixture.line(&["--template", template]));

    // A different template over the same flags still uses the snapshot.
    assert_eq!("1", fixture.line(&["--template", "{prompts}"]));

    // `--max-age 0` never trusts it, and now sees nothing.
    assert_eq!(
        "0|0||0",
        fixture.line(&["--template", template, "--max-age", "0"])
    );
}

#[test]
fn other_flags_do_not_reuse_the_snapshot() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    assert_eq!("1", fixture.line(&["--template", "{prompts}"]));
    fs::remove_dir_all(fixture.history()).unwrap();
    // Same snapshot file, different flags: recomputed, so the removal shows.
    assert_eq!(
        "0",
        fixture.line(&["--template", "{prompts}", "--provider", "claude"])
    );
}

#[test]
fn the_config_now_block_supplies_the_defaults() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let config = fixture.root.join("config.json");
    fs::write(
        &config,
        json!({"now": {"template": "[{prompts}]", "max_age": "1h", "active_within": "1m"}})
            .to_string(),
    )
    .unwrap();
    let config = config.to_str().unwrap();
    assert_eq!("[1]", fixture.line(&["--config", config]));
    fs::remove_dir_all(fixture.history()).unwrap();
    // One hour of freshness from the config: still the snapshot.
    assert_eq!("[1]", fixture.line(&["--config", config]));
    // The flag overrides the config.
    assert_eq!("[0]", fixture.line(&["--config", config, "--max-age", "0"]));
}

#[test]
fn editing_the_config_invalidates_the_snapshot() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let config = fixture.root.join("config.json");
    fs::write(&config, "{}").unwrap();
    let config = config.to_str().unwrap();
    assert_eq!(
        "1",
        fixture.line(&["--config", config, "--template", "{prompts}"])
    );
    fs::remove_dir_all(fixture.history()).unwrap();
    // A different size is enough to change the fingerprint.
    fs::write(
        fixture.root.join("config.json"),
        r#"{"check_updates": false}"#,
    )
    .unwrap();
    assert_eq!(
        "0",
        fixture.line(&["--config", config, "--template", "{prompts}"])
    );
}

#[test]
fn json_output_carries_the_figures() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let output = fixture.now(&["--format", "json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(false, value["stale"]);
    assert_eq!(1, value["prompts"]);
    assert!(value["human_seconds"].as_f64().unwrap() > 0.0);
    assert!(value["week_human_seconds"].as_f64().unwrap() > 0.0);
    assert!(
        value["value_month_usd"].is_number(),
        "json includes the month"
    );
    assert_eq!("pi", value["active"]["provider"]);
    assert!(value["goals"]["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn an_old_session_is_not_active_and_other_formats_are_refused() {
    let fixture = Fixture::new();
    // 30 minutes ago, with the default 10 minute window: not active, but it
    // still counts toward today's figures.
    fixture.write_session(30);
    assert_eq!("1|", fixture.line(&["--template", "{prompts}|{active}"]));
    assert_eq!(
        "●",
        fixture.line(&[
            "--template",
            "{active}",
            "--active-within",
            "2h",
            "--max-age",
            "0"
        ])
    );
    let refused = fixture.now(&["--format", "csv"]);
    assert_eq!(Some(2), refused.status.code());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not available for `workstats now`"));
}

#[test]
fn window_flags_are_refused() {
    let fixture = Fixture::new();
    let output = fixture.now(&["--since", "2026-01-01"]);
    assert_eq!(Some(2), output.status.code());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--since"));
}

#[test]
fn an_unknown_token_is_an_error_unless_errors_are_quiet() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let output = fixture.now(&["--template", "{human} {nope}"]);
    assert_eq!(Some(2), output.status.code());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("{nope}"));

    let quiet = fixture.now(&["--template", "{human} {nope}", "--quiet-errors"]);
    assert_eq!(Some(0), quiet.status.code());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());
}

#[test]
fn a_bad_goals_block_stops_now_but_not_with_no_goals() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let config = fixture.root.join("config.json");
    fs::write(&config, json!({"goals": {"weekly_hours": -3}}).to_string()).unwrap();
    let config = config.to_str().unwrap();
    let output = fixture.now(&["--config", config]);
    assert_eq!(Some(2), output.status.code());
    assert!(String::from_utf8_lossy(&output.stderr).contains("goals"));
    assert_eq!(
        "1",
        fixture.line(&["--config", config, "--no-goals", "--template", "{prompts}"])
    );
}

#[test]
fn goals_set_the_warning_the_week_target_and_the_cap_share() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    let config = fixture.root.join("config.json");
    // A cap of one cent: any priced token usage is over it.
    fs::write(
        &config,
        json!({"goals": {"weekly_hours": 10,
            "list_value_caps": [{"pool": "claude", "period": "month", "usd": 0.01}]}})
        .to_string(),
    )
    .unwrap();
    let config = config.to_str().unwrap();
    let line = fixture.line(&["--config", config, "--template", "{week_target}h{warn}"]);
    assert!(line.starts_with("10h ⚠ claude "), "{line}");
    let off = fixture.line(&[
        "--config",
        config,
        "--no-goals",
        "--template",
        "[{week_target}{warn}]",
    ]);
    assert_eq!("[]", off);
}

#[test]
fn rebuild_cache_removes_the_snapshot_from_any_command() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    fixture.line(&[]);
    assert!(fixture.snapshot().is_file());
    let output = Command::new(binary())
        .args([
            "--no-ai",
            "--no-git",
            "--no-default-events",
            "--no-progress",
            "--rebuild-cache",
            "--format",
            "json",
            "--config",
            fixture.root.join("missing-config.json").to_str().unwrap(),
        ])
        .env("WORKSTATS_NOW_CACHE", fixture.snapshot())
        .env(
            "WORKSTATS_CACHE",
            fixture.root.join("state").join("index.sqlite3"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.snapshot().exists());
}

#[test]
fn no_wait_prints_the_old_line_and_a_detached_copy_refreshes_it() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    fixture.line(&["--template", "{prompts}"]);
    let before = fixture.stored()["computed_at"]
        .as_str()
        .unwrap()
        .to_string();

    // Never fresh, so it is stale; --no-wait answers from the file and marks it.
    let line = fixture.line(&[
        "--template",
        "{prompts}{stale}",
        "--max-age",
        "0",
        "--no-wait",
    ]);
    assert_eq!("1~", line);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let current = fixture.stored()["computed_at"]
            .as_str()
            .unwrap()
            .to_string();
        if current != before {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the background refresh never replaced the snapshot"
        );
        sleep(Duration::from_millis(100));
    }
    // The refresh lets go of its lock when it is done.
    let lock = fixture.snapshot().with_extension("lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while lock.exists() {
        assert!(Instant::now() < deadline, "the lock was never released");
        sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_fresh_lock_stops_no_wait_from_starting_another_refresh() {
    let fixture = Fixture::new();
    fixture.write_session(5);
    fixture.line(&["--template", "{prompts}"]);
    let before = fixture.stored()["computed_at"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(fixture.snapshot().with_extension("lock"), "999999").unwrap();

    let line = fixture.line(&[
        "--template",
        "{prompts}{stale}",
        "--max-age",
        "0",
        "--no-wait",
    ]);
    assert_eq!("1~", line);
    sleep(Duration::from_millis(1500));
    assert_eq!(
        before,
        fixture.stored()["computed_at"].as_str().unwrap(),
        "nothing may refresh while the lock is held"
    );
    assert!(fixture.snapshot().with_extension("lock").exists());
}
