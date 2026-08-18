mod aggregate;
mod ai;
mod cache;
mod classify;
mod git;
mod model;
mod output;
mod paths;
mod progress;
mod sources;
mod timeutil;
mod tui;
mod update;

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use aggregate::{DIMENSIONS, build_report};
use ai::{
    read_claude_sessions_indexed, read_codex_sessions_indexed, read_copilot_sessions_indexed,
    read_event_sessions_indexed, read_gemini_sessions_indexed, read_opencode_sessions_indexed,
};
use cache::TranscriptCache;
use git::{default_git_author, read_git_commits};
use model::{Diagnostics, Inputs, Report, Session};
use output::{print_csv, print_json, print_table};
use paths::{
    PathResolver, configured_rules, default_cache_path, default_update_check_path, load_config,
};
use progress::Progress;
use sources::{
    default_codex_database, default_events_path, default_history_paths, normalize_provider,
    parse_history_overrides, resolve_opencode_database, source_inventory,
};
use timeutil::{parse_bound, parse_duration, parse_timestamp};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
    Csv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventKind {
    Activity,
    Prompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventRole {
    Foreground,
    Subagent,
}

#[derive(Debug, Subcommand)]
enum Command {
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
}

#[derive(Debug, Args)]
struct SourcesArguments {
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    output_format: OutputFormat,
}

#[derive(Debug, Args)]
struct ClassifyArguments {
    #[arg(
        value_name = "PATH",
        required = true,
        help = "Repository-relative path to classify; repeatable"
    )]
    paths: Vec<String>,
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    output_format: OutputFormat,
    #[arg(long, help = "JSON config (default: platform config directory)")]
    config: Option<PathBuf>,
}

/// One path, the category it lands in, and why.
#[derive(Serialize)]
struct ClassifiedPath {
    path: String,
    category: String,
    rule: &'static str,
    pattern: String,
}

#[derive(Debug, Args)]
struct UpdateArguments {
    #[arg(
        long,
        help = "Report whether a newer version exists without installing it"
    )]
    check: bool,
}

#[derive(Debug, Args)]
struct RecordArguments {
    #[arg(long, help = "Tool or API name, for example cursor or openai-api")]
    provider: String,
    #[arg(long, help = "Stable session, request-group, or task identifier")]
    session: String,
    #[arg(long, help = "Model identifier (content is never accepted)")]
    model: Option<String>,
    #[arg(
        long,
        value_name = "DIR",
        help = "Working directory (default: current directory)"
    )]
    cwd: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = EventKind::Activity)]
    kind: EventKind,
    #[arg(long, value_enum, default_value_t = EventRole::Foreground)]
    role: EventRole,
    #[arg(long, help = "RFC 3339 signal time (default: now)")]
    timestamp: Option<String>,
    #[arg(
        long,
        requires = "completed_at",
        help = "RFC 3339 exact interval start"
    )]
    started_at: Option<String>,
    #[arg(long, requires = "started_at", help = "RFC 3339 exact interval end")]
    completed_at: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Event log (default: platform data directory; '-' writes stdout)"
    )]
    output: Option<PathBuf>,
}

#[derive(Serialize)]
struct RecordedEvent {
    timestamp: String,
    provider: String,
    session_id: String,
    cwd: String,
    model: String,
    event: EventKind,
    role: EventRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
}

/// Used when neither `--group-by` nor one of its shortcut flags is given.
const DEFAULT_GROUP_BY: &str = "repo";

/// Everything that shapes the report itself, flattened into both the default
/// command and `workstats ui`. Sharing one struct is what makes the explorer
/// answer the same question the printed report does, rather than a similar one.
#[derive(Debug, Args)]
struct ReportArguments {
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "DIR",
        help = "Git repository or directory to scan (default: current directory)"
    )]
    directory: Option<PathBuf>,
    #[arg(short = 'a', long, help = "Git author regex")]
    author: Option<String>,
    #[arg(
        short = 'R',
        long,
        help = "Case-insensitive repository/path substring filter"
    )]
    repo: Option<String>,
    #[arg(long, help = "Exact repo label or final folder name")]
    repo_exact: Option<String>,
    #[arg(short = 's', long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    since: Option<String>,
    #[arg(short = 'u', long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    until: Option<String>,
    #[arg(
        long,
        default_value = "5m",
        help = "Agent activity gap cap: 30s, 5m, 1h"
    )]
    gap_cap: String,
    #[arg(
        long,
        default_value = "1h",
        help = "Silent gap that ends a human-involvement block"
    )]
    human_idle: String,
    #[arg(
        long = "review-credit",
        visible_alias = "isolated-credit",
        default_value = "30m",
        help = "Setup and review time credited around each work block"
    )]
    review_credit: String,
    // Optional rather than defaulted so clap can tell "the user asked for this
    // grouping" from "nobody said"; the shortcut flags below conflict with the
    // former only.
    #[arg(
        long = "group-by",
        visible_alias = "by",
        conflicts_with_all = ["by_repo", "matrix", "by_dir"],
        help = "Comma-separated: root,repo,cwd,provider,model,day,month (default: repo)"
    )]
    group_by: Option<String>,
    #[arg(long, value_parser = ["day", "month"], help = "Append a calendar grouping")]
    period: Option<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Include provider(s); repeatable/comma-separated (default: all)")]
    provider: Vec<String>,
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append, help = "Exclude provider(s); repeatable/comma-separated")]
    exclude_provider: Vec<String>,
    #[arg(long, value_name = "PROVIDER=PATH", action = clap::ArgAction::Append, help = "Override a built-in history location; repeatable")]
    history: Vec<String>,
    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append, help = "Add a Workstats Events JSONL file or directory; repeatable")]
    events: Vec<PathBuf>,
    #[arg(long, help = "Skip the event log written by `workstats record`")]
    no_default_events: bool,
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    output_format: OutputFormat,
    #[arg(long, default_value_t = 30, help = "Maximum table rows (0 means all)")]
    top: usize,
    #[arg(long, help = "Skip Git history")]
    no_git: bool,
    #[arg(long, help = "Skip all AI histories")]
    no_ai: bool,
    #[arg(long, hide = true)]
    no_codex: bool,
    #[arg(long, hide = true)]
    no_claude: bool,
    #[arg(long, value_name = "CODEX_DIR", hide = true)]
    codex_dir: Option<PathBuf>,
    #[arg(long, value_name = "CODEX_DB", hide = true)]
    codex_db: Option<PathBuf>,
    #[arg(long, value_name = "CLAUDE_DIR", hide = true)]
    claude_dir: Option<PathBuf>,
    #[arg(long, help = "JSON config (default: platform config directory)")]
    config: Option<PathBuf>,
    #[arg(
        long,
        value_name = "CACHE",
        help = "Transcript index (default: platform cache directory)"
    )]
    cache: Option<PathBuf>,
    #[arg(long, help = "Disable the persistent transcript index")]
    no_cache: bool,
    #[arg(
        long,
        conflicts_with = "no_cache",
        help = "Rebuild the transcript index"
    )]
    rebuild_cache: bool,
    #[arg(long, value_name = "REGEX=NAME", action = clap::ArgAction::Append, help = "Custom source-root rule; repeatable")]
    source_rule: Vec<String>,
    #[arg(long, default_value_t = 4, help = "Git repository discovery depth")]
    depth: usize,
    #[arg(long, action = clap::ArgAction::Append, help = "Git file include glob; repeatable/comma-separated")]
    path: Vec<String>,
    #[arg(short = 'P', long, action = clap::ArgAction::Append, help = "Additional Git ignore glob")]
    path_exclude: Vec<String>,
    #[arg(long, help = "Include generated/vendor Git paths")]
    no_ignore: bool,
    #[arg(long, help = "Disable color in interactive output")]
    no_color: bool,
    #[arg(long, help = "Disable the interactive progress animation")]
    no_progress: bool,
    // Each of these rewrites the grouping wholesale, so two of them together —
    // or either with --group-by — used to mean one silently won (AUDIT V).
    #[arg(
        short = 'r',
        long,
        conflicts_with_all = ["matrix", "by_dir"],
        help = "Alias for --group-by month,repo"
    )]
    by_repo: bool,
    #[arg(
        short = 'm',
        long,
        conflicts_with = "by_dir",
        help = "Alias for --group-by repo,month"
    )]
    matrix: bool,
    #[arg(short = 'D', long, help = "Alias for --group-by cwd")]
    by_dir: bool,
    #[arg(
        long = "raw",
        visible_alias = "show-agent-work",
        help = "Show detailed parallel agent/model activity"
    )]
    raw: bool,
    #[arg(
        long,
        help = "Opt in to a throttled (~daily) background check for newer releases; shown as a footer notice, never installed automatically"
    )]
    check_updates: bool,
    #[arg(
        long,
        help = "Suppress the update-available footer and background check for this run"
    )]
    no_update_check: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "workstats",
    version,
    about = "Measures local Git output and active AI-assisted work across supported CLIs, IDEs, and API event logs. Transcript text is never emitted and no network calls are made unless you run `workstats update` or opt into --check-updates.",
    after_help = "Human work is a supervision-inclusive estimate from prompts, foreground session boundaries, and authored commits, not a stopwatch. Autonomous agent output does not imply continuous human presence."
)]
struct Arguments {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    report: ReportArguments,
}

/// What happens to the report once it is built. It is built identically either
/// way; only the last step differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Presentation {
    Print,
    Explore,
}

fn main() {
    let Arguments { command, report } = Arguments::parse();
    let result = match command {
        Some(Command::Ui(command)) => run(*command, Presentation::Explore),
        Some(Command::Sources(command)) => print_sources(&command),
        // `--config` reads naturally on either side of the subcommand, and a
        // flag that is silently ignored on one side is the same trap as the
        // grouping aliases that used to discard `--group-by` (AUDIT V).
        Some(Command::Classify(command)) => classify_paths(&command, report.config.as_deref()),
        Some(Command::Record(command)) => record_event(&command),
        Some(Command::Update(command)) => run_update_command(&command),
        None => run(report, Presentation::Print),
    };
    if let Err(error) = result {
        eprintln!("workstats: {error:#}");
        std::process::exit(2);
    }
}

/// Names the flag and the value it was given. A parse failure used to say only
/// what a duration should look like, never which flag was wrong (AUDIT V).
fn duration_flag(flag: &str, value: &str) -> Result<Duration> {
    parse_duration(value).with_context(|| format!("invalid {flag} {value:?}"))
}

fn bound_flag(flag: &str, value: Option<&str>, until: bool) -> Result<Option<DateTime<Utc>>> {
    parse_bound(value, until)
        .with_context(|| format!("invalid {flag} {:?}", value.unwrap_or_default()))
}

/// The directory Git history is scanned from. It takes its candidates instead
/// of reading the environment itself so both the precedence and the error stay
/// testable.
fn scan_directory(
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

/// The grouping dimensions for one run, validated. Split out of `run` so the
/// shortcut flags can be exercised without building a report.
fn grouping_dimensions(arguments: &ReportArguments) -> Result<Vec<String>> {
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
    if dimensions.iter().any(|name| name == "day") && dimensions.iter().any(|name| name == "month")
    {
        bail!("day and month are alternative calendar groupings; choose one");
    }
    Ok(dimensions)
}

fn run(arguments: ReportArguments, presentation: Presentation) -> Result<()> {
    // Refused before any scanning: `workstats ui --format json` can only mean
    // the user wanted one of the two, and picking silently is how --by-repo
    // used to lose an explicit --group-by.
    if presentation == Presentation::Explore && arguments.output_format != OutputFormat::Table {
        bail!(
            "`workstats ui` is interactive and writes no machine-readable output; drop --format, or run workstats without `ui` for json or csv"
        );
    }
    let gap_cap = duration_flag("--gap-cap", &arguments.gap_cap)?;
    let human_idle = duration_flag("--human-idle", &arguments.human_idle)?;
    let review_credit = duration_flag("--review-credit", &arguments.review_credit)?;
    let since = bound_flag("--since", arguments.since.as_deref(), false)?;
    let until = bound_flag("--until", arguments.until.as_deref(), true)?;
    let dimensions = grouping_dimensions(&arguments)?;

    let progress = Progress::new(
        arguments.no_progress,
        !arguments.no_color && env::var_os("NO_COLOR").is_none(),
    );
    progress.set("Loading configuration");
    let directory = scan_directory(
        arguments.directory.as_deref(),
        env::var_os("WORKSTATS_DIR").map(PathBuf::from),
        env::current_dir().ok(),
    )?;
    let author = arguments.author.clone().unwrap_or_else(|| {
        env::var("WORKSTATS_AUTHOR")
            .ok()
            .or_else(default_git_author)
            .unwrap_or_default()
    });
    if !arguments.no_git && author.is_empty() {
        bail!("Git author is not configured; set git config --global user.email or pass --author");
    }
    let mut history_paths = default_history_paths();
    history_paths.retain(|provider, paths| {
        paths.iter().any(|path| {
            if provider == "opencode" {
                resolve_opencode_database(path).is_file()
            } else {
                path.is_dir()
            }
        })
    });
    if let Some(path) = &arguments.codex_dir {
        history_paths.insert("codex".to_string(), vec![path.clone()]);
    }
    if let Some(path) = &arguments.claude_dir {
        history_paths.insert("claude".to_string(), vec![path.clone()]);
    }
    for (provider, paths) in parse_history_overrides(&arguments.history)? {
        history_paths.insert(provider, paths);
    }
    let codex_db = arguments
        .codex_db
        .clone()
        .unwrap_or_else(default_codex_database);
    let mut event_paths = arguments.events.clone();
    event_paths.extend(history_paths.remove("events").unwrap_or_default());
    // Everything `workstats record` wrote is part of the picture unless the
    // run says otherwise; adding one --events file must not silently drop it.
    let default_events = default_events_path();
    if !arguments.no_default_events && default_events.is_file() {
        event_paths.push(default_events);
    }
    let mut seen_event_paths = BTreeSet::new();
    event_paths.retain(|path| {
        seen_event_paths.insert(path.canonicalize().unwrap_or_else(|_| path.clone()))
    });
    let mut included: BTreeSet<String> = arguments
        .provider
        .iter()
        .map(|provider| normalize_provider(provider))
        .collect();
    if included.remove("all") {
        included.clear();
    }
    let mut excluded: BTreeSet<String> = arguments
        .exclude_provider
        .iter()
        .map(|provider| normalize_provider(provider))
        .collect();
    if included
        .iter()
        .chain(excluded.iter())
        .any(|provider| !valid_provider_identifier(provider, true))
    {
        bail!("provider filters must use letters, numbers, '.', '/', or '-'");
    }
    if arguments.no_codex {
        excluded.insert("codex".to_string());
    }
    if arguments.no_claude {
        excluded.insert("claude".to_string());
    }
    let mut diagnostics = Diagnostics::default();
    let config = load_config(arguments.config.as_deref(), &mut diagnostics);
    let check_updates_configured = config.check_updates.unwrap_or(false);
    let update_check_suppressed =
        arguments.no_update_check || env::var_os("WORKSTATS_NO_UPDATE_CHECK").is_some();
    let update_check_opt_in = !update_check_suppressed
        && (arguments.check_updates
            || env::var_os("WORKSTATS_CHECK_UPDATES").is_some()
            || check_updates_configured);
    // Before anything classifies a path, so every commit in this run is read
    // through the same registry.
    classify::install(config.category_registry()?)?;
    let rules = configured_rules(config, &arguments.source_rule)?;
    let mut resolver = PathResolver::new(rules);
    let cache_path = arguments.cache.clone().unwrap_or_else(default_cache_path);
    if arguments.rebuild_cache {
        progress.set("Rebuilding transcript index");
    } else if !arguments.no_ai && !arguments.no_cache {
        progress.set("Opening transcript index");
    }
    let mut transcript_cache = if arguments.no_ai || arguments.no_cache {
        None
    } else {
        match TranscriptCache::open(&cache_path, arguments.rebuild_cache) {
            Ok(cache) => Some(cache),
            Err(error) => {
                diagnostics.warn(format!("transcript cache disabled: {error:#}"));
                None
            }
        }
    };

    let mut sessions = Vec::new();
    if !arguments.no_ai {
        for (provider, paths) in &history_paths {
            if !provider_enabled(provider, &included, &excluded) {
                continue;
            }
            progress.set(format!("Loading {provider} activity"));
            for path in paths {
                let loaded = match provider.as_str() {
                    "claude" => read_claude_sessions_indexed(
                        path,
                        &mut resolver,
                        &mut diagnostics,
                        transcript_cache.as_mut(),
                        since,
                        until,
                    ),
                    "codex" => read_codex_sessions_indexed(
                        path,
                        &mut resolver,
                        &mut diagnostics,
                        Some(&codex_db),
                        transcript_cache.as_mut(),
                        since,
                        until,
                    ),
                    "copilot" => read_copilot_sessions_indexed(
                        path,
                        &mut resolver,
                        &mut diagnostics,
                        transcript_cache.as_mut(),
                        since,
                        until,
                    ),
                    "gemini" => read_gemini_sessions_indexed(
                        path,
                        &mut resolver,
                        &mut diagnostics,
                        transcript_cache.as_mut(),
                        since,
                        until,
                    ),
                    "opencode" => read_opencode_sessions_indexed(
                        &resolve_opencode_database(path),
                        &mut resolver,
                        &mut diagnostics,
                        transcript_cache.as_mut(),
                        since,
                        until,
                    ),
                    _ => Vec::new(),
                };
                sessions.extend(loaded);
            }
        }
        for path in &event_paths {
            progress.set("Loading open event activity");
            sessions.extend(read_event_sessions_indexed(
                path,
                &mut resolver,
                &mut diagnostics,
                transcript_cache.as_mut(),
                since,
                until,
            ));
        }
    }
    sessions.retain(|session| provider_enabled(&session.provider, &included, &excluded));
    filter_sessions(
        &mut sessions,
        arguments.repo.as_deref(),
        arguments.repo_exact.as_deref(),
    );
    let repo_filter = arguments
        .repo
        .as_deref()
        .or(arguments.repo_exact.as_deref());
    let mut git_scan_roots = Vec::new();
    let mut commits = Vec::new();
    if !arguments.no_git {
        git_scan_roots.push(directory.clone());
        if repo_filter.is_some() {
            git_scan_roots.extend(inferred_repository_roots(&sessions));
        }
        let mut seen_roots = BTreeSet::new();
        git_scan_roots
            .retain(|root| seen_roots.insert(root.canonicalize().unwrap_or_else(|_| root.clone())));
        let configured_root = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.clone());
        progress.set("Scanning Git repositories");
        for root in &git_scan_roots {
            let scan_root = root.canonicalize().unwrap_or_else(|_| root.clone());
            let depth = if scan_root == configured_root {
                arguments.depth
            } else {
                0
            };
            commits.extend(read_git_commits(
                root,
                &author,
                &mut resolver,
                &mut diagnostics,
                depth,
                since,
                until,
                repo_filter,
                &csv_globs(&arguments.path),
                &csv_globs(&arguments.path_exclude),
                arguments.no_ignore,
            ));
        }
        let mut seen_commits = HashSet::new();
        commits.retain(|commit| seen_commits.insert(commit.sha.clone()));
        if let Some(exact) = &arguments.repo_exact {
            commits.retain(|commit| exact_repo(&commit.repo, &commit.cwd, exact));
        }
    }
    progress.set("Estimating human involvement");
    let built = build_report(
        &sessions,
        &commits,
        gap_cap,
        since,
        until,
        &dimensions,
        human_idle,
        review_credit,
    );
    let report = Report {
        methodology: built.methodology,
        observed: built.observed,
        summary: built.summary,
        group_by: built.group_by,
        rows: built.rows,
        diagnostics: diagnostics.clone(),
        inputs: Inputs {
            git_root: directory.to_string_lossy().into_owned(),
            git_scan_roots: git_scan_roots
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            history_sources: history_paths
                .iter()
                .map(|(provider, paths)| {
                    (
                        provider.clone(),
                        paths
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect(),
                    )
                })
                .chain((!event_paths.is_empty()).then(|| {
                    (
                        "events".to_string(),
                        event_paths
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect(),
                    )
                }))
                .collect(),
            included_providers: included.into_iter().collect(),
            excluded_providers: excluded.into_iter().collect(),
            author,
            repo_filter: arguments.repo,
            repo_exact_filter: arguments.repo_exact,
            human_idle: arguments.human_idle,
            review_credit: arguments.review_credit,
            cache: transcript_cache
                .as_ref()
                .map(|cache| cache.path().to_string_lossy().into_owned()),
        },
    };
    let cache_summary = if diagnostics.cache_hits == 0 && diagnostics.cache_misses == 0 {
        String::new()
    } else {
        format!(
            " · {} cached, {} refreshed",
            diagnostics.cache_hits, diagnostics.cache_misses
        )
    };
    progress.finish(format!(
        "Analyzed {} commits and {} AI sessions{cache_summary}",
        report.summary.commit_count, report.summary.session_count
    ));
    if presentation == Presentation::Explore {
        // Nothing above this line knows about the explorer: it browses the
        // report the default command would have printed, and `commits` carries
        // the per-commit detail the report itself aggregates away.
        return tui::run(&report, commits);
    }
    match arguments.output_format {
        OutputFormat::Json => print_json(&report)?,
        OutputFormat::Csv => print_csv(&report)?,
        OutputFormat::Table => {
            print_table(&report, &diagnostics, arguments.top, arguments.raw);
            let update_notice =
                update::maybe_check_for_update(&default_update_check_path(), update_check_opt_in);
            if let Some(latest) = update_notice {
                println!(
                    "\nworkstats {latest} is available (you have {}) — run `workstats update`.",
                    update::current_version()
                );
            }
        }
    }
    Ok(())
}

fn provider_enabled(
    provider: &str,
    included: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
) -> bool {
    let provider = normalize_provider(provider);
    !excluded.contains("all")
        && !excluded.contains(&provider)
        && (included.is_empty() || included.contains(&provider))
}

fn valid_provider_identifier(provider: &str, allow_all: bool) -> bool {
    !provider.is_empty()
        && (allow_all || provider != "all")
        && provider.len() <= 64
        && provider.as_bytes()[0].is_ascii_alphanumeric()
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'-'))
}

fn run_update_command(arguments: &UpdateArguments) -> Result<()> {
    let cache_path = default_update_check_path();
    println!("Current version: workstats {}", update::current_version());
    if arguments.check {
        let outcome = update::check_now(&cache_path)?;
        if outcome.available {
            println!(
                "A new version is available: workstats {}  (run `workstats update` to install it)",
                outcome.latest
            );
        } else {
            println!("workstats is up to date.");
        }
        return Ok(());
    }
    let outcome = update::install_latest(&cache_path)?;
    if outcome.available {
        println!(
            "Updated workstats {} → {}. Restart to use the new version.",
            outcome.current, outcome.latest
        );
    } else {
        println!("workstats is already up to date.");
    }
    Ok(())
}

fn print_sources(arguments: &SourcesArguments) -> Result<()> {
    let inventory = source_inventory();
    match arguments.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&inventory)?),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(io::stdout());
            for item in inventory {
                writer.serialize(item)?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => {
            println!("AI HISTORY SOURCES\n");
            println!(
                "{:<4} {:<12} {:<22} {:<24} {:<12} PATH",
                "", "ID", "SOURCE", "FORMAT", "SUPPORT"
            );
            for item in inventory {
                println!(
                    "{:<4} {:<12} {:<22} {:<24} {:<12} {}",
                    if item.detected { "●" } else { "○" },
                    item.id,
                    item.name,
                    item.format,
                    item.support,
                    item.path
                );
            }
            println!(
                "\n● detected  ○ not found  · add any other tool with `workstats record` or `--events`"
            );
        }
    }
    Ok(())
}

/// Answers "why did this file land there?" against the configured registry,
/// which is the only way to debug a category rule without running a report.
fn classify_paths(arguments: &ClassifyArguments, fallback_config: Option<&Path>) -> Result<()> {
    let mut diagnostics = Diagnostics::default();
    let config = load_config(
        arguments.config.as_deref().or(fallback_config),
        &mut diagnostics,
    );
    let registry = config.category_registry()?;
    for message in &diagnostics.messages {
        eprintln!("workstats: {message}");
    }
    let classified: Vec<ClassifiedPath> = arguments
        .paths
        .iter()
        .map(|path| {
            let matched = registry.explain(path);
            ClassifiedPath {
                path: path.clone(),
                category: registry.name(matched.category).to_string(),
                rule: matched.rule.as_str(),
                pattern: matched.pattern,
            }
        })
        .collect();
    match arguments.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&classified)?),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(io::stdout());
            for item in &classified {
                writer.serialize(item)?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => {
            println!("{:<52} {:<10} {:<18} MATCHED", "PATH", "CATEGORY", "RULE");
            for item in &classified {
                let pattern = if item.pattern.is_empty() {
                    "—"
                } else {
                    item.pattern.as_str()
                };
                println!(
                    "{:<52} {:<10} {:<18} {pattern}",
                    item.path, item.category, item.rule
                );
            }
            println!(
                "\nCategories in match order: {}",
                registry.names().collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(())
}

fn record_event(arguments: &RecordArguments) -> Result<()> {
    let provider = normalize_provider(&arguments.provider);
    if !valid_provider_identifier(&provider, false) {
        bail!("--provider must be a short identifier using letters, numbers, '.', '/', or '-'");
    }
    if arguments.session.trim().is_empty()
        || arguments.session.len() > 256
        || arguments.session.chars().any(char::is_control)
    {
        bail!("--session must be a non-empty identifier of at most 256 bytes");
    }
    if let Some(model) = arguments.model.as_deref()
        && (!model.is_ascii()
            || model.is_empty()
            || model.len() > 128
            || !model.as_bytes()[0].is_ascii_alphanumeric()
            || !model.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b':' | b'/' | b'+' | b'<' | b'>' | b'-')
            }))
    {
        bail!("--model must be a short model identifier, not message content");
    }
    // The value is echoed back because an RFC 3339 timestamp is usually wrong
    // in a way you can only see next to what you typed (AUDIT V).
    let timestamp = arguments
        .timestamp
        .as_deref()
        .map(|value| {
            parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --timestamp {value:?}"))
        })
        .transpose()?;
    let started_at = arguments
        .started_at
        .as_deref()
        .map(|value| {
            parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --started-at {value:?}"))
        })
        .transpose()?;
    let completed_at = arguments
        .completed_at
        .as_deref()
        .map(|value| {
            parse_timestamp(value)
                .ok_or_else(|| anyhow::anyhow!("invalid --completed-at {value:?}"))
        })
        .transpose()?;
    if started_at
        .zip(completed_at)
        .is_some_and(|(start, end)| end <= start)
    {
        bail!("--completed-at must be later than --started-at");
    }
    let cwd = arguments
        .cwd
        .clone()
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let event = RecordedEvent {
        timestamp: timestamp
            .or(completed_at)
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
        provider,
        session_id: arguments.session.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: arguments
            .model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        event: arguments.kind,
        role: arguments.role,
        started_at: started_at.map(|value| value.to_rfc3339()),
        completed_at: completed_at.map(|value| value.to_rfc3339()),
    };
    let mut encoded = serde_json::to_vec(&event)?;
    encoded.push(b'\n');
    let output = arguments.output.clone().unwrap_or_else(default_events_path);
    if output.as_os_str() == "-" {
        io::stdout().write_all(&encoded)?;
        return Ok(());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create event directory {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output)
        .with_context(|| format!("cannot open event log {}", output.display()))?;
    file.write_all(&encoded)?;
    eprintln!("Recorded content-free event → {}", output.display());
    Ok(())
}

fn filter_sessions(sessions: &mut Vec<Session>, pattern: Option<&str>, exact: Option<&str>) {
    if let Some(exact) = exact {
        sessions.retain(|session| exact_repo(&session.repo, &session.cwd, exact));
    }
    if let Some(pattern) = pattern {
        let needle = pattern.to_lowercase();
        sessions.retain(|session| {
            session.repo.to_lowercase().contains(&needle)
                || session.cwd.to_lowercase().contains(&needle)
                || session.root.to_lowercase().contains(&needle)
        });
    }
}

fn exact_repo(repo: &str, cwd: &str, exact: &str) -> bool {
    repo.eq_ignore_ascii_case(exact)
        || Path::new(cwd)
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(exact))
}

fn inferred_repository_roots(sessions: &[Session]) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for session in sessions {
        let cwd = Path::new(&session.cwd);
        if !cwd.is_dir() {
            continue;
        }
        if let Some(root) = cwd.ancestors().find(|path| path.join(".git").exists()) {
            roots.insert(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
        }
    }
    roots.into_iter().collect()
}

fn csv_globs(values: &[String]) -> Vec<String> {
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

    #[test]
    fn exact_repo_filter_does_not_match_similar_names() {
        assert!(exact_repo(
            "studio/widget",
            "/repos/studio/widget",
            "widget"
        ));
        assert!(!exact_repo(
            "misc/widget-tools",
            "/repos/misc/widget-tools",
            "widget"
        ));
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

    #[test]
    fn repeated_and_comma_separated_globs_are_normalized() {
        assert_eq!(
            vec!["src/**", "tests/**", "docs/**"],
            csv_globs(&["src/**, tests/**".into(), "docs/**".into()])
        );
    }

    #[test]
    fn repository_roots_are_inferred_from_session_working_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("project");
        let nested = repository.join("src/nested");
        fs::create_dir_all(repository.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        let session = Session {
            provider: "test".into(),
            session_id: "session".into(),
            cwd: nested.to_string_lossy().into_owned(),
            repo: "project".into(),
            root: "tmp/scratch".into(),
            points: Vec::new(),
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: false,
        };

        assert_eq!(
            vec![repository.canonicalize().unwrap()],
            inferred_repository_roots(&[session])
        );
    }
}
