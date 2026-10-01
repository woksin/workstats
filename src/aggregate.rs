use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::attribution::{self, Ctx};
use crate::classify::{CategoryTally, ShapeTally, active_registry, change_shape};
use crate::model::{
    CompositionEntry, DayFigures, GitCommit, HumanSignal, HumanTimeExplanation, Interval,
    Methodology, Observed, ReportRow, Session, ShapeEntry, Summary, TokenUsage,
};
use crate::timeutil::{
    build_session_intervals, calculate_human_time, calendar_days, clip_interval, local_date,
    local_month, local_week, split_interval, union_seconds,
};

pub const DIMENSIONS: &[&str] = &[
    "repo",
    "root",
    "cwd",
    "provider",
    "model",
    "day",
    "week",
    "month",
    "branch",
    "issue",
    "feature",
    "engagement",
];

type SessionKey = (String, String);
type CommitIdentity = (String, String);
type SignalKey = (DateTime<Utc>, String, String);

#[derive(Default)]
struct Bucket {
    key: BTreeMap<String, String>,
    repo_id: Option<String>,
    active_seconds: f64,
    sessions: HashSet<SessionKey>,
    foreground_sessions: HashSet<SessionKey>,
    subagent_sessions: HashSet<SessionKey>,
    ai_intervals: Vec<Interval>,
    human_seconds: f64,
    human_blocks: HashSet<String>,
    human_signals: HashSet<SignalKey>,
    human_days: HashSet<String>,
    providers: BTreeSet<String>,
    models: BTreeSet<String>,
    commits: HashSet<CommitIdentity>,
    files: HashSet<(String, String)>,
    additions: u64,
    deletions: u64,
    ignored_additions: u64,
    ignored_deletions: u64,
    agent_commits: HashSet<CommitIdentity>,
    agent_additions: u64,
    agent_deletions: u64,
    ai_assisted_commits: HashSet<CommitIdentity>,
    autofix_assisted_commits: HashSet<CommitIdentity>,
    categories: CategoryTally,
    shapes: ShapeTally,
    tokens: TokenUsage,
    first_seen: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
    active_days: HashSet<String>,
}

pub struct BuiltReport {
    pub methodology: Methodology,
    pub human_time_explanation: Option<HumanTimeExplanation>,
    /// Logical repositories with evidence that actually survives the report
    /// window and contributes to at least one row.
    pub active_repository_checkouts: HashSet<(String, String)>,
    pub observed: Observed,
    pub summary: Summary,
    pub group_by: Vec<String>,
    pub rows: Vec<ReportRow>,
    /// Per local day, only when asked for (HTML, the explorer, `--daily`).
    pub daily: Option<Vec<DayFigures>>,
    pub timeline: Timeline,
}

/// The clipped, filtered vectors the report was built from. Not serialized: it
/// is what timesheets, branch reports and insights are computed over, so they
/// sum the same pieces the report did and cannot double count.
// Consumed by the timesheet, branch and insights work packages.
pub struct Timeline {
    /// Non-overlapping human pieces, each labelled by the nearest signal. The
    /// piece's `session_id` is its work block (`work-block:N`).
    pub human_intervals: Vec<Interval>,
    /// The effective human signals inside the window.
    pub human_signals: Vec<HumanSignal>,
    /// Agent intervals inside the window; they may overlap one another.
    pub ai_intervals: Vec<Interval>,
}

fn foreground_human_signals(sessions: &[Session]) -> Vec<HumanSignal> {
    let mut signals = Vec::new();
    for session in sessions.iter().filter(|session| !session.is_subagent) {
        let edge_kind = format!("{}_session_edge", session.provider);
        let prompt_kind = format!("{}_prompt", session.provider);
        let mut push = |timestamp: DateTime<Utc>, model: &str, kind: &str| {
            signals.push(HumanSignal {
                timestamp,
                provider: session.provider.clone(),
                session_id: session.session_id.clone(),
                cwd: session.cwd.clone(),
                repo: session.repo.clone(),
                repo_id: session.repo_id.clone(),
                root: session.root.clone(),
                kind: kind.to_string(),
                model: model.to_string(),
                branch: session.branch_at(timestamp).map(str::to_string),
            });
        };
        for point in &session.human_points {
            push(point.timestamp, &point.model, &prompt_kind);
        }

        // Foreground transcript activity is mostly autonomous assistant/tool output. Treating
        // every event as human presence can bridge an entire day while the developer is away.
        // Session boundaries still provide useful upper-leaning setup/review evidence without
        // turning dense model output into continuous human time.
        let mut first: Option<(DateTime<Utc>, String)> = None;
        let mut last: Option<(DateTime<Utc>, String)> = None;
        let mut include_edge = |timestamp: DateTime<Utc>, model: &str| {
            if first.as_ref().is_none_or(|(value, _)| timestamp < *value) {
                first = Some((timestamp, model.to_string()));
            }
            if last.as_ref().is_none_or(|(value, _)| timestamp > *value) {
                last = Some((timestamp, model.to_string()));
            }
        };
        for point in &session.points {
            include_edge(point.timestamp, &point.model);
        }
        for interval in &session.exact_intervals {
            include_edge(interval.start, &interval.model);
            include_edge(interval.end, &interval.model);
        }
        for point in &session.human_points {
            include_edge(point.timestamp, &point.model);
        }
        if let Some((timestamp, model)) = &first {
            push(*timestamp, model, &edge_kind);
        }
        if let Some((timestamp, model)) = &last
            && first
                .as_ref()
                .is_none_or(|(first_timestamp, _)| timestamp != first_timestamp)
        {
            push(*timestamp, model, &edge_kind);
        }
    }
    signals
}

/// `agent_commits` arrives separately from `commits` because it comes from a
/// separate `git log` pass, but which side a commit belongs on is decided by
/// the commit itself. `authorship` is the authority here, so swapping the two
/// slices at a call site would change nothing about the report — and the human
/// timeline is reachable only through `GitCommit::human_signal`, which hands
/// back nothing at all for agent-authored work.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn build_report(
    sessions: &[Session],
    commits: &[GitCommit],
    agent_commits: &[GitCommit],
    gap_cap: Duration,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    dimensions: &[String],
    human_idle: Duration,
    review_credit: Duration,
) -> BuiltReport {
    build_report_with_human_time_explanation(
        sessions,
        commits,
        agent_commits,
        gap_cap,
        since,
        until,
        dimensions,
        human_idle,
        review_credit,
        false,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_report_with_human_time_explanation(
    sessions: &[Session],
    commits: &[GitCommit],
    agent_commits: &[GitCommit],
    gap_cap: Duration,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    dimensions: &[String],
    human_idle: Duration,
    review_credit: Duration,
    explain_human_time: bool,
    daily: bool,
) -> BuiltReport {
    let intervals: Vec<_> = sessions
        .iter()
        .flat_map(|session| build_session_intervals(session, gap_cap))
        .filter_map(|interval| clip_interval(&interval, since, until))
        .collect();
    let (filtered_agent_commits, filtered_commits): (Vec<_>, Vec<_>) = commits
        .iter()
        .chain(agent_commits)
        .filter(|commit| {
            since.is_none_or(|bound| commit.timestamp >= bound)
                && until.is_none_or(|bound| commit.timestamp < bound)
        })
        .partition(|commit| commit.authorship.is_agent_authored());
    let filtered_tokens: Vec<_> = sessions
        .iter()
        .flat_map(|session| {
            session.token_events.iter().map(move |event| TokenRecord {
                timestamp: event.timestamp,
                branch: session.branch_at(event.timestamp).map(str::to_string),
                repo: session.repo.clone(),
                repo_id: session.repo_id.clone(),
                root: session.root.clone(),
                cwd: session.cwd.clone(),
                provider: session.provider.clone(),
                model: event.model.clone(),
                usage: event.usage,
            })
        })
        .filter(|token| {
            since.is_none_or(|bound| token.timestamp >= bound)
                && until.is_none_or(|bound| token.timestamp < bound)
        })
        .collect();
    let mut human_signals = foreground_human_signals(sessions);
    // `filter_map`, not `map`: a commit decides for itself whether it is
    // evidence anyone was at the keyboard, and an agent-authored one answers
    // `None`. See `GitCommit::human_signal`.
    human_signals.extend(
        filtered_commits
            .iter()
            .filter_map(|commit| commit.human_signal()),
    );
    let filtered_human_signals: Vec<_> = human_signals
        .into_iter()
        .filter(|signal| {
            since.is_none_or(|bound| signal.timestamp >= bound)
                && until.is_none_or(|bound| signal.timestamp < bound)
        })
        .collect();
    let human_time = calculate_human_time(
        &filtered_human_signals,
        human_idle,
        review_credit,
        since,
        until,
        explain_human_time,
    );
    let human_intervals = human_time.intervals;
    let human_seconds = human_time.total_seconds;
    let human_time_explanation = human_time.explanation;
    let session_roles: HashMap<SessionKey, bool> = sessions
        .iter()
        .map(|session| {
            (
                (session.provider.clone(), session.session_id.clone()),
                session.is_subagent,
            )
        })
        .collect();
    let mut buckets: HashMap<Vec<String>, Bucket> = HashMap::new();

    for interval in &intervals {
        for (group_key, key, piece) in keys_for_interval(interval, dimensions) {
            let row = bucket(&mut buckets, group_key, key, dimensions);
            row.active_seconds += piece.seconds();
            let role_key = (piece.provider.clone(), piece.session_id.clone());
            row.sessions.insert(role_key.clone());
            row.ai_intervals.push(piece.clone());
            if session_roles.get(&role_key).copied().unwrap_or(false) {
                row.subagent_sessions.insert(role_key);
            } else {
                row.foreground_sessions.insert(role_key);
            }
            row.providers.insert(piece.provider.clone());
            row.models.insert(piece.model.clone());
            row.active_days.extend(
                split_interval(&piece, "day")
                    .into_iter()
                    .map(|(day, _)| day),
            );
            include_time(row, piece.start, piece.end);
        }
    }

    for interval in &human_intervals {
        for (group_key, key, piece) in keys_for_interval(interval, dimensions) {
            let row = bucket(&mut buckets, group_key, key, dimensions);
            row.human_seconds += piece.seconds();
            row.human_blocks.insert(piece.session_id.clone());
            include_time(row, piece.start, piece.end);
        }
    }

    for signal in &filtered_human_signals {
        let values = signal_values(signal, dimensions);
        let (group_key, key) = dimension_keys(&values, &signal.repo_id, dimensions);
        let row = bucket(&mut buckets, group_key, key, dimensions);
        row.human_signals.insert((
            signal.timestamp,
            signal.kind.clone(),
            signal.session_id.clone(),
        ));
        row.human_days.insert(local_date(signal.timestamp));
    }

    let active_session_keys: HashSet<SessionKey> = intervals
        .iter()
        .map(|interval| (interval.provider.clone(), interval.session_id.clone()))
        .collect();
    let mut eligible_session_keys = active_session_keys.clone();
    let mut single_point_dates = HashSet::new();
    let mut single_point_sessions: Vec<(NaiveDate, SessionKey)> = Vec::new();
    for session in sessions {
        let session_key = (session.provider.clone(), session.session_id.clone());
        let Some(first) = session.first_seen() else {
            continue;
        };
        if since.is_some_and(|bound| first < bound) || until.is_some_and(|bound| first >= bound) {
            continue;
        }
        eligible_session_keys.insert(session_key.clone());
        if active_session_keys.contains(&session_key) {
            continue;
        }
        let model = session
            .points
            .first()
            .map(|point| point.model.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let values = session_values(session, &model, first, dimensions);
        let (group_key, key) = dimension_keys(&values, &session.repo_id, dimensions);
        let row = bucket(&mut buckets, group_key, key, dimensions);
        row.sessions.insert(session_key.clone());
        if session.is_subagent {
            row.subagent_sessions.insert(session_key);
        } else {
            row.foreground_sessions.insert(session_key);
        }
        row.providers.insert(session.provider.clone());
        row.models.insert(model);
        let day = local_date(first);
        row.active_days.insert(day.clone());
        if let Ok(date) = NaiveDate::parse_from_str(&day, "%Y-%m-%d") {
            single_point_sessions.push((date, session_key_for_daily(session)));
        }
        single_point_dates.insert(day);
        include_time(row, first, first);
    }

    for commit in &filtered_commits {
        let key: Vec<_> = dimensions
            .iter()
            .map(|dimension| bounded_value(&commit_value(commit, dimension)))
            .collect();
        let group_key = grouped_key(&key, &commit.repo_id, dimensions);
        let row = bucket(&mut buckets, group_key, key, dimensions);
        let commit_identity = (commit.repo_member_id.clone(), commit.sha.clone());
        row.commits.insert(commit_identity.clone());
        row.files.extend(
            commit
                .files
                .iter()
                .cloned()
                .map(|path| (commit.repo_member_id.clone(), path)),
        );
        row.additions += commit.additions;
        row.deletions += commit.deletions;
        row.ignored_additions += commit.ignored_additions;
        row.ignored_deletions += commit.ignored_deletions;
        row.categories.merge(&commit.categories);
        if let Some(shape) = change_shape(&commit.categories) {
            row.shapes.add(shape);
        }
        // A share of the commits just counted, never an addition to them.
        if commit.authorship.is_agent_assisted() {
            row.ai_assisted_commits.insert(commit_identity.clone());
        }
        if commit.authorship.is_autofix_assisted() {
            row.autofix_assisted_commits.insert(commit_identity);
        }
        row.active_days.insert(local_date(commit.timestamp));
        include_time(row, commit.timestamp, commit.timestamp);
    }

    // Agent output is kept out of the churn figures above on purpose: `--author`
    // is this tool's statement about whose work is being measured, and lines an
    // agent pushed are not the developer's to claim. What they do contribute is
    // calendar coverage — the day an agent landed code is a day AI worked on
    // this repository — and nothing whatever to the human estimate.
    for commit in &filtered_agent_commits {
        let key: Vec<_> = dimensions
            .iter()
            .map(|dimension| bounded_value(&commit_value(commit, dimension)))
            .collect();
        let group_key = grouped_key(&key, &commit.repo_id, dimensions);
        let row = bucket(&mut buckets, group_key, key, dimensions);
        row.agent_commits
            .insert((commit.repo_member_id.clone(), commit.sha.clone()));
        row.agent_additions += commit.additions;
        row.agent_deletions += commit.deletions;
        row.active_days.insert(local_date(commit.timestamp));
        include_time(row, commit.timestamp, commit.timestamp);
    }

    for token in &filtered_tokens {
        let key: Vec<_> = dimensions
            .iter()
            .map(|dimension| bounded_value(&token_value(token, dimension)))
            .collect();
        let group_key = grouped_key(&key, &token.repo_id, dimensions);
        let row = bucket(&mut buckets, group_key, key, dimensions);
        row.tokens += token.usage;
        row.active_days.insert(local_date(token.timestamp));
        include_time(row, token.timestamp, token.timestamp);
    }

    let mut rows: Vec<_> = buckets
        .into_values()
        .map(|row| {
            let active_days = row.active_days.len();
            let days = calendar_days(row.first_seen, row.last_seen);
            ReportRow {
                key: row.key,
                repo_id: row.repo_id,
                active_seconds: round3(row.active_seconds),
                parallel_agent_seconds: round3(row.active_seconds),
                ai_wall_seconds: round3(union_seconds(&row.ai_intervals)),
                human_estimated_seconds: round3(row.human_seconds),
                human_signal_count: row.human_signals.len(),
                work_block_count: row.human_blocks.len(),
                session_count: row.sessions.len(),
                commit_count: row.commits.len(),
                foreground_session_count: row.foreground_sessions.len(),
                subagent_session_count: row.subagent_sessions.len(),
                file_count: row.files.len(),
                additions: row.additions,
                deletions: row.deletions,
                ignored_additions: row.ignored_additions,
                ignored_deletions: row.ignored_deletions,
                net_lines: row.additions as i64 - row.deletions as i64,
                agent_commit_count: row.agent_commits.len(),
                agent_additions: row.agent_additions,
                agent_deletions: row.agent_deletions,
                ai_assisted_commit_count: row.ai_assisted_commits.len(),
                autofix_assisted_commit_count: row.autofix_assisted_commits.len(),
                composition: composition_entries(
                    row.files.iter().map(|(_, path)| path.as_str()),
                    &row.categories,
                ),
                change_shapes: shape_entries(&row.shapes),
                input_tokens: row.tokens.input_tokens,
                output_tokens: row.tokens.output_tokens,
                cache_read_tokens: row.tokens.cache_read_tokens,
                cache_creation_tokens: row.tokens.cache_creation_tokens,
                total_tokens: row.tokens.total(),
                active_days,
                human_active_days: row.human_days.len(),
                calendar_days: days,
                average_human_seconds_per_active_day: if row.human_days.is_empty() {
                    0.0
                } else {
                    round3(row.human_seconds / row.human_days.len() as f64)
                },
                average_active_seconds_per_active_day: if active_days == 0 {
                    0.0
                } else {
                    round3(row.active_seconds / active_days as f64)
                },
                average_active_seconds_per_calendar_day: if days == 0 {
                    0.0
                } else {
                    round3(row.active_seconds / days as f64)
                },
                first_seen: row.first_seen.map(iso),
                last_seen: row.last_seen.map(iso),
                providers: row.providers.into_iter().collect(),
                models: row.models.into_iter().collect(),
            }
        })
        .collect();
    let calendar = dimensions.iter().any(|name| is_calendar(name));
    rows.sort_by(|left, right| {
        let compare_number =
            |left: f64, right: f64| left.partial_cmp(&right).unwrap_or(Ordering::Equal);
        let ordering = if calendar {
            let left_calendar = left
                .key
                .get("month")
                .or_else(|| left.key.get("week"))
                .or_else(|| left.key.get("day"))
                .map(String::as_str)
                .unwrap_or("");
            let right_calendar = right
                .key
                .get("month")
                .or_else(|| right.key.get("week"))
                .or_else(|| right.key.get("day"))
                .map(String::as_str)
                .unwrap_or("");
            left_calendar
                .cmp(right_calendar)
                .then_with(|| {
                    compare_number(left.human_estimated_seconds, right.human_estimated_seconds)
                })
                .then_with(|| compare_number(left.ai_wall_seconds, right.ai_wall_seconds))
                .then_with(|| left.commit_count.cmp(&right.commit_count))
        } else {
            compare_number(left.human_estimated_seconds, right.human_estimated_seconds)
                .then_with(|| compare_number(left.ai_wall_seconds, right.ai_wall_seconds))
                .then_with(|| left.commit_count.cmp(&right.commit_count))
        };
        ordering.reverse()
    });

    let mut all_times = Vec::new();
    for interval in &intervals {
        all_times.extend([interval.start, interval.end]);
    }
    for interval in &human_intervals {
        all_times.extend([interval.start, interval.end]);
    }
    all_times.extend(filtered_human_signals.iter().map(|signal| signal.timestamp));
    // Observed at, not worked at: an agent commit is a moment this report saw
    // activity, even though it contributes no human interval to bound.
    all_times.extend(filtered_agent_commits.iter().map(|commit| commit.timestamp));

    let mut active_dates: HashSet<_> = intervals
        .iter()
        .flat_map(|interval| {
            split_interval(interval, "day")
                .into_iter()
                .map(|(day, _)| day)
        })
        .collect();
    active_dates.extend(single_point_dates);
    active_dates.extend(
        filtered_commits
            .iter()
            .chain(filtered_agent_commits.iter())
            .map(|commit| local_date(commit.timestamp)),
    );
    let human_dates: HashSet<_> = filtered_human_signals
        .iter()
        .map(|signal| local_date(signal.timestamp))
        .collect();
    let mut provider_seconds: BTreeMap<String, f64> = BTreeMap::new();
    let mut model_seconds: BTreeMap<String, f64> = BTreeMap::new();
    for interval in &intervals {
        *provider_seconds
            .entry(interval.provider.clone())
            .or_default() += interval.seconds();
        *model_seconds.entry(interval.model.clone()).or_default() += interval.seconds();
    }
    for value in provider_seconds.values_mut() {
        *value = round3(*value);
    }
    for value in model_seconds.values_mut() {
        *value = round3(*value);
    }
    let mut provider_tokens: BTreeMap<String, u64> = BTreeMap::new();
    let mut model_tokens: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_tokens = TokenUsage::default();
    for token in &filtered_tokens {
        *provider_tokens.entry(token.provider.clone()).or_default() += token.usage.total();
        *model_tokens.entry(token.model.clone()).or_default() += token.usage.total();
        total_tokens += token.usage;
    }
    let agent_seconds: f64 = intervals.iter().map(Interval::seconds).sum();
    let foreground_session_count = eligible_session_keys
        .iter()
        .filter(|key| !session_roles.get(*key).copied().unwrap_or(false))
        .count();
    let subagent_session_count = eligible_session_keys
        .iter()
        .filter(|key| session_roles.get(*key).copied().unwrap_or(false))
        .count();
    let unique_commits: HashSet<_> = filtered_commits
        .iter()
        .map(|commit| (&commit.repo_member_id, &commit.sha))
        .collect();
    let unique_agent_commits: HashSet<_> = filtered_agent_commits
        .iter()
        .map(|commit| (&commit.repo_member_id, &commit.sha))
        .collect();
    // Member-qualified SHAs rather than a running count, so a commit reachable
    // from two worktrees is described once while distinct alias members that
    // legitimately share history remain distinct.
    let mut ai_assisted_commits: HashSet<(&String, &String)> = HashSet::new();
    let mut autofix_assisted_commits: HashSet<(&String, &String)> = HashSet::new();
    for commit in &filtered_commits {
        if commit.authorship.is_agent_assisted() {
            ai_assisted_commits.insert((&commit.repo_member_id, &commit.sha));
        }
        if commit.authorship.is_autofix_assisted() {
            autofix_assisted_commits.insert((&commit.repo_member_id, &commit.sha));
        }
    }
    let mut summary_categories = CategoryTally::default();
    let mut summary_shapes = ShapeTally::default();
    let mut summary_files: HashSet<(&str, &str)> = HashSet::new();
    for commit in &filtered_commits {
        summary_categories.merge(&commit.categories);
        if let Some(shape) = change_shape(&commit.categories) {
            summary_shapes.add(shape);
        }
        summary_files.extend(
            commit
                .files
                .iter()
                .map(|path| (commit.repo_member_id.as_str(), path.as_str())),
        );
    }
    let (foreground_sessions_with_commits, foreground_sessions_without_commits) =
        foreground_session_output(
            sessions,
            &filtered_commits,
            &eligible_session_keys,
            &session_roles,
            human_idle,
        );
    let human_signal_keys: HashSet<_> = filtered_human_signals
        .iter()
        .map(|signal| {
            (
                signal.timestamp,
                signal.kind.as_str(),
                signal.session_id.as_str(),
            )
        })
        .collect();
    let prompt_signal_count = filtered_human_signals
        .iter()
        .filter(|signal| signal.kind.ends_with("_prompt"))
        .map(|signal| {
            (
                signal.timestamp,
                signal.kind.as_str(),
                signal.session_id.as_str(),
            )
        })
        .collect::<HashSet<_>>()
        .len();
    let foreground_session_edge_signal_count = filtered_human_signals
        .iter()
        .filter(|signal| signal.kind.ends_with("_session_edge"))
        .map(|signal| {
            (
                signal.timestamp,
                signal.kind.as_str(),
                signal.session_id.as_str(),
            )
        })
        .collect::<HashSet<_>>()
        .len();
    let commit_signal_count = filtered_human_signals
        .iter()
        .filter(|signal| signal.kind == "commit")
        .map(|signal| {
            (
                signal.timestamp,
                signal.kind.as_str(),
                signal.session_id.as_str(),
            )
        })
        .collect::<HashSet<_>>()
        .len();

    let mut active_repository_checkouts: HashSet<(String, String)> = sessions
        .iter()
        .filter(|session| {
            eligible_session_keys.contains(&(session.provider.clone(), session.session_id.clone()))
        })
        .map(|session| (session.repo_id.clone(), session.cwd.clone()))
        .collect();
    active_repository_checkouts.extend(
        filtered_commits
            .iter()
            .chain(filtered_agent_commits.iter())
            .map(|commit| (commit.repo_id.clone(), commit.cwd.clone())),
    );
    active_repository_checkouts.extend(
        filtered_tokens
            .iter()
            .map(|token| (token.repo_id.clone(), token.cwd.clone())),
    );

    let daily_figures = daily.then(|| {
        compute_daily_figures(
            &human_intervals,
            &intervals,
            &filtered_human_signals,
            &filtered_commits,
            &single_point_sessions,
        )
    });

    BuiltReport {
        methodology: Methodology {
            human_work: "human prompts, foreground session boundaries, and authored commits clustered into non-overlapping involvement blocks",
            human_time_algorithm_version: "signal-blocks-v1",
            human_time_timezone_basis: "UTC signal and ledger timestamps; review-credit edges clamp to host-local calendar midnights",
            human_time_boundary_basis: "half-open report window [since, until); a gap strictly greater than the idle threshold starts a block; review credit is split equally around each block",
            human_idle_threshold_seconds: duration_seconds(human_idle),
            review_credit_seconds: duration_seconds(review_credit),
            human_estimate_caveat: "a supervision-inclusive estimate with bounded setup/review credit around foreground sessions; autonomous transcript output is not treated as continuous human presence",
            ai_time: "consecutive structural activity signals capped at the idle gap; exact intervals are merged when a source records them",
            deduplication: "headline time is the union of all AI intervals; grouped AI totals may overlap across parallel repos/providers",
            gap_cap_seconds: duration_seconds(gap_cap),
            composition: format!(
                "changed Git lines bucketed into {} from the file path alone; this is churn, not the size of the codebase",
                active_registry().names().collect::<Vec<_>>().join("/")
            ),
            change_shapes: "each commit described by the area holding at least 60% of its changed lines and by its addition/deletion balance; commit messages and file contents are never read",
            agent_output: "commits a coding agent authored are matched by Git identity, reported apart from your own, and contribute no human time; a Co-authored-by trailer flags a commit you already wrote rather than adding another",
            scope: "local retained histories, explicit event logs, and locally available Git repositories only",
        },
        human_time_explanation,
        active_repository_checkouts,
        observed: Observed {
            first_seen: all_times.iter().min().copied().map(iso),
            last_seen: all_times.iter().max().copied().map(iso),
        },
        summary: Summary {
            human_estimated_seconds: human_seconds,
            human_active_days: human_dates.len(),
            average_human_seconds_per_active_day: if human_dates.is_empty() {
                0.0
            } else {
                round3(human_seconds / human_dates.len() as f64)
            },
            work_block_count: human_intervals
                .iter()
                .map(|item| &item.session_id)
                .collect::<HashSet<_>>()
                .len(),
            human_signal_count: human_signal_keys.len(),
            prompt_signal_count,
            foreground_session_edge_signal_count,
            commit_signal_count,
            deduplicated_active_seconds: round3(union_seconds(&intervals)),
            attributed_active_seconds: round3(agent_seconds),
            agent_wall_seconds: round3(union_seconds(&intervals)),
            parallel_agent_seconds: round3(agent_seconds),
            session_count: eligible_session_keys.len(),
            foreground_session_count,
            subagent_session_count,
            foreground_sessions_with_commits,
            foreground_sessions_without_commits,
            commit_count: unique_commits.len(),
            additions: filtered_commits.iter().map(|item| item.additions).sum(),
            deletions: filtered_commits.iter().map(|item| item.deletions).sum(),
            ignored_additions: filtered_commits
                .iter()
                .map(|item| item.ignored_additions)
                .sum(),
            ignored_deletions: filtered_commits
                .iter()
                .map(|item| item.ignored_deletions)
                .sum(),
            agent_commit_count: unique_agent_commits.len(),
            agent_additions: filtered_agent_commits
                .iter()
                .map(|item| item.additions)
                .sum(),
            agent_deletions: filtered_agent_commits
                .iter()
                .map(|item| item.deletions)
                .sum(),
            ai_assisted_commit_count: ai_assisted_commits.len(),
            autofix_assisted_commit_count: autofix_assisted_commits.len(),
            composition: composition_entries(
                summary_files.iter().map(|(_, path)| *path),
                &summary_categories,
            ),
            change_shapes: shape_entries(&summary_shapes),
            active_days: active_dates.len(),
            provider_seconds,
            model_seconds,
            input_tokens: total_tokens.input_tokens,
            output_tokens: total_tokens.output_tokens,
            cache_read_tokens: total_tokens.cache_read_tokens,
            cache_creation_tokens: total_tokens.cache_creation_tokens,
            total_tokens: total_tokens.total(),
            provider_tokens,
            model_tokens,
        },
        group_by: dimensions.to_vec(),
        rows,
        daily: daily_figures,
        timeline: Timeline {
            human_intervals,
            human_signals: filtered_human_signals,
            ai_intervals: intervals,
        },
    }
}

/// Human and agent figures per local calendar day. Days with nothing on them
/// are left out; whoever draws a calendar fills them in.
fn compute_daily_figures(
    human_intervals: &[Interval],
    ai_intervals: &[Interval],
    signals: &[HumanSignal],
    commits: &[&GitCommit],
    single_point_sessions: &[(NaiveDate, SessionKey)],
) -> Vec<DayFigures> {
    #[derive(Default)]
    struct Day {
        human: f64,
        agent: Vec<Interval>,
        prompts: HashSet<(DateTime<Utc>, String)>,
        commits: HashSet<CommitIdentity>,
        sessions: HashSet<SessionKey>,
    }
    let mut days: BTreeMap<NaiveDate, Day> = BTreeMap::new();
    let parse = |label: &str| NaiveDate::parse_from_str(label, "%Y-%m-%d").ok();
    for interval in human_intervals {
        for (label, piece) in split_interval(interval, "day") {
            if let Some(date) = parse(&label) {
                days.entry(date).or_default().human += piece.seconds();
            }
        }
    }
    for interval in ai_intervals {
        for (label, piece) in split_interval(interval, "day") {
            if let Some(date) = parse(&label) {
                let day = days.entry(date).or_default();
                day.sessions
                    .insert((piece.provider.clone(), piece.session_id.clone()));
                day.agent.push(piece);
            }
        }
    }
    for signal in signals.iter().filter(|s| s.kind.ends_with("_prompt")) {
        if let Some(date) = parse(&local_date(signal.timestamp)) {
            days.entry(date)
                .or_default()
                .prompts
                .insert((signal.timestamp, signal.session_id.clone()));
        }
    }
    for commit in commits {
        if let Some(date) = parse(&local_date(commit.timestamp)) {
            days.entry(date)
                .or_default()
                .commits
                .insert((commit.repo_member_id.clone(), commit.sha.clone()));
        }
    }
    for (date, key) in single_point_sessions {
        days.entry(*date).or_default().sessions.insert(key.clone());
    }
    days.into_iter()
        .map(|(date, day)| DayFigures {
            date,
            human_seconds: round3(day.human),
            agent_wall_seconds: round3(union_seconds(&day.agent)),
            prompts: day.prompts.len(),
            commits: day.commits.len(),
            sessions: day.sessions.len(),
        })
        .collect()
}

/// Distinct changed paths and changed lines per file area, largest first.
/// Areas with nothing in them are left out rather than emitted as zeroes.
fn composition_entries<'a>(
    files: impl IntoIterator<Item = &'a str>,
    tally: &CategoryTally,
) -> Vec<CompositionEntry> {
    let registry = active_registry();
    let mut counts = vec![0_usize; registry.len()];
    for path in files {
        if let Some(count) = counts.get_mut(registry.classify(path)) {
            *count += 1;
        }
    }
    let total = tally.touched() as f64;
    let mut entries: Vec<_> = (0..registry.len())
        .filter_map(|category| {
            let lines = tally.get(category);
            let files = counts[category];
            (files != 0 || lines.touched() != 0).then(|| CompositionEntry {
                category: registry.name(category).to_string(),
                files,
                additions: lines.additions,
                deletions: lines.deletions,
                share_of_changed_lines: if total == 0.0 {
                    0.0
                } else {
                    round3(lines.touched() as f64 / total)
                },
            })
        })
        .collect();
    entries.sort_by(|left, right| {
        (right.additions + right.deletions)
            .cmp(&(left.additions + left.deletions))
            .then_with(|| right.files.cmp(&left.files))
            .then_with(|| left.category.cmp(&right.category))
    });
    entries
}

/// Commit counts per diff shape, largest first.
fn shape_entries(tally: &ShapeTally) -> Vec<ShapeEntry> {
    let total = tally.total();
    let mut entries: Vec<_> = tally
        .iter()
        .map(|(shape, commits)| ShapeEntry {
            shape: shape.as_str().to_string(),
            commits,
            share_of_classified_commits: if total == 0 {
                0.0
            } else {
                round3(commits as f64 / total as f64)
            },
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .commits
            .cmp(&left.commits)
            .then_with(|| left.shape.cmp(&right.shape))
    });
    entries
}

/// Splits foreground sessions into those that have an authored commit in the
/// same repo within one idle window and those that do not, answering "did this
/// session leave committed output?".
///
/// Only sessions in repos that produced commits in scope are counted at all.
/// Git is usually scanned over one directory while AI history covers the whole
/// machine, so a session in an unscanned repo says nothing about output and
/// would otherwise inflate the "no commit" side. What remains genuinely covers
/// reading, review, and uncommitted work, which local structure cannot tell
/// apart without reading transcript text.
pub(crate) fn foreground_session_output(
    sessions: &[Session],
    commits: &[&GitCommit],
    eligible: &HashSet<SessionKey>,
    roles: &HashMap<SessionKey, bool>,
    human_idle: Duration,
) -> (usize, usize) {
    let mut by_repo: HashMap<&str, Vec<DateTime<Utc>>> = HashMap::new();
    for commit in commits {
        by_repo
            .entry(commit.repo_id.as_str())
            .or_default()
            .push(commit.timestamp);
    }
    for times in by_repo.values_mut() {
        times.sort_unstable();
    }
    let mut with: HashSet<&SessionKey> = HashSet::new();
    let mut comparable: HashSet<&SessionKey> = HashSet::new();
    for session in sessions {
        let key = (session.provider.clone(), session.session_id.clone());
        let Some(key) = eligible.get(&key) else {
            continue;
        };
        if roles.get(key).copied().unwrap_or(false) {
            continue;
        }
        let Some(times) = by_repo.get(session.repo_id.as_str()) else {
            continue;
        };
        comparable.insert(key);
        let (Some(first), Some(last)) = (session.first_seen(), session.last_seen()) else {
            continue;
        };
        let (start, end) = (first - human_idle, last + human_idle);
        let index = times.partition_point(|time| *time < start);
        if times.get(index).is_some_and(|time| *time <= end) {
            with.insert(key);
        }
    }
    (with.len(), comparable.len() - with.len())
}

fn session_key_for_daily(session: &Session) -> SessionKey {
    (session.provider.clone(), session.session_id.clone())
}

fn bucket<'a>(
    buckets: &'a mut HashMap<Vec<String>, Bucket>,
    group_key: Vec<String>,
    key: Vec<String>,
    dimensions: &[String],
) -> &'a mut Bucket {
    let repo_id = dimensions
        .iter()
        .position(|dimension| dimension == "repo")
        .and_then(|index| group_key.get(index).cloned());
    buckets.entry(group_key).or_insert_with(|| Bucket {
        key: dimensions.iter().cloned().zip(key).collect(),
        repo_id,
        ..Bucket::default()
    })
}

fn grouped_key(key: &[String], repo_id: &str, dimensions: &[String]) -> Vec<String> {
    key.iter()
        .zip(dimensions)
        .map(|(value, dimension)| {
            if dimension == "repo" {
                bounded_value(repo_id)
            } else {
                value.clone()
            }
        })
        .collect()
}

fn dimension_keys(
    values: &HashMap<String, String>,
    repo_id: &str,
    dimensions: &[String],
) -> (Vec<String>, Vec<String>) {
    let key: Vec<_> = dimensions
        .iter()
        .map(|name| bounded_value(&values[name]))
        .collect();
    let group_key = grouped_key(&key, repo_id, dimensions);
    (group_key, key)
}

/// The groupings that cut time into buckets. A run may use at most one of them
/// (the CLI enforces that), which is what lets an interval be split on "the"
/// calendar dimension.
fn is_calendar(dimension: &str) -> bool {
    matches!(dimension, "day" | "week" | "month")
}

fn keys_for_interval(
    interval: &Interval,
    dimensions: &[String],
) -> Vec<(Vec<String>, Vec<String>, Interval)> {
    let calendar = dimensions.iter().find(|name| is_calendar(name));
    let pieces = calendar.map_or_else(
        || vec![(String::new(), interval.clone())],
        |dimension| split_interval(interval, dimension),
    );
    pieces
        .into_iter()
        .map(|(calendar_key, piece)| {
            let values = interval_values(&piece, &calendar_key, dimensions);
            let (group_key, key) = dimension_keys(&values, &piece.repo_id, dimensions);
            (group_key, key, piece)
        })
        .collect()
}

/// The label for one of the attribution dimensions, which are all derived from
/// the branch and the checkout rather than from a timestamp.
fn attributed_value(dimension: &str, context: &Ctx<'_>) -> Option<String> {
    match dimension {
        "branch" => Some(attribution::branch_label(context.branch)),
        "issue" => Some(attribution::issue_label(context.branch)),
        "feature" => Some(attribution::feature_label(context.branch)),
        "engagement" => Some(attribution::engagement_label(context)),
        _ => None,
    }
}

/// Only the requested dimensions are computed: the attribution ones are not
/// free, and most runs group by one or two.
fn interval_values(
    interval: &Interval,
    calendar_key: &str,
    dimensions: &[String],
) -> HashMap<String, String> {
    let context = Ctx {
        repo_id: &interval.repo_id,
        cwd: &interval.cwd,
        branch: interval.branch.as_deref(),
    };
    dimensions
        .iter()
        .map(|name| {
            let value = match name.as_str() {
                "repo" => interval.repo.clone(),
                "root" => interval.root.clone(),
                "cwd" => interval.cwd.clone(),
                "provider" => interval.provider.clone(),
                "model" => interval.model.clone(),
                "day" | "week" | "month" => calendar_key.to_string(),
                other => attributed_value(other, &context).unwrap_or_default(),
            };
            (name.clone(), value)
        })
        .collect()
}

fn signal_values(signal: &HumanSignal, dimensions: &[String]) -> HashMap<String, String> {
    let context = Ctx {
        repo_id: &signal.repo_id,
        cwd: &signal.cwd,
        branch: signal.branch.as_deref(),
    };
    dimensions
        .iter()
        .map(|name| {
            let value = match name.as_str() {
                "repo" => signal.repo.clone(),
                "root" => signal.root.clone(),
                "cwd" => signal.cwd.clone(),
                "provider" => signal.provider.clone(),
                "model" => signal.model.clone(),
                "day" => local_date(signal.timestamp),
                "week" => local_week(signal.timestamp),
                "month" => local_month(signal.timestamp),
                other => attributed_value(other, &context).unwrap_or_default(),
            };
            (name.clone(), value)
        })
        .collect()
}

fn session_values(
    session: &Session,
    model: &str,
    first: DateTime<Utc>,
    dimensions: &[String],
) -> HashMap<String, String> {
    let context = Ctx {
        repo_id: &session.repo_id,
        cwd: &session.cwd,
        branch: session.branch_at(first),
    };
    dimensions
        .iter()
        .map(|name| {
            let value = match name.as_str() {
                "repo" => session.repo.clone(),
                "root" => session.root.clone(),
                "cwd" => session.cwd.clone(),
                "provider" => session.provider.clone(),
                "model" => model.to_string(),
                "day" => local_date(first),
                "week" => local_week(first),
                "month" => local_month(first),
                other => attributed_value(other, &context).unwrap_or_default(),
            };
            (name.clone(), value)
        })
        .collect()
}

fn commit_value(commit: &GitCommit, dimension: &str) -> String {
    match dimension {
        "repo" => commit.repo.clone(),
        "root" => commit.root.clone(),
        "cwd" => commit.cwd.clone(),
        // Grouping by provider is asking "where did this come from?", and
        // folding an agent's commits into the row holding the developer's own
        // would answer it wrongly. Every other grouping still puts them on the
        // same repository, day or directory row, in their own columns.
        "provider" => if commit.authorship.is_agent_authored() {
            "git-agent"
        } else {
            "git"
        }
        .to_string(),
        // A commit records no model, whoever wrote it.
        "model" => "—".to_string(),
        "day" => local_date(commit.timestamp),
        "week" => local_week(commit.timestamp),
        "month" => local_month(commit.timestamp),
        other => attributed_value(
            other,
            &Ctx {
                repo_id: &commit.repo_id,
                cwd: &commit.cwd,
                branch: commit.branch.as_deref(),
            },
        )
        .unwrap_or_default(),
    }
}

struct TokenRecord {
    timestamp: DateTime<Utc>,
    repo: String,
    repo_id: String,
    root: String,
    cwd: String,
    provider: String,
    model: String,
    branch: Option<String>,
    usage: TokenUsage,
}

fn token_value(token: &TokenRecord, dimension: &str) -> String {
    match dimension {
        "repo" => token.repo.clone(),
        "root" => token.root.clone(),
        "cwd" => token.cwd.clone(),
        "provider" => token.provider.clone(),
        "model" => token.model.clone(),
        "day" => local_date(token.timestamp),
        "week" => local_week(token.timestamp),
        "month" => local_month(token.timestamp),
        other => attributed_value(
            other,
            &Ctx {
                repo_id: &token.repo_id,
                cwd: &token.cwd,
                branch: token.branch.as_deref(),
            },
        )
        .unwrap_or_default(),
    }
}

fn include_time(row: &mut Bucket, first: DateTime<Utc>, last: DateTime<Utc>) {
    row.first_seen = Some(row.first_seen.map_or(first, |value| value.min(first)));
    row.last_seen = Some(row.last_seen.map_or(last, |value| value.max(last)));
}

/// Longer than any real repository path, and short enough that a pathological
/// one cannot be stored once per bucket and written once per row.
const MAX_KEY_CHARACTERS: usize = 4096;

/// A grouping key is a repository path, a working directory, or a model name —
/// none of it text this tool chose. It is bounded here, and otherwise carried
/// exactly as it is on disk.
///
/// Character replacement used to happen here too, so that the table, JSON and
/// CSV all named a row the same way. That is the wrong place for it twice over.
/// A key is an *identifier*: `jq` and spreadsheets join on it, and this tool's
/// own explorer joins on it in `tui::state::join_report_seconds`, so a
/// substituted character silently breaks the match against the checkout it
/// names. And because the bucket is keyed by this value, two repositories
/// differing only in a replaced character collapsed into one row and
/// under-counted both.
///
/// Safety belongs with the destination that needs it, so that is where it now
/// lives: `output::safe_text` replaces control characters and direction
/// overrides on the way to the table and the CSV — both of which a terminal
/// draws — and the explorer does the same in `tui::views`. JSON is left
/// faithful, because RFC 8259 escaping already renders a control character
/// inert there and a consumer parsing it needs the name the checkout has.
fn bounded_value(value: &str) -> String {
    value.chars().take(MAX_KEY_CHARACTERS).collect()
}

/// Three decimals, and never a negative zero. `round_ties_even` keeps the sign
/// of what it was given, so a total that reaches zero from below — a negative
/// zero, or a tiny negative float left by summing — rounds to `-0.0`, which
/// `serde_json` writes as `-0.0` and a reader takes for a bug. Every derived
/// seconds field in the report is rounded here, so normalising the sign once
/// covers all of them. The comparison is `== 0.0` because that is the one test
/// IEEE 754 answers `true` for both zeroes.
fn round3(value: f64) -> f64 {
    let rounded = (value * 1000.0).round_ties_even() / 1000.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

fn duration_seconds(value: Duration) -> f64 {
    value.num_microseconds().unwrap_or(0) as f64 / 1_000_000.0
}

fn iso(value: DateTime<Utc>) -> String {
    let precision = if value.timestamp_subsec_micros() == 0 {
        chrono::SecondsFormat::Secs
    } else {
        chrono::SecondsFormat::Micros
    };
    value.to_rfc3339_opts(precision, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;
    use crate::model::{ActivityPoint, Authorship, ExactInterval, TokenEvent};
    use crate::timeutil::parse_timestamp;

    fn commit(sha: &str, repo: &str, at: &str, files: &[(&str, u64, u64)]) -> GitCommit {
        let mut categories = CategoryTally::default();
        let mut additions = 0;
        let mut deletions = 0;
        for (path, added, removed) in files {
            categories.add(classify(path), *added, *removed);
            additions += added;
            deletions += removed;
        }
        GitCommit {
            sha: sha.into(),
            timestamp: parse_timestamp(at).unwrap(),
            repo: repo.into(),
            repo_id: repo.into(),
            repo_member_id: repo.into(),
            cwd: format!("/{repo}"),
            root: "root".into(),
            additions,
            deletions,
            files: files.iter().map(|(path, ..)| (*path).to_string()).collect(),
            ignored_additions: 0,
            ignored_deletions: 0,
            categories,
            authorship: Authorship::default(),
            branch: None,
            branch_source: crate::model::BranchSource::None,
        }
    }

    fn agent_commit(sha: &str, repo: &str, at: &str, files: &[(&str, u64, u64)]) -> GitCommit {
        GitCommit {
            authorship: Authorship::agent(),
            ..commit(sha, repo, at, files)
        }
    }

    fn session(
        id: &str,
        repo: &str,
        points: Vec<ActivityPoint>,
        human: Vec<ActivityPoint>,
    ) -> Session {
        Session {
            provider: "codex".into(),
            session_id: id.into(),
            cwd: format!("/{repo}"),
            repo: repo.into(),
            repo_id: repo.into(),
            root: "root".into(),
            points,
            exact_intervals: vec![],
            human_points: human,
            token_events: vec![],
            is_subagent: false,
            branch_source: crate::model::BranchSource::None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
            source_file: std::path::PathBuf::new(),
        }
    }

    fn point(value: &str) -> ActivityPoint {
        ActivityPoint {
            timestamp: parse_timestamp(value).unwrap(),
            model: "gpt".into(),
        }
    }

    #[test]
    fn human_time_is_one_global_timeline() {
        let sessions = vec![
            session(
                "a",
                "a",
                vec![],
                vec![point("2026-01-01T10:00:00Z"), point("2026-01-01T10:10:00Z")],
            ),
            session(
                "b",
                "b",
                vec![],
                vec![point("2026-01-01T10:05:00Z"), point("2026-01-01T10:15:00Z")],
            ),
        ];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(1200.0, report.summary.human_estimated_seconds);
        assert_eq!(
            report.summary.human_estimated_seconds,
            report
                .rows
                .iter()
                .map(|row| row.human_estimated_seconds)
                .sum::<f64>()
        );
    }

    #[test]
    fn explained_human_total_and_summary_share_the_same_rounding_boundary() {
        let sessions = vec![session(
            "fractional",
            "repo",
            vec![],
            vec![
                point("2026-01-01T10:00:00Z"),
                point("2026-01-01T10:00:00.062500Z"),
            ],
        )];
        let report = build_report_with_human_time_explanation(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::seconds(1),
            Duration::zero(),
            true,
            false,
        );
        let explanation = report.human_time_explanation.unwrap();
        assert_eq!(0.062, report.summary.human_estimated_seconds);
        assert_eq!(
            report.summary.human_estimated_seconds,
            explanation.total_seconds
        );
        assert_eq!(0.0625, explanation.unrounded_block_seconds_total);
        assert_eq!(
            explanation.total_seconds,
            explanation.unrounded_block_seconds_total
                + explanation.total_rounding_adjustment_seconds
        );
    }

    fn branched_session() -> Session {
        let mut value = session(
            "branched",
            "repo",
            vec![],
            vec![
                point("2026-01-01T10:00:00Z"),
                point("2026-01-01T10:10:00Z"),
                point("2026-01-01T10:25:00Z"),
                point("2026-01-01T10:30:00Z"),
            ],
        );
        value.branches = vec![
            crate::model::BranchMark {
                from: None,
                branch: "feat/a".into(),
            },
            crate::model::BranchMark {
                from: Some(parse_timestamp("2026-01-01T10:20:00Z").unwrap()),
                branch: "feat/b".into(),
            },
        ];
        value
    }

    fn built_with_dimensions(dimensions: &[&str], daily: bool) -> BuiltReport {
        let dimensions: Vec<String> = dimensions.iter().map(|name| (*name).to_string()).collect();
        build_report_with_human_time_explanation(
            &[branched_session()],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &dimensions,
            Duration::hours(1),
            Duration::minutes(10),
            false,
            daily,
        )
    }

    #[test]
    fn human_time_is_split_by_the_branch_in_force_at_each_signal() {
        let report = built_with_dimensions(&["branch"], false);
        let by_branch: BTreeMap<_, _> = report
            .rows
            .iter()
            .map(|row| (row.key["branch"].clone(), row.human_estimated_seconds))
            .collect();
        assert_eq!(
            vec!["feat/a", "feat/b"],
            by_branch.keys().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            report.summary.human_estimated_seconds,
            by_branch.values().sum::<f64>(),
            "branches partition the human timeline, they never add to it"
        );
        assert!(report.timeline.human_intervals.iter().all(|piece| {
            piece
                .branch
                .as_deref()
                .is_some_and(|branch| branch.starts_with("feat/"))
        }));
    }

    #[test]
    fn the_attribution_dimensions_fall_back_to_their_placeholders() {
        let report = built_with_dimensions(&["issue", "feature", "engagement"], false);
        assert!(!report.rows.is_empty());
        for row in &report.rows {
            assert_eq!("—", row.key["issue"]);
            assert_eq!("(unassigned)", row.key["engagement"]);
        }
        let features: BTreeSet<_> = report
            .rows
            .iter()
            .map(|row| row.key["feature"].as_str())
            .collect();
        // No issue is named, so a feature is the slug: the default rules strip
        // the `feat/` prefix.
        assert_eq!(BTreeSet::from(["a", "b"]), features);
    }

    #[test]
    fn the_timeline_is_the_vectors_the_report_was_built_from() {
        let report = built_with_dimensions(&["repo"], false);
        let piece_seconds: f64 = report
            .timeline
            .human_intervals
            .iter()
            .map(Interval::seconds)
            .sum();
        assert!((piece_seconds - report.summary.human_estimated_seconds).abs() < 0.001);
        // Four prompts and the two session edges.
        assert_eq!(6, report.timeline.human_signals.len());
        assert!(report.daily.is_none(), "daily is computed only on request");
    }

    #[test]
    fn daily_figures_account_for_the_whole_human_timeline_when_asked() {
        let report = built_with_dimensions(&["repo"], true);
        let daily = report.daily.expect("requested");
        assert!(!daily.is_empty());
        let human: f64 = daily.iter().map(|day| day.human_seconds).sum();
        assert!((human - report.summary.human_estimated_seconds).abs() < 0.01);
        assert_eq!(4, daily.iter().map(|day| day.prompts).sum::<usize>());
    }

    #[test]
    fn foreground_session_edges_add_bounded_human_involvement() {
        let sessions = vec![session(
            "supervised",
            "repo",
            vec![point("2026-01-01T10:00:00Z"), point("2026-01-01T10:30:00Z")],
            vec![],
        )];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(3600.0, report.summary.human_estimated_seconds);
        assert_eq!(2, report.summary.foreground_session_edge_signal_count);
        assert_eq!(0, report.summary.prompt_signal_count);
    }

    #[test]
    fn dense_autonomous_activity_does_not_bridge_unattended_time() {
        let sessions = vec![session(
            "autonomous",
            "repo",
            vec![
                point("2026-01-01T10:00:00Z"),
                point("2026-01-01T10:30:00Z"),
                point("2026-01-01T11:00:00Z"),
                point("2026-01-01T11:30:00Z"),
                point("2026-01-01T12:00:00Z"),
            ],
            vec![],
        )];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(3600.0, report.summary.human_estimated_seconds);
        assert_eq!(2, report.summary.foreground_session_edge_signal_count);
        assert_eq!(2, report.summary.work_block_count);
    }

    #[test]
    fn exact_foreground_edges_count_but_subagents_do_not() {
        let mut foreground = session("foreground", "repo", vec![], vec![]);
        foreground.exact_intervals.push(ExactInterval {
            start: parse_timestamp("2026-01-01T10:00:00Z").unwrap(),
            end: parse_timestamp("2026-01-01T10:30:00Z").unwrap(),
            model: "gpt".into(),
        });
        let mut subagent = session(
            "subagent",
            "repo",
            vec![point("2026-01-01T12:00:00Z"), point("2026-01-01T13:00:00Z")],
            vec![],
        );
        subagent.is_subagent = true;
        let report = build_report(
            &[foreground, subagent],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(3600.0, report.summary.human_estimated_seconds);
        assert_eq!(2, report.summary.foreground_session_edge_signal_count);
    }

    #[test]
    fn long_unattended_silence_is_not_human_time() {
        let sessions = vec![session(
            "foreground",
            "repo",
            vec![point("2026-01-01T10:00:00Z"), point("2026-01-01T12:00:00Z")],
            vec![],
        )];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(3600.0, report.summary.human_estimated_seconds);
        assert_eq!(2, report.summary.work_block_count);
    }

    #[test]
    fn single_timestamp_counts_as_a_zero_time_session() {
        let sessions = vec![session(
            "single",
            "repo",
            vec![point("2026-01-01T12:00:00Z")],
            vec![],
        )];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["model".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(1, report.summary.session_count);
        assert_eq!(0.0, report.summary.deduplicated_active_seconds);
        assert_eq!(Some(&"gpt".to_string()), report.rows[0].key.get("model"));
    }

    #[test]
    fn calendar_rows_are_sorted_newest_first() {
        let sessions = vec![
            session("april", "a", vec![], vec![point("2026-04-10T10:00:00Z")]),
            session("may", "b", vec![], vec![point("2026-05-10T10:00:00Z")]),
        ];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["month".into(), "repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(
            Some(&"2026-05".to_string()),
            report.rows[0].key.get("month")
        );
    }

    /// 2025-12-27 is in the last week of 2025 and 2026-01-01 in the first of
    /// 2026; both are a whole day clear of a Monday boundary, so the answer does
    /// not depend on the timezone the suite runs in.
    #[test]
    fn week_rows_cross_a_year_boundary_and_are_sorted_newest_first() {
        let sessions = vec![
            session("old", "a", vec![], vec![point("2025-12-27T12:00:00Z")]),
            session("new", "b", vec![], vec![point("2026-01-01T12:00:00Z")]),
        ];
        let report = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["week".into(), "repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        let weeks: Vec<_> = report
            .rows
            .iter()
            .map(|row| row.key["week"].as_str())
            .collect();
        assert_eq!(vec!["2026-W01", "2025-W52"], weeks);
    }

    #[test]
    fn git_output_is_split_into_file_areas_and_change_shapes() {
        let commits = vec![
            commit(
                "a",
                "repo",
                "2026-01-01T10:00:00Z",
                &[("src/lib.rs", 200, 4)],
            ),
            commit(
                "b",
                "repo",
                "2026-01-02T10:00:00Z",
                &[("tests/lib_test.rs", 120, 0)],
            ),
            commit(
                "c",
                "repo",
                "2026-01-03T10:00:00Z",
                &[("README.md", 30, 5), ("src/lib.rs", 2, 1)],
            ),
        ];
        let report = build_report(
            &[],
            &commits,
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );

        let area = |name: &str| {
            report
                .summary
                .composition
                .iter()
                .find(|entry| entry.category == name)
                .cloned()
                .unwrap_or_else(|| panic!("missing {name} composition"))
        };
        let source = area("source");
        assert_eq!(202, source.additions);
        assert_eq!(5, source.deletions);
        // src/lib.rs is touched by two commits but counts as one changed file.
        assert_eq!(1, source.files);
        assert_eq!(120, area("test").additions);
        assert_eq!(30, area("docs").additions);
        let shares: f64 = report
            .summary
            .composition
            .iter()
            .map(|entry| entry.share_of_changed_lines)
            .sum();
        assert!((shares - 1.0).abs() < 0.01, "shares summed to {shares}");

        let shapes: Vec<_> = report
            .summary
            .change_shapes
            .iter()
            .map(|entry| (entry.shape.as_str(), entry.commits))
            .collect();
        assert!(shapes.contains(&("new code", 1)), "{shapes:?}");
        assert!(shapes.contains(&("tests", 1)), "{shapes:?}");
        assert!(shapes.contains(&("docs", 1)), "{shapes:?}");

        // A single-repo run puts the whole breakdown on the one row too.
        assert_eq!(
            report.summary.composition.len(),
            report.rows[0].composition.len()
        );
        assert_eq!(202, report.rows[0].composition[0].additions);
    }

    /// The number this whole tool exists to protect. A repository holding
    /// nothing but a coding agent's commits describes a developer who did not
    /// touch it, and every one of those commits would otherwise cluster into a
    /// work block carrying setup and review credit. Real machines hold
    /// thousands of them, so the failure is measured in weeks, not minutes.
    #[test]
    fn agent_authored_commits_are_output_and_not_one_second_of_human_time() {
        let commits: Vec<_> = (0..5)
            .map(|day| {
                agent_commit(
                    &format!("bot{day}"),
                    "repo",
                    &format!("2026-01-0{}T10:00:00Z", day + 1),
                    &[("src/lib.rs", 100, 20)],
                )
            })
            .collect();
        let report = build_report(
            &[],
            &[],
            &commits,
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(0.0, report.summary.human_estimated_seconds);
        assert_eq!(0, report.summary.work_block_count);
        assert_eq!(0, report.summary.human_signal_count);
        assert_eq!(0, report.summary.commit_signal_count);
        assert_eq!(0, report.summary.human_active_days);

        // It is still output, and still visible.
        assert_eq!(0, report.summary.commit_count, "not the developer's own");
        assert_eq!(0, report.summary.additions);
        assert_eq!(5, report.summary.agent_commit_count);
        assert_eq!(500, report.summary.agent_additions);
        assert_eq!(100, report.summary.agent_deletions);
        assert_eq!(5, report.summary.active_days);
        assert_eq!(5, report.rows[0].agent_commit_count);
        assert_eq!(0.0, report.rows[0].human_estimated_seconds);

        // Which side a commit lands on is the commit's own answer, so passing
        // the same history through the other parameter changes nothing.
        let swapped = build_report(
            &[],
            &commits,
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(0.0, swapped.summary.human_estimated_seconds);
        assert_eq!(5, swapped.summary.agent_commit_count);
    }

    /// A commit the developer wrote with an agent is already counted once. The
    /// trailer says how it was written, so it must change the description of
    /// that commit and nothing else — same count, same signal, same estimate.
    #[test]
    fn a_co_authored_commit_is_described_differently_and_counted_the_same() {
        let plain = commit(
            "a",
            "repo",
            "2026-01-01T10:00:00Z",
            &[("src/lib.rs", 10, 2)],
        );
        let mut assisted = plain.clone();
        assisted
            .authorship
            .note_co_author("Copilot <223556219+Copilot@users.noreply.github.com>");
        assisted
            .authorship
            .note_co_author("Copilot Autofix powered by AI <62310815+github-advanced-security[bot]@users.noreply.github.com>");

        let before = build_report(
            &[],
            std::slice::from_ref(&plain),
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        let after = build_report(
            &[],
            std::slice::from_ref(&assisted),
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );

        assert_eq!(1, after.summary.commit_count, "one commit, not two");
        assert_eq!(1, after.summary.commit_signal_count);
        assert_eq!(0, after.summary.agent_commit_count);
        assert_eq!(
            before.summary.human_estimated_seconds, after.summary.human_estimated_seconds,
            "a trailer is not extra work"
        );
        assert_eq!(before.summary.additions, after.summary.additions);
        assert_eq!(1, after.summary.ai_assisted_commit_count);
        assert_eq!(1, after.summary.autofix_assisted_commit_count);
        assert_eq!(1, after.rows[0].ai_assisted_commit_count);
        assert_eq!(0, before.summary.ai_assisted_commit_count);
    }

    #[test]
    fn foreground_sessions_pair_with_commits_only_in_repos_git_actually_scanned() {
        let near = session("near", "repo", vec![], vec![point("2026-01-01T09:30:00Z")]);
        let far = session("far", "repo", vec![], vec![point("2026-06-01T09:30:00Z")]);
        let unscanned = session(
            "unscanned",
            "other-repo",
            vec![],
            vec![point("2026-01-01T09:30:00Z")],
        );
        let commits = vec![commit(
            "a",
            "repo",
            "2026-01-01T10:00:00Z",
            &[("src/lib.rs", 10, 0)],
        )];
        let report = build_report(
            &[near, far, unscanned],
            &commits,
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::hours(1),
            Duration::minutes(30),
        );
        assert_eq!(3, report.summary.foreground_session_count);
        assert_eq!(1, report.summary.foreground_sessions_with_commits);
        // The session in the unscanned repo is left out of both sides.
        assert_eq!(1, report.summary.foreground_sessions_without_commits);
    }

    #[test]
    fn token_usage_is_grouped_by_repo_and_totaled_in_the_summary() {
        let mut a = session("a", "repo-a", vec![point("2026-01-01T10:00:00Z")], vec![]);
        a.token_events.push(TokenEvent {
            timestamp: parse_timestamp("2026-01-01T10:00:00Z").unwrap(),
            model: "gpt".into(),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 5,
                cache_creation_tokens: 1,
            },
        });
        let mut b = session("b", "repo-b", vec![point("2026-01-01T11:00:00Z")], vec![]);
        b.token_events.push(TokenEvent {
            timestamp: parse_timestamp("2026-01-01T11:00:00Z").unwrap(),
            model: "gpt".into(),
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        });
        let report = build_report(
            &[a, b],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(186, report.summary.total_tokens);
        assert_eq!(186, *report.summary.provider_tokens.get("codex").unwrap());
        let repo_a = report
            .rows
            .iter()
            .find(|row| row.key.get("repo") == Some(&"repo-a".to_string()))
            .unwrap();
        assert_eq!(126, repo_a.total_tokens);
        let repo_b = report
            .rows
            .iter()
            .find(|row| row.key.get("repo") == Some(&"repo-b".to_string()))
            .unwrap();
        assert_eq!(60, repo_b.total_tokens);
    }

    #[test]
    fn attribution_repositories_come_from_evidence_inside_the_window() {
        let mut sparse = session(
            "sparse",
            "sparse-repo",
            vec![point("2026-01-01T09:00:00Z"), point("2026-01-01T11:00:00Z")],
            vec![],
        );
        sparse.exact_intervals.push(ExactInterval {
            start: parse_timestamp("2026-01-01T09:30:00Z").unwrap(),
            end: parse_timestamp("2026-01-01T10:00:00Z").unwrap(),
            model: "model".into(),
        });
        let report = build_report(
            &[sparse],
            &[],
            &[],
            Duration::minutes(5),
            Some(parse_timestamp("2026-01-01T10:00:00Z").unwrap()),
            Some(parse_timestamp("2026-01-01T10:30:00Z").unwrap()),
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert!(report.rows.is_empty());
        assert!(report.active_repository_checkouts.is_empty());

        let mut token_only = session(
            "token",
            "token-repo",
            vec![point("2026-01-01T09:00:00Z")],
            vec![],
        );
        token_only.token_events.push(TokenEvent {
            timestamp: parse_timestamp("2026-01-01T10:15:00Z").unwrap(),
            model: "model".into(),
            usage: TokenUsage {
                output_tokens: 1,
                ..TokenUsage::default()
            },
        });
        let report = build_report(
            &[token_only],
            &[],
            &[],
            Duration::minutes(5),
            Some(parse_timestamp("2026-01-01T10:00:00Z").unwrap()),
            Some(parse_timestamp("2026-01-01T10:30:00Z").unwrap()),
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(
            HashSet::from([("token-repo".to_string(), "/token-repo".to_string())]),
            report.active_repository_checkouts
        );
    }

    #[test]
    fn logical_repository_identity_combines_checkout_rows() {
        let mut primary = session(
            "primary",
            "product-primary",
            vec![point("2026-01-01T10:00:00Z")],
            vec![],
        );
        primary.repo_id = "remote:github.com/acme/product".into();
        let mut worktree = session(
            "worktree",
            "feature-checkout",
            vec![point("2026-01-01T11:00:00Z")],
            vec![],
        );
        worktree.repo_id = primary.repo_id.clone();

        let report = build_report(
            &[primary, worktree],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );

        assert_eq!(1, report.rows.len());
        assert_eq!(2, report.rows[0].session_count);
        assert_eq!(
            Some("remote:github.com/acme/product"),
            report.rows[0].repo_id.as_deref()
        );
    }

    #[test]
    fn project_alias_members_keep_identical_relative_files_distinct() {
        let mut api = commit(
            "aaaaaaaaaaaa",
            "Product",
            "2026-01-01T10:00:00Z",
            &[("README.md", 2, 0)],
        );
        api.repo_id = "project:product".into();
        api.repo_member_id = "remote:host/acme/api".into();
        let mut web = commit(
            "aaaaaaaaaaaa",
            "Product",
            "2026-01-01T11:00:00Z",
            &[("README.md", 3, 0)],
        );
        web.repo_id = api.repo_id.clone();
        web.repo_member_id = "remote:host/acme/web".into();

        let report = build_report(
            &[],
            &[api, web],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );

        assert_eq!(1, report.rows.len());
        assert_eq!(2, report.rows[0].commit_count);
        assert_eq!(2, report.rows[0].file_count);
        assert_eq!(2, report.summary.composition[0].files);
    }

    /// A key is an identifier a consumer joins on, so it is bounded and
    /// otherwise left alone. What makes a crafted name safe to *look* at is the
    /// renderer that draws it — `output::safe_text` for the table and the CSV,
    /// `tui::views::safe_chars` for the explorer — and neither is reached by
    /// this value on its way into JSON.
    #[test]
    fn a_grouping_key_is_the_name_the_checkout_has() {
        // U+202E would print the rest of a *cell* right-to-left, so a checkout
        // called `gnp.exe` could name a row that reads `exe.png`. The cell is
        // where that is dealt with; the key still says which checkout it is.
        assert_eq!("repo\u{202e}name", bounded_value("repo\u{202e}name"));
        assert_eq!("a\u{1b}b", bounded_value("a\u{1b}b"));
        assert_eq!("~/src/prosjekt-æøå", bounded_value("~/src/prosjekt-æøå"));
        // The one thing that is not carried through: a key long enough to be
        // pathological, which is stored once per bucket and written once per
        // row.
        assert_eq!(
            MAX_KEY_CHARACTERS,
            bounded_value(&"x".repeat(MAX_KEY_CHARACTERS + 904))
                .chars()
                .count()
        );
    }

    /// Two checkouts that differ only in a character the report used to replace
    /// are two checkouts. Replacing before bucketing merged them, so one row
    /// claimed both repositories' work and the other repository vanished from
    /// the report.
    #[test]
    fn two_repositories_that_differ_by_one_hidden_character_stay_two_rows() {
        let report = build_report(
            &[
                session(
                    "a",
                    "repo\u{202e}name",
                    vec![point("2026-01-01T10:00:00Z")],
                    vec![],
                ),
                session(
                    "b",
                    "repo\u{202d}name",
                    vec![point("2026-01-01T11:00:00Z")],
                    vec![],
                ),
            ],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        assert_eq!(2, report.rows.len());
        // And each row is named by the directory it is actually in, so the
        // explorer's own join in `tui::state::join_report_seconds` — and any
        // `jq` join a reader writes — still matches the checkout.
        let names: HashSet<&String> = report
            .rows
            .iter()
            .filter_map(|row| row.key.get("repo"))
            .collect();
        assert!(names.contains(&"repo\u{202e}name".to_string()), "{names:?}");
        assert!(names.contains(&"repo\u{202d}name".to_string()), "{names:?}");
    }

    #[test]
    fn a_right_to_left_checkout_keeps_the_name_it_has_on_disk() {
        // U+200F is a directional mark, not a scope, and it is ordinary content
        // in a Hebrew directory name. Escaped rather than written literally so
        // that this file carries no invisible characters of its own.
        let hebrew = "~/\u{5de}\u{5e1}\u{5de}\u{5db}\u{5d9}\u{5dd}\u{200f}/workstats";
        assert_eq!(hebrew, bounded_value(hebrew));
        assert_eq!("report\u{200e}-2026", bounded_value("report\u{200e}-2026"));
    }

    /// `-0.0` is a fine `f64` and a terrible report field: `serde_json` writes
    /// it as `-0.0`, and a history with no human time in it reads as though
    /// something went negative.
    #[test]
    fn a_zero_total_is_never_reported_as_a_negative_zero() {
        assert!(round3(-0.0).is_sign_positive());
        // What summing produces in practice: a residue too small to round to
        // anything, on the wrong side of zero.
        assert!(round3(-0.000_000_1).is_sign_positive());
        assert_eq!("0.0", serde_json::to_string(&round3(-0.0)).unwrap());
        // Rounding itself is unchanged, ties included, and a value that really
        // is negative keeps its sign.
        assert_eq!(0.062, round3(0.0625));
        assert_eq!(-0.062, round3(-0.0625));
        assert_eq!(-1.5, round3(-1.5));

        // The fields a reader parses, end to end: a report with no human
        // involvement at all still says `0.0`.
        let report = build_report(
            &[session(
                "a",
                "repo",
                vec![point("2026-01-01T10:00:00Z")],
                vec![],
            )],
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        for value in [
            report.summary.human_estimated_seconds,
            report.summary.average_human_seconds_per_active_day,
            report.summary.parallel_agent_seconds,
            report.rows[0].human_estimated_seconds,
        ] {
            assert!(value.is_sign_positive(), "{value} carries a negative zero");
        }
    }
}
