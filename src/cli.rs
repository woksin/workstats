//! Command-line surface: every clap definition plus the helpers that turn the
//! parsed flags into validated values.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::aggregate::DIMENSIONS;
use crate::allocate;
use crate::git::DEFAULT_AGENT_AUTHORS;
use crate::timeutil::{month_span, parse_bound, parse_duration, week_span, year_span};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum OutputFormat {
    Table,
    Json,
    Csv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    Activity,
    Prompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventRole {
    Foreground,
    Subagent,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Explore the same report interactively; takes the report flags itself,
    /// as in `workstats ui --dir . --since 2026-01`
    Ui(Box<ReportArguments>),
    /// Show supported and automatically detected local histories
    Sources(SourcesArguments),
    /// Show the category and the rule a path matches
    Classify(ClassifyArguments),
    /// Append one content-free event for a CLI, IDE, script, or API wrapper
    #[command(visible_alias = "event")]
    Record(Box<RecordArguments>),
    /// Check for and install a newer workstats release from GitHub
    Update(UpdateArguments),
    /// Apportion flat-rate subscription spend to one project, as in
    /// `workstats allocate -p Ada --sub claude=2 --sub codex=4 --month 2026-08`
    Allocate(Box<AllocateArguments>),
}

#[derive(Debug, Args)]
pub(crate) struct AllocateArguments {
    #[arg(
        long = "project",
        short = 'p',
        value_name = "NAME",
        action = clap::ArgAction::Append,
        help = "Repository the spend is being apportioned to; omit to break the whole spend down by project"
    )]
    pub(crate) projects: Vec<String>,
    #[arg(
        long = "sub",
        required = true,
        value_name = "PLAN=N[@PRICE]",
        action = clap::ArgAction::Append,
        help = "Subscriptions held, as claude=2 or codex=3@1992 to price one vendor separately; repeatable"
    )]
    pub(crate) subscriptions: Vec<String>,
    #[arg(
        long,
        default_value_t = 200.0,
        help = "Advertised price per subscription per month, before tax"
    )]
    pub(crate) price: f64,
    #[arg(
        long,
        default_value_t = 0.0,
        value_name = "PERCENT",
        help = "Consumption tax added at checkout, e.g. 25 for Norwegian MVA"
    )]
    pub(crate) vat: f64,
    #[arg(
        long,
        default_value = "USD",
        value_name = "CODE",
        help = "Currency --price is stated in; labels output and converts nothing"
    )]
    pub(crate) currency: String,
    #[arg(
        long,
        value_enum,
        default_value_t = allocate::Basis::Output,
        help = "Measured quantity the split is computed from"
    )]
    pub(crate) basis: allocate::Basis,
    #[arg(
        long = "gap-policy",
        value_enum,
        default_value_t = allocate::GapPolicy::Skip,
        help = "How to treat a month whose history has been pruned"
    )]
    pub(crate) gap_policy: allocate::GapPolicy,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

#[derive(Debug, Args)]
pub(crate) struct SourcesArguments {
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    pub(crate) output_format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct ClassifyArguments {
    #[arg(
        value_name = "PATH",
        required = true,
        help = "Repository-relative path to classify; repeatable"
    )]
    pub(crate) paths: Vec<String>,
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    pub(crate) output_format: OutputFormat,
    #[arg(long, help = "JSON config (default: platform config directory)")]
    pub(crate) config: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct UpdateArguments {
    #[arg(
        long,
        help = "Report whether a newer version exists without installing it"
    )]
    pub(crate) check: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RecordArguments {
    #[arg(long, help = "Tool or API name, for example cursor or openai-api")]
    pub(crate) provider: String,
    #[arg(long, help = "Stable session, request-group, or task identifier")]
    pub(crate) session: String,
    #[arg(long, help = "Model identifier (content is never accepted)")]
    pub(crate) model: Option<String>,
    #[arg(
        long,
        value_name = "DIR",
        help = "Working directory (default: current directory)"
    )]
    pub(crate) cwd: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = EventKind::Activity)]
    pub(crate) kind: EventKind,
    #[arg(long, value_enum, default_value_t = EventRole::Foreground)]
    pub(crate) role: EventRole,
    #[arg(long, help = "RFC 3339 signal time (default: now)")]
    pub(crate) timestamp: Option<String>,
    #[arg(
        long,
        requires = "completed_at",
        help = "RFC 3339 exact interval start"
    )]
    pub(crate) started_at: Option<String>,
    #[arg(long, requires = "started_at", help = "RFC 3339 exact interval end")]
    pub(crate) completed_at: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Event log (default: platform data directory; '-' writes stdout)"
    )]
    pub(crate) output: Option<PathBuf>,
}

/// Used when neither `--group-by` nor one of its shortcut flags is given.
pub(crate) const DEFAULT_GROUP_BY: &str = "repo";

/// Everything that shapes the report itself, flattened into both the default
/// command and `workstats ui`. Sharing one struct is what makes the explorer
/// answer the same question the printed report does, rather than a similar one.
#[derive(Debug, Args)]
pub(crate) struct ReportArguments {
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "DIR",
        help = "Git repository or directory to scan (default: current directory)"
    )]
    pub(crate) directory: Option<PathBuf>,
    #[arg(
        short = 'a',
        long,
        value_name = "REGEX",
        help = "Git author regex; repeat for several identities (config: \"authors\")"
    )]
    pub(crate) author: Vec<String>,
    #[arg(
        short = 'R',
        long,
        help = "Case-insensitive repository/path substring filter"
    )]
    pub(crate) repo: Option<String>,
    #[arg(
        long,
        help = "Exact repo label, or final folder name when no label matches"
    )]
    pub(crate) repo_exact: Option<String>,
    #[arg(short = 's', long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    pub(crate) since: Option<String>,
    #[arg(short = 'u', long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    pub(crate) until: Option<String>,
    // These pick the window the report covers; --group-by month and --period
    // month split the rows inside whatever window is already picked. The words
    // are otherwise identical, so all the help lines say which one they are.
    // The conflicts are the AUDIT V shape again: --month with --since could
    // only mean one of the two, and silently picking is what --by-repo used to
    // do to --group-by.
    #[arg(
        long,
        conflicts_with_all = ["year", "week", "since", "until"],
        help = "Filter to one calendar month: YYYY-MM, current (this), or last (previous)"
    )]
    pub(crate) month: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["since", "until"],
        help = "Filter to one calendar year: YYYY, current (this), or last (previous)"
    )]
    pub(crate) year: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["month", "year", "since", "until"],
        help = "Filter to one ISO week (Monday start): YYYY-Www, current (this), or last (previous)"
    )]
    pub(crate) week: Option<String>,
    #[arg(
        long,
        default_value = "5m",
        help = "Agent activity gap cap: 30s, 5m, 1h"
    )]
    pub(crate) gap_cap: String,
    #[arg(
        long,
        default_value = "1h",
        help = "Silent gap that ends a human-involvement block"
    )]
    pub(crate) human_idle: String,
    #[arg(
        long = "review-credit",
        visible_alias = "isolated-credit",
        default_value = "30m",
        help = "Setup and review time credited around each work block"
    )]
    pub(crate) review_credit: String,
    #[arg(
        long,
        help = "Print or serialize the auditable human-time calculation ledger (table and JSON only)"
    )]
    pub(crate) explain_human_time: bool,
    #[arg(
        long = "explain-repository-attribution",
        visible_alias = "explain-repos",
        help = "Explain how checkouts were combined into logical repositories (table and JSON only)"
    )]
    pub(crate) explain_repository_attribution: bool,
    // Optional rather than defaulted so clap can tell "the user asked for this
    // grouping" from "nobody said"; the shortcut flags below conflict with the
    // former only.
    #[arg(
        long = "group-by",
        visible_alias = "by",
        conflicts_with_all = ["by_repo", "matrix", "by_dir"],
        help = "Comma-separated grouping dimensions: root,repo,cwd,provider,model,day,week,month (default: repo)"
    )]
    pub(crate) group_by: Option<String>,
    #[arg(
        long,
        value_parser = ["day", "week", "month"],
        help = "Append a calendar grouping to the rows; --month/--year choose the window"
    )]
    pub(crate) period: Option<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Include provider(s); repeatable/comma-separated (default: all)")]
    pub(crate) provider: Vec<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Exclude provider(s); repeatable/comma-separated")]
    pub(crate) exclude_provider: Vec<String>,
    #[arg(long, value_name = "PROVIDER=PATH", action = clap::ArgAction::Append, help = "Override a built-in history location; repeatable")]
    pub(crate) history: Vec<String>,
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append, help = "Add a Workstats Events JSONL file or directory; repeatable")]
    pub(crate) events: Vec<PathBuf>,
    #[arg(long, help = "Skip the event log written by `workstats record`")]
    pub(crate) no_default_events: bool,
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    pub(crate) output_format: OutputFormat,
    #[arg(long, default_value_t = 30, help = "Maximum table rows (0 means all)")]
    pub(crate) top: usize,
    #[arg(long, help = "Skip Git history")]
    pub(crate) no_git: bool,
    #[arg(long, help = "Skip all AI histories")]
    pub(crate) no_ai: bool,
    #[arg(long, hide = true)]
    pub(crate) no_codex: bool,
    #[arg(long, hide = true)]
    pub(crate) no_claude: bool,
    #[arg(long, value_name = "CODEX_DIR", hide = true)]
    pub(crate) codex_dir: Option<PathBuf>,
    #[arg(long, value_name = "CODEX_DB", hide = true)]
    pub(crate) codex_db: Option<PathBuf>,
    #[arg(long, value_name = "CLAUDE_DIR", hide = true)]
    pub(crate) claude_dir: Option<PathBuf>,
    #[arg(long, help = "JSON config (default: platform config directory)")]
    pub(crate) config: Option<PathBuf>,
    #[arg(
        long,
        value_name = "CACHE",
        help = "Transcript index (default: platform cache directory)"
    )]
    pub(crate) cache: Option<PathBuf>,
    #[arg(long, help = "Disable the persistent transcript index")]
    pub(crate) no_cache: bool,
    #[arg(
        long,
        conflicts_with = "no_cache",
        help = "Rebuild the transcript index"
    )]
    pub(crate) rebuild_cache: bool,
    #[arg(long, value_name = "REGEX=NAME", action = clap::ArgAction::Append, help = "Custom source-root rule; repeatable")]
    pub(crate) source_rule: Vec<String>,
    #[arg(long, default_value_t = 4, help = "Git repository discovery depth")]
    pub(crate) depth: usize,
    #[arg(long, action = clap::ArgAction::Append, help = "Git file include glob; repeatable/comma-separated")]
    pub(crate) path: Vec<String>,
    #[arg(short = 'P', long, action = clap::ArgAction::Append, help = "Additional Git ignore glob")]
    pub(crate) path_exclude: Vec<String>,
    #[arg(long, help = "Include generated/vendor Git paths")]
    pub(crate) no_ignore: bool,
    // Off by default because `--author` is this tool's statement about whose
    // work a report describes, and a second identity is a second answer to
    // that. `=REGEX` is handed to Git raw, the same contract `--author` has,
    // and it *replaces* the built-in identities rather than adding to them —
    // "just these" is the only thing a single pattern can honestly mean.
    #[arg(
        long,
        value_name = "AUTHOR_REGEX",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        help = "Also read commits a coding agent authored, reported apart from your own and never as human time; =REGEX replaces the built-in identities"
    )]
    pub(crate) agent_commits: Option<String>,
    #[arg(
        long,
        help = "Read Co-authored-by: trailers, to flag your own commits as AI-assisted; identities only, never the message"
    )]
    pub(crate) co_authors: bool,
    #[arg(long, help = "Disable color in interactive output")]
    pub(crate) no_color: bool,
    #[arg(long, help = "Disable the interactive progress animation")]
    pub(crate) no_progress: bool,
    // Each of these rewrites the grouping wholesale, so two of them together —
    // or either with --group-by — used to mean one silently won (AUDIT V).
    #[arg(
        short = 'r',
        long,
        conflicts_with_all = ["matrix", "by_dir"],
        help = "Alias for --group-by month,repo"
    )]
    pub(crate) by_repo: bool,
    #[arg(
        short = 'm',
        long,
        conflicts_with = "by_dir",
        help = "Alias for --group-by repo,month"
    )]
    pub(crate) matrix: bool,
    #[arg(short = 'D', long, help = "Alias for --group-by cwd")]
    pub(crate) by_dir: bool,
    #[arg(
        long = "raw",
        visible_alias = "show-agent-work",
        help = "Show detailed parallel agent/model activity"
    )]
    pub(crate) raw: bool,
    #[arg(
        long,
        help = "Opt in to a throttled (~daily) background check for newer releases; shown as a footer notice, never installed automatically"
    )]
    pub(crate) check_updates: bool,
    #[arg(
        long,
        help = "Suppress the update-available footer and background check for this run"
    )]
    pub(crate) no_update_check: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "workstats",
    version,
    about = "Measures local Git output and active AI-assisted work across supported CLIs, IDEs, and API event logs. Transcript text is never emitted and no network calls are made unless you run `workstats update` or opt into --check-updates.",
    after_help = "Human work is a supervision-inclusive estimate from prompts, foreground session boundaries, and authored commits, not a stopwatch. Autonomous agent output does not imply continuous human presence."
)]
pub(crate) struct Arguments {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

/// Names the flag and the value it was given. A parse failure used to say only
/// what a duration should look like, never which flag was wrong (AUDIT V).
pub(crate) fn duration_flag(flag: &str, value: &str) -> Result<Duration> {
    parse_duration(value).with_context(|| format!("invalid {flag} {value:?}"))
}

pub(crate) fn bound_flag(
    flag: &str,
    value: Option<&str>,
    until: bool,
) -> Result<Option<DateTime<Utc>>> {
    parse_bound(value, until)
        .with_context(|| format!("invalid {flag} {:?}", value.unwrap_or_default()))
}

/// The half-open `[since, until)` window one run reports on; `None` on either
/// end means unbounded there.
pub(crate) type ReportWindow = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// `--month`, `--year` and `--week` are shorthand for the whole window, and clap
/// has already refused them alongside `--since`/`--until`, so whichever is
/// present decides both ends. The reference instant is taken as an argument rather than
/// read from the clock so `current` and `last` stay testable.
pub(crate) fn report_window(
    arguments: &ReportArguments,
    reference: DateTime<Utc>,
) -> Result<ReportWindow> {
    if let Some(value) = arguments.month.as_deref() {
        let (since, until) =
            month_span(value, reference).with_context(|| format!("invalid --month {value:?}"))?;
        return Ok((Some(since), Some(until)));
    }
    if let Some(value) = arguments.year.as_deref() {
        let (since, until) =
            year_span(value, reference).with_context(|| format!("invalid --year {value:?}"))?;
        return Ok((Some(since), Some(until)));
    }
    if let Some(value) = arguments.week.as_deref() {
        let (since, until) =
            week_span(value, reference).with_context(|| format!("invalid --week {value:?}"))?;
        return Ok((Some(since), Some(until)));
    }
    Ok((
        bound_flag("--since", arguments.since.as_deref(), false)?,
        bound_flag("--until", arguments.until.as_deref(), true)?,
    ))
}

/// The directory Git history is scanned from. It takes its candidates instead
/// of reading the environment itself so both the precedence and the error stay
/// testable.
pub(crate) fn scan_directory(
    explicit: Option<&Path>,
    from_environment: Option<PathBuf>,
    current: Option<PathBuf>,
) -> Result<PathBuf> {
    let (directory, origin) = match (explicit, from_environment) {
        (Some(path), _) => (path.to_path_buf(), "--dir"),
        (None, Some(path)) => (path, "WORKSTATS_DIR"),
        (None, None) => (
            current.unwrap_or_else(|| PathBuf::from(".")),
            "the current working directory",
        ),
    };
    // A scan root that does not exist used to produce an all-zero report and
    // exit 0 (AUDIT V), which reads as "no work found", not "wrong path".
    if !directory.is_dir() {
        bail!(
            "{origin} does not name an existing directory: {}",
            directory.display()
        );
    }
    Ok(directory)
}

/// The Git author patterns a run describes, from the first source that names
/// any: `--author` (repeatable), then `WORKSTATS_AUTHOR` (one value), then the
/// config file's `authors`, then the global Git identity.
///
/// The sources replace each other rather than combine. A flag on the command
/// line has to be able to narrow a run to one identity even when the config
/// file lists three, and a union would make that impossible. The Git default is
/// the last resort and is only consulted — it spawns `git config` — when
/// nothing more specific was given.
pub(crate) fn resolve_authors(
    flags: &[String],
    environment: Option<String>,
    configured: &[String],
    default: impl FnOnce() -> Option<String>,
) -> Vec<String> {
    if !flags.is_empty() {
        return flags.to_vec();
    }
    if let Some(value) = environment {
        return vec![value];
    }
    if !configured.is_empty() {
        return configured.to_vec();
    }
    default().into_iter().collect()
}

/// The Git identities the second, agent-authorship pass matches.
///
/// Empty is the default and means no second pass runs at all, not that it runs
/// unfiltered — `read_agent_commits` spawns nothing for an empty list, so the
/// feature costs nothing when it is off. A bare `--agent-commits` takes the
/// built-in identities; `--agent-commits=REGEX` replaces them, because one
/// pattern can only honestly mean "just this one".
pub(crate) fn agent_author_patterns(argument: Option<&str>) -> Vec<String> {
    match argument {
        None => Vec::new(),
        Some(value) if value.trim().is_empty() => DEFAULT_AGENT_AUTHORS
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        Some(pattern) => vec![pattern.to_string()],
    }
}

/// The grouping dimensions for one run, validated. Split out of `run` so the
/// shortcut flags can be exercised without building a report.
pub(crate) fn grouping_dimensions(arguments: &ReportArguments) -> Result<Vec<String>> {
    let mut dimensions: Vec<String> = if arguments.by_repo {
        vec!["month".to_string(), "repo".to_string()]
    } else if arguments.matrix {
        vec!["repo".to_string(), "month".to_string()]
    } else if arguments.by_dir {
        vec!["cwd".to_string()]
    } else {
        arguments
            .group_by
            .as_deref()
            .unwrap_or(DEFAULT_GROUP_BY)
            .split(',')
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
            .map(str::to_string)
            .collect()
    };
    if let Some(period) = &arguments.period
        && !dimensions.contains(period)
    {
        dimensions.push(period.clone());
    }
    let unique: HashSet<_> = dimensions.iter().collect();
    if dimensions.is_empty()
        || unique.len() != dimensions.len()
        || dimensions
            .iter()
            .any(|name| !DIMENSIONS.contains(&name.as_str()))
    {
        bail!("--group-by must contain unique values from: {}", {
            let mut values = DIMENSIONS.to_vec();
            values.sort();
            values.join(", ")
        });
    }
    // A row sits in one calendar bucket, and a week straddles months, so any
    // two of these would describe a bucket that does not exist.
    if ["day", "week", "month"]
        .iter()
        .filter(|calendar| dimensions.iter().any(|name| name == *calendar))
        .count()
        > 1
    {
        bail!("day, week and month are alternative calendar groupings; choose one");
    }
    Ok(dimensions)
}
pub(crate) fn valid_provider_identifier(provider: &str, allow_all: bool) -> bool {
    !provider.is_empty()
        && (allow_all || provider != "all")
        && provider.len() <= 64
        && provider.as_bytes()[0].is_ascii_alphanumeric()
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'-'))
}

pub(crate) fn csv_globs(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    /// Mid-January, so `last` has to cross a year boundary, and fixed, so no
    /// assertion below depends on when the suite runs.
    fn reference() -> DateTime<Utc> {
        local_moment(2026, 1, 15, 12, 0)
    }

    fn local_moment(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    /// The `--since X --until Y` pair a calendar shorthand has to be equal to;
    /// anything else means the shorthand quietly reports a different window
    /// than the longhand it stands for.
    fn longhand(since: &str, until: &str) -> ReportWindow {
        (
            bound_flag("--since", Some(since), false).unwrap(),
            bound_flag("--until", Some(until), true).unwrap(),
        )
    }

    fn window(flags: &[&str]) -> ReportWindow {
        report_window(&report_arguments(flags), reference()).unwrap()
    }

    /// The `>= since && < until` test `build_report` applies, run against the
    /// window `flags` select.
    fn covers(flags: &[&str], moment: DateTime<Utc>) -> bool {
        let (since, until) = window(flags);
        since.unwrap() <= moment && moment < until.unwrap()
    }

    #[test]
    fn the_classify_subcommand_takes_paths_and_a_format() {
        let arguments = Arguments::try_parse_from([
            "workstats",
            "classify",
            "src/main.rs",
            "tests/lib.rs",
            "--format",
            "json",
        ])
        .unwrap();
        let Some(Command::Classify(command)) = arguments.command else {
            panic!("expected the classify subcommand");
        };
        assert_eq!(2, command.paths.len());
        assert_eq!(OutputFormat::Json, command.output_format);
        assert!(Arguments::try_parse_from(["workstats", "classify"]).is_err());
    }

    /// `--config` before the subcommand lands on the report arguments, which is
    /// why `classify_paths` takes it as a fallback: without that it parses fine
    /// and is then silently ignored, so the user sees the built-in categories
    /// and no error.
    #[test]
    fn classify_accepts_the_config_flag_on_either_side_of_the_subcommand() {
        let before = Arguments::try_parse_from([
            "workstats",
            "--config",
            "/tmp/rules.json",
            "classify",
            "src/main.rs",
        ])
        .unwrap();
        let Some(Command::Classify(command)) = &before.command else {
            panic!("expected the classify subcommand");
        };
        assert_eq!(None, command.config);
        assert_eq!(
            Some(Path::new("/tmp/rules.json")),
            before.report.config.as_deref()
        );

        let after = Arguments::try_parse_from([
            "workstats",
            "classify",
            "--config",
            "/tmp/rules.json",
            "src/main.rs",
        ])
        .unwrap();
        let Some(Command::Classify(command)) = &after.command else {
            panic!("expected the classify subcommand");
        };
        assert_eq!(
            Some(Path::new("/tmp/rules.json")),
            command.config.as_deref()
        );
    }

    /// The explorer is only worth having if it answers the same question the
    /// printed report does, which means taking the same filters.
    #[test]
    fn the_ui_subcommand_takes_the_report_flags() {
        let arguments = Arguments::try_parse_from([
            "workstats",
            "ui",
            "--dir",
            "/repos/widget",
            "--since",
            "2026-01",
            "--provider",
            "claude,codex",
            "--group-by",
            "repo,month",
        ])
        .unwrap();
        let Some(Command::Ui(command)) = arguments.command else {
            panic!("expected the ui subcommand");
        };
        assert_eq!(Some(PathBuf::from("/repos/widget")), command.directory);
        assert_eq!(Some("2026-01".to_string()), command.since);
        assert_eq!(vec!["claude", "codex"], command.provider);
        assert_eq!(
            vec!["repo".to_string(), "month".to_string()],
            grouping_dimensions(&command).unwrap()
        );
        assert!(Arguments::try_parse_from(["workstats", "ui"]).is_ok());
    }

    fn report_arguments(flags: &[&str]) -> ReportArguments {
        let mut command = vec!["workstats"];
        command.extend_from_slice(flags);
        Arguments::try_parse_from(command).unwrap().report
    }

    #[test]
    fn the_grouping_shortcuts_expand_and_default_to_repo() {
        assert_eq!(
            vec!["repo"],
            grouping_dimensions(&report_arguments(&[])).unwrap()
        );
        assert_eq!(
            vec!["month", "repo"],
            grouping_dimensions(&report_arguments(&["--by-repo"])).unwrap()
        );
        assert_eq!(
            vec!["repo", "month"],
            grouping_dimensions(&report_arguments(&["--matrix"])).unwrap()
        );
        assert_eq!(
            vec!["cwd"],
            grouping_dimensions(&report_arguments(&["--by-dir"])).unwrap()
        );
        assert_eq!(
            vec!["cwd", "day"],
            grouping_dimensions(&report_arguments(&["--by-dir", "--period", "day"])).unwrap()
        );
        assert!(grouping_dimensions(&report_arguments(&["--group-by", "repo,repo"])).is_err());
        assert!(grouping_dimensions(&report_arguments(&["--group-by", "nonsense"])).is_err());
        assert!(grouping_dimensions(&report_arguments(&["--group-by", "day,month"])).is_err());
    }

    /// Each shortcut used to overwrite the grouping wholesale, so two together
    /// meant one silently won (AUDIT V).
    #[test]
    fn the_grouping_shortcuts_conflict_instead_of_overriding_each_other() {
        for flags in [
            ["--by-repo", "--matrix"],
            ["--by-repo", "--by-dir"],
            ["--matrix", "--by-dir"],
        ] {
            assert!(
                Arguments::try_parse_from(["workstats", flags[0], flags[1]]).is_err(),
                "{flags:?} should conflict"
            );
        }
        for shortcut in ["--by-repo", "--matrix", "--by-dir"] {
            assert!(
                Arguments::try_parse_from(["workstats", shortcut, "--group-by", "cwd"]).is_err(),
                "{shortcut} should conflict with --group-by"
            );
            assert!(Arguments::try_parse_from(["workstats", shortcut]).is_ok());
        }
    }

    #[test]
    fn a_bad_duration_or_date_names_the_flag_and_the_value() {
        let error = format!("{:#}", duration_flag("--gap-cap", "5x").unwrap_err());
        assert!(error.contains("--gap-cap"), "{error}");
        assert!(error.contains("\"5x\""), "{error}");
        let error = format!(
            "{:#}",
            bound_flag("--since", Some("2026-13"), false).unwrap_err()
        );
        assert!(error.contains("--since"), "{error}");
        assert!(error.contains("2026-13"), "{error}");
        assert!(bound_flag("--until", None, true).unwrap().is_none());
        assert_eq!(
            Duration::minutes(5),
            duration_flag("--gap-cap", "5m").unwrap()
        );
    }

    /// The shorthand is only trustworthy if it lands on the very pair the user
    /// would otherwise have typed, and it lives on the shared struct so the
    /// explorer answers the same question the printed report does.
    #[test]
    fn the_month_shorthand_sets_both_bounds_and_reaches_the_explorer() {
        assert_eq!(
            longhand("2026-08", "2026-08"),
            window(&["--month", "2026-08"])
        );
        assert_eq!(longhand("2026-01", "2026-12"), window(&["--year", "2026"]));

        let parsed = Arguments::try_parse_from(["workstats", "ui", "--month", "last"]).unwrap();
        let Some(Command::Ui(command)) = parsed.command else {
            panic!("expected the ui subcommand");
        };
        assert_eq!(Some("last".to_string()), command.month);
    }

    /// Relative values are what earn the flag its keep — a recurring report is
    /// one fixed command — so they resolve against a reference instant rather
    /// than the clock, on the local calendar every other bound snaps to.
    #[test]
    fn relative_calendar_values_resolve_against_the_reference() {
        for value in ["current", "this"] {
            assert_eq!(longhand("2026-01", "2026-01"), window(&["--month", value]));
        }
        // January's previous month is the December before it, not month zero.
        for value in ["last", "previous"] {
            assert_eq!(longhand("2025-12", "2025-12"), window(&["--month", value]));
        }
        for value in ["current", "this"] {
            assert_eq!(longhand("2026-01", "2026-12"), window(&["--year", value]));
        }
        for value in ["last", "previous"] {
            assert_eq!(longhand("2025-01", "2025-12"), window(&["--year", value]));
        }
    }

    /// `build_report` filters with `>= since && < until`, so the shorthand has
    /// to end on the first instant *after* the span; ending on its last day
    /// would silently drop that day's work.
    #[test]
    fn a_calendar_window_is_half_open_in_local_time() {
        let january = ["--month", "2026-01"];
        assert!(covers(&january, local_moment(2026, 1, 1, 0, 1)));
        assert!(covers(&january, local_moment(2026, 1, 31, 23, 59)));
        assert!(!covers(&january, local_moment(2026, 2, 1, 0, 0)));
        assert!(!covers(&january, local_moment(2025, 12, 31, 23, 59)));

        // December is the case a naive "month + 1" gets wrong, and it has to
        // roll the year over for a month and for a year alike.
        let december = ["--month", "2026-12"];
        assert!(covers(&december, local_moment(2026, 12, 31, 23, 59)));
        assert!(!covers(&december, local_moment(2027, 1, 1, 0, 0)));
        let year = ["--year", "2026"];
        assert!(covers(&year, local_moment(2026, 1, 1, 0, 1)));
        assert!(covers(&year, local_moment(2026, 12, 31, 23, 59)));
        assert!(!covers(&year, local_moment(2027, 1, 1, 0, 0)));
        assert!(!covers(&year, local_moment(2025, 12, 31, 23, 59)));
    }

    /// Same shape as the grouping shortcuts: a combination that could only mean
    /// one of the two has to say so rather than let one quietly win (AUDIT V).
    #[test]
    fn the_calendar_shorthands_conflict_with_each_other_and_with_the_bounds() {
        for flags in [
            ["--month", "2026-08", "--year", "2026"],
            ["--month", "2026-08", "--since", "2026-01"],
            ["--month", "2026-08", "--until", "2026-12"],
            ["--year", "2026", "--since", "2026-01"],
            ["--year", "2026", "--until", "2026-12"],
            ["--week", "2026-W09", "--month", "2026-08"],
            ["--week", "2026-W09", "--year", "2026"],
            ["--week", "2026-W09", "--since", "2026-01"],
            ["--week", "2026-W09", "--until", "2026-12"],
        ] {
            let mut command = vec!["workstats"];
            command.extend_from_slice(&flags);
            assert!(
                Arguments::try_parse_from(command).is_err(),
                "{flags:?} should conflict"
            );
        }
        // The pieces each shorthand replaces stay legal on their own.
        for flags in [
            vec!["--month", "2026-08"],
            vec!["--year", "2026"],
            vec!["--since", "2026-01", "--until", "2026-12"],
            // A filter and a grouping are orthogonal, so these must coexist.
            vec!["--month", "current", "--group-by", "repo"],
            vec!["--year", "last", "--period", "month"],
            vec!["--week", "2026-W09"],
            vec!["--week", "last", "--period", "week"],
        ] {
            let mut command = vec!["workstats"];
            command.extend_from_slice(&flags);
            assert!(
                Arguments::try_parse_from(command).is_ok(),
                "{flags:?} should parse"
            );
        }
    }

    /// `--week` is the ISO week as a half-open Monday-to-Monday window, so the
    /// first week of 2026 starts in December 2025 and the last of 2026 ends in
    /// January 2027. It has to equal the `--since`/`--until` pair it stands for.
    #[test]
    fn the_week_shorthand_is_an_iso_monday_to_sunday_window() {
        assert_eq!(
            longhand("2025-12-29", "2026-01-04"),
            window(&["--week", "2026-W01"])
        );
        assert_eq!(
            longhand("2026-02-23", "2026-03-01"),
            window(&["--week", "2026-W09"])
        );
        assert_eq!(
            longhand("2026-12-28", "2027-01-03"),
            window(&["--week", "2026-W53"])
        );
        // Reference is Thursday 2026-01-15, in 2026-W03.
        for value in ["current", "this"] {
            assert_eq!(
                longhand("2026-01-12", "2026-01-18"),
                window(&["--week", value])
            );
        }
        for value in ["last", "previous"] {
            assert_eq!(
                longhand("2026-01-05", "2026-01-11"),
                window(&["--week", value])
            );
        }
        // Sunday night belongs to the week, Monday 00:00 to the next one.
        assert!(covers(
            &["--week", "2026-W01"],
            local_moment(2026, 1, 4, 23, 59)
        ));
        assert!(!covers(
            &["--week", "2026-W01"],
            local_moment(2026, 1, 5, 0, 0)
        ));
        assert!(covers(
            &["--week", "2026-W01"],
            local_moment(2025, 12, 29, 0, 0)
        ));
    }

    #[test]
    fn a_bad_week_names_the_flag_and_the_value() {
        // 2025 has no week 53.
        for value in ["2025-W53", "2026-09", "nope"] {
            let error = format!(
                "{:#}",
                report_window(&report_arguments(&["--week", value]), reference()).unwrap_err()
            );
            assert!(error.contains("--week"), "{error}");
            assert!(error.contains(value), "{error}");
        }
    }

    #[test]
    fn week_is_a_calendar_grouping_that_excludes_day_and_month() {
        assert_eq!(
            vec!["repo", "week"],
            grouping_dimensions(&report_arguments(&["--period", "week"])).unwrap()
        );
        assert_eq!(
            vec!["week", "repo"],
            grouping_dimensions(&report_arguments(&["--group-by", "week,repo"])).unwrap()
        );
        for flags in [
            vec!["--group-by", "week,month"],
            vec!["--group-by", "day,week"],
            vec!["--by-repo", "--period", "week"],
        ] {
            assert!(
                grouping_dimensions(&report_arguments(&flags)).is_err(),
                "{flags:?} should be refused"
            );
        }
    }

    /// A month that does not exist used to be the kind of thing you only catch
    /// by seeing it next to what you typed (AUDIT V).
    #[test]
    fn a_bad_month_or_year_names_the_flag_and_the_value() {
        let error = format!(
            "{:#}",
            report_window(&report_arguments(&["--month", "2026-13"]), reference()).unwrap_err()
        );
        assert!(error.contains("--month"), "{error}");
        assert!(error.contains("2026-13"), "{error}");
        let error = format!(
            "{:#}",
            report_window(&report_arguments(&["--year", "26"]), reference()).unwrap_err()
        );
        assert!(error.contains("--year"), "{error}");
        assert!(error.contains("\"26\""), "{error}");
    }

    /// A typo'd scan root used to look exactly like a quiet week (AUDIT V).
    #[test]
    fn a_missing_scan_directory_is_an_error_that_names_where_it_came_from() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("nope");
        assert_eq!(
            temporary.path().to_path_buf(),
            scan_directory(Some(temporary.path()), None, None).unwrap()
        );
        // An explicit --dir wins over both fallbacks, so its own absence is
        // what gets reported.
        let error = format!(
            "{:#}",
            scan_directory(
                Some(&missing),
                Some(temporary.path().to_path_buf()),
                Some(temporary.path().to_path_buf())
            )
            .unwrap_err()
        );
        assert!(error.contains("--dir"), "{error}");
        assert!(error.contains("nope"), "{error}");
        let error = format!(
            "{:#}",
            scan_directory(None, Some(missing.clone()), None).unwrap_err()
        );
        assert!(error.contains("WORKSTATS_DIR"), "{error}");
        let error = format!(
            "{:#}",
            scan_directory(None, None, Some(missing)).unwrap_err()
        );
        assert!(error.contains("current working directory"), "{error}");
    }

    /// Nothing happens unless the run asks for it, and asking for it with a
    /// pattern replaces the built-in identities rather than joining them.
    #[test]
    fn author_is_repeatable_and_the_sources_replace_rather_than_merge() {
        let flags = report_arguments(&["-a", "a@x", "--author", "b@y"]).author;
        assert_eq!(vec!["a@x", "b@y"], flags);
        assert!(report_arguments(&[]).author.is_empty());

        let configured = vec!["c@z".to_string(), "d@z".to_string()];
        let env = Some("e@env".to_string());
        let unused =
            || panic!("the Git default must not be read when something more specific exists");
        // CLI > env > config > git config.
        assert_eq!(
            flags,
            resolve_authors(&flags, env.clone(), &configured, unused)
        );
        assert_eq!(
            vec!["e@env"],
            resolve_authors(&[], env, &configured, unused)
        );
        assert_eq!(configured, resolve_authors(&[], None, &configured, unused));
        assert_eq!(
            vec!["g@git"],
            resolve_authors(&[], None, &[], || Some("g@git".to_string()))
        );
        assert!(resolve_authors(&[], None, &[], || None).is_empty());
    }

    #[test]
    fn agent_commits_are_off_until_asked_for_and_the_pattern_replaces_the_defaults() {
        assert!(
            report_arguments(&[]).agent_commits.is_none(),
            "the second pass must not run by default"
        );
        assert!(agent_author_patterns(None).is_empty());

        let bare = report_arguments(&["--agent-commits"]);
        assert_eq!(Some(""), bare.agent_commits.as_deref());
        assert_eq!(
            DEFAULT_AGENT_AUTHORS.len(),
            agent_author_patterns(bare.agent_commits.as_deref()).len()
        );

        let custom = report_arguments(&["--agent-commits=my-bot@example.com"]);
        assert_eq!(
            vec!["my-bot@example.com".to_string()],
            agent_author_patterns(custom.agent_commits.as_deref())
        );

        // Reading trailers is its own decision: it widens what is read out of a
        // commit message, so it does not ride along with the second pass.
        assert!(!report_arguments(&["--agent-commits"]).co_authors);
        assert!(report_arguments(&["--co-authors"]).co_authors);
        assert!(report_arguments(&["--co-authors"]).agent_commits.is_none());
    }

    #[test]
    fn repeated_and_comma_separated_globs_are_normalized() {
        assert_eq!(
            vec!["src/**", "tests/**", "docs/**"],
            csv_globs(&["src/**, tests/**".into(), "docs/**".into()])
        );
    }
}
