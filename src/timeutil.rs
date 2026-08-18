use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashSet};

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, TimeZone, Utc};
use regex::Regex;

use crate::model::{ActivityPoint, HumanSignal, Interval, Session};

pub fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn parse_epoch_milliseconds(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    let micros = (value * 1000.0).round() as i64;
    DateTime::from_timestamp_micros(micros)
}

/// A gap cap, idle threshold, or review credit beyond a leap year is never a real
/// request, and unbounded values used to saturate at `i64::MAX` microseconds and
/// panic the first time one was added to a timestamp.
const MAX_DURATION_SECONDS: f64 = 366.0 * 24.0 * 3600.0;

pub fn parse_duration(value: &str) -> Result<Duration> {
    let expression = Regex::new(r"(?i)^(\d+(?:\.\d+)?)(s|m|h)$").expect("static regex");
    let Some(captures) = expression.captures(value.trim()) else {
        bail!("duration must look like 30s, 5m, or 1h");
    };
    let amount: f64 = captures[1].parse()?;
    if amount <= 0.0 {
        bail!("duration must be greater than zero");
    }
    let factor = match captures[2].to_ascii_lowercase().as_str() {
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => unreachable!(),
    };
    let seconds = amount * factor;
    // A digit string too large for f64 parses to infinity, which this rejects too.
    if seconds > MAX_DURATION_SECONDS {
        bail!("duration must be at most 8784h (366 days)");
    }
    Ok(Duration::microseconds(
        (seconds * 1_000_000.0).round() as i64
    ))
}

/// chrono panics when a timestamp plus a delta leaves the representable range.
/// Timestamps come from transcripts we do not control and deltas from flags, so
/// every offset in this module clamps instead of trusting both to stay in range.
fn saturating_add(value: DateTime<Utc>, delta: Duration) -> DateTime<Utc> {
    let bound = if delta < Duration::zero() {
        DateTime::<Utc>::MIN_UTC
    } else {
        DateTime::<Utc>::MAX_UTC
    };
    value.checked_add_signed(delta).unwrap_or(bound)
}

fn saturating_sub(value: DateTime<Utc>, delta: Duration) -> DateTime<Utc> {
    let bound = if delta < Duration::zero() {
        DateTime::<Utc>::MAX_UTC
    } else {
        DateTime::<Utc>::MIN_UTC
    };
    value.checked_sub_signed(delta).unwrap_or(bound)
}

pub fn parse_bound(value: Option<&str>, until: bool) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let month = Regex::new(r"^\d{4}-\d{2}$").expect("static regex");
    let day = Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("static regex");
    let date = if month.is_match(value) {
        let mut pieces = value.split('-');
        let year: i32 = pieces.next().unwrap().parse()?;
        let month: u32 = pieces.next().unwrap().parse()?;
        let start = NaiveDate::from_ymd_opt(year, month, 1)
            .ok_or_else(|| anyhow::anyhow!("date must be YYYY-MM or YYYY-MM-DD"))?;
        if until {
            let (year, month) = month_after(year, month);
            NaiveDate::from_ymd_opt(year, month, 1).unwrap()
        } else {
            start
        }
    } else if day.is_match(value) {
        let start = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("date must be YYYY-MM or YYYY-MM-DD"))?;
        if until {
            start.succ_opt().unwrap()
        } else {
            start
        }
    } else {
        bail!("date must be YYYY-MM or YYYY-MM-DD");
    };
    Ok(Some(local_midnight(date)))
}

/// Rolling December into the following January, and January back into the
/// preceding December, is the case every calendar boundary in this module gets
/// wrong first, so both live in one place instead of at each call site.
fn month_after(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn month_before(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn month_start(year: i32, month: u32) -> Option<DateTime<Utc>> {
    NaiveDate::from_ymd_opt(year, month, 1).map(local_midnight)
}

/// The window a calendar shorthand stands for, half-open as `[since, until)`:
/// `until` is the first instant *after* the span, which is what
/// `parse_bound(.., true)` produces and what the report's `>= since && < until`
/// filter expects. An inclusive end would silently drop the span's last day.
pub type CalendarSpan = (DateTime<Utc>, DateTime<Utc>);

/// One calendar month, or `None` when there is no such month. Both ends are
/// built by the same pair of helpers, so the December rollover cannot come out
/// right on one end and wrong on the other.
fn month_span_of(year: i32, month: u32) -> Option<CalendarSpan> {
    let (next_year, next_month) = month_after(year, month);
    let start = month_start(year, month)?;
    let end = month_start(next_year, next_month)?;
    Some((start, end))
}

/// The two relative spans `--month` and `--year` accept. They are what makes the
/// shorthand worth having: without them a recurring monthly report is a date the
/// user has to edit every month.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelativeSpan {
    Current,
    Previous,
}

fn relative_span(value: &str) -> Option<RelativeSpan> {
    match value.to_ascii_lowercase().as_str() {
        "current" | "this" => Some(RelativeSpan::Current),
        "last" | "previous" => Some(RelativeSpan::Previous),
        _ => None,
    }
}

/// The span `--month` is shorthand for. The reference instant is passed in
/// rather than read from the clock so `current` and `last` stay testable, and
/// it is read on the *local* calendar, the same one every bound here snaps to.
pub fn month_span(value: &str, reference: DateTime<Utc>) -> Result<CalendarSpan> {
    let value = value.trim();
    let (year, month): (i32, u32) = match relative_span(value) {
        Some(RelativeSpan::Current) => {
            let today = reference.with_timezone(&Local).date_naive();
            (today.year(), today.month())
        }
        Some(RelativeSpan::Previous) => {
            let today = reference.with_timezone(&Local).date_naive();
            month_before(today.year(), today.month())
        }
        None => {
            let expression = Regex::new(r"^(\d{4})-(\d{2})$").expect("static regex");
            let Some(captures) = expression.captures(value) else {
                bail!("month must be YYYY-MM, current (this), or last (previous)");
            };
            (captures[1].parse()?, captures[2].parse()?)
        }
    };
    let Some(span) = month_span_of(year, month) else {
        bail!("month must be YYYY-MM, current (this), or last (previous)");
    };
    Ok(span)
}

/// The span `--year` is shorthand for. It runs from January's start to
/// December's end, so the year rollover is the same one `month_span` uses.
pub fn year_span(value: &str, reference: DateTime<Utc>) -> Result<CalendarSpan> {
    let value = value.trim();
    let year: i32 = match relative_span(value) {
        Some(RelativeSpan::Current) => reference.with_timezone(&Local).year(),
        Some(RelativeSpan::Previous) => reference.with_timezone(&Local).year() - 1,
        None => {
            let expression = Regex::new(r"^\d{4}$").expect("static regex");
            if !expression.is_match(value) {
                bail!("year must be YYYY, current (this), or last (previous)");
            }
            value.parse()?
        }
    };
    let (Some(january), Some(december)) = (month_span_of(year, 1), month_span_of(year, 12)) else {
        bail!("year must be YYYY, current (this), or last (previous)");
    };
    Ok((january.0, december.1))
}

pub fn nearest_models(points: &[ActivityPoint]) -> Vec<ActivityPoint> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut ordered = points.to_vec();
    ordered.sort_by_key(|point| point.timestamp);
    let mut future = vec!["unknown".to_string(); ordered.len()];
    let mut next_known = "unknown".to_string();
    for index in (0..ordered.len()).rev() {
        if ordered[index].model != "unknown" {
            next_known.clone_from(&ordered[index].model);
        }
        future[index].clone_from(&next_known);
    }
    let mut current = "unknown".to_string();
    for (index, point) in ordered.iter_mut().enumerate() {
        if point.model != "unknown" {
            current.clone_from(&point.model);
        }
        if current != "unknown" {
            point.model.clone_from(&current);
        } else {
            point.model.clone_from(&future[index]);
        }
    }
    ordered
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeapEntry {
    known: bool,
    start_micros: i64,
    index: usize,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.known, self.start_micros, self.index).cmp(&(
            other.known,
            other.start_micros,
            other.index,
        ))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn build_session_intervals(session: &Session, gap_cap: Duration) -> Vec<Interval> {
    let points = nearest_models(&session.points);
    let mut ranges = Vec::with_capacity(points.len() + session.exact_intervals.len());
    for pair in points.windows(2) {
        let current = &pair[0];
        let following = &pair[1];
        if following.timestamp <= current.timestamp {
            continue;
        }
        ranges.push((
            current.timestamp,
            following
                .timestamp
                .min(saturating_add(current.timestamp, gap_cap)),
            current.model.clone(),
        ));
    }
    ranges.extend(
        session
            .exact_intervals
            .iter()
            .map(|item| (item.start, item.end, item.model.clone())),
    );

    let mut events: BTreeMap<DateTime<Utc>, Vec<(bool, usize)>> = BTreeMap::new();
    for (index, (start, end, _)) in ranges.iter().enumerate() {
        if end <= start {
            continue;
        }
        events.entry(*start).or_default().push((true, index));
        events.entry(*end).or_default().push((false, index));
    }
    let mut active: HashSet<usize> = HashSet::new();
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
    let mut result: Vec<Interval> = Vec::new();
    let mut previous = None;
    for (moment, changes) in events {
        while heap
            .peek()
            .is_some_and(|entry| !active.contains(&entry.index))
        {
            heap.pop();
        }
        if let (Some(start), Some(entry)) = (previous, heap.peek())
            && moment > start
        {
            let model = ranges[entry.index].2.clone();
            if let Some(prior) = result.last_mut() {
                if prior.end == start && prior.model == model {
                    prior.end = moment;
                } else {
                    result.push(interval_for_session(session, start, moment, model));
                }
            } else {
                result.push(interval_for_session(session, start, moment, model));
            }
        }
        for (starting, index) in &changes {
            if !starting {
                active.remove(index);
            }
        }
        for (starting, index) in changes {
            if starting {
                active.insert(index);
                let (start, _, model) = &ranges[index];
                heap.push(HeapEntry {
                    known: model != "unknown",
                    start_micros: start.timestamp_micros(),
                    index,
                });
            }
        }
        previous = Some(moment);
    }
    result
}

fn interval_for_session(
    session: &Session,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    model: String,
) -> Interval {
    Interval {
        start,
        end,
        provider: session.provider.clone(),
        model,
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        repo: session.repo.clone(),
        root: session.root.clone(),
    }
}

pub fn build_human_intervals(
    signals: &[HumanSignal],
    idle_threshold: Duration,
    block_credit: Duration,
) -> Vec<Interval> {
    if signals.is_empty() {
        return Vec::new();
    }
    let priority = |kind: &str| {
        if kind.ends_with("_prompt") {
            3
        } else if kind == "commit" {
            2
        } else {
            1
        }
    };
    let mut by_timestamp: BTreeMap<DateTime<Utc>, &HumanSignal> = BTreeMap::new();
    for signal in signals {
        match by_timestamp.get(&signal.timestamp) {
            Some(existing) if priority(&existing.kind) >= priority(&signal.kind) => {}
            _ => {
                by_timestamp.insert(signal.timestamp, signal);
            }
        }
    }
    let ordered: Vec<&HumanSignal> = by_timestamp.into_values().collect();
    let mut blocks: Vec<Vec<&HumanSignal>> = Vec::new();
    let mut current = Vec::new();
    for signal in ordered {
        if current.last().is_some_and(|previous: &&HumanSignal| {
            signal.timestamp - previous.timestamp > idle_threshold
        }) {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(signal);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let edge = block_credit / 2;
    let mut intervals = Vec::new();
    for (block_index, block) in blocks.into_iter().enumerate() {
        let first_day = local_midnight(block[0].timestamp.with_timezone(&Local).date_naive());
        let next_day = local_midnight(
            block
                .last()
                .unwrap()
                .timestamp
                .with_timezone(&Local)
                .date_naive()
                .succ_opt()
                .unwrap(),
        );
        let mut left = saturating_sub(block[0].timestamp, edge).max(first_day);
        for (index, signal) in block.iter().enumerate() {
            let right = if let Some(next) = block.get(index + 1) {
                saturating_add(signal.timestamp, (next.timestamp - signal.timestamp) / 2)
            } else {
                saturating_add(signal.timestamp, edge).min(next_day)
            };
            if right > left {
                intervals.push(Interval {
                    start: left,
                    end: right,
                    provider: signal.provider.clone(),
                    model: signal.model.clone(),
                    session_id: format!("work-block:{block_index}"),
                    cwd: signal.cwd.clone(),
                    repo: signal.repo.clone(),
                    root: signal.root.clone(),
                });
            }
            left = right;
        }
    }
    intervals
}

pub fn clip_interval(
    interval: &Interval,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Option<Interval> {
    let start = since.map_or(interval.start, |bound| interval.start.max(bound));
    let end = until.map_or(interval.end, |bound| interval.end.min(bound));
    (end > start).then(|| Interval {
        start,
        end,
        ..interval.clone()
    })
}

pub fn union_seconds(intervals: &[Interval]) -> f64 {
    let mut ranges: Vec<_> = intervals
        .iter()
        .filter(|item| item.end > item.start)
        .map(|item| (item.start, item.end))
        .collect();
    ranges.sort();
    let mut total = 0.0;
    let mut current: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
    for (start, end) in ranges {
        match current {
            Some((first, last)) if start <= last => current = Some((first, last.max(end))),
            Some((first, last)) => {
                total += duration_seconds(last - first);
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((first, last)) = current {
        total += duration_seconds(last - first);
    }
    total
}

pub fn split_interval(interval: &Interval, dimension: &str) -> Vec<(String, Interval)> {
    if dimension != "day" && dimension != "month" {
        return Vec::new();
    }
    let mut pieces = Vec::new();
    let mut cursor = interval.start;
    while cursor < interval.end {
        let local = cursor.with_timezone(&Local);
        let date = local.date_naive();
        let (key, boundary_date) = if dimension == "day" {
            (
                date.format("%Y-%m-%d").to_string(),
                date.succ_opt().unwrap(),
            )
        } else {
            let (year, month) = month_after(date.year(), date.month());
            let next = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
            (date.format("%Y-%m").to_string(), next)
        };
        let end = interval.end.min(local_midnight(boundary_date));
        pieces.push((
            key,
            Interval {
                start: cursor,
                end,
                ..interval.clone()
            },
        ));
        cursor = end;
    }
    pieces
}

pub fn calendar_days(first: Option<DateTime<Utc>>, last: Option<DateTime<Utc>>) -> usize {
    match (first, last) {
        (Some(first), Some(last)) => {
            (last.with_timezone(&Local).date_naive() - first.with_timezone(&Local).date_naive())
                .num_days()
                .max(0) as usize
                + 1
        }
        _ => 0,
    }
}

pub fn local_date(value: DateTime<Utc>) -> String {
    value.with_timezone(&Local).date_naive().to_string()
}

pub fn local_month(value: DateTime<Utc>) -> String {
    value.with_timezone(&Local).format("%Y-%m").to_string()
}

pub fn duration_seconds(value: Duration) -> f64 {
    value.num_microseconds().unwrap_or(0) as f64 / 1_000_000.0
}

fn local_midnight(date: NaiveDate) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    let local = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => {
            let noon = date.and_hms_opt(12, 0, 0).unwrap();
            Local.from_local_datetime(&noon).earliest().unwrap()
        }
    };
    local.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ExactInterval;

    fn session(points: Vec<ActivityPoint>) -> Session {
        Session {
            provider: "codex".into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            root: "root".into(),
            points,
            exact_intervals: vec![],
            human_points: vec![],
            token_events: vec![],
            is_subagent: false,
        }
    }

    fn point(timestamp: &str, model: &str) -> ActivityPoint {
        ActivityPoint {
            timestamp: parse_timestamp(timestamp).unwrap(),
            model: model.into(),
        }
    }

    fn exact(start: &str, end: &str, model: &str) -> ExactInterval {
        ExactInterval {
            start: parse_timestamp(start).unwrap(),
            end: parse_timestamp(end).unwrap(),
            model: model.into(),
        }
    }

    fn interval(start: &str, end: &str, model: &str) -> Interval {
        Interval {
            start: parse_timestamp(start).unwrap(),
            end: parse_timestamp(end).unwrap(),
            provider: "codex".into(),
            model: model.into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            root: "root".into(),
        }
    }

    fn shape(intervals: &[Interval]) -> Vec<(&str, f64)> {
        intervals
            .iter()
            .map(|item| (item.model.as_str(), item.seconds()))
            .collect()
    }

    fn seconds_by_model(intervals: &[Interval]) -> BTreeMap<&str, f64> {
        let mut totals: BTreeMap<&str, f64> = BTreeMap::new();
        for item in intervals {
            *totals.entry(item.model.as_str()).or_default() += item.seconds();
        }
        totals
    }

    #[test]
    fn gap_cap_and_union_match_reference() {
        let base = parse_timestamp("2026-01-01T10:00:00Z").unwrap();
        let value = session(
            [0, 2, 20]
                .into_iter()
                .map(|minute| ActivityPoint {
                    timestamp: base + Duration::minutes(minute),
                    model: "m".into(),
                })
                .collect(),
        );
        let intervals = build_session_intervals(&value, Duration::minutes(5));
        assert_eq!(420.0, intervals.iter().map(Interval::seconds).sum::<f64>());
        assert_eq!(420.0, union_seconds(&intervals));
    }

    #[test]
    fn a_known_model_outranks_an_unknown_one_while_they_overlap() {
        let value = Session {
            exact_intervals: vec![exact("2026-01-01T10:02:00Z", "2026-01-01T10:06:00Z", "gpt")],
            ..session(vec![
                point("2026-01-01T10:00:00Z", "unknown"),
                point("2026-01-01T10:10:00Z", "unknown"),
            ])
        };
        let intervals = build_session_intervals(&value, Duration::minutes(30));
        let expected = vec![("unknown", 120.0), ("gpt", 240.0), ("unknown", 240.0)];
        assert_eq!(expected, shape(&intervals));
        assert_eq!(600.0, union_seconds(&intervals));
    }

    #[test]
    fn touching_ranges_with_one_model_become_a_single_interval() {
        let value = Session {
            exact_intervals: vec![
                exact("2026-01-01T10:00:00Z", "2026-01-01T10:10:00Z", "m"),
                exact("2026-01-01T10:10:00Z", "2026-01-01T10:20:00Z", "m"),
            ],
            ..session(vec![])
        };
        let intervals = build_session_intervals(&value, Duration::minutes(30));
        assert_eq!(vec![("m", 1200.0)], shape(&intervals));
        assert_eq!(
            parse_timestamp("2026-01-01T10:00:00Z").unwrap(),
            intervals[0].start
        );
        assert_eq!(
            parse_timestamp("2026-01-01T10:20:00Z").unwrap(),
            intervals[0].end
        );
    }

    #[test]
    fn an_exact_interval_wins_inside_a_point_derived_range() {
        let value = Session {
            exact_intervals: vec![exact("2026-01-01T10:05:00Z", "2026-01-01T10:10:00Z", "n")],
            ..session(vec![
                point("2026-01-01T10:00:00Z", "m"),
                point("2026-01-01T10:20:00Z", "m"),
            ])
        };
        let intervals = build_session_intervals(&value, Duration::minutes(30));
        // The wall clock is unchanged; only the attribution inside the overlap moves.
        assert_eq!(1200.0, union_seconds(&intervals));
        assert_eq!(
            BTreeMap::from([("m", 900.0), ("n", 300.0)]),
            seconds_by_model(&intervals)
        );
    }

    #[test]
    fn an_absurd_gap_cap_clamps_instead_of_panicking() {
        let value = session(vec![
            point("2026-01-01T10:00:00Z", "m"),
            point("2026-01-01T10:05:00Z", "m"),
        ]);
        let intervals = build_session_intervals(&value, Duration::MAX);
        assert_eq!(300.0, union_seconds(&intervals));
    }

    #[test]
    fn an_absurd_review_credit_clamps_to_the_local_day() {
        let timestamp = parse_timestamp("2026-01-01T12:00:00Z").unwrap();
        let signal = HumanSignal {
            timestamp,
            provider: "git".into(),
            session_id: "commit".into(),
            cwd: "/repo".into(),
            repo: "repo".into(),
            root: "root".into(),
            kind: "commit".into(),
            model: "—".into(),
        };
        let intervals = build_human_intervals(&[signal], Duration::minutes(30), Duration::MAX);
        let date = timestamp.with_timezone(&Local).date_naive();
        assert_eq!(local_midnight(date), intervals[0].start);
        assert_eq!(local_midnight(date.succ_opt().unwrap()), intervals[0].end);
    }

    #[test]
    fn union_counts_a_nested_interval_once() {
        let intervals = [
            interval("2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z", "m"),
            interval("2026-01-01T10:10:00Z", "2026-01-01T10:20:00Z", "n"),
            interval("2026-01-01T10:30:00Z", "2026-01-01T10:30:00Z", "n"),
        ];
        // Without the max() the enclosing range would be truncated to the nested end.
        assert_eq!(3600.0, union_seconds(&intervals));
    }

    #[test]
    fn union_joins_exactly_touching_intervals() {
        let intervals = [
            interval("2026-01-01T10:30:00Z", "2026-01-01T11:00:00Z", "n"),
            interval("2026-01-01T10:00:00Z", "2026-01-01T10:30:00Z", "m"),
        ];
        assert_eq!(3600.0, union_seconds(&intervals));
    }

    #[test]
    fn durations_parse_within_bounds_and_are_rejected_beyond_them() {
        assert_eq!(Duration::seconds(30), parse_duration("30s").unwrap());
        assert_eq!(Duration::minutes(5), parse_duration(" 5m ").unwrap());
        assert_eq!(Duration::minutes(90), parse_duration("1.5H").unwrap());
        assert_eq!(Duration::days(366), parse_duration("8784h").unwrap());
        for value in ["", "5", "5x", "0s", "-5m", "1h30m"] {
            assert!(parse_duration(value).is_err(), "{value} must be rejected");
        }
        // Past the clamp the value used to saturate at i64::MAX microseconds and
        // panic on the first addition to a timestamp.
        assert!(parse_duration("8785h").is_err());
        assert!(parse_duration("99999999999999999999h").is_err());
    }

    #[test]
    fn inclusive_bounds_match_reference() {
        let february = parse_bound(Some("2026-02"), true).unwrap().unwrap();
        assert_eq!("2026-03-01", local_date(february));
        let day = parse_bound(Some("2026-02-01"), true).unwrap().unwrap();
        assert_eq!("2026-02-02", local_date(day));
    }

    /// The shorthand has to land on exactly the pair a user would have typed by
    /// hand, or `--month 2026-12` quietly reports a different window than
    /// `--since 2026-12 --until 2026-12`.
    #[test]
    fn calendar_spans_match_the_bounds_they_stand_for() {
        let reference = Local
            .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let bounds = |since: &str, until: &str| {
            (
                parse_bound(Some(since), false).unwrap().unwrap(),
                parse_bound(Some(until), true).unwrap().unwrap(),
            )
        };
        let month = |value: &str| month_span(value, reference).unwrap();
        let year = |value: &str| year_span(value, reference).unwrap();

        assert_eq!(bounds("2026-08", "2026-08"), month("2026-08"));
        // December has to roll the year over rather than reach for month 13.
        assert_eq!(bounds("2026-12", "2026-12"), month("2026-12"));
        assert_eq!("2027-01-01", local_date(month("2026-12").1));
        assert_eq!(bounds("2026-01", "2026-12"), year("2026"));

        // Resolved against the reference, never the clock, so these hold in
        // whatever month the suite happens to run in.
        for value in ["current", "This"] {
            assert_eq!(bounds("2026-01", "2026-01"), month(value));
        }
        // The month before January is the December of the year before.
        for value in ["last", "PREVIOUS"] {
            assert_eq!(bounds("2025-12", "2025-12"), month(value));
        }
        assert_eq!(bounds("2026-01", "2026-12"), year("current"));
        assert_eq!(bounds("2025-01", "2025-12"), year("last"));

        for value in ["", "2026", "2026-13", "2026-00", "2026-8", "next"] {
            let rejected = month_span(value, reference).is_err();
            assert!(rejected, "--month {value} must be rejected");
        }
        for value in ["", "26", "2026-01", "next"] {
            let rejected = year_span(value, reference).is_err();
            assert!(rejected, "--year {value} must be rejected");
        }
    }

    #[test]
    fn human_blocks_are_non_overlapping_and_attributed() {
        let signal = |timestamp: &str, provider: &str, repo: &str, kind: &str| HumanSignal {
            timestamp: parse_timestamp(timestamp).unwrap(),
            provider: provider.into(),
            session_id: repo.into(),
            cwd: format!("/{repo}"),
            repo: repo.into(),
            root: "root".into(),
            kind: kind.into(),
            model: "model".into(),
        };
        let intervals = build_human_intervals(
            &[
                signal("2026-01-01T10:00:00Z", "claude", "a", "claude_prompt"),
                signal("2026-01-01T10:20:00Z", "codex", "b", "codex_prompt"),
                signal("2026-01-01T12:00:00Z", "git", "c", "commit"),
            ],
            Duration::minutes(30),
            Duration::minutes(10),
        );
        assert_eq!(2400.0, intervals.iter().map(Interval::seconds).sum::<f64>());
        assert_eq!(2400.0, union_seconds(&intervals));
        assert_eq!(
            HashSet::from(["a".to_string(), "b".to_string(), "c".to_string()]),
            intervals.iter().map(|item| item.repo.clone()).collect()
        );
    }

    #[test]
    fn human_edge_credit_stops_at_local_midnight() {
        let timestamp = Local
            .with_ymd_and_hms(2026, 1, 1, 23, 58, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let signal = HumanSignal {
            timestamp,
            provider: "git".into(),
            session_id: "commit".into(),
            cwd: "/repo".into(),
            repo: "repo".into(),
            root: "root".into(),
            kind: "commit".into(),
            model: "—".into(),
        };
        let intervals =
            build_human_intervals(&[signal], Duration::minutes(30), Duration::minutes(10));
        assert_eq!(
            local_midnight(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
            intervals[0].end
        );
    }

    #[test]
    fn cross_month_interval_is_split() {
        let boundary = parse_bound(Some("2026-01"), true).unwrap().unwrap();
        let interval = Interval {
            start: boundary - Duration::minutes(1),
            end: boundary + Duration::minutes(1),
            provider: "codex".into(),
            model: "m".into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            root: "root".into(),
        };
        let pieces = split_interval(&interval, "month");
        assert_eq!(
            vec!["2026-01", "2026-02"],
            pieces
                .iter()
                .map(|item| item.0.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec![60.0, 60.0],
            pieces
                .iter()
                .map(|item| item.1.seconds())
                .collect::<Vec<_>>()
        );
    }
}
