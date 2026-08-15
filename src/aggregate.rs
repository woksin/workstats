use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::model::{
    GitCommit, HumanSignal, Interval, Methodology, Observed, ReportRow, Session, Summary,
};
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
        let activity_kind = format!("{}_activity", session.provider);
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
        for point in &session.points {
            push(point.timestamp, &point.model, &activity_kind);
        }
        for interval in &session.exact_intervals {
            push(interval.start, &interval.model, &activity_kind);
            push(interval.end, &interval.model, &activity_kind);
        }
        for point in &session.human_points {
            push(point.timestamp, &point.model, &prompt_kind);
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
        row.active_days.insert(local_date(commit.timestamp));
        include_time(row, commit.timestamp, commit.timestamp);
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
    let foreground_activity_signal_count = filtered_human_signals
        .iter()
        .filter(|signal| signal.kind.ends_with("_activity"))
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
            human_work: "foreground agent activity, human prompts, exact foreground interval edges, and authored commits clustered into non-overlapping involvement blocks",
            human_idle_threshold_seconds: duration_seconds(human_idle),
            review_credit_seconds: duration_seconds(review_credit),
            human_estimate_caveat: "a supervision-inclusive estimate that includes likely setup, review, planning, and babysitting time; not stopwatch or attendance data",
            ai_time: "consecutive structural activity signals capped at the idle gap; exact intervals are merged when a source records them",
            deduplication: "headline time is the union of all AI intervals; grouped AI totals may overlap across parallel repos/providers",
            gap_cap_seconds: duration_seconds(gap_cap),
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
            foreground_activity_signal_count,
            commit_signal_count,
            deduplicated_active_seconds: round3(union_seconds(&intervals)),
            attributed_active_seconds: round3(agent_seconds),
            agent_wall_seconds: round3(union_seconds(&intervals)),
            parallel_agent_seconds: round3(agent_seconds),
            session_count: eligible_session_keys.len(),
            foreground_session_count,
            subagent_session_count,
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
            active_days: active_dates.len(),
            provider_seconds,
            model_seconds,
        },
        group_by: dimensions.to_vec(),
        rows,
    }
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

fn include_time(row: &mut Bucket, first: DateTime<Utc>, last: DateTime<Utc>) {
    row.first_seen = Some(row.first_seen.map_or(first, |value| value.min(first)));
    row.last_seen = Some(row.last_seen.map_or(last, |value| value.max(last)));
}

fn safe_value(value: &str) -> String {
    value
        .chars()
        .take(4096)
        .map(|character| {
            if character.is_control() {
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
    use crate::model::{ActivityPoint, ExactInterval};
    use crate::timeutil::parse_timestamp;

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
    fn foreground_agent_activity_counts_as_human_involvement() {
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
        assert_eq!(2, report.summary.foreground_activity_signal_count);
        assert_eq!(0, report.summary.prompt_signal_count);
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
        assert_eq!(2, report.summary.foreground_activity_signal_count);
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
}
