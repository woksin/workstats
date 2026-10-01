//! One end-to-end pass over every subcommand: each answers `--help`, and each
//! command added with the timesheet batch runs on a small fixture (a Git
//! repository on a feature branch plus a Pi history) and succeeds. The detail
//! is checked in the per-feature test files; this one fails when a command is
//! left unwired.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::tempdir;

mod common;
use common::*;

const SUBCOMMANDS: &[&str] = &[
    "ui",
    "sources",
    "classify",
    "record",
    "update",
    "allocate",
    "timesheet",
    "branch",
    "pr",
    "insights",
    "digest",
    "now",
    "export",
    "merge",
    "calendar",
];

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    repo: String,
    history: String,
    config: PathBuf,
}

fn fixture() -> Fixture {
    let directory = tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let repo = repository_on_main(&root, "repo");
    commit_on(&repo, "base.txt", "one\n", "2026-03-01");
    assert!(
        git(&["-C", &repo, "checkout", "-q", "-b", "feat/ACME-1"])
            .status
            .success()
    );
    commit_on(&repo, "a.txt", "a\nb\n", "2026-03-02");
    let history = root.join("pi-sessions");
    pi_session(&history, "s1", Path::new(&repo), "claude-opus-5", 10);
    Fixture {
        config: root.join("missing-config.json"),
        history: format!("pi={}", history.display()),
        repo,
        root,
        _directory: directory,
    }
}

impl Fixture {
    /// The flags every report command takes, keeping the run off the real
    /// machine: no cache, no network, no default event logs.
    fn base(&self) -> Vec<String> {
        [
            "--dir",
            &self.repo,
            "--author",
            "fixture@example.com",
            "--provider",
            "pi",
            "--history",
            &self.history,
            "--config",
            self.config.to_str().unwrap(),
            "--no-cache",
            "--no-progress",
            "--no-default-events",
            "--no-update-check",
        ]
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    fn command(&self, subcommand: &[&str], extra: &[&str]) -> Output {
        let mut arguments: Vec<String> = subcommand.iter().map(ToString::to_string).collect();
        arguments.extend(self.base());
        arguments.extend(extra.iter().map(ToString::to_string));
        let mut command = Command::new(binary());
        command
            .args(&arguments)
            .env("WORKSTATS_CONFIG", &self.config)
            .env(
                "WORKSTATS_NOW_CACHE",
                self.root.join("state").join("now.json"),
            )
            .env_remove("WORKSTATS_AUTHOR");
        command.output().unwrap()
    }

    fn succeeds(&self, subcommand: &[&str], extra: &[&str]) -> String {
        let output = self.command(subcommand, extra);
        assert!(
            output.status.success(),
            "workstats {} {} failed:\n{}",
            subcommand.join(" "),
            extra.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn every_subcommand_answers_help() {
    for subcommand in SUBCOMMANDS {
        let output = run(&[subcommand, "--help"]);
        assert!(
            output.status.success(),
            "workstats {subcommand} --help failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage:"),
            "workstats {subcommand} --help printed no usage"
        );
    }
}

#[test]
fn the_new_commands_run_on_a_small_fixture() {
    let fixture = fixture();
    let month = ["--month", "2026-03"];

    let text = fixture.succeeds(&["timesheet"], &month);
    assert!(text.contains("2026-03-02"), "{text}");
    fixture.succeeds(
        &["timesheet"],
        &["--month", "2026-03", "--format", "markdown"],
    );

    let text = fixture.succeeds(&["branch", "feat/ACME-1"], &month);
    assert!(text.contains("feat/ACME-1"), "{text}");
    fixture.succeeds(
        &["pr", "feat/ACME-1"],
        &["--month", "2026-03", "--format", "markdown"],
    );

    let text = fixture.succeeds(&["insights"], &month);
    assert!(!text.trim().is_empty());
    let text = fixture.succeeds(&["digest"], &["--week", "2026-W10"]);
    assert!(!text.trim().is_empty());

    let text = fixture.succeeds(&["calendar"], &month);
    assert!(text.contains("Mar"), "{text}");

    // `now` writes its snapshot where WORKSTATS_NOW_CACHE says, not in the
    // real cache directory.
    fixture.succeeds(&["now"], &[]);
    assert!(fixture.root.join("state").join("now.json").is_file());

    let bundle = fixture.root.join("bundle.json");
    fixture.succeeds(
        &["export"],
        &[
            "--month",
            "2026-03",
            "--output",
            bundle.to_str().unwrap(),
            "--label",
            "laptop",
        ],
    );
    assert!(bundle.is_file());
    let merged = Command::new(binary())
        .args(["merge", bundle.to_str().unwrap(), "--config"])
        .arg(&fixture.config)
        .args([
            "--no-progress",
            "--no-update-check",
            "--month",
            "2026-03",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert!(fs::read_to_string(&bundle).unwrap().contains("laptop"));
}
