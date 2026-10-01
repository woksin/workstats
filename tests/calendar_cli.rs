//! The calendar heatmap on every surface that is not the explorer: the HTML
//! page, the Markdown report and `workstats calendar`.

use std::fs;
use std::path::Path;
use std::process::Output;

use tempfile::tempdir;

mod common;
use common::*;

/// One Pi session in March 2026, read with nothing else, and `arguments`
/// appended. Returns the command's output without judging it.
fn with_one_session(directory: &Path, arguments: &[&str]) -> Output {
    let project = directory.join("project");
    fs::create_dir_all(&project).unwrap();
    let history = directory.join("pi-sessions");
    pi_session(&history, "s1", &project, "claude-opus-5", 10);
    let history = format!("pi={}", history.display());
    let config = directory.join("missing-config.json");
    // A subcommand takes its own flags, so it has to come before them.
    let (command, arguments) = match arguments.split_first() {
        Some((first, rest)) if *first == "calendar" => (vec![*first], rest),
        _ => (Vec::new(), arguments),
    };
    let mut all = command;
    all.extend([
        "--no-git",
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
    ]);
    all.extend(arguments);
    run(&all)
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn the_calendar_command_draws_the_window_as_a_unicode_grid() {
    let directory = tempdir().unwrap();
    let text = stdout(&with_one_session(
        directory.path(),
        &["calendar", "--month", "2026-03"],
    ));
    assert!(text.starts_with("WORKSTATS calendar\n"), "{text}");
    assert!(text.contains("\n2026\n"), "{text}");
    for label in ["Mon ", "Wed ", "Fri ", "Sun ", "Mar"] {
        assert!(text.contains(label), "missing {label:?}\n{text}");
    }
    // The one active day is the busiest, and every other day is a dot.
    let grid = text
        .lines()
        .filter(|line| !line.starts_with("Human time"))
        .collect::<String>();
    assert_eq!(1, grid.matches('█').count(), "{text}");
    assert!(text.contains('·'));
    assert!(text.contains("over 1 active day"), "{text}");
    // A window was given, so the default-window sentence is not.
    assert!(!text.contains("The last 365 days"), "{text}");
}

#[test]
fn the_calendar_command_defaults_to_the_last_365_days_and_says_so() {
    let directory = tempdir().unwrap();
    let text = stdout(&with_one_session(directory.path(), &["calendar"]));
    assert!(text.contains("The last 365 days"), "{text}");
    assert!(text.contains("--year"), "{text}");
    // Up to 53 weeks plus the label column, and never wider.
    for line in text.lines().filter(|line| line.starts_with("Mon ")) {
        assert!(line.chars().count() <= 4 + 54, "{line}");
    }
}

#[test]
fn the_calendar_command_has_markdown_and_html_forms_but_no_json() {
    let directory = tempdir().unwrap();
    let markdown = stdout(&with_one_session(
        directory.path(),
        &["calendar", "--month", "2026-03", "--format", "markdown"],
    ));
    assert!(markdown.contains("```text\n"), "{markdown}");
    assert!(markdown.contains('█'));

    let html = stdout(&with_one_session(
        directory.path(),
        &["calendar", "--month", "2026-03", "--format", "html"],
    ));
    assert!(html.contains("<svg class=\"cal\""), "{html}");
    assert!(!html.contains("<script"));

    for format in ["json", "csv"] {
        let output = with_one_session(directory.path(), &["calendar", "--format", format]);
        assert!(!output.status.success());
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("no json form") || message.contains("no csv form"),
            "{message}"
        );
        assert!(message.contains("--daily"), "{message}");
    }
}

#[test]
fn the_html_report_includes_a_calendar_only_for_a_window_of_four_weeks_or_more() {
    let directory = tempdir().unwrap();
    let month = stdout(&with_one_session(
        directory.path(),
        &["--month", "2026-03", "--format", "html"],
    ));
    assert!(month.contains("<h2>Calendar</h2>"), "{month}");
    assert!(month.contains("<svg class=\"cal\""));
    assert!(month.contains("<title>2026-03-0"), "{month}");
    assert!(month.contains("class=\"l4\""));
    assert!(!month.contains("<script"));
    assert!(!month.contains("href"));
    assert!(month.contains("default-src 'none'; style-src 'unsafe-inline'"));

    let week = stdout(&with_one_session(
        directory.path(),
        &["--week", "2026-W10", "--format", "html"],
    ));
    assert!(!week.contains("<svg"), "{week}");

    // No window at all is unbounded, which is calendar enough.
    let open = stdout(&with_one_session(directory.path(), &["--format", "html"]));
    assert!(open.contains("<svg class=\"cal\""), "{open}");
}

#[test]
fn the_markdown_report_has_a_calendar_when_per_day_figures_are_asked_for() {
    let directory = tempdir().unwrap();
    let plain = stdout(&with_one_session(
        directory.path(),
        &["--month", "2026-03", "--format", "markdown"],
    ));
    assert!(!plain.contains("## Calendar"), "{plain}");

    let daily = stdout(&with_one_session(
        directory.path(),
        &["--month", "2026-03", "--format", "markdown", "--daily"],
    ));
    assert!(daily.contains("## Calendar\n"), "{daily}");
    assert!(daily.contains("```text\n"));
    assert!(daily.contains('█'));
}
