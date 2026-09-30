//! Command-line surface: every clap definition plus the helpers that turn the
//! parsed flags into validated values.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::aggregate::DIMENSIONS;
use crate::allocate;
use crate::git::DEFAULT_AGENT_AUTHORS;
use crate::paths::expand_path;
use crate::sources::normalize_provider;
use crate::timeutil::{month_span, parse_bound, parse_duration, week_span, year_span};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum OutputFormat {
    Table,
    Json,
    Csv,
    /// GitHub-flavoured tables for a PR, issue, or wiki. The report and
    /// `allocate` only: `sources` and `classify` print records, not reports.
    Markdown,
    /// One self-contained static page: inline CSS, no script, nothing fetched.
    /// The report and `allocate` only.
    Html,
}

impl OutputFormat {
    /// The spelling `--format` takes, for messages that name the flag.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Json => "json",
            Self::Csv => "csv",
            Self::Markdown => "markdown",
            Self::Html => "html",
        }
    }
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
/// Built-in values for the flags a config `defaults` block may set. They are
/// applied by [`ConfigDefaults::resolve`] rather than by clap, because clap
/// cannot say whether a value was typed or defaulted.
pub(crate) const DEFAULT_DEPTH: usize = 4;
pub(crate) const DEFAULT_GAP_CAP: &str = "5m";
pub(crate) const DEFAULT_HUMAN_IDLE: &str = "1h";
pub(crate) const DEFAULT_REVIEW_CREDIT: &str = "30m";

/// Everything that shapes the report itself, flattened into both the default
/// command and `workstats ui`. Sharing one struct is what makes the explorer
/// answer the same question the printed report does, rather than a similar one.
#[derive(Debug, Args)]
pub(crate) struct ReportArguments {
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "DIR",
        help = "Git repository or directory to scan (default: current directory; config: \"defaults.dir\")"
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
        help = "Agent activity gap cap: 30s, 5m, 1h (default: 5m; config: \"defaults.gap_cap\")"
    )]
    pub(crate) gap_cap: Option<String>,
    #[arg(
        long,
        help = "Silent gap that ends a human-involvement block (default: 1h; config: \"defaults.human_idle\")"
    )]
    pub(crate) human_idle: Option<String>,
    #[arg(
        long = "review-credit",
        visible_alias = "isolated-credit",
        help = "Setup and review time credited around each work block (default: 30m; config: \"defaults.review_credit\")"
    )]
    pub(crate) review_credit: Option<String>,
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
        help = "Comma-separated grouping dimensions: root,repo,cwd,provider,model,day,week,month (default: repo; config: \"defaults.group_by\")"
    )]
    pub(crate) group_by: Option<String>,
    #[arg(
        long,
        value_parser = ["day", "week", "month"],
        help = "Append a calendar grouping to the rows; --month/--year choose the window"
    )]
    pub(crate) period: Option<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Include provider(s); repeatable/comma-separated (default: all; config: \"defaults.providers\")")]
    pub(crate) provider: Vec<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Exclude provider(s); repeatable/comma-separated")]
    pub(crate) exclude_provider: Vec<String>,
    #[arg(long, value_name = "PROVIDER=PATH", action = clap::ArgAction::Append, help = "Override a built-in history location; repeatable")]
    pub(crate) history: Vec<String>,
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append, help = "Add a Workstats Events JSONL file or directory; repeatable")]
    pub(crate) events: Vec<PathBuf>,
    #[arg(long, help = "Skip the event log written by `workstats record`")]
    pub(crate) no_default_events: bool,
    // Optional rather than defaulted, like the durations and `--depth`: a
    // config default may only fill in a flag that was not given, and a clap
    // default is indistinguishable from `--format table` typed by hand.
    #[arg(
        long = "format",
        value_enum,
        help = "Output format (default: table; config: \"defaults.format\")"
    )]
    pub(crate) output_format: Option<OutputFormat>,
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
    #[arg(
        long,
        help = "Git repository discovery depth (default: 4; config: \"defaults.depth\")"
    )]
    pub(crate) depth: Option<usize>,
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

/// The directory Git history is scanned from: `--dir`, then `WORKSTATS_DIR`,
/// then the config's `defaults.dir`, then the working directory. It takes its candidates instead
/// of reading the environment itself so both the precedence and the error stay
/// testable.
pub(crate) fn scan_directory(
    explicit: Option<&Path>,
    from_environment: Option<PathBuf>,
    configured: Option<PathBuf>,
    current: Option<PathBuf>,
) -> Result<PathBuf> {
    let (directory, origin) = match (explicit, from_environment, configured) {
        (Some(path), _, _) => (path.to_path_buf(), "--dir"),
        (None, Some(path), _) => (path, "WORKSTATS_DIR"),
        (None, None, Some(path)) => (path, "defaults.dir"),
        (None, None, None) => (
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
    validate_dimensions(&dimensions)?;
    Ok(dimensions)
}

fn validate_dimensions(dimensions: &[String]) -> Result<()> {
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
    Ok(())
}
/// The config file's `defaults` block: the everyday flags a user would rather
/// not retype. Every key is named after its flag's long name and sits below the
/// flag (and, where one exists, the environment variable) in precedence:
/// flag > environment > config default > built-in default.
///
/// Values are kept as raw JSON until [`ConfigDefaults::parse`] so that a bad
/// one is refused with the key it came from (`defaults.depth`) instead of
/// serde's positionless "invalid type". Unknown keys are refused the same way:
/// a misspelt default silently doing nothing is the failure this prevents.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDefaults {
    dir: Option<serde_json::Value>,
    depth: Option<serde_json::Value>,
    format: Option<serde_json::Value>,
    providers: Option<serde_json::Value>,
    group_by: Option<serde_json::Value>,
    gap_cap: Option<serde_json::Value>,
    human_idle: Option<serde_json::Value>,
    review_credit: Option<serde_json::Value>,
}

/// The validated `defaults` block.
#[derive(Debug, Default)]
pub(crate) struct ConfigDefaults {
    pub(crate) dir: Option<PathBuf>,
    pub(crate) depth: Option<usize>,
    pub(crate) format: Option<OutputFormat>,
    pub(crate) providers: Vec<String>,
    pub(crate) group_by: Option<String>,
    pub(crate) gap_cap: Option<String>,
    pub(crate) human_idle: Option<String>,
    pub(crate) review_credit: Option<String>,
}

/// What one run's scalar flags resolved to, and which of them came from the
/// config file (key to the value used) so the report can say so.
#[derive(Debug)]
pub(crate) struct ResolvedDefaults {
    pub(crate) depth: usize,
    pub(crate) format: OutputFormat,
    pub(crate) gap_cap: String,
    pub(crate) human_idle: String,
    pub(crate) review_credit: String,
    pub(crate) from_config: BTreeMap<String, String>,
}

fn default_string(key: &str, value: &serde_json::Value) -> Result<String> {
    match value.as_str() {
        Some(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        _ => bail!("defaults.{key} must be a non-empty string"),
    }
}

impl ConfigDefaults {
    pub(crate) fn parse(value: &serde_json::Value, home: &Path) -> Result<Self> {
        let raw: RawDefaults =
            serde_json::from_value(value.clone()).context("invalid \"defaults\" configuration")?;
        let mut defaults = Self::default();
        if let Some(value) = &raw.dir {
            defaults.dir = Some(expand_path(&default_string("dir", value)?, home));
        }
        if let Some(value) = &raw.depth {
            let depth = value
                .as_u64()
                .and_then(|depth| usize::try_from(depth).ok())
                .with_context(|| "defaults.depth must be a whole number of at least 0")?;
            defaults.depth = Some(depth);
        }
        if let Some(value) = &raw.format {
            let name = default_string("format", value)?;
            defaults.format = Some(OutputFormat::from_str(&name, true).map_err(|_| {
                anyhow::anyhow!(
                    "defaults.format {name:?} is not a format; expected one of {}",
                    OutputFormat::value_variants()
                        .iter()
                        .filter_map(|variant| variant.to_possible_value())
                        .map(|variant| variant.get_name().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?);
        }
        if let Some(value) = &raw.providers {
            // A list, or one comma-separated string like `--provider`.
            let names: Vec<String> = match value {
                serde_json::Value::Array(items) => items
                    .iter()
                    .map(|item| default_string("providers", item))
                    .collect::<Result<_>>()?,
                serde_json::Value::String(text) => text.split(',').map(str::to_string).collect(),
                _ => bail!("defaults.providers must be a list of provider names"),
            };
            for name in names
                .iter()
                .flat_map(|name| name.split(','))
                .map(normalize_provider)
                .filter(|name| !name.is_empty())
            {
                if !valid_provider_identifier(&name, true) {
                    bail!(
                        "defaults.providers {name:?} must use letters, numbers, '.', '/', or '-'"
                    );
                }
                defaults.providers.push(name);
            }
        }
        if let Some(value) = &raw.group_by {
            let text = default_string("group_by", value)?;
            let dimensions: Vec<String> = text
                .split(',')
                .map(str::trim)
                .filter(|piece| !piece.is_empty())
                .map(str::to_string)
                .collect();
            validate_dimensions(&dimensions).context("invalid defaults.group_by")?;
            defaults.group_by = Some(text);
        }
        for (key, slot, value) in [
            ("gap_cap", &mut defaults.gap_cap, &raw.gap_cap),
            ("human_idle", &mut defaults.human_idle, &raw.human_idle),
            (
                "review_credit",
                &mut defaults.review_credit,
                &raw.review_credit,
            ),
        ] {
            if let Some(value) = value {
                let text = default_string(key, value)?;
                duration_flag(&format!("defaults.{key}"), &text)?;
                *slot = Some(text);
            }
        }
        Ok(defaults)
    }

    /// Fills in whatever the command line left unsaid. `arguments` holds only
    /// what the user typed (the flags have no clap default), so a flag given
    /// with the built-in value still beats the config. `explore` is `workstats
    /// ui`, which has no machine-readable output and therefore ignores
    /// `defaults.format`; `providers` and `group_by` are written back into
    /// `arguments` because they are already optional there.
    pub(crate) fn resolve(
        &self,
        arguments: &mut ReportArguments,
        explore: bool,
    ) -> ResolvedDefaults {
        let mut from_config = BTreeMap::new();
        if arguments.provider.is_empty() && !self.providers.is_empty() {
            arguments.provider = self.providers.clone();
            from_config.insert("providers".to_string(), self.providers.join(","));
        }
        if arguments.group_by.is_none()
            && !grouping_is_overridden(arguments)
            && let Some(group_by) = &self.group_by
        {
            arguments.group_by = Some(group_by.clone());
            from_config.insert("group_by".to_string(), group_by.clone());
        }
        let mut pick = |key: &str,
                        flag: &Option<String>,
                        config: &Option<String>,
                        built_in: &str| match (flag, config) {
            (Some(value), _) => value.clone(),
            (None, Some(value)) => {
                from_config.insert(key.to_string(), value.clone());
                value.clone()
            }
            (None, None) => built_in.to_string(),
        };
        let gap_cap = pick(
            "gap_cap",
            &arguments.gap_cap,
            &self.gap_cap,
            DEFAULT_GAP_CAP,
        );
        let human_idle = pick(
            "human_idle",
            &arguments.human_idle,
            &self.human_idle,
            DEFAULT_HUMAN_IDLE,
        );
        let review_credit = pick(
            "review_credit",
            &arguments.review_credit,
            &self.review_credit,
            DEFAULT_REVIEW_CREDIT,
        );
        let depth = match (arguments.depth, self.depth) {
            (Some(depth), _) => depth,
            (None, Some(depth)) => {
                from_config.insert("depth".to_string(), depth.to_string());
                depth
            }
            (None, None) => DEFAULT_DEPTH,
        };
        let format = match (arguments.output_format, self.format) {
            (Some(format), _) => format,
            (None, Some(format)) if !explore => {
                let name = format
                    .to_possible_value()
                    .map(|value| value.get_name().to_string())
                    .unwrap_or_default();
                from_config.insert("format".to_string(), name);
                format
            }
            _ => OutputFormat::Table,
        };
        ResolvedDefaults {
            depth,
            format,
            gap_cap,
            human_idle,
            review_credit,
            from_config,
        }
    }
}

/// The grouping shortcut flags, which `--group-by` conflicts with; a config
/// `group_by` must not compete with them either.
fn grouping_is_overridden(arguments: &ReportArguments) -> bool {
    arguments.by_repo || arguments.matrix || arguments.by_dir
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
            scan_directory(Some(temporary.path()), None, None, None).unwrap()
        );
        // An explicit --dir wins over both fallbacks, so its own absence is
        // what gets reported.
        let error = format!(
            "{:#}",
            scan_directory(
                Some(&missing),
                Some(temporary.path().to_path_buf()),
                Some(temporary.path().to_path_buf()),
                Some(temporary.path().to_path_buf())
            )
            .unwrap_err()
        );
        assert!(error.contains("--dir"), "{error}");
        assert!(error.contains("nope"), "{error}");
        let error = format!(
            "{:#}",
            scan_directory(None, Some(missing.clone()), None, None).unwrap_err()
        );
        assert!(error.contains("WORKSTATS_DIR"), "{error}");
        let error = format!(
            "{:#}",
            scan_directory(None, None, Some(missing.clone()), None).unwrap_err()
        );
        assert!(error.contains("defaults.dir"), "{error}");
        let error = format!(
            "{:#}",
            scan_directory(None, None, None, Some(missing)).unwrap_err()
        );
        assert!(error.contains("current working directory"), "{error}");
        // Precedence: the environment beats the config, the config beats the
        // working directory.
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            temporary.path().to_path_buf(),
            scan_directory(
                None,
                Some(temporary.path().to_path_buf()),
                Some(other.path().to_path_buf()),
                None
            )
            .unwrap()
        );
        assert_eq!(
            other.path().to_path_buf(),
            scan_directory(
                None,
                None,
                Some(other.path().to_path_buf()),
                Some(temporary.path().to_path_buf())
            )
            .unwrap()
        );
    }

    fn defaults(json: &str) -> Result<ConfigDefaults> {
        ConfigDefaults::parse(&serde_json::from_str(json).unwrap(), Path::new("/home/ada"))
    }

    #[test]
    fn a_defaults_block_parses_every_supported_key() {
        let parsed = defaults(
            r#"{"dir": "~/code", "depth": 2, "format": "json", "providers": ["Claude_Code", "codex"],
                "group_by": "repo,month", "gap_cap": "10m", "human_idle": "2h", "review_credit": "15m"}"#,
        )
        .unwrap();
        assert_eq!(Some(PathBuf::from("/home/ada/code")), parsed.dir);
        assert_eq!(Some(2), parsed.depth);
        assert_eq!(Some(OutputFormat::Json), parsed.format);
        assert_eq!(vec!["claude", "codex"], parsed.providers);
        assert_eq!(Some("repo,month".to_string()), parsed.group_by);
        assert_eq!(Some("10m".to_string()), parsed.gap_cap);
        assert_eq!(Some("2h".to_string()), parsed.human_idle);
        assert_eq!(Some("15m".to_string()), parsed.review_credit);
        // A comma list works as it does for --provider.
        assert_eq!(
            vec!["claude", "pi"],
            defaults(r#"{"providers": "claude,pi"}"#).unwrap().providers
        );
    }

    #[test]
    fn a_defaults_block_refuses_unknown_keys_and_bad_values_naming_them() {
        let error = format!("{:#}", defaults(r#"{"depht": 2}"#).unwrap_err());
        assert!(
            error.contains("defaults") && error.contains("depht"),
            "{error}"
        );
        for (json, key) in [
            (r#"{"gap_cap": "soon"}"#, "defaults.gap_cap"),
            (r#"{"human_idle": "x"}"#, "defaults.human_idle"),
            (r#"{"review_credit": 5}"#, "defaults.review_credit"),
            (r#"{"format": "xml"}"#, "defaults.format"),
            (r#"{"depth": "deep"}"#, "defaults.depth"),
            (r#"{"depth": -1}"#, "defaults.depth"),
            (r#"{"group_by": "repo,repo"}"#, "defaults.group_by"),
            (r#"{"providers": ["bad name"]}"#, "defaults.providers"),
            (r#"{"providers": 3}"#, "defaults.providers"),
            (r#"{"dir": ""}"#, "defaults.dir"),
        ] {
            let error = format!("{:#}", defaults(json).unwrap_err());
            assert!(error.contains(key), "{json}: {error}");
        }
    }

    /// The point of the exercise: a flag typed with the built-in value is still
    /// the user's word and must beat the config.
    #[test]
    fn a_config_default_never_overrides_an_explicit_flag_even_at_the_built_in_value() {
        let configured = defaults(
            r#"{"depth": 1, "format": "json", "gap_cap": "10m", "human_idle": "2h",
                "review_credit": "15m", "group_by": "month", "providers": ["codex"]}"#,
        )
        .unwrap();

        let mut bare = report_arguments(&[]);
        let resolved = configured.resolve(&mut bare, false);
        assert_eq!(1, resolved.depth);
        assert_eq!(OutputFormat::Json, resolved.format);
        assert_eq!("10m", resolved.gap_cap);
        assert_eq!("2h", resolved.human_idle);
        assert_eq!("15m", resolved.review_credit);
        assert_eq!(Some("month"), bare.group_by.as_deref());
        assert_eq!(vec!["codex"], bare.provider);
        assert_eq!(7, resolved.from_config.len());

        let mut typed = report_arguments(&[
            "--depth",
            "4",
            "--format",
            "table",
            "--gap-cap",
            "5m",
            "--human-idle",
            "1h",
            "--review-credit",
            "30m",
            "--group-by",
            "repo",
            "--provider",
            "claude",
        ]);
        let resolved = configured.resolve(&mut typed, false);
        assert_eq!(DEFAULT_DEPTH, resolved.depth);
        assert_eq!(OutputFormat::Table, resolved.format);
        assert_eq!(DEFAULT_GAP_CAP, resolved.gap_cap);
        assert_eq!(DEFAULT_HUMAN_IDLE, resolved.human_idle);
        assert_eq!(DEFAULT_REVIEW_CREDIT, resolved.review_credit);
        assert_eq!(Some("repo"), typed.group_by.as_deref());
        assert_eq!(vec!["claude"], typed.provider);
        assert!(resolved.from_config.is_empty());

        // The shortcut flags own the grouping, so the config must not add a
        // --group-by that would conflict with them.
        let mut shortcut = report_arguments(&["--by-dir"]);
        configured.resolve(&mut shortcut, false);
        assert_eq!(None, shortcut.group_by);
        assert_eq!(vec!["cwd"], grouping_dimensions(&shortcut).unwrap());

        // Nothing configured: the built-ins.
        let mut none = report_arguments(&[]);
        let resolved = ConfigDefaults::default().resolve(&mut none, false);
        assert_eq!(
            (DEFAULT_DEPTH, OutputFormat::Table, "5m", "1h", "30m"),
            (
                resolved.depth,
                resolved.format,
                resolved.gap_cap.as_str(),
                resolved.human_idle.as_str(),
                resolved.review_credit.as_str()
            )
        );

        // `ui` has no machine-readable output, so a configured format is moot.
        let mut explore = report_arguments(&[]);
        assert_eq!(
            OutputFormat::Table,
            configured.resolve(&mut explore, true).format
        );
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
