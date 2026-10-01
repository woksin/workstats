//! Opt-in descriptions end to end: commit subjects, session titles and the
//! summarizer, and the privacy boundary around them. Dates are chosen at noon
//! and mid-morning UTC so the day an activity falls on does not depend on the
//! timezone the suite runs in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::*;

const HUMAN: &str = "Fixture <fixture@example.com>";
const AGENT: &str = "Copilot <5551212+Copilot@users.noreply.github.com>";

/// Text that must never reach any output: the prompts and queue entries a
/// Claude transcript holds, the agent's commit subject and a Codex prompt.
const SECRETS: &[&str] = &[
    "LAST_PROMPT_SECRET",
    "QUEUE_SECRET",
    "USER_PROMPT_SECRET",
    "AGENT_SUBJECT_SECRET",
    "CODEX_PROMPT_SECRET",
];

struct Fixture {
    _directory: tempfile::TempDir,
    config: PathBuf,
    ledger: PathBuf,
    cache: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    root: PathBuf,
}

fn commit(repo: &str, file: &str, author: &str, subject: &str) {
    fs::write(Path::new(repo).join(file), subject).unwrap();
    assert!(git(&["-C", repo, "add", "."]).status.success());
    let date = "2026-03-02T12:00:00Z";
    let author = format!("--author={author}");
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
            subject,
            author.as_str(),
        ])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Fixture {
    let directory = tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let acme = repository_on_main(&root, "acme");
    // The subject carries a right-to-left override and runs past 200 characters.
    let hostile = format!("Fix rounding \u{202e}gnidnuof {}", "x".repeat(300));
    commit(&acme, "a.txt", HUMAN, "Add the invoice export");
    commit(&acme, "b.txt", HUMAN, &hostile);
    commit(&acme, "c.txt", AGENT, "AGENT_SUBJECT_SECRET");

    let claude = root.join("claude");
    fs::create_dir_all(claude.join("project")).unwrap();
    let line = |value: Value| value.to_string();
    let record = |kind: &str, time: &str| {
        line(json!({
            "type": kind, "timestamp": format!("2026-03-02T{time}Z"), "cwd": acme,
            "sessionId": "c1", "message": {"model": "claude-x", "content": "USER_PROMPT_SECRET"}
        }))
    };
    fs::write(
        claude.join("project/session.jsonl"),
        [
            record("user", "09:00:00"),
            record("assistant", "09:01:00"),
            line(json!({"type": "summary", "summary": "Legacy summary"})),
            line(json!({"type": "ai-title", "aiTitle": "Reconcile the invoices"})),
            line(json!({"type": "last-prompt", "lastPrompt": "LAST_PROMPT_SECRET"})),
            line(json!({"type": "queue-operation", "content": "QUEUE_SECRET"})),
            record("user", "09:05:00"),
            record("assistant", "09:06:00"),
        ]
        .join("\n"),
    )
    .unwrap();

    // A Codex rollout in the same repository, named by the index only.
    let codex = root.join("codex-home");
    let rollouts = codex.join("sessions/2026/03/02");
    fs::create_dir_all(&rollouts).unwrap();
    let rollout = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex/rollout-2026-01-01T10-00-00-fixture.jsonl"),
    )
    .unwrap()
    .replace("2026-01-01T10", "2026-03-02T10")
    .replace("/home/example/project", &acme.replace('\\', "\\\\"));
    fs::write(
        rollouts.join("rollout-2026-03-02T10-00-00-fixture.jsonl"),
        rollout,
    )
    .unwrap();
    fs::write(
        codex.join("session_index.jsonl"),
        json!({"id": "codex-fixture", "thread_name": "Codex thread name",
            "title": "CODEX_PROMPT_SECRET"})
        .to_string(),
    )
    .unwrap();

    let config = root.join("config.json");
    fs::write(
        &config,
        json!({"engagements": {"acme": {
            "label": "ACME", "client": "ACME AS", "rate": 1000, "currency": "NOK",
            "paths": [acme]
        }}})
        .to_string(),
    )
    .unwrap();
    Fixture {
        ledger: root.join("timesheet.json"),
        cache: root.join("index.sqlite3"),
        config,
        claude,
        codex,
        root,
        _directory: directory,
    }
}

impl Fixture {
    fn command(&self, head: &[&str], extra: &[&str]) -> Command {
        let history = format!("claude={}", self.claude.display());
        let mut command = Command::new(binary());
        command
            .env("WORKSTATS_CONFIG", &self.config)
            .env("WORKSTATS_TIMESHEET", &self.ledger)
            .env("WORKSTATS_CACHE", &self.cache)
            .arg("timesheet")
            .args(head)
            .args([
                "--no-progress",
                "--no-default-events",
                "--no-update-check",
                "--provider",
                "claude,codex",
                "--history",
                history.as_str(),
                "--codex-dir",
                self.codex.join("sessions").to_str().unwrap(),
                "--codex-db",
                self.root.join("missing.sqlite").to_str().unwrap(),
                "--dir",
                self.root.to_str().unwrap(),
                "--author",
                "Fixture",
                "--agent-commits",
                "--config",
                self.config.to_str().unwrap(),
            ])
            .args(extra);
        command
    }

    fn sheet(&self, extra: &[&str]) -> Output {
        let mut arguments = vec!["--month", "2026-03", "--format", "json", "--no-cache"];
        arguments.extend_from_slice(extra);
        self.command(&[], &arguments).output().unwrap()
    }
}

fn ok(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn everything(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn descriptions(sheet: &Value) -> Vec<String> {
    sheet["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["description"].as_str().map(str::to_string))
        .collect()
}

fn assert_no_secrets(text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
}

#[test]
fn nothing_is_described_unless_asked() {
    let fixture = fixture();
    let output = fixture.sheet(&[]);
    let sheet = ok(&output);
    assert!(descriptions(&sheet).is_empty());
    let text = everything(&output);
    assert_no_secrets(&text);
    assert!(!text.contains("Add the invoice export"));
}

#[test]
fn commit_subjects_are_the_users_own_bounded_and_sanitised() {
    let fixture = fixture();
    let output = fixture.sheet(&["--describe", "commits"]);
    let sheet = ok(&output);
    let found = descriptions(&sheet);
    assert_eq!(1, found.len(), "{found:?}");
    let text = &found[0];
    assert!(text.contains("Add the invoice export"), "{text}");
    assert!(text.contains("Fix rounding"), "{text}");
    assert!(!text.contains('\u{202e}'), "{text}");
    assert!(!text.contains("Reconcile"), "no session titles asked for");
    // Each subject is cut at 200 characters, the whole line at 500.
    assert!(text.chars().count() <= 500, "{}", text.chars().count());
    assert!(
        !text.contains(&"x".repeat(250)),
        "the long subject was not bounded"
    );
    assert_no_secrets(&everything(&output));
}

#[test]
fn session_titles_come_from_title_fields_only_and_codex_needs_naming() {
    let fixture = fixture();
    let output = fixture.sheet(&["--describe", "sessions"]);
    let found = descriptions(&ok(&output)).join(" | ");
    assert!(found.contains("Reconcile the invoices"), "{found}");
    assert!(
        !found.contains("Legacy summary"),
        "the last title wins: {found}"
    );
    assert!(
        !found.contains("Codex thread name"),
        "Codex is excluded unless named: {found}"
    );
    assert!(!found.contains("Add the invoice export"));
    assert_no_secrets(&everything(&output));

    let output = fixture.sheet(&["--describe", "sessions=codex"]);
    let found = descriptions(&ok(&output)).join(" | ");
    assert!(found.contains("Codex thread name"), "{found}");
    assert!(
        !found.contains("Reconcile the invoices"),
        "only the named provider is read: {found}"
    );
    assert_no_secrets(&everything(&output));

    let output = fixture.sheet(&["--describe", "sessions=claude+codex,commits"]);
    let found = descriptions(&ok(&output)).join(" | ");
    for expected in [
        "Reconcile the invoices",
        "Codex thread name",
        "Add the invoice export",
    ] {
        assert!(found.contains(expected), "{expected} missing: {found}");
    }
    assert_no_secrets(&everything(&output));
}

#[test]
fn a_bad_describe_value_is_refused_by_name() {
    let fixture = fixture();
    for (value, expected) in [
        ("prompts", "unknown --describe source"),
        ("sessions=nobody", "no session titles"),
    ] {
        let output = fixture.sheet(&["--describe", value]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn csv_and_the_vendor_presets_carry_the_description_only_when_asked() {
    let fixture = fixture();
    let plain = fixture.command(
        &[],
        &["--month", "2026-03", "--format", "csv", "--no-cache"],
    );
    let plain = plain.output_checked();
    assert!(!plain.contains("Add the invoice export"));

    let with = fixture
        .command(
            &[],
            &[
                "--month",
                "2026-03",
                "--format",
                "csv",
                "--no-cache",
                "--describe",
                "commits",
            ],
        )
        .output_checked();
    assert!(with.contains("Add the invoice export"), "{with}");

    let preset = fixture
        .command(
            &[],
            &[
                "--month",
                "2026-03",
                "--export",
                "toggl",
                "--no-cache",
                "--describe",
                "commits",
            ],
        )
        .output_checked();
    assert!(preset.contains("Add the invoice export"), "{preset}");
}

trait Checked {
    fn output_checked(self) -> String;
}

impl Checked for Command {
    fn output_checked(mut self) -> String {
        let output = self.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

#[test]
fn digest_prints_the_schema_and_runs_nothing() {
    let fixture = fixture();
    let marker = fixture.root.join("ran");
    let command = format!("echo ran > \"{}\"", marker.display());
    let output = fixture.sheet(&["--digest", "--summarize-with", command.as_str()]);
    let digests = ok(&output);
    assert!(!marker.exists(), "--digest must not run the summarizer");
    let digest = &digests[0];
    assert_eq!(1, digest["version"]);
    assert_eq!("acme", digest["engagement"]);
    assert!(digest["date"].as_str().unwrap().starts_with("2026-03-"));
    assert!(digest["hours"].as_f64().unwrap() > 0.0);
    for field in ["repos", "branches", "issues"] {
        assert!(digest[field].is_array(), "{field}");
    }
    for field in ["prompts", "commits", "sessions"] {
        assert!(digest["counts"][field].is_u64(), "{field}");
    }
    assert!(digest.get("commit_subjects").is_none());
    assert!(digest.get("session_titles").is_none());
    assert_no_secrets(&everything(&output));

    let output = fixture.sheet(&["--digest", "--describe", "commits,sessions"]);
    let digests = ok(&output);
    let digest = &digests[0];
    let subjects = digest["commit_subjects"].as_array().unwrap();
    assert!(subjects.iter().any(|s| s == "Add the invoice export"));
    assert_eq!(
        json!(["Reconcile the invoices"]),
        digest["session_titles"],
        "{digest}"
    );
    assert_no_secrets(&everything(&output));
}

#[test]
fn nothing_described_reaches_the_cache() {
    let fixture = fixture();
    let output = fixture
        .command(
            &[],
            &[
                "--month",
                "2026-03",
                "--format",
                "json",
                "--describe",
                "commits,sessions=claude+codex",
            ],
        )
        .output()
        .unwrap();
    assert!(!descriptions(&ok(&output)).is_empty());
    let bytes = fs::read(&fixture.cache).expect("the run should have written a cache");
    let text = String::from_utf8_lossy(&bytes);
    for needle in [
        "Add the invoice export",
        "Reconcile the invoices",
        "Codex thread name",
        "Fix rounding",
    ] {
        assert!(!text.contains(needle), "{needle} reached the cache");
    }
    // The cached history must not change what a later undescribed run prints.
    let again = fixture
        .command(&[], &["--month", "2026-03", "--format", "json"])
        .output()
        .unwrap();
    assert!(descriptions(&ok(&again)).is_empty());
}

#[test]
fn lock_keeps_the_description_it_had_when_locked() {
    let fixture = fixture();
    let output = fixture
        .command(
            &["lock", "2026-03"],
            &["--no-cache", "--describe", "commits"],
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ledger: Value = serde_json::from_slice(&fs::read(&fixture.ledger).unwrap()).unwrap();
    let entries = ledger["locks"][0]["entries"].as_array().unwrap();
    let stored: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["description"].as_str())
        .collect();
    assert!(
        stored.iter().any(|d| d.contains("Add the invoice export")),
        "{ledger}"
    );

    // Shown from the snapshot later, with no flag asking for it.
    let sheet = ok(&fixture.sheet(&["--describe", "sessions"]));
    assert!(
        descriptions(&sheet)
            .iter()
            .any(|d| d.contains("Add the invoice export")),
        "{sheet}"
    );
    assert_no_secrets(&ledger.to_string());
}

#[cfg(unix)]
mod summarizer {
    use super::*;

    #[test]
    fn the_command_gets_the_digest_and_its_first_line_becomes_the_description() {
        let fixture = fixture();
        let received = fixture.root.join("stdin.json");
        let command = format!(
            "cat > \"{}\"; printf 'Did the\\tinvoice\\nwork \\033[31mred'",
            received.display()
        );
        let output = fixture.sheet(&["--describe", "commits", "--summarize-with", &command]);
        let found = descriptions(&ok(&output));
        // The escape character is replaced, not passed on.
        assert_eq!(1, found.len(), "{found:?}");
        assert!(found[0].starts_with("Did the invoice work"), "{found:?}");
        assert!(!found[0].contains('\u{1b}'));
        let digest: Value = serde_json::from_slice(&fs::read(&received).unwrap()).unwrap();
        assert_eq!(1, digest["version"]);
        assert_eq!("acme", digest["engagement"]);
        assert!(
            digest["commit_subjects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == "Add the invoice export")
        );
        assert!(digest.get("session_titles").is_none());
        assert_no_secrets(&digest.to_string());
    }

    #[test]
    fn a_long_answer_is_cut_at_500_characters() {
        let fixture = fixture();
        let command = "head -c 2000 /dev/zero | tr '\\0' 'y'";
        let found = descriptions(&ok(&fixture.sheet(&["--summarize-with", command])));
        assert_eq!(500, found[0].chars().count());
    }

    #[test]
    fn a_failing_summarizer_is_a_warning_and_leaves_no_description() {
        let fixture = fixture();
        let output = fixture.sheet(&["--summarize-with", "echo boom >&2; exit 3"]);
        let sheet = ok(&output);
        assert!(descriptions(&sheet).is_empty());
        let warnings = sheet["warnings"].to_string();
        assert!(
            warnings.contains("--summarize-with") && warnings.contains("boom"),
            "{warnings}"
        );
    }

    #[test]
    fn a_slow_summarizer_is_stopped_at_the_timeout() {
        let fixture = fixture();
        let started = std::time::Instant::now();
        let output = fixture.sheet(&["--summarize-with", "sleep 30", "--summarize-timeout", "1s"]);
        let sheet = ok(&output);
        assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
        assert!(descriptions(&sheet).is_empty());
        assert!(
            sheet["warnings"].to_string().contains("timed out"),
            "{}",
            sheet["warnings"]
        );
    }
}

#[cfg(windows)]
mod summarizer_windows {
    use super::*;

    #[test]
    fn a_failing_summarizer_is_a_warning_and_leaves_no_description() {
        let fixture = fixture();
        let sheet = ok(&fixture.sheet(&["--summarize-with", "exit /b 3"]));
        assert!(descriptions(&sheet).is_empty());
        assert!(sheet["warnings"].to_string().contains("--summarize-with"));
    }
}
