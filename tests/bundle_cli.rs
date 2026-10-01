//! Bundles: `workstats export` writes one machine's evidence, and `--import`
//! and `workstats merge` fold it into another run's report.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::tempdir;

mod common;
use common::*;

/// Commits `body` to `file` as the fixture developer on `day`, with `subject`
/// as the message, so a test can plant text that must never be exported.
fn commit_dated(repo: &str, file: &str, body: &str, day: &str, subject: &str) {
    let target = Path::new(repo).join(file);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, body).unwrap();
    assert!(git(&["-C", repo, "add", "."]).status.success());
    let date = format!("{day}T12:00:00Z");
    let output = std::process::Command::new("git")
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

/// One machine's history in March 2026: a repository with a remote and one
/// without, each with a Pi session and a commit.
struct Machine {
    directory: tempfile::TempDir,
    remote_repo: String,
    history: PathBuf,
    config: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let remote_repo = repository_on_main(directory.path(), "api");
        assert!(
            git(&[
                "-C",
                &remote_repo,
                "remote",
                "add",
                "origin",
                "https://github.com/acme/api.git"
            ])
            .status
            .success()
        );
        commit_dated(
            &remote_repo,
            "src/lib.rs",
            "fn one() {}
",
            "2026-03-02",
            "SECRET-SUBJECT-ONE",
        );
        let scratch_repo = repository_on_main(directory.path(), "scratch");
        commit_dated(
            &scratch_repo,
            "notes.md",
            "# notes
",
            "2026-03-04",
            "SECRET-SUBJECT-TWO",
        );
        let history = directory.path().join("pi");
        let nested = Path::new(&remote_repo).join("src");
        fs::create_dir_all(&nested).unwrap();
        pi_session_on(
            &history,
            "api-root",
            Path::new(&remote_repo),
            "claude-opus-5",
            10,
            "2026-03-02",
        );
        pi_session_on(
            &history,
            "api-src",
            &nested,
            "claude-opus-5",
            20,
            "2026-03-03",
        );
        pi_session_on(
            &history,
            "scratch",
            Path::new(&scratch_repo),
            "claude-opus-5",
            30,
            "2026-03-04",
        );
        let config = directory.path().join("config/config.json");
        Self {
            directory,
            remote_repo,
            history,
            config,
        }
    }

    /// The flags that make a run read exactly this machine's fixture.
    fn base(&self) -> Vec<String> {
        [
            "--dir",
            &self.remote_repo,
            "--no-progress",
            "--no-cache",
            "--no-default-events",
            "--no-update-check",
            "--provider",
            "pi",
            "--history",
            &format!("pi={}", self.history.display()),
            "--config",
            self.config.to_str().unwrap(),
            "--author",
            "fixture@example.com",
            "--month",
            "2026-03",
        ]
        .iter()
        .map(|flag| (*flag).to_string())
        .collect()
    }

    fn local_report(&self, extra: &[&str]) -> Value {
        let mut arguments = self.base();
        arguments.extend(["--format".into(), "json".into()]);
        report_json(&arguments, extra)
    }

    fn export(&self, label: &str, extra: &[&str]) -> (PathBuf, std::process::Output) {
        let bundle = self.directory.path().join(format!("{label}.json"));
        let mut arguments: Vec<String> = vec!["export".into()];
        arguments.extend(self.base());
        arguments.extend([
            "--output".into(),
            bundle.to_str().unwrap().into(),
            "--label".into(),
            label.into(),
        ]);
        arguments.extend(extra.iter().map(|flag| (*flag).to_string()));
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        (bundle, run(&arguments))
    }
}

/// A merge report over bundles alone, from a config that does not exist.
fn merged(directory: &Path, bundles: &[&Path], extra: &[&str]) -> Value {
    let output = run_merge(directory, bundles, extra);
    json_stdout(&output)
}

fn run_merge(directory: &Path, bundles: &[&Path], extra: &[&str]) -> std::process::Output {
    let config = directory.join("merge-config.json");
    let mut arguments: Vec<&str> = vec!["merge"];
    let paths: Vec<String> = bundles
        .iter()
        .map(|path| path.to_str().unwrap().to_string())
        .collect();
    arguments.extend(paths.iter().map(String::as_str));
    arguments.extend([
        "--config",
        config.to_str().unwrap(),
        "--no-progress",
        "--no-update-check",
        "--month",
        "2026-03",
        "--format",
        "json",
    ]);
    arguments.extend(extra);
    run(&arguments)
}

/// The first place two reports differ, so a failure names a figure rather
/// than printing two whole reports.
fn first_difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => a.keys().chain(b.keys()).find_map(|key| {
            first_difference(
                a.get(key).unwrap_or(&Value::Null),
                b.get(key).unwrap_or(&Value::Null),
                &format!("{path}.{key}"),
            )
        }),
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(index, (a, b))| first_difference(a, b, &format!("{path}[{index}]"))),
        _ => (left != right).then(|| format!("{path}: {left} != {right}")),
    }
}

fn assert_same(left: &Value, right: &Value, context: &str) {
    if let Some(difference) = first_difference(left, right, "") {
        panic!("{context}: {difference}");
    }
}

/// The rows in key order. Rows that tie on the report's sort keys come out in
/// either order from one run to the next, which says nothing about the figures.
fn rows_by_key(report: &Value) -> Value {
    let mut rows = report["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|row| row["key"].to_string());
    Value::Array(rows)
}

fn without_run_specific_parts(mut report: Value) -> Value {
    let object = report.as_object_mut().unwrap();
    // Where the history was read from, and how this run's notes read, are
    // about the run rather than the figures.
    object.remove("inputs");
    object.remove("diagnostics");
    let sorted = rows_by_key(&Value::Object(object.clone()));
    object.insert("rows".into(), sorted);
    report
}

#[test]
fn a_bundle_round_trips_to_the_report_the_machine_would_have_printed() {
    let machine = Machine::new();
    let (bundle, output) = machine.export("laptop", &["--include-paths"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let local = machine.local_report(&[]);
    assert!(local["summary"]["session_count"].as_u64().unwrap() >= 3);
    assert!(local["summary"]["commit_count"].as_u64().unwrap() >= 2);

    let merged = merged(machine.directory.path(), &[&bundle], &[]);
    assert_same(
        &without_run_specific_parts(local),
        &without_run_specific_parts(merged),
        "round trip",
    );
}

#[test]
fn the_same_round_trip_holds_for_every_grouping() {
    let machine = Machine::new();
    let (bundle, _) = machine.export("laptop", &["--include-paths"]);
    for grouping in ["repo,provider", "day", "model,month", "repo,week"] {
        let local = machine.local_report(&["--group-by", grouping]);
        let merged = merged(
            machine.directory.path(),
            &[&bundle],
            &["--group-by", grouping],
        );
        assert_same(&rows_by_key(&local), &rows_by_key(&merged), grouping);
    }
}

#[test]
fn importing_a_machines_own_bundle_back_into_its_report_changes_nothing() {
    let machine = Machine::new();
    let (bundle, _) = machine.export("laptop", &["--include-paths"]);
    let local = machine.local_report(&[]);
    let mut arguments = machine.base();
    arguments.extend(["--format".into(), "json".into()]);
    let with_import = report_json(&arguments, &["--import", bundle.to_str().unwrap()]);
    // Sessions and remote-backed commits are the same ones, so nothing is
    // counted twice. The repository with no remote cannot be recognised, and
    // its commit is the one honest exception, which the run warns about.
    assert_eq!(
        local["summary"]["human_estimated_seconds"],
        with_import["summary"]["human_estimated_seconds"]
    );
    assert_eq!(
        local["summary"]["session_count"],
        with_import["summary"]["session_count"]
    );
    let warnings = with_import["diagnostics"]["messages"].to_string();
    assert!(warnings.contains("no shared remote"), "{warnings}");
}

#[test]
fn a_default_bundle_holds_no_paths_subjects_or_file_names() {
    let machine = Machine::new();
    let (bundle, output) = machine.export("laptop", &[]);
    assert!(output.status.success());
    let text = fs::read_to_string(&bundle).unwrap();
    let root = machine.directory.path().to_string_lossy().into_owned();
    // The path as the pipeline sees it (symlinks resolved) and as JSON writes
    // it (backslashes doubled) are the spellings that could leak.
    let canonical = machine
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let escaped = root.replace('\\', "\\\\");
    for forbidden in [
        root.as_str(),
        canonical.as_str(),
        escaped.as_str(),
        "SECRET-SUBJECT",
        "lib.rs",
        "notes.md",
        "fixture@example.com/",
    ] {
        assert!(
            !text.contains(forbidden),
            "{forbidden} leaked into the bundle"
        );
    }
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!("workstats-bundle", parsed["format"]);
    assert_eq!(1, parsed["version"]);
    assert_eq!("laptop", parsed["machine"]["label"]);
    assert_eq!(json!(["fixture@example.com"]), parsed["person"]["authors"]);
    let keys: Vec<&str> = parsed["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repository| repository["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"remote:github.com/acme/api"), "{keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("local:")), "{keys:?}");
    for commit in parsed["commits"].as_array().unwrap() {
        assert!(commit["files"].is_null());
    }
    // Sessions run in a subdirectory keep only the relative part.
    let subdirs: Vec<&str> = parsed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["subdir"].as_str().unwrap())
        .collect();
    assert!(subdirs.contains(&"src"), "{subdirs:?}");

    // The same bundle with --include-paths names the changed files, and
    // still no subject or absolute path.
    let (with_paths, _) = machine.export("paths", &["--include-paths"]);
    let text = fs::read_to_string(with_paths).unwrap();
    assert!(text.contains("src/lib.rs"));
    assert!(!text.contains("SECRET-SUBJECT"));
    assert!(!text.contains(&root));
    assert!(!text.contains(&canonical));

    // Without file paths the bundle still merges to the same hours and
    // commits; only the file counts have nothing to count.
    let local = machine.local_report(&[]);
    let report = merged(machine.directory.path(), &[&bundle], &[]);
    assert_eq!(
        local["summary"]["human_estimated_seconds"],
        report["summary"]["human_estimated_seconds"]
    );
    assert_eq!(
        local["summary"]["commit_count"],
        report["summary"]["commit_count"]
    );
    let notes = report["diagnostics"]["notes"].to_string();
    assert!(notes.contains("no file paths"), "{notes}");
}

#[test]
fn export_remembers_the_machine_beside_the_config() {
    let machine = Machine::new();
    let (_, output) = machine.export("laptop", &[]);
    assert!(output.status.success());
    let file = machine.config.parent().unwrap().join("machine.json");
    let stored: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!("laptop", stored["label"]);
    let id = stored["id"].as_str().unwrap().to_string();
    assert_eq!(32, id.len());

    // A second export, without a label, is the same machine under the same name.
    let bundle = machine.directory.path().join("again.json");
    let mut arguments: Vec<String> = vec!["export".into()];
    arguments.extend(machine.base());
    arguments.extend(["--output".into(), bundle.to_str().unwrap().into()]);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let mut command = std::process::Command::new(binary());
    command
        .args(&arguments)
        .env_remove("HOSTNAME")
        .env_remove("COMPUTERNAME");
    assert!(command.output().unwrap().status.success());
    let parsed: Value = serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
    assert_eq!(id, parsed["machine"]["id"]);
    assert_eq!("laptop", parsed["machine"]["label"]);
}

#[test]
fn a_machine_with_no_label_and_no_host_name_is_asked_for_one() {
    let machine = Machine::new();
    let bundle = machine.directory.path().join("out.json");
    let mut arguments: Vec<String> = vec!["export".into()];
    arguments.extend(machine.base());
    arguments.extend(["--output".into(), bundle.to_str().unwrap().into()]);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = std::process::Command::new(binary())
        .args(&arguments)
        .env_remove("HOSTNAME")
        .env_remove("COMPUTERNAME")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--label"));
    assert!(!bundle.exists());
}

#[test]
fn export_to_stdout_writes_only_the_bundle() {
    let machine = Machine::new();
    let mut arguments: Vec<String> = vec!["export".into()];
    arguments.extend(machine.base());
    arguments.extend([
        "--output".into(),
        "-".into(),
        "--label".into(),
        "laptop".into(),
    ]);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = run(&arguments);
    let parsed = json_stdout(&output);
    assert_eq!("workstats-bundle", parsed["format"]);
}

#[test]
fn export_refuses_to_bundle_a_bundle() {
    let machine = Machine::new();
    let (bundle, _) = machine.export("laptop", &[]);
    let (_, output) = machine.export("again", &["--import", bundle.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("own history"));
}

#[test]
fn a_report_is_refused_and_so_are_bundles_for_different_people() {
    let machine = Machine::new();
    let report = machine.directory.path().join("report.json");
    fs::write(
        &report,
        serde_json::to_vec(&machine.local_report(&[])).unwrap(),
    )
    .unwrap();
    let output = run_merge(machine.directory.path(), &[&report], &[]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("a report is a conclusion"), "{message}");

    let (ada, _) = machine.export("laptop", &[]);
    let mut other: Value = serde_json::from_slice(&fs::read(&ada).unwrap()).unwrap();
    other["person"]["authors"] = json!(["grace@example.com"]);
    other["machine"]["id"] = json!("b".repeat(32));
    let grace = machine.directory.path().join("grace.json");
    fs::write(&grace, serde_json::to_vec(&other).unwrap()).unwrap();
    let output = run_merge(machine.directory.path(), &[&ada, &grace], &[]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("team merges are not supported yet"),
        "{message}"
    );
}

/// A hand-written bundle for one machine, so a test can state exactly which
/// instants each machine saw.
fn bundle_file(
    directory: &Path,
    name: &str,
    machine: char,
    sessions: &[(&str, &[&str])],
    commits: &[(&str, &str, &str)],
) -> PathBuf {
    let key = "remote:github.com/acme/api";
    let point = |timestamp: &&str| json!({"timestamp": timestamp, "model": "claude-opus-5"});
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|(id, times)| {
            json!({
                "provider": "claude", "session_id": id, "repo": key, "subdir": "",
                "points": times.iter().map(point).collect::<Vec<_>>(),
                "human_points": times.iter().map(point).collect::<Vec<_>>(),
            })
        })
        .collect();
    let commits: Vec<Value> = commits
        .iter()
        .map(|(repo, sha, timestamp)| {
            json!({
                "repo": repo, "sha": sha, "timestamp": timestamp,
                "additions": 10, "deletions": 2,
                "categories": {"source": [8, 2], "mystery": [2, 0]},
            })
        })
        .collect();
    let bundle = json!({
        "format": "workstats-bundle", "version": 1,
        "exported_at": "2026-03-31T00:00:00Z", "workstats_version": "test",
        "machine": {"id": machine.to_string().repeat(32), "label": name},
        "person": {"authors": ["fixture@example.com"]},
        "window": {"since": null, "until": null},
        "settings": {"human_idle": "1h", "review_credit": "30m", "gap_cap": "5m"},
        "repositories": [
            {"key": key, "label": "api", "portable": true},
            {"key": format!("local:{}:scratch", machine.to_string().repeat(32)),
             "label": "scratch", "portable": false},
        ],
        "sessions": sessions,
        "commits": commits,
    });
    let path = directory.join(format!("{name}.json"));
    fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    path
}

fn human_seconds(report: &Value) -> f64 {
    report["summary"]["human_estimated_seconds"]
        .as_f64()
        .unwrap()
}

#[test]
fn hours_both_machines_were_at_work_are_counted_once() {
    let directory = tempdir().unwrap();
    // The laptop and the desktop each saw every other prompt of one working
    // hour, so each alone credits only its own stretch.
    let laptop = bundle_file(
        directory.path(),
        "laptop",
        'a',
        &[(
            "laptop-session",
            &[
                "2026-03-02T10:00:00Z",
                "2026-03-02T10:20:00Z",
                "2026-03-02T10:40:00Z",
            ],
        )],
        &[],
    );
    let desktop = bundle_file(
        directory.path(),
        "desktop",
        'b',
        &[(
            "desktop-session",
            &[
                "2026-03-02T10:10:00Z",
                "2026-03-02T10:30:00Z",
                "2026-03-02T10:50:00Z",
            ],
        )],
        &[],
    );
    // One machine that saw everything is the truth the merge must reproduce.
    let everything = bundle_file(
        directory.path(),
        "both",
        'c',
        &[
            (
                "laptop-session",
                &[
                    "2026-03-02T10:00:00Z",
                    "2026-03-02T10:20:00Z",
                    "2026-03-02T10:40:00Z",
                ],
            ),
            (
                "desktop-session",
                &[
                    "2026-03-02T10:10:00Z",
                    "2026-03-02T10:30:00Z",
                    "2026-03-02T10:50:00Z",
                ],
            ),
        ],
        &[],
    );
    let alone_laptop = human_seconds(&merged(directory.path(), &[&laptop], &[]));
    let alone_desktop = human_seconds(&merged(directory.path(), &[&desktop], &[]));
    let together = merged(directory.path(), &[&laptop, &desktop], &[]);
    let truth = human_seconds(&merged(directory.path(), &[&everything], &[]));
    assert!(alone_laptop > 0.0 && alone_desktop > 0.0);
    assert_eq!(truth, human_seconds(&together));
    assert!(
        human_seconds(&together) < alone_laptop + alone_desktop,
        "the overlapping hour was added twice"
    );
    assert_eq!(2, together["summary"]["session_count"]);
}

#[test]
fn one_session_in_two_bundles_and_one_commit_in_two_bundles_count_once() {
    let directory = tempdir().unwrap();
    let key = "remote:github.com/acme/api";
    let times: &[&str] = &["2026-03-02T10:00:00Z", "2026-03-02T10:05:00Z"];
    let first = bundle_file(
        directory.path(),
        "laptop",
        'a',
        &[("shared", times)],
        &[(key, "abc123", "2026-03-02T10:06:00Z")],
    );
    let second = bundle_file(
        directory.path(),
        "synced",
        'b',
        &[("shared", &times[..1])],
        &[(key, "abc123", "2026-03-02T10:06:00Z")],
    );
    let report = merged(directory.path(), &[&first, &second], &[]);
    assert_eq!(1, report["summary"]["session_count"]);
    assert_eq!(1, report["summary"]["commit_count"]);
    assert_eq!(10, report["summary"]["additions"]);
    // Unknown categories are counted as other, and the run says so.
    let notes = report["diagnostics"]["notes"].to_string();
    assert!(notes.contains("mystery"), "{notes}");
}

#[test]
fn repositories_without_a_remote_are_never_merged_across_machines() {
    let directory = tempdir().unwrap();
    let scratch = |machine: char| format!("local:{}:scratch", machine.to_string().repeat(32));
    let one = bundle_file(
        directory.path(),
        "laptop",
        'a',
        &[],
        &[(&scratch('a'), "abc123", "2026-03-02T10:06:00Z")],
    );
    let two = bundle_file(
        directory.path(),
        "desktop",
        'b',
        &[],
        &[(&scratch('b'), "abc123", "2026-03-02T10:06:00Z")],
    );
    let report = merged(directory.path(), &[&one, &two], &[]);
    assert_eq!(2, report["summary"]["commit_count"]);
    let warnings = report["diagnostics"]["messages"].to_string();
    assert!(warnings.contains("no shared remote"), "{warnings}");
    assert!(
        warnings.contains("laptop") && warnings.contains("desktop"),
        "{warnings}"
    );
}

#[test]
fn repo_and_provider_filters_apply_to_imported_sessions() {
    let directory = tempdir().unwrap();
    let times: &[&str] = &["2026-03-02T10:00:00Z", "2026-03-02T10:05:00Z"];
    let bundle = bundle_file(directory.path(), "laptop", 'a', &[("s1", times)], &[]);
    let kept = merged(directory.path(), &[&bundle], &["--repo", "api"]);
    assert_eq!(1, kept["summary"]["session_count"]);
    let excluded = merged(directory.path(), &[&bundle], &["--repo", "elsewhere"]);
    assert_eq!(0, excluded["summary"]["session_count"]);
    let exact = merged(directory.path(), &[&bundle], &["--repo-exact", "api"]);
    assert_eq!(1, exact["summary"]["session_count"]);
    let wrong_provider = merged(directory.path(), &[&bundle], &["--provider", "codex"]);
    assert_eq!(0, wrong_provider["summary"]["session_count"]);
}

#[test]
fn the_importers_settings_apply_and_a_different_exporter_setting_is_noted() {
    let directory = tempdir().unwrap();
    let bundle = bundle_file(
        directory.path(),
        "laptop",
        'a',
        &[("s1", &["2026-03-02T10:00:00Z", "2026-03-02T10:50:00Z"])],
        &[],
    );
    let default = merged(directory.path(), &[&bundle], &[]);
    // The 50-minute gap is inside the default hour of idle time and outside 10 minutes.
    let strict = merged(
        directory.path(),
        &[&bundle],
        &["--human-idle", "10m", "--review-credit", "1m"],
    );
    assert!(human_seconds(&strict) < human_seconds(&default));
    let notes = strict["diagnostics"]["notes"].to_string();
    assert!(notes.contains("human_idle 1h"), "{notes}");
}

#[test]
fn merge_with_local_adds_this_machines_history_to_the_bundles() {
    let machine = Machine::new();
    let directory = tempdir().unwrap();
    let bundle = bundle_file(
        directory.path(),
        "desktop",
        'b',
        &[(
            "elsewhere",
            &["2026-03-10T10:00:00Z", "2026-03-10T10:05:00Z"],
        )],
        &[],
    );
    let local = machine.local_report(&[]);
    let mut arguments: Vec<String> = vec!["merge".into(), bundle.to_str().unwrap().into()];
    arguments.extend(machine.base());
    arguments.extend([
        "--with-local".into(),
        "--provider".into(),
        "claude".into(),
        "--format".into(),
        "json".into(),
    ]);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let report = json_stdout(&run(&arguments));
    assert_eq!(
        local["summary"]["session_count"].as_u64().unwrap() + 1,
        report["summary"]["session_count"].as_u64().unwrap()
    );
    // Without --with-local this machine's own sessions are not read at all.
    let alone = merged(machine.directory.path(), &[&bundle], &[]);
    assert_eq!(1, alone["summary"]["session_count"]);
}
