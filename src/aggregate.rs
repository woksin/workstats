use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::classify::{CategoryTally, ShapeTally, active_registry, change_shape};
use crate::model::{
    CompositionEntry, GitCommit, HumanSignal, Interval, Methodology, Observed, ReportRow, Session,
    ShapeEntry, Summary, TokenUsage,
};
use crate::output::is_direction_override;
use crate::timeutil::{
    build_human_intervals, build_session_intervals, calendar_days, clip_interval, local_date,
    local_month, split_interval, union_seconds,
};

pub const DIMENSIONS: &[&str] = &["repo", "root", "cwd", "provider", "model", "day", "month"];

type SessionKey = (String, String);
type SignalKey = (DateTime<Utc>, String, String);

#[derive(Default)]
struct Bucket {
    key: BTreeMap<String, String>,
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
    commits: HashSet<String>,
    files: HashSet<String>,
    additions: u64,
    deletions: u64,
    ignored_additions: u64,
    ignored_deletions: u64,
    categories: CategoryTally,
    shapes: ShapeTally,
    tokens: TokenUsage,
    first_seen: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
    active_days: HashSet<String>,
}

pub struct BuiltReport {
    pub methodology: Methodology,
    pub observed: Observed,
    pub summary: Summary,
    pub group_by: Vec<String>,
    pub rows: Vec<ReportRow>,
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
                root: session.root.clone(),
                kind: kind.to_string(),
                model: model.to_string(),
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

#[allow(clippy::too_many_arguments)]
pub fn build_report(
    sessions: &[Session],
    commits: &[GitCommit],
    gap_cap: Duration,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    dimensions: &[String],
    human_idle: Duration,
    review_credit: Duration,
) -> BuiltReport {
    let intervals: Vec<_> = sessions
        .iter()
        .flat_map(|session| build_session_intervals(session, gap_cap))
        .filter_map(|interval| clip_interval(&interval, since, until))
        .collect();
    let filtered_commits: Vec<_> = commits
        .iter()
        .filter(|commit| {
            since.is_none_or(|bound| commit.timestamp >= bound)
                && until.is_none_or(|bound| commit.timestamp < bound)
        })
        .collect();
    let filtered_tokens: Vec<_> = sessions
        .iter()
        .flat_map(|session| {
            session.token_events.iter().map(move |event| TokenRecord {
                timestamp: event.timestamp,
                repo: session.repo.clone(),
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
    human_signals.extend(filtered_commits.iter().map(|commit| HumanSignal {
        timestamp: commit.timestamp,
        provider: "git".to_string(),
        session_id: commit.sha.clone(),
        cwd: commit.cwd.clone(),
        repo: commit.repo.clone(),
        root: commit.root.clone(),
        kind: "commit".to_string(),
        model: "—".to_string(),
    }));
    let filtered_human_signals: Vec<_> = human_signals
        .into_iter()
        .filter(|signal| {
            since.is_none_or(|bound| signal.timestamp >= bound)
                && until.is_none_or(|bound| signal.timestamp < bound)
        })
        .collect();
    let human_intervals: Vec<_> =
        build_human_intervals(&filtered_human_signals, human_idle, review_credit)
            .into_iter()
            .filter_map(|interval| clip_interval(&interval, since, until))
            .collect();
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
        for (key, piece) in keys_for_interval(interval, dimensions) {
            let row = bucket(&mut buckets, key, dimensions);
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
        for (key, piece) in keys_for_interval(interval, dimensions) {
            let row = bucket(&mut buckets, key, dimensions);
            row.human_seconds += piece.seconds();
            row.human_blocks.insert(piece.session_id.clone());
            include_time(row, piece.start, piece.end);
        }
    }

    for signal in &filtered_human_signals {
        let values = signal_values(signal);
        let key = dimensions
            .iter()
            .map(|name| safe_value(&values[name]))
            .collect();
        let row = bucket(&mut buckets, key, dimensions);
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
        let values = session_values(session, &model, first);
        let key = dimensions
            .iter()
            .map(|name| safe_value(&values[name]))
            .collect();
        let row = bucket(&mut buckets, key, dimensions);
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
        single_point_dates.insert(day);
        include_time(row, first, first);
    }

    for commit in &filtered_commits {
        let key = dimensions
            .iter()
            .map(|dimension| safe_value(&commit_value(commit, dimension)))
            .collect();
        let row = bucket(&mut buckets, key, dimensions);
        row.commits.insert(commit.sha.clone());
        row.files.extend(commit.files.iter().cloned());
        row.additions += commit.additions;
        row.deletions += commit.deletions;
        row.ignored_additions += commit.ignored_additions;
        row.ignored_deletions += commit.ignored_deletions;
        row.categories.merge(&commit.categories);
        if let Some(shape) = change_shape(&commit.categories) {
            row.shapes.add(shape);
        }
        row.active_days.insert(local_date(commit.timestamp));
        include_time(row, commit.timestamp, commit.timestamp);
    }

    for token in &filtered_tokens {
        let key = dimensions
            .iter()
            .map(|dimension| safe_value(&token_value(token, dimension)))
            .collect();
        let row = bucket(&mut buckets, key, dimensions);
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
                composition: composition_entries(
                    row.files.iter().map(String::as_str),
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
    let calendar = dimensions
        .iter()
        .any(|name| name == "day" || name == "month");
    rows.sort_by(|left, right| {
        let compare_number =
            |left: f64, right: f64| left.partial_cmp(&right).unwrap_or(Ordering::Equal);
        let ordering = if calendar {
            let left_calendar = left
                .key
                .get("month")
                .or_else(|| left.key.get("day"))
                .map(String::as_str)
                .unwrap_or("");
            let right_calendar = right
                .key
                .get("month")
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
    let human_seconds: f64 = human_intervals.iter().map(Interval::seconds).sum();
    let agent_seconds: f64 = intervals.iter().map(Interval::seconds).sum();
    let foreground_session_count = eligible_session_keys
        .iter()
        .filter(|key| !session_roles.get(*key).copied().unwrap_or(false))
        .count();
    let subagent_session_count = eligible_session_keys
        .iter()
        .filter(|key| session_roles.get(*key).copied().unwrap_or(false))
        .count();
    let unique_commits: HashSet<_> = filtered_commits.iter().map(|commit| &commit.sha).collect();
    let mut summary_categories = CategoryTally::default();
    let mut summary_shapes = ShapeTally::default();
    let mut summary_files: HashSet<&str> = HashSet::new();
    for commit in &filtered_commits {
        summary_categories.merge(&commit.categories);
        if let Some(shape) = change_shape(&commit.categories) {
            summary_shapes.add(shape);
        }
        summary_files.extend(commit.files.iter().map(String::as_str));
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

    BuiltReport {
        methodology: Methodology {
            human_work: "human prompts, foreground session boundaries, and authored commits clustered into non-overlapping involvement blocks",
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
            scope: "local retained histories, explicit event logs, and locally available Git repositories only",
        },
        observed: Observed {
            first_seen: all_times.iter().min().copied().map(iso),
            last_seen: all_times.iter().max().copied().map(iso),
        },
        summary: Summary {
            human_estimated_seconds: round3(human_seconds),
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
            composition: composition_entries(summary_files.iter().copied(), &summary_categories),
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
    }
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
fn foreground_session_output(
    sessions: &[Session],
    commits: &[&GitCommit],
    eligible: &HashSet<SessionKey>,
    roles: &HashMap<SessionKey, bool>,
    human_idle: Duration,
) -> (usize, usize) {
    let mut by_repo: HashMap<&str, Vec<DateTime<Utc>>> = HashMap::new();
    for commit in commits {
        by_repo
            .entry(commit.repo.as_str())
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
        let Some(times) = by_repo.get(session.repo.as_str()) else {
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

fn bucket<'a>(
    buckets: &'a mut HashMap<Vec<String>, Bucket>,
    key: Vec<String>,
    dimensions: &[String],
) -> &'a mut Bucket {
    buckets.entry(key.clone()).or_insert_with(|| Bucket {
        key: dimensions.iter().cloned().zip(key).collect(),
        ..Bucket::default()
    })
}

fn keys_for_interval(interval: &Interval, dimensions: &[String]) -> Vec<(Vec<String>, Interval)> {
    let calendar = dimensions
        .iter()
        .find(|name| name.as_str() == "day" || name.as_str() == "month");
    let pieces = calendar.map_or_else(
        || vec![(String::new(), interval.clone())],
        |dimension| split_interval(interval, dimension),
    );
    pieces
        .into_iter()
        .map(|(calendar_key, piece)| {
            let values = interval_values(&piece, &calendar_key);
            let key = dimensions
                .iter()
                .map(|name| safe_value(&values[name]))
                .collect();
            (key, piece)
        })
        .collect()
}

fn interval_values(interval: &Interval, calendar_key: &str) -> HashMap<String, String> {
    HashMap::from([
        ("repo".into(), interval.repo.clone()),
        ("root".into(), interval.root.clone()),
        ("cwd".into(), interval.cwd.clone()),
        ("provider".into(), interval.provider.clone()),
        ("model".into(), interval.model.clone()),
        ("day".into(), calendar_key.to_string()),
        ("month".into(), calendar_key.to_string()),
    ])
}

fn signal_values(signal: &HumanSignal) -> HashMap<String, String> {
    HashMap::from([
        ("repo".into(), signal.repo.clone()),
        ("root".into(), signal.root.clone()),
        ("cwd".into(), signal.cwd.clone()),
        ("provider".into(), signal.provider.clone()),
        ("model".into(), signal.model.clone()),
        ("day".into(), local_date(signal.timestamp)),
        ("month".into(), local_month(signal.timestamp)),
    ])
}

fn session_values(session: &Session, model: &str, first: DateTime<Utc>) -> HashMap<String, String> {
    HashMap::from([
        ("repo".into(), session.repo.clone()),
        ("root".into(), session.root.clone()),
        ("cwd".into(), session.cwd.clone()),
        ("provider".into(), session.provider.clone()),
        ("model".into(), model.to_string()),
        ("day".into(), local_date(first)),
        ("month".into(), local_month(first)),
    ])
}

fn commit_value(commit: &GitCommit, dimension: &str) -> String {
    match dimension {
        "repo" => commit.repo.clone(),
        "root" => commit.root.clone(),
        "cwd" => commit.cwd.clone(),
        "provider" => "git".to_string(),
        "model" => "—".to_string(),
        "day" => local_date(commit.timestamp),
        "month" => local_month(commit.timestamp),
        _ => String::new(),
    }
}

struct TokenRecord {
    timestamp: DateTime<Utc>,
    repo: String,
    root: String,
    cwd: String,
    provider: String,
    model: String,
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
        "month" => local_month(token.timestamp),
        _ => String::new(),
    }
}

fn include_time(row: &mut Bucket, first: DateTime<Utc>, last: DateTime<Utc>) {
    row.first_seen = Some(row.first_seen.map_or(first, |value| value.min(first)));
    row.last_seen = Some(row.last_seen.map_or(last, |value| value.max(last)));
}

/// A grouping key is a repository path, a working directory, or a model name —
/// none of it text this tool chose. The same value is printed as a table cell,
/// so it gets the treatment `safe_message` gives a warning: control characters
/// would hand an escape sequence to the terminal, and a direction override
/// would let a crafted path reorder the row around it. Both become the
/// replacement character, and the same substitution reaches JSON and CSV so
/// that every format names a row identically.
///
/// That reach is why `is_direction_override` stops short of LRM and RLM. They
/// open no directional scope, so they cannot spoof a row, but they are real
/// characters in Hebrew and Arabic paths — replacing them here would hand a
/// downstream consumer a repository name that no longer matches the checkout.
fn safe_value(value: &str) -> String {
    value
        .chars()
        .take(4096)
        .map(|character| {
            if character.is_control() || is_direction_override(character) {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round_ties_even() / 1000.0
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
    use crate::model::{ActivityPoint, ExactInterval, TokenEvent};
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
            cwd: format!("/{repo}"),
            root: "root".into(),
            additions,
            deletions,
            files: files.iter().map(|(path, ..)| (*path).to_string()).collect(),
            ignored_additions: 0,
            ignored_deletions: 0,
            categories,
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
            root: "root".into(),
            points,
            exact_intervals: vec![],
            human_points: human,
            token_events: vec![],
            is_subagent: false,
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
    fn a_grouping_key_cannot_repaint_or_reorder_the_row_it_names() {
        // U+202E would print the rest of the cell right-to-left, so a checkout
        // called `gnp.exe` could name a row that reads `exe.png`.
        assert_eq!("repo\u{fffd}name", safe_value("repo\u{202e}name"));
        assert_eq!(
            "a\u{fffd}b\u{fffd}c\u{fffd}",
            safe_value("a\u{2066}b\u{202c}c\u{202a}")
        );
        assert_eq!("a\u{fffd}b", safe_value("a\u{1b}b"));
        // Anything a terminal draws as itself is left alone, including the
        // scripts a direction override is otherwise legitimately used with.
        assert_eq!("~/src/prosjekt-æøå", safe_value("~/src/prosjekt-æøå"));
        assert_eq!(4096, safe_value(&"x".repeat(5000)).chars().count());
    }

    #[test]
    fn a_right_to_left_checkout_keeps_the_name_it_has_on_disk() {
        // U+200F is a directional mark, not a scope: it cannot reorder the row
        // it names, and it is ordinary content in a Hebrew directory name. A
        // key is what JSON and CSV consumers join on, so mangling one here
        // would leave them unable to match this row to the checkout. Escaped
        // rather than written literally so that this file carries no invisible
        // characters of its own.
        let hebrew = "~/\u{5de}\u{5e1}\u{5de}\u{5db}\u{5d9}\u{5dd}\u{200f}/workstats";
        assert_eq!(hebrew, safe_value(hebrew));
        // The mark U+200E and the override U+202D both nudge text leftward and
        // both are invisible, so the pair is asserted together: only the one
        // that opens a scope is replaced.
        assert_eq!("report\u{200e}-2026", safe_value("report\u{200e}-2026"));
        assert_eq!("report\u{fffd}-2026", safe_value("report\u{202d}-2026"));
    }

    #[test]
    fn a_crafted_repository_name_is_neutralised_in_every_format() {
        let report = build_report(
            &[session(
                "a",
                "repo\u{202e}name",
                vec![point("2026-01-01T10:00:00Z")],
                vec![],
            )],
            &[],
            Duration::minutes(5),
            None,
            None,
            &["repo".into()],
            Duration::minutes(15),
            Duration::minutes(5),
        );
        // The key is sanitised once, here, so the table, JSON and CSV all carry
        // the same name for the row rather than three spellings of it.
        assert_eq!(
            Some(&"repo\u{fffd}name".to_string()),
            report.rows[0].key.get("repo")
        );
    }
}
