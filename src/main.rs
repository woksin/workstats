mod aggregate;
mod ai;
mod cache;
mod git;
mod model;
mod output;
mod paths;
mod progress;
mod sources;
mod timeutil;

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
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
use paths::{PathResolver, configured_rules, default_cache_path, load_config};
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
    /// Show supported and automatically detected local histories
    Sources(SourcesArguments),
    /// Append one content-free event for a CLI, IDE, script, or API wrapper
    #[command(visible_alias = "event")]
    Record(RecordArguments),
}

#[derive(Debug, Args)]
struct SourcesArguments {
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    output_format: OutputFormat,
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

#[derive(Debug, Parser)]
#[command(
    name = "workstats",
    version,
    about = "Measures local Git output and active AI-assisted work across supported CLIs, IDEs, and API event logs. Transcript text is never emitted and no network APIs are used.",
    after_help = "Human work is an estimate from foreground prompts and authored commits, not a stopwatch. Local history is retention-dependent; work on other machines is not visible."
)]
struct Arguments {
    #[command(subcommand)]
    command: Option<Command>,
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
    #[arg(long, default_value = "5m", help = "Idle gap cap: 30s, 5m, 1h")]
    gap_cap: String,
    #[arg(
        long,
        default_value = "15m",
        help = "Start a new hands-on work block after this idle gap"
    )]
    human_idle: String,
    #[arg(
        long,
        default_value = "5m",
        help = "Time credited to an isolated human signal"
    )]
    isolated_credit: String,
    #[arg(
        long = "group-by",
        visible_alias = "by",
        default_value = "repo",
        help = "Comma-separated: root,repo,cwd,provider,model,day,month"
    )]
    group_by: String,
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
    #[arg(short = 'r', long, help = "Group by month and repo")]
    by_repo: bool,
    #[arg(short = 'm', long, help = "Alias for --group-by repo --period month")]
    matrix: bool,
    #[arg(short = 'D', long, help = "Group by exact working area")]
    by_dir: bool,
    #[arg(
        long = "raw",
        visible_alias = "show-agent-work",
        help = "Show detailed parallel agent/model activity"
    )]
    raw: bool,
}

fn main() {
    let invoked_as_gitstats = env::var_os("WORKSTATS_LEGACY_ENTRYPOINT").is_some()
        || env::args_os()
            .next()
            .and_then(|path| PathBuf::from(path).file_stem().map(|name| name.to_owned()))
            .is_some_and(|name| name == "gitstats");
    if invoked_as_gitstats {
        eprintln!("gitstats is now workstats; running the combined dashboard.");
    }
    let arguments = Arguments::parse();
    let result = match arguments.command.as_ref() {
        Some(Command::Sources(command)) => print_sources(command),
        Some(Command::Record(command)) => record_event(command),
        None => run(arguments),
    };
    if let Err(error) = result {
        eprintln!("workstats: {error:#}");
        std::process::exit(2);
    }
}

fn run(arguments: Arguments) -> Result<()> {
    let gap_cap = parse_duration(&arguments.gap_cap)?;
    let human_idle = parse_duration(&arguments.human_idle)?;
    let isolated_credit = parse_duration(&arguments.isolated_credit)?;
    let since = parse_bound(arguments.since.as_deref(), false)?;
    let until = parse_bound(arguments.until.as_deref(), true)?;
    let mut dimensions: Vec<String> = arguments
        .group_by
        .split(',')
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .map(str::to_string)
        .collect();
    if arguments.by_repo {
        dimensions = vec!["month".into(), "repo".into()];
    } else if arguments.matrix {
        dimensions = vec!["repo".into(), "month".into()];
    } else if arguments.by_dir {
        dimensions = vec!["cwd".into()];
    }
    if let Some(period) = &arguments.period
        && !dimensions.contains(period)
    {
        dimensions.push(period.clone());
    }
    let unique: std::collections::HashSet<_> = dimensions.iter().collect();
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

    let progress = Progress::new(
        arguments.no_progress,
        !arguments.no_color && env::var_os("NO_COLOR").is_none(),
    );
    progress.set("Loading configuration");
    let directory = arguments.directory.clone().unwrap_or_else(|| {
        env::var_os("WORKSTATS_DIR")
            .or_else(|| env::var_os("GITSTATS_DIR"))
            .map(PathBuf::from)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let author = arguments.author.clone().unwrap_or_else(|| {
        env::var("WORKSTATS_AUTHOR")
            .or_else(|_| env::var("GITSTATS_AUTHOR"))
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
    let default_events = default_events_path();
    if event_paths.is_empty() && default_events.is_file() {
        event_paths.push(default_events);
    }
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

    let mut commits = if arguments.no_git {
        Vec::new()
    } else {
        progress.set("Scanning Git repositories");
        read_git_commits(
            &directory,
            &author,
            &mut resolver,
            &mut diagnostics,
            arguments.depth,
            since,
            until,
            arguments
                .repo
                .as_deref()
                .or(arguments.repo_exact.as_deref()),
            &csv_globs(&arguments.path),
            &csv_globs(&arguments.path_exclude),
            arguments.no_ignore,
        )
    };
    if let Some(exact) = &arguments.repo_exact {
        commits.retain(|commit| exact_repo(&commit.repo, &commit.cwd, exact));
    }

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
    progress.set("Calculating work blocks");
    let built = build_report(
        &sessions,
        &commits,
        gap_cap,
        since,
        until,
        &dimensions,
        human_idle,
        isolated_credit,
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
            isolated_credit: arguments.isolated_credit,
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
    match arguments.output_format {
        OutputFormat::Json => print_json(&report)?,
        OutputFormat::Csv => print_csv(&report)?,
        OutputFormat::Table => print_table(&report, &diagnostics, arguments.top, arguments.raw),
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
    let timestamp = arguments
        .timestamp
        .as_deref()
        .map(|value| parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --timestamp")))
        .transpose()?;
    let started_at = arguments
        .started_at
        .as_deref()
        .map(|value| parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --started-at")))
        .transpose()?;
    let completed_at = arguments
        .completed_at
        .as_deref()
        .map(|value| {
            parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --completed-at"))
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

fn csv_globs(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .map(str::to_string)
        .collect()
}
