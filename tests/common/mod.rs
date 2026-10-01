//! Helpers shared by the integration tests. A new test file declares
//! `mod common;` and uses what it needs; `tests/rust_cli.rs` does the same.
// Each test binary compiles this module and uses a different part of it.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

pub fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_workstats")
}

pub fn run(arguments: &[&str]) -> Output {
    Command::new(binary()).args(arguments).output().unwrap()
}

pub fn git(arguments: &[&str]) -> Output {
    Command::new("git").args(arguments).output().unwrap()
}

/// Writes `body` to `file` and commits it to `repo` as `author`.
///
/// The committer is always the fixture identity; only the *author* varies,
/// because `--author` and `--agent-commits` both filter on authorship. Each
/// `message` becomes its own paragraph, which is how a `Co-authored-by:`
/// trailer is attached to a commit.
pub fn commit_as(repo: &str, file: &str, body: &str, author: &str, message: &[&str]) {
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

/// Commits `body` to `file` as the fixture developer at noon UTC on `day`, a
/// whole half-day clear of any local midnight so the month it lands in does not
/// depend on the timezone the suite runs in.
pub fn commit_on(repo: &str, file: &str, body: &str, day: &str) {
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

/// Writes one pi session under `history` that ran `model` in `cwd` and produced
/// `output` output tokens, so an allocation fixture can be described in the
/// terms allocation actually splits on.
pub fn pi_session(history: &Path, name: &str, cwd: &Path, model: &str, output: u64) {
    pi_session_on(history, name, cwd, model, output, "2026-03-02");
}

/// The same session on another day, for tests that need history in two windows.
pub fn pi_session_on(history: &Path, name: &str, cwd: &Path, model: &str, output: u64, day: &str) {
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

/// A report over `path` with `arguments` appended, parsed. The shared flags
/// keep every Git test away from AI history, the cache and the terminal.
pub fn git_report(path: &str, arguments: &[&str]) -> Value {
    report_with_env(path, arguments, &[])
}

pub fn report_with_env(path: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Value {
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
pub fn repository_on_main(temporary: &Path, name: &str) -> String {
    let project = temporary.join(name);
    fs::create_dir_all(&project).unwrap();
    let path = project.to_str().unwrap().to_string();
    assert!(git(&["init", "-q", "-b", "main", &path]).status.success());
    path
}

pub fn report_json(base: &[String], extra: &[&str]) -> Value {
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

pub fn failure(base: &[String], extra: &[&str]) -> String {
    let mut arguments: Vec<&str> = base.iter().map(String::as_str).collect();
    arguments.extend(extra);
    let output = run(&arguments);
    assert!(!output.status.success(), "expected {extra:?} to be refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn json_stdout(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
