//! The report pipeline: reads AI histories and Git, aggregates, and presents
//! the result as a table, JSON, CSV, Markdown, HTML, the interactive explorer, or an
//! allocation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::aggregate::{BuiltReport, build_report_with_human_time_explanation};
use crate::ai::{
    read_claude_sessions_indexed, read_codex_sessions_indexed, read_copilot_sessions_indexed,
    read_copilot_vscode_sessions_indexed, read_event_sessions_indexed,
    read_gemini_sessions_indexed, read_opencode_sessions_indexed, read_pi_sessions_indexed,
};
use crate::allocate;
use crate::cache::TranscriptCache;
use crate::classify;
use crate::cli::{
    AllocateArguments, OutputFormat, ReportArguments, agent_author_patterns, compare_windows,
    csv_globs, duration_flag, grouping_dimensions, report_window, resolve_authors, scan_directory,
    valid_provider_identifier,
};
use crate::compare::{Comparison, Period};
use crate::git::{default_git_author, read_agent_commits, read_git_commits};
use crate::model::{self, Diagnostics, Inputs, Report, Session};
use crate::output::{print_csv, print_html, print_json, print_markdown, print_table};
use crate::paths::{
    PathResolver, ProjectAliases, SourceRule, configured_rules, default_cache_path,
    default_update_check_path, disambiguated_repository_label, home_dir, load_config,
};
use crate::pricing;
use crate::progress::Progress;
use crate::sources::{
    default_codex_database, default_events_path, default_history_paths, normalize_provider,
    parse_history_overrides, resolve_opencode_database,
};
use crate::tui;
use crate::update;

/// What happens to the report once it is built. It is built identically either
/// way; only the last step differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Presentation {
    Print,
    Explore,
}

/// Parses the allocation flags, pins the grouping the apportionment needs, and
/// hands off to the normal report pipeline. `allocate` is a different view of
/// the same measurements, not a different measurement.
pub(crate) fn run_allocation(command: AllocateArguments) -> Result<()> {
    let AllocateArguments {
        projects,
        subscriptions,
        price,
        vat,
        currency,
        basis,
        gap_policy,
        mut report,
    } = command;

    let price_default = price;
    if !price_default.is_finite() || price_default <= 0.0 {
        bail!("--price must be a positive amount");
    }
    if !vat.is_finite() || !(0.0..=100.0).contains(&vat) {
        bail!("--vat must be a percentage between 0 and 100");
    }
    // Three letters, so a currency can be printed beside an amount without the
    // reader having to guess. No rate is applied and nothing is converted.
    let currency = currency.trim().to_ascii_uppercase();
    if currency.len() != 3
        || !currency
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        bail!("--currency expects a three-letter ISO code, such as USD or NOK");
    }
    let mut plans: BTreeMap<String, allocate::Plan> = BTreeMap::new();
    for entry in &subscriptions {
        let (name, rest) = entry
            .split_once('=')
            .with_context(|| format!("--sub expects PLAN=N, got `{entry}`"))?;
        // `claude=2@1992` prices one vendor apart from the rest. Two vendors
        // billing a buyer in different currencies is the normal case outside
        // the US, and a single --price cannot say it.
        let (count, price) = match rest.split_once('@') {
            Some((count, price)) => {
                let price: f64 = price.trim().parse().with_context(|| {
                    format!("--sub price after '@' must be a number, got `{price}`")
                })?;
                if !price.is_finite() || price <= 0.0 {
                    bail!("--sub {name} price must be a positive amount");
                }
                (count, Some(price))
            }
            None => (rest, None),
        };
        // A vendor family, or a client that bills on its own seat. Anything
        // else is a typo, and accepting it would silently declare an empty
        // pool that claims nothing.
        let pool = pricing::Family::parse(name)
            .map(|family| family.as_str().to_string())
            .or_else(|| {
                let name = name.trim().to_ascii_lowercase();
                pricing::SEPARATE_PLANS
                    .iter()
                    .find(|plan| **plan == name)
                    .map(|plan| (*plan).to_string())
            })
            .with_context(|| {
                format!(
                    "unknown subscription `{name}`; expected one of {}",
                    pricing::Family::ALL
                        .iter()
                        .map(|family| family.as_str())
                        .chain(pricing::SEPARATE_PLANS.iter().copied())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let count: u32 = count
            .trim()
            .parse()
            .with_context(|| format!("--sub count must be a whole number, got `{count}`"))?;
        if count == 0 {
            bail!("--sub {name}=0 declares no subscriptions; omit it instead");
        }
        // Repeating a plan is ambiguous — two or six? — and silently picking
        // one is how a claim becomes wrong without anyone noticing.
        if plans
            .insert(
                pool.clone(),
                allocate::Plan {
                    count,
                    price: price.unwrap_or(price_default),
                },
            )
            .is_some()
        {
            bail!("--sub {pool} was given more than once; state the total in one flag");
        }
    }

    // Allocation apportions per vendor per month, so those dimensions are not
    // the caller's to choose; --month/--since/--until still pick the window.
    if report.group_by.is_some() {
        bail!(
            "`allocate` sets its own grouping (repo, provider, model, month); drop --group-by and use --month, --year, --week, or --since/--until to pick the window"
        );
    }
    report.group_by = Some("repo,provider,model,month".to_string());

    let top = report.top;
    run(
        report,
        Presentation::Print,
        Some(allocate::AllocationOptions {
            projects,
            top,
            subscriptions: plans,
            vat_percent: vat,
            currency,
            basis,
            gap_policy,
            // Filled in by `run`, which owns the loaded config.
            rate_overrides: pricing::RateOverrides::default(),
            today: Utc::now().date_naive(),
        }),
    )
}

pub(crate) fn run(
    mut arguments: ReportArguments,
    presentation: Presentation,
    allocation: Option<allocate::AllocationOptions>,
) -> Result<()> {
    // The config is read first because its `defaults` decide what several of
    // the checks below are checking: flag > environment > config > built-in.
    let mut diagnostics = Diagnostics::default();
    let config = load_config(arguments.config.as_deref(), &mut diagnostics);
    let defaults = config.config_defaults(&home_dir())?;
    let configured_authors = config.configured_authors()?;
    let mut resolved = defaults.resolve(&mut arguments, presentation == Presentation::Explore);
    let output_format = resolved.format;
    // Where the format came from decides what a refusal can usefully suggest:
    // `--format csv` is fixed by dropping the flag, a configured one is not.
    let format_from_config = resolved.from_config.contains_key("format");
    let format_origin = if format_from_config {
        format!("defaults.format \"{}\" (from config)", output_format.name())
    } else {
        format!("--format {}", output_format.name())
    };
    // Refused before any scanning: `workstats ui --format json` can only mean
    // the user wanted one of the two, and picking silently is how --by-repo
    // used to lose an explicit --group-by.
    if presentation == Presentation::Explore
        && arguments
            .output_format
            .is_some_and(|format| format != OutputFormat::Table)
    {
        bail!(
            "`workstats ui` is interactive and writes no machine-readable output; drop --format, or run workstats without `ui` for json, csv, markdown, or html"
        );
    }
    if presentation == Presentation::Explore
        && (arguments.explain_human_time || arguments.explain_repository_attribution)
    {
        bail!(
            "explanation flags are not available in `workstats ui`; run workstats with table or JSON output"
        );
    }
    // A ledger is one-to-many relative to a row, which CSV cannot hold, and the
    // Markdown and HTML documents deliberately mirror only the report itself.
    if matches!(
        output_format,
        OutputFormat::Csv | OutputFormat::Markdown | OutputFormat::Html
    ) && (arguments.explain_human_time || arguments.explain_repository_attribution)
    {
        let fix = if format_from_config {
            "pass --format table or --format json"
        } else {
            "use table output or --format json"
        };
        bail!("explanation flags are not available with {format_origin}; {fix}");
    }
    // A comparison is a second report beside the first. The explorer has no
    // place for one, an allocation is a statement about a single period, and
    // CSV is one flat table with no way to hold two windows without changing
    // its columns — so all three are refused rather than shown half a
    // comparison. The other formats can carry it.
    if arguments.compare.is_some() {
        if presentation == Presentation::Explore {
            bail!(
                "--compare is not available in `workstats ui`; run workstats without `ui` for table, json, markdown, or html"
            );
        }
        if allocation.is_some() {
            bail!(
                "--compare is not available with `workstats allocate`; an allocation covers one period, so run it once per period"
            );
        }
        if output_format == OutputFormat::Csv {
            let fix = if format_from_config {
                "pass --format table or --format json"
            } else {
                "use table, json, markdown, or html"
            };
            bail!(
                "--compare is not available with {format_origin}, which has one row per group and nowhere to put a second window; {fix}"
            );
        }
    }
    let gap_cap = duration_flag("--gap-cap", &resolved.gap_cap)?;
    let human_idle = duration_flag("--human-idle", &resolved.human_idle)?;
    let review_credit = duration_flag("--review-credit", &resolved.review_credit)?;
    if review_credit > human_idle {
        bail!(
            "--review-credit ({}) must not exceed --human-idle ({})",
            resolved.review_credit,
            resolved.human_idle
        );
    }
    let now = Utc::now();
    let window = report_window(&arguments, now)?;
    let compare = compare_windows(&arguments, window, now)?;
    let dimensions = grouping_dimensions(&arguments)?;

    let progress = Progress::new(
        arguments.no_progress,
        !arguments.no_color && env::var_os("NO_COLOR").is_none(),
    );
    progress.set("Loading configuration");
    let directory = scan_directory(
        arguments.directory.as_deref(),
        env::var_os("WORKSTATS_DIR").map(PathBuf::from),
        defaults.dir.clone(),
        env::current_dir().ok(),
    )?;
    // `scan_directory` ranks the sources; this only notes whether the config's
    // was the one that won, so the report can say where its root came from.
    if arguments.directory.is_none()
        && env::var_os("WORKSTATS_DIR").is_none()
        && defaults.dir.is_some()
    {
        resolved
            .from_config
            .insert("dir".to_string(), directory.to_string_lossy().into_owned());
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
    let authors = resolve_authors(
        &arguments.author,
        env::var("WORKSTATS_AUTHOR").ok(),
        &configured_authors,
        default_git_author,
    );
    // A blank pattern is refused as well as a missing one: Git treats an empty
    // `--author` as matching every commit, which would report other people's
    // work as the developer's.
    if !arguments.no_git && (authors.is_empty() || authors.iter().any(|a| a.trim().is_empty())) {
        bail!("Git author is not configured; set git config --global user.email or pass --author");
    }
    let rules = configured_rules(&config, &arguments.source_rule)?;
    let aliases = config.compiled_project_aliases(&home_dir())?;
    let rate_overrides = config.compiled_model_rates()?;
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

    // Each window is produced by the same code a standalone run of that window
    // uses — sources, Git, labels, filters and all — so `--compare` can only
    // ever show what two separate runs would have. A single read over both
    // windows would not do: the labels that tell same-named repositories apart,
    // `--repo-exact`, the discovery bounds of some readers and the checkouts
    // inferred from sessions all depend on which data was read. The parse cache
    // keeps the second pass cheap.
    let agent_authors = agent_author_patterns(arguments.agent_commits.as_deref());
    let scan = WindowScan {
        arguments: &arguments,
        directory: &directory,
        depth: resolved.depth,
        history_paths: &history_paths,
        codex_db: &codex_db,
        event_paths: &event_paths,
        included: &included,
        excluded: &excluded,
        authors: &authors,
        agent_authors: &agent_authors,
        rules: &rules,
        aliases: &aliases,
        dimensions: &dimensions,
        gap_cap,
        human_idle,
        review_credit,
        progress: &progress,
    };
    let WindowRun {
        built,
        commits,
        scan_roots,
        attribution,
    } = scan_window(
        &scan,
        window,
        arguments.explain_human_time,
        &mut transcript_cache,
        &mut diagnostics,
    )?;
    let comparison = match compare {
        Some(plan) => {
            // The baseline's own counters and warnings describe a window the
            // report is not about; one line says when it had trouble of its
            // own. Most warnings do not depend on the window at all — a
            // malformed transcript, a Git failure — and the selected window
            // has already shown them, so only the baseline's new ones count.
            // Without that, one bad line anywhere in history would add a
            // warning pointing at a run that finds nothing.
            let mut baseline_diagnostics = Diagnostics::default();
            let earlier = scan_window(
                &scan,
                (Some(plan.previous.0), Some(plan.previous.1)),
                false,
                &mut transcript_cache,
                &mut baseline_diagnostics,
            )?;
            let new_warnings = baseline_only_warnings(&diagnostics, &baseline_diagnostics);
            if new_warnings > 0 {
                diagnostics.warn(format!(
                    "the comparison window raised {new_warnings} warning(s) of its own; run it on its own to see them"
                ));
            }
            Some(Comparison::new(
                Period::new(plan.current, &built.summary),
                Period::new(plan.previous, &earlier.built.summary),
                plan.basis,
            ))
        }
        None => None,
    };
    diagnostics.repository_history_hits = attribution.history_hits;
    diagnostics.repository_history_ambiguities = attribution.history_ambiguities;
    diagnostics.unresolved_repository_cwds = attribution.unresolved_checkouts as u64;
    let repository_attribution = arguments
        .explain_repository_attribution
        .then_some(attribution);
    let report = Report {
        methodology: built.methodology,
        human_time_explanation: built.human_time_explanation,
        repository_attribution,
        observed: built.observed,
        summary: built.summary,
        group_by: built.group_by,
        rows: built.rows,
        diagnostics: diagnostics.clone(),
        comparison,
        inputs: Inputs {
            git_root: directory.to_string_lossy().into_owned(),
            git_scan_roots: if arguments.no_git {
                Vec::new()
            } else {
                scan_roots
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect()
            },
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
            author: authors.join(", "),
            authors,
            agent_authors,
            co_authors: arguments.co_authors,
            repo_filter: arguments.repo,
            repo_exact_filter: arguments.repo_exact,
            human_idle: resolved.human_idle,
            review_credit: resolved.review_credit,
            config_defaults: resolved.from_config,
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
    if let Some(mut options) = allocation {
        options.rate_overrides = rate_overrides;
        // A different presentation of the report that was just built, so the
        // numbers behind a claim are the numbers `workstats` would print.
        let mut allocation = allocate::build(&report.rows, &options);
        allocation.config_defaults = report.inputs.config_defaults.clone();
        match output_format {
            OutputFormat::Json => allocate::print_json(&allocation)?,
            OutputFormat::Csv => allocate::print_csv(&allocation)?,
            OutputFormat::Table => allocate::print_table(&allocation),
            OutputFormat::Markdown => allocate::print_markdown(&allocation),
            OutputFormat::Html => allocate::print_html(&allocation),
        }
        return Ok(());
    }
    match output_format {
        OutputFormat::Json => print_json(&report)?,
        OutputFormat::Csv => print_csv(&report)?,
        // Like json and csv, and unlike the table: no update notice. A document
        // is going into a file, a PR, or a pipe, and a "new version available"
        // line would end up published with it.
        OutputFormat::Markdown => {
            print_markdown(&report, &diagnostics, arguments.top, arguments.raw)
        }
        OutputFormat::Html => print_html(&report, &diagnostics, arguments.top, arguments.raw),
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

/// Everything a window's scan needs that does not depend on the window.
struct WindowScan<'a> {
    arguments: &'a ReportArguments,
    directory: &'a Path,
    depth: usize,
    history_paths: &'a BTreeMap<String, Vec<PathBuf>>,
    codex_db: &'a Path,
    event_paths: &'a [PathBuf],
    included: &'a BTreeSet<String>,
    excluded: &'a BTreeSet<String>,
    authors: &'a [String],
    agent_authors: &'a [String],
    rules: &'a [SourceRule],
    aliases: &'a ProjectAliases,
    dimensions: &'a [String],
    gap_cap: chrono::Duration,
    human_idle: chrono::Duration,
    review_credit: chrono::Duration,
    progress: &'a Progress,
}

/// What scanning one window produced.
struct WindowRun {
    built: BuiltReport,
    /// The retained human commits, which the explorer lists individually.
    commits: Vec<model::GitCommit>,
    scan_roots: Vec<PathBuf>,
    attribution: model::RepositoryAttribution,
}

/// Loads the sources and Git history for one window, labels and filters them,
/// and builds that window's report. A standalone run is exactly one call, and
/// `--compare` makes a second for the baseline, so a window is never built any
/// other way.
fn scan_window(
    scan: &WindowScan,
    window: (Option<DateTime<Utc>>, Option<DateTime<Utc>>),
    explain_human_time: bool,
    transcript_cache: &mut Option<TranscriptCache>,
    diagnostics: &mut Diagnostics,
) -> Result<WindowRun> {
    let WindowScan {
        arguments,
        directory,
        history_paths,
        codex_db,
        event_paths,
        included,
        excluded,
        authors,
        agent_authors,
        dimensions,
        gap_cap,
        human_idle,
        review_credit,
        progress,
        ..
    } = *scan;
    let repository_history = if let Some(cache) = transcript_cache.as_ref() {
        match cache.load_repository_history() {
            Ok(history) => history,
            Err(error) => {
                diagnostics.warn(format!("repository identity history ignored: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let mut resolver = PathResolver::with_context(
        scan.rules.to_vec(),
        scan.aliases.clone(),
        repository_history,
        home_dir(),
    );

    let mut sessions = Vec::new();
    if !arguments.no_ai {
        for (provider, paths) in history_paths {
            if !provider_enabled(provider, included, excluded) {
                continue;
            }
            progress.set(format!("Loading {provider} activity"));
            for path in paths {
                let loaded = match provider.as_str() {
                    "claude" => read_claude_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "codex" => read_codex_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        Some(codex_db),
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "copilot" => read_copilot_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "copilot-vscode" => read_copilot_vscode_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "gemini" => read_gemini_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "opencode" => read_opencode_sessions_indexed(
                        &resolve_opencode_database(path),
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    "pi" => read_pi_sessions_indexed(
                        path,
                        &mut resolver,
                        diagnostics,
                        transcript_cache.as_mut(),
                        window.0,
                        window.1,
                    ),
                    _ => Vec::new(),
                };
                sessions.extend(loaded);
            }
        }
        for path in event_paths {
            progress.set("Loading open event activity");
            sessions.extend(read_event_sessions_indexed(
                path,
                &mut resolver,
                diagnostics,
                transcript_cache.as_mut(),
                window.0,
                window.1,
            ));
        }
    }
    sessions.retain(|session| provider_enabled(&session.provider, included, excluded));
    // Broad substring filtering can happen immediately. Exact filtering waits
    // until every session and Git repository has its final disambiguated label;
    // otherwise an explicit alias and a natural repository with the same raw
    // name are both retained even though only the alias is displayed plainly.
    filter_sessions(&mut sessions, arguments.repo.as_deref(), None, true);
    let repo_filter = arguments.repo.as_deref();
    let canonical_root = |root: &Path| root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // The checkouts Git is read from: `--dir`, plus the checkout of every
    // retained session. Sessions can belong to locally available checkouts
    // outside `--dir`. Always scan those roots so adding or removing a repo
    // filter cannot change which commits contribute to a retained session's
    // report.
    let scan_roots: Vec<PathBuf> = {
        let mut roots = vec![directory.to_path_buf()];
        roots.extend(inferred_repository_roots(sessions.iter()));
        let mut seen_roots = BTreeSet::new();
        roots.retain(|root| seen_roots.insert(canonical_root(root)));
        roots
    };
    let mut commits = Vec::new();
    let mut agent_commits = Vec::new();
    if !arguments.no_git {
        let configured_root = canonical_root(directory);
        progress.set("Scanning Git repositories");
        for root in &scan_roots {
            let scan_root = canonical_root(root);
            // Everything but the configured directory got here by being the
            // checkout of a retained session.
            let from_session = scan_root != configured_root;
            let depth = if from_session { 0 } else { scan.depth };
            // Re-applying the filter to such a root would reject it: the filter
            // matched the session's own working directory, which may be deep
            // inside the repository, while the repository is described by its
            // root. That defeated the inference this loop exists to perform.
            let scoped_filter = if from_session { None } else { repo_filter };
            let human = read_git_commits(
                root,
                authors,
                &mut resolver,
                diagnostics,
                depth,
                window.0,
                window.1,
                scoped_filter,
                &csv_globs(&arguments.path),
                &csv_globs(&arguments.path_exclude),
                arguments.no_ignore,
                arguments.co_authors,
            );
            // A separate pass over the same repositories rather than a wider
            // `--author` on the one above: these commits must never reach the
            // collection the human estimate is built from.
            let agent = read_agent_commits(
                root,
                agent_authors,
                &mut resolver,
                diagnostics,
                depth,
                window.0,
                window.1,
                scoped_filter,
                &csv_globs(&arguments.path),
                &csv_globs(&arguments.path_exclude),
                arguments.no_ignore,
            );
            commits.extend(human);
            agent_commits.extend(agent);
        }
        let mut seen_agent_commits = HashSet::new();
        agent_commits.retain(|commit| {
            seen_agent_commits.insert((commit.repo_member_id.clone(), commit.sha.clone()))
        });
        let mut seen_commits = HashSet::new();
        commits.retain(|commit| {
            // An `--author` wide enough to match a bot — a developer literally
            // named Copilot, or a deliberately broad regex — would otherwise
            // put one commit on both sides. Agent authorship wins within the
            // same natural repository; an alias may legitimately combine two
            // repositories that happen to contain the same SHA.
            let key = (commit.repo_member_id.clone(), commit.sha.clone());
            !seen_agent_commits.contains(&key) && seen_commits.insert(key)
        });
    }
    resolver.validate_project_aliases()?;
    let display_labels =
        disambiguate_repository_labels(&mut sessions, &mut commits, &mut agent_commits);
    resolver.apply_display_labels(&display_labels);
    if let Some(exact) = arguments.repo_exact.as_deref() {
        // A final display label wins globally. Only when no label matches do
        // final checkout folder names participate, preserving the historical
        // path fallback without making a natural `Product` checkout shadow an
        // explicit alias whose final label is exactly `Product`.
        let label_matches = sessions
            .iter()
            .map(|item| (&item.repo, &item.repo_id))
            .chain(
                commits
                    .iter()
                    .chain(agent_commits.iter())
                    .map(|item| (&item.repo, &item.repo_id)),
            )
            .any(|(repo, repo_id)| exact_repo_label(repo, repo_id, exact));
        let allow_folder = !label_matches;
        filter_sessions(&mut sessions, None, Some(exact), allow_folder);
        commits.retain(|commit| {
            exact_repo(
                &commit.repo,
                &commit.repo_id,
                &commit.cwd,
                exact,
                allow_folder,
            )
        });
        agent_commits.retain(|commit| {
            exact_repo(
                &commit.repo,
                &commit.repo_id,
                &commit.cwd,
                exact,
                allow_folder,
            )
        });
    }

    let observations = resolver.take_repository_observations();
    if let Some(cache) = transcript_cache.as_mut()
        && let Err(error) = cache.remember_repository_identities(&observations)
    {
        diagnostics.warn(format!(
            "repository identity history write ignored: {error}"
        ));
    }

    progress.set("Estimating human involvement");
    let built = build_report_with_human_time_explanation(
        &sessions,
        &commits,
        &agent_commits,
        gap_cap,
        window.0,
        window.1,
        dimensions,
        human_idle,
        review_credit,
        explain_human_time,
    );
    let attribution = resolver.repository_attribution(&built.active_repository_checkouts);
    Ok(WindowRun {
        built,
        commits,
        scan_roots,
        attribution,
    })
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

fn filter_sessions(
    sessions: &mut Vec<Session>,
    pattern: Option<&str>,
    exact: Option<&str>,
    allow_folder: bool,
) {
    if let Some(exact) = exact {
        sessions.retain(|session| {
            exact_repo(
                &session.repo,
                &session.repo_id,
                &session.cwd,
                exact,
                allow_folder,
            )
        });
    }
    if let Some(pattern) = pattern {
        let needle = pattern.to_lowercase();
        sessions.retain(|session| {
            session.repo.to_lowercase().contains(&needle)
                || disambiguated_repository_label(&session.repo, &session.repo_id)
                    .to_lowercase()
                    .contains(&needle)
                || session.cwd.to_lowercase().contains(&needle)
                || session.root.to_lowercase().contains(&needle)
        });
    }
}

fn exact_repo_label(repo: &str, repo_id: &str, exact: &str) -> bool {
    repo.eq_ignore_ascii_case(exact)
        || disambiguated_repository_label(repo, repo_id).eq_ignore_ascii_case(exact)
}

fn exact_repo(repo: &str, repo_id: &str, cwd: &str, exact: &str, allow_folder: bool) -> bool {
    exact_repo_label(repo, repo_id, exact)
        || (allow_folder
            && Path::new(cwd)
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(exact)))
}

fn repository_display_labels<'a>(
    repositories: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> HashMap<String, String> {
    let mut by_label: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut labels = HashMap::new();
    for (repo_id, label) in repositories {
        by_label
            .entry(label.to_lowercase())
            .or_default()
            .insert(repo_id.to_string());
        labels
            .entry(repo_id.to_string())
            .or_insert_with(|| label.to_string());
    }
    for (_, repo_ids) in by_label {
        if repo_ids.len() < 2 {
            continue;
        }
        let configured: Vec<_> = repo_ids
            .iter()
            .filter(|repo_id| repo_id.starts_with("project:"))
            .collect();
        for repo_id in &repo_ids {
            // An explicit product label wins over colliding natural repository
            // names. Two aliases cannot collide because config validation
            // rejects that before scanning.
            if configured.len() == 1 && repo_id == configured[0] {
                continue;
            }
            let label = labels
                .get(repo_id)
                .cloned()
                .expect("every grouped repository has a label");
            labels.insert(
                repo_id.clone(),
                disambiguated_repository_label(&label, repo_id),
            );
        }
    }
    labels
}

fn disambiguate_repository_labels(
    sessions: &mut [Session],
    commits: &mut [model::GitCommit],
    agent_commits: &mut [model::GitCommit],
) -> HashMap<String, String> {
    let labels = repository_display_labels(
        sessions
            .iter()
            .map(|item| (item.repo_id.as_str(), item.repo.as_str()))
            .chain(
                commits
                    .iter()
                    .chain(agent_commits.iter())
                    .map(|item| (item.repo_id.as_str(), item.repo.as_str())),
            ),
    );
    for (repo_id, label) in sessions
        .iter_mut()
        .map(|item| (&item.repo_id, &mut item.repo))
        .chain(
            commits
                .iter_mut()
                .chain(agent_commits.iter_mut())
                .map(|item| (&item.repo_id, &mut item.repo)),
        )
    {
        if let Some(final_label) = labels.get(repo_id) {
            label.clone_from(final_label);
        }
    }
    labels
}

fn inferred_repository_roots<'a>(sessions: impl IntoIterator<Item = &'a Session>) -> Vec<PathBuf> {
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

/// How many of the baseline pass's warnings the selected window did not
/// already show: a lower bound that never blames the baseline for a warning
/// the report has.
///
/// Only stored texts can be compared, so only they count, each by whether the
/// selected window showed the same text; a shared text the baseline repeats
/// is still the report's. Counts are not compared at all, because a
/// difference in counts cannot tell a warning of the baseline's own from a
/// shared one raised more often. Past the storage cap the texts are gone:
/// the baseline's overflow is never counted, and once the selected window
/// itself hit the cap its dropped texts might be any baseline text, so
/// nothing is counted. A report already showing that many warnings gains
/// nothing from one more line, and an undercount there is the better failure
/// than a line sending the reader to a run that finds nothing new.
fn baseline_only_warnings(selected: &Diagnostics, baseline: &Diagnostics) -> u64 {
    if selected.warning_count > selected.messages.len() as u64 {
        return 0;
    }
    baseline
        .messages
        .iter()
        .filter(|message| !selected.messages.contains(message))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warned(messages: &[&str]) -> Diagnostics {
        let mut diagnostics = Diagnostics::default();
        for message in messages {
            diagnostics.warn(*message);
        }
        diagnostics
    }

    #[test]
    fn the_baseline_is_blamed_only_for_warnings_the_report_has_not_shown() {
        let selected = warned(&["shared"]);
        assert_eq!(0, baseline_only_warnings(&selected, &warned(&["shared"])));
        assert_eq!(
            2,
            baseline_only_warnings(&selected, &warned(&["shared", "own", "another"]))
        );
        assert_eq!(1, baseline_only_warnings(&warned(&[]), &warned(&["own"])));
        // A shared text raised twice by the baseline is still the report's.
        assert_eq!(
            0,
            baseline_only_warnings(&warned(&["a"]), &warned(&["a", "a"]))
        );
    }

    #[test]
    fn a_shared_flood_past_the_storage_cap_blames_the_baseline_for_nothing() {
        let flood: Vec<String> = (0..crate::model::MAX_STORED_MESSAGES + 20)
            .map(|line| format!("malformed line {line}"))
            .collect();
        let flood: Vec<&str> = flood.iter().map(String::as_str).collect();
        let selected = warned(&flood);
        assert_eq!(0, baseline_only_warnings(&selected, &warned(&flood)));
        // With the selected window capped nothing can be attributed safely.
        let mut more = flood.clone();
        more.push("own");
        assert_eq!(0, baseline_only_warnings(&selected, &warned(&more)));
        // When the report stored everything it raised, the baseline's
        // stored texts count and its textless overflow does not.
        assert_eq!(100, baseline_only_warnings(&warned(&[]), &warned(&flood)));
        // A shared text repeated past the cap is still the report's.
        let repeated = vec!["a"; crate::model::MAX_STORED_MESSAGES + 50];
        assert_eq!(
            0,
            baseline_only_warnings(&warned(&["a"]), &warned(&repeated))
        );
        // Own warnings first push shared ones into the baseline's overflow;
        // those are still the report's, so only the five own ones count.
        let shared: Vec<&str> = flood[..100].to_vec();
        let mut own_first = vec!["own 1", "own 2", "own 3", "own 4", "own 5"];
        own_first.extend(shared.iter().copied());
        assert_eq!(
            5,
            baseline_only_warnings(&warned(&shared), &warned(&own_first))
        );
    }

    use std::fs;

    #[test]
    fn duplicate_repository_labels_are_disambiguated_without_renaming_an_explicit_project() {
        let labels = repository_display_labels([
            ("remote:host/one/product", "product"),
            ("remote:host/two/product", "product"),
        ]);
        assert_ne!(
            labels["remote:host/one/product"],
            labels["remote:host/two/product"]
        );
        assert!(labels["remote:host/one/product"].starts_with("product ["));

        let labels = repository_display_labels([
            ("project:product", "Product"),
            ("remote:host/other/product", "Product"),
        ]);
        assert_eq!("Product", labels["project:product"]);
        assert!(labels["remote:host/other/product"].starts_with("Product ["));
        assert!(exact_repo(
            &labels["project:product"],
            "project:product",
            "/checkouts/alias",
            "Product",
            false
        ));
        assert!(!exact_repo(
            &labels["remote:host/other/product"],
            "remote:host/other/product",
            "/checkouts/Product",
            "Product",
            false
        ));
        assert!(exact_repo(
            &labels["remote:host/other/product"],
            "remote:host/other/product",
            "/checkouts/Product",
            "Product",
            true
        ));

        let case_only = repository_display_labels([
            ("remote:host/upper", "Product"),
            ("remote:host/lower", "product"),
        ]);
        assert!(case_only.values().all(|label| label.contains('[')));
    }

    #[test]
    fn exact_repo_filter_does_not_match_similar_names() {
        assert!(exact_repo(
            "studio/widget",
            "remote:host/studio/widget",
            "/repos/studio/widget",
            "widget",
            true
        ));
        assert!(!exact_repo(
            "misc/widget-tools",
            "remote:host/misc/widget-tools",
            "/repos/misc/widget-tools",
            "widget",
            true
        ));
        let displayed = disambiguated_repository_label("widget", "remote:host/studio/widget");
        assert!(exact_repo(
            "widget",
            "remote:host/studio/widget",
            "/repos/other",
            &displayed,
            false
        ));
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
            repo_id: "project".into(),
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
