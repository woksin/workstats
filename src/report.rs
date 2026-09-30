//! The report pipeline: reads AI histories and Git, aggregates, and presents
//! the result as a table, JSON, CSV, the interactive explorer, or an
//! allocation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::aggregate::build_report_with_human_time_explanation;
use crate::ai::{
    read_claude_sessions_indexed, read_codex_sessions_indexed, read_copilot_sessions_indexed,
    read_copilot_vscode_sessions_indexed, read_event_sessions_indexed,
    read_gemini_sessions_indexed, read_opencode_sessions_indexed, read_pi_sessions_indexed,
};
use crate::allocate;
use crate::cache::TranscriptCache;
use crate::classify;
use crate::cli::{
    AllocateArguments, OutputFormat, ReportArguments, agent_author_patterns, csv_globs,
    duration_flag, grouping_dimensions, report_window, resolve_authors, scan_directory,
    valid_provider_identifier,
};
use crate::git::{default_git_author, read_agent_commits, read_git_commits};
use crate::model::{self, Diagnostics, Inputs, Report, Session};
use crate::output::{print_csv, print_json, print_table};
use crate::paths::{
    PathResolver, configured_rules, default_cache_path, default_update_check_path,
    disambiguated_repository_label, home_dir, load_config,
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
            "`allocate` sets its own grouping (repo, provider, model, month); drop --group-by and use --month, --since, or --until to pick the window"
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
    let resolved = defaults.resolve(&mut arguments, presentation == Presentation::Explore);
    let output_format = resolved.format;
    // Refused before any scanning: `workstats ui --format json` can only mean
    // the user wanted one of the two, and picking silently is how --by-repo
    // used to lose an explicit --group-by.
    if presentation == Presentation::Explore
        && arguments
            .output_format
            .is_some_and(|format| format != OutputFormat::Table)
    {
        bail!(
            "`workstats ui` is interactive and writes no machine-readable output; drop --format, or run workstats without `ui` for json or csv"
        );
    }
    if presentation == Presentation::Explore
        && (arguments.explain_human_time || arguments.explain_repository_attribution)
    {
        bail!(
            "explanation flags are not available in `workstats ui`; run workstats with table or JSON output"
        );
    }
    if output_format == OutputFormat::Csv
        && (arguments.explain_human_time || arguments.explain_repository_attribution)
    {
        bail!(
            "explanation flags are not available with --format csv; use table output or --format json"
        );
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
    let (since, until) = report_window(&arguments, Utc::now())?;
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
        &config.authors,
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
    let mut resolver = PathResolver::with_context(rules, aliases, repository_history, home_dir());

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
                    "copilot-vscode" => read_copilot_vscode_sessions_indexed(
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
                    "pi" => read_pi_sessions_indexed(
                        path,
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
    // Broad substring filtering can happen immediately. Exact filtering waits
    // until every session and Git repository has its final disambiguated label;
    // otherwise an explicit alias and a natural repository with the same raw
    // name are both retained even though only the alias is displayed plainly.
    filter_sessions(&mut sessions, arguments.repo.as_deref(), None, true);
    let repo_filter = arguments.repo.as_deref();
    let agent_authors = agent_author_patterns(arguments.agent_commits.as_deref());
    let mut git_scan_roots = Vec::new();
    let mut commits = Vec::new();
    let mut agent_commits = Vec::new();
    if !arguments.no_git {
        git_scan_roots.push(directory.clone());
        // Sessions can belong to locally available checkouts outside `--dir`.
        // Always scan those roots so adding or removing a repo filter cannot
        // change which commits contribute to a retained session's report.
        git_scan_roots.extend(inferred_repository_roots(&sessions));
        let mut seen_roots = BTreeSet::new();
        git_scan_roots
            .retain(|root| seen_roots.insert(root.canonicalize().unwrap_or_else(|_| root.clone())));
        let configured_root = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.clone());
        progress.set("Scanning Git repositories");
        for root in &git_scan_roots {
            let scan_root = root.canonicalize().unwrap_or_else(|_| root.clone());
            // Everything but the configured directory got here by being the
            // checkout of a retained session.
            let from_session = scan_root != configured_root;
            let depth = if from_session { 0 } else { resolved.depth };
            // Re-applying the filter to such a root would reject it: the filter
            // matched the session's own working directory, which may be deep
            // inside the repository, while the repository is described by its
            // root. That defeated the inference this loop exists to perform.
            let scoped_filter = if from_session { None } else { repo_filter };
            commits.extend(read_git_commits(
                root,
                &authors,
                &mut resolver,
                &mut diagnostics,
                depth,
                since,
                until,
                scoped_filter,
                &csv_globs(&arguments.path),
                &csv_globs(&arguments.path_exclude),
                arguments.no_ignore,
                arguments.co_authors,
            ));
            // A separate pass over the same repositories rather than a wider
            // `--author` on the one above: these commits must never reach the
            // collection the human estimate is built from.
            agent_commits.extend(read_agent_commits(
                root,
                &agent_authors,
                &mut resolver,
                &mut diagnostics,
                depth,
                since,
                until,
                scoped_filter,
                &csv_globs(&arguments.path),
                &csv_globs(&arguments.path_exclude),
                arguments.no_ignore,
            ));
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
        since,
        until,
        &dimensions,
        human_idle,
        review_credit,
        arguments.explain_human_time,
    );
    let attribution = resolver.repository_attribution(&built.active_repository_checkouts);
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
        let allocation = allocate::build(&report.rows, &options);
        match output_format {
            OutputFormat::Json => allocate::print_json(&allocation)?,
            OutputFormat::Csv => allocate::print_csv(&allocation)?,
            OutputFormat::Table => allocate::print_table(&allocation),
        }
        return Ok(());
    }
    match output_format {
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

#[cfg(test)]
mod tests {
    use super::*;
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
