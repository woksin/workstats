mod aggregate;
mod ai;
mod cache;
mod git;
mod model;
mod output;
mod paths;
mod progress;
mod timeutil;

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};

use aggregate::{DIMENSIONS, build_report};
use ai::{read_claude_sessions_indexed, read_codex_sessions_indexed};
use cache::TranscriptCache;
use git::{default_git_author, read_git_commits};
use model::{Diagnostics, Inputs, Report, Session};
use output::{print_csv, print_json, print_table};
use paths::{PathResolver, configured_rules, default_cache_path, home_dir, load_config};
use progress::Progress;
use timeutil::{parse_bound, parse_duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Provider {
    All,
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
    Csv,
}

#[derive(Debug, Parser)]
#[command(
    name = "workstats",
    version,
    about = "Measures local Git output and active AI-assisted work from retained Codex and Claude Code transcripts. No transcript text is emitted and no network APIs are used.",
    after_help = "Human work is an estimate from foreground prompts and authored commits, not a stopwatch. Local history is retention-dependent; work on other machines is not visible."
)]
struct Arguments {
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "DIR",
        help = "Git repositories root"
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
        default_value = "root",
        help = "Comma-separated: root,repo,cwd,provider,model,day,month"
    )]
    group_by: String,
    #[arg(long, value_parser = ["day", "month"], help = "Append a calendar grouping")]
    period: Option<String>,
    #[arg(long, value_enum, default_value_t = Provider::All)]
    provider: Provider,
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    output_format: OutputFormat,
    #[arg(long, default_value_t = 30, help = "Maximum table rows (0 means all)")]
    top: usize,
    #[arg(long, help = "Skip Git history")]
    no_git: bool,
    #[arg(long, help = "Skip all AI histories")]
    no_ai: bool,
    #[arg(long)]
    no_codex: bool,
    #[arg(long)]
    no_claude: bool,
    #[arg(long, value_name = "CODEX_DIR")]
    codex_dir: Option<PathBuf>,
    #[arg(long, value_name = "CODEX_DB")]
    codex_db: Option<PathBuf>,
    #[arg(long, value_name = "CLAUDE_DIR")]
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
    if let Err(error) = run(arguments) {
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
    let home = home_dir();
    let directory = arguments.directory.clone().unwrap_or_else(|| {
        env::var_os("WORKSTATS_DIR")
            .or_else(|| env::var_os("GITSTATS_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("src/repos"))
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
    let codex_dir = arguments
        .codex_dir
        .clone()
        .unwrap_or_else(|| home.join(".codex/sessions"));
    let codex_db = arguments
        .codex_db
        .clone()
        .unwrap_or_else(|| home.join(".codex/state_5.sqlite"));
    let claude_dir = arguments
        .claude_dir
        .clone()
        .unwrap_or_else(|| home.join(".claude/projects"));
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
        if !arguments.no_claude && matches!(arguments.provider, Provider::All | Provider::Claude) {
            progress.set("Loading Claude activity");
            sessions.extend(read_claude_sessions_indexed(
                &claude_dir,
                &mut resolver,
                &mut diagnostics,
                transcript_cache.as_mut(),
                since,
                until,
            ));
        }
        if !arguments.no_codex && matches!(arguments.provider, Provider::All | Provider::Codex) {
            progress.set("Loading Codex activity");
            sessions.extend(read_codex_sessions_indexed(
                &codex_dir,
                &mut resolver,
                &mut diagnostics,
                Some(&codex_db),
                transcript_cache.as_mut(),
                since,
                until,
            ));
        }
    }
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
            claude_root: claude_dir.to_string_lossy().into_owned(),
            codex_root: codex_dir.to_string_lossy().into_owned(),
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
