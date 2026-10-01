use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashSet};

use anyhow::{Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, Months, NaiveDate, TimeZone, Utc, Weekday,
};
use regex::Regex;

use crate::model::{
    ActivityPoint, HumanSignal, HumanTimeBlockClipping, HumanTimeBlockExplanation,
    HumanTimeDeduplication, HumanTimeDeduplicationGroup, HumanTimeExplanation,
    HumanTimeSignalExplanation, HumanTimeSignalSet, Interval, Session,
};

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

/// The ISO 8601 week a local calendar date falls in, spelled `2026-W09`.
///
/// The year is the ISO week-numbering year, not the calendar year: 2025-12-29 is
/// in `2026-W01` and 2027-01-01 is in `2026-W53`. Formatting the calendar year
/// instead would file the first days of January under a week that ended the
/// year before, and sort them in the wrong place. The fixed width keeps the
/// labels in chronological order when they are compared as text, which is how
/// the report sorts them.
fn iso_week_label(date: NaiveDate) -> String {
    let week = date.iso_week();
    format!("{:04}-W{:02}", week.year(), week.week())
}

/// The Monday an ISO week starts on, for any date inside it.
fn week_monday(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// One ISO week, Monday 00:00 to the next Monday 00:00 local time.
///
/// Both ends are local midnights of *dates*, never a start plus 168 hours, so a
/// week that contains a daylight-saving change is 167 or 169 hours long rather
/// than ending an hour into the wrong Monday.
fn week_span_of(monday: NaiveDate) -> Option<CalendarSpan> {
    let next = monday.checked_add_days(chrono::Days::new(7))?;
    Some((local_midnight(monday), local_midnight(next)))
}

/// The span `--week` is shorthand for: `YYYY-Www` (ISO 8601), `current`
/// (`this`), or `last` (`previous`). Like `month_span`, the reference instant is
/// passed in and read on the local calendar. The previous week is found by
/// stepping back seven *dates*, which crosses a year boundary correctly —
/// 2026-W01's predecessor is 2025-W52 — with no week-count arithmetic to get
/// wrong in a 53-week year. An explicit week that does not exist (week 53 is
/// only in some years, and there is no week 00) is rejected, not rolled over.
pub fn week_span(value: &str, reference: DateTime<Utc>) -> Result<CalendarSpan> {
    const EXPECTED: &str =
        "week must be YYYY-Www (ISO 8601, e.g. 2026-W09), current (this), or last (previous)";
    let value = value.trim();
    let today = reference.with_timezone(&Local).date_naive();
    let monday = match relative_span(value) {
        Some(RelativeSpan::Current) => Some(week_monday(today)),
        Some(RelativeSpan::Previous) => Some(week_monday(today) - Duration::days(7)),
        None => {
            let expression = Regex::new(r"^(\d{4})-[Ww](\d{2})$").expect("static regex");
            let Some(captures) = expression.captures(value) else {
                bail!(EXPECTED);
            };
            NaiveDate::from_isoywd_opt(captures[1].parse()?, captures[2].parse()?, Weekday::Mon)
        }
    };
    let Some(span) = monday.and_then(week_span_of) else {
        bail!(EXPECTED);
    };
    Ok(span)
}

/// The window immediately before `[since, until)`, with the same length and
/// the same kind: the month before a month, the week before a week, the year
/// before a year, and for anything else the same number of days.
///
/// The kind is read off the two local dates rather than remembered from the
/// flag, so `--month 2026-01` and `--since 2026-01 --until 2026-01` compare to
/// the same December. Whole months step back by *months*: 30 days before
/// 1 March is not February. Everything else steps back by *dates*, which is
/// also exactly right for a week, and keeps a daylight-saving change inside the
/// span from shifting the boundary by an hour.
pub fn previous_span(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<CalendarSpan> {
    let first = since.with_timezone(&Local).date_naive();
    let end = until.with_timezone(&Local).date_naive();
    if end <= first {
        bail!("the window is empty: it ends before it starts");
    }
    let whole_months = first.day() == 1 && end.day() == 1;
    let start = if whole_months {
        let months = (end.year() - first.year()) * 12 + end.month() as i32 - first.month() as i32;
        first.checked_sub_months(Months::new(months as u32))
    } else {
        first.checked_sub_days(chrono::Days::new((end - first).num_days() as u64))
    };
    let Some(start) = start else {
        bail!("the window before {first} is outside the supported calendar");
    };
    Ok((local_midnight(start), local_midnight(first)))
}

/// A comparison baseline named outright: `YYYY-MM`, `YYYY-Www` or `YYYY`.
/// The shape picks the span, so the three shorthands cannot be confused.
pub fn named_span(value: &str, reference: DateTime<Utc>) -> Result<CalendarSpan> {
    let value = value.trim();
    let digits = |text: &str| text.chars().all(|character| character.is_ascii_digit());
    match value.len() {
        4 if digits(value) => year_span(value, reference),
        7 if value.as_bytes()[4] == b'-' && digits(&value[..4]) && digits(&value[5..]) => {
            month_span(value, reference)
        }
        8 if value.as_bytes()[4] == b'-' && matches!(value.as_bytes()[5], b'W' | b'w') => {
            week_span(value, reference)
        }
        _ => bail!("expected previous, YYYY-MM, YYYY-Www or YYYY"),
    }
}

/// How a report names its window: `2026-08`, `2026-W09` or `2026` when the span
/// is exactly that, and the first and last day otherwise. Dates are local, like
/// every other calendar boundary here, and `until` is exclusive, so the last
/// day is the day before it.
pub fn window_label(since: DateTime<Utc>, until: DateTime<Utc>) -> String {
    let first = since.with_timezone(&Local).date_naive();
    let end = until.with_timezone(&Local).date_naive();
    let months = (end.year() - first.year()) * 12 + end.month() as i32 - first.month() as i32;
    if first.day() == 1 && end.day() == 1 {
        if months == 1 {
            return format!("{:04}-{:02}", first.year(), first.month());
        }
        if months == 12 && first.month() == 1 {
            return format!("{:04}", first.year());
        }
    }
    if first.weekday() == Weekday::Mon && (end - first).num_days() == 7 {
        return iso_week_label(first);
    }
    let last = end.pred_opt().unwrap_or(end);
    format!("{first} → {last}")
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
        repo_id: session.repo_id.clone(),
        root: session.root.clone(),
        branch: session.branch_at(start).map(str::to_string),
    }
}

pub struct HumanTimeCalculation {
    pub intervals: Vec<Interval>,
    pub total_seconds: f64,
    pub explanation: Option<HumanTimeExplanation>,
}

fn human_signal_kind(kind: &str) -> &'static str {
    if kind.ends_with("_prompt") {
        "prompt"
    } else if kind == "commit" {
        "commit"
    } else {
        "foreground_session_edge"
    }
}

fn human_signal_priority(kind: &str) -> u8 {
    match human_signal_kind(kind) {
        "prompt" => 3,
        "commit" => 2,
        _ => 1,
    }
}

fn signal_id(index: usize) -> String {
    format!("signal:{}", index + 1)
}

fn explanation_timestamp(value: DateTime<Utc>) -> String {
    let precision = if value.timestamp_subsec_micros() == 0 {
        chrono::SecondsFormat::Secs
    } else {
        chrono::SecondsFormat::Micros
    };
    value.to_rfc3339_opts(precision, false)
}

fn positive_seconds(value: Duration) -> f64 {
    duration_seconds(value).max(0.0)
}

fn round_microseconds_to_milliseconds(microseconds: i128) -> i128 {
    let milliseconds = microseconds / 1000;
    let remainder = microseconds % 1000;
    if remainder > 500 || (remainder == 500 && milliseconds % 2 != 0) {
        milliseconds + 1
    } else {
        milliseconds
    }
}

fn microseconds_as_seconds(microseconds: i128) -> f64 {
    microseconds as f64 / 1_000_000.0
}

fn signal_counts<'a>(
    signals: impl IntoIterator<Item = &'a HumanSignal>,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::from([
        ("commit".to_string(), 0),
        ("foreground_session_edge".to_string(), 0),
        ("prompt".to_string(), 0),
    ]);
    for signal in signals {
        *counts
            .entry(human_signal_kind(&signal.kind).to_string())
            .or_default() += 1;
    }
    counts
}

fn signal_explanation(index: usize, signal: &HumanSignal) -> HumanTimeSignalExplanation {
    HumanTimeSignalExplanation {
        id: signal_id(index),
        timestamp: explanation_timestamp(signal.timestamp),
        kind: human_signal_kind(&signal.kind).to_string(),
        provider: signal.provider.clone(),
        repo: signal.repo.clone(),
    }
}

pub fn calculate_human_time(
    signals: &[HumanSignal],
    idle_threshold: Duration,
    block_credit: Duration,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    explain: bool,
) -> HumanTimeCalculation {
    let mut by_timestamp: BTreeMap<DateTime<Utc>, Vec<usize>> = BTreeMap::new();
    for (index, signal) in signals.iter().enumerate() {
        by_timestamp
            .entry(signal.timestamp)
            .or_default()
            .push(index);
    }

    let mut effective_indices = Vec::new();
    let mut deduplication_groups = Vec::new();
    for (timestamp, indices) in by_timestamp {
        let kept = indices.iter().copied().fold(indices[0], |kept, candidate| {
            if human_signal_priority(&signals[candidate].kind)
                > human_signal_priority(&signals[kept].kind)
            {
                candidate
            } else {
                kept
            }
        });
        effective_indices.push(kept);
        if explain && indices.len() > 1 {
            deduplication_groups.push(HumanTimeDeduplicationGroup {
                timestamp: explanation_timestamp(timestamp),
                kept_signal_id: signal_id(kept),
                discarded_signal_ids: indices
                    .into_iter()
                    .filter(|index| *index != kept)
                    .map(signal_id)
                    .collect(),
            });
        }
    }

    let mut blocks: Vec<Vec<usize>> = Vec::new();
    let mut current = Vec::new();
    for index in effective_indices.iter().copied() {
        if current.last().is_some_and(|previous: &usize| {
            signals[index].timestamp - signals[*previous].timestamp > idle_threshold
        }) {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(index);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let edge = block_credit / 2;
    let mut intervals = Vec::new();
    let mut block_microsecond_totals = Vec::new();
    let mut explained_blocks = Vec::new();
    for (block_index, block) in blocks.into_iter().enumerate() {
        let first = signals[block[0]].timestamp;
        let last = signals[*block.last().unwrap()].timestamp;
        let first_day = local_midnight(first.with_timezone(&Local).date_naive());
        let next_day = local_midnight(last.with_timezone(&Local).date_naive().succ_opt().unwrap());
        let requested_start = saturating_sub(first, edge);
        let requested_end = saturating_add(last, edge);
        let local_start = requested_start.max(first_day);
        let local_end = requested_end.min(next_day);
        let final_start = since.map_or(local_start, |bound| local_start.max(bound));
        let final_end = until.map_or(local_end, |bound| local_end.min(bound));
        if final_end <= final_start {
            continue;
        }

        let block_id = format!("work-block:{block_index}");
        let first_interval = intervals.len();
        let mut left = final_start;
        for (position, index) in block.iter().copied().enumerate() {
            let signal = &signals[index];
            let boundary = if let Some(next_index) = block.get(position + 1) {
                let next = &signals[*next_index];
                saturating_add(signal.timestamp, (next.timestamp - signal.timestamp) / 2)
            } else {
                local_end
            };
            let right = boundary.min(final_end);
            if right > left {
                intervals.push(Interval {
                    start: left,
                    end: right,
                    provider: signal.provider.clone(),
                    model: signal.model.clone(),
                    session_id: block_id.clone(),
                    cwd: signal.cwd.clone(),
                    repo: signal.repo.clone(),
                    repo_id: signal.repo_id.clone(),
                    root: signal.root.clone(),
                    branch: signal.branch.clone(),
                });
            }
            left = left.max(right);
        }
        let block_microseconds: i128 = intervals[first_interval..]
            .iter()
            .map(|interval| {
                (interval.end - interval.start)
                    .num_microseconds()
                    .unwrap_or(0)
                    .max(0) as i128
            })
            .sum();
        let block_seconds = microseconds_as_seconds(block_microseconds);
        block_microsecond_totals.push(block_microseconds);

        if explain {
            let block_signals: Vec<_> = block.iter().map(|index| &signals[*index]).collect();
            explained_blocks.push(HumanTimeBlockExplanation {
                id: block_id,
                start: explanation_timestamp(final_start),
                end: explanation_timestamp(final_end),
                first_signal_timestamp: explanation_timestamp(first),
                last_signal_timestamp: explanation_timestamp(last),
                seconds: block_seconds,
                requested_review_credit_seconds: positive_seconds(block_credit),
                actual_start_credit_seconds: positive_seconds(first - final_start),
                actual_end_credit_seconds: positive_seconds(final_end - last),
                signal_count: block.len(),
                counts_by_kind: signal_counts(block_signals.iter().copied()),
                signal_ids: block.iter().copied().map(signal_id).collect(),
                clipping: HumanTimeBlockClipping {
                    local_day_start_seconds: positive_seconds(local_start - requested_start),
                    local_day_end_seconds: positive_seconds(requested_end - local_end),
                    report_window_start_seconds: positive_seconds(final_start - local_start),
                    report_window_end_seconds: positive_seconds(local_end - final_end),
                },
            });
        }
    }

    let total_microseconds: i128 = block_microsecond_totals.iter().sum();
    let unrounded_block_seconds_total = microseconds_as_seconds(total_microseconds);
    let total_seconds = round_microseconds_to_milliseconds(total_microseconds) as f64 / 1000.0;
    let explanation = explain.then(|| {
        let input_signals = HumanTimeSignalSet {
            count: signals.len(),
            counts_by_kind: signal_counts(signals),
            signals: signals
                .iter()
                .enumerate()
                .map(|(index, signal)| signal_explanation(index, signal))
                .collect(),
        };
        let effective_signals = HumanTimeSignalSet {
            count: effective_indices.len(),
            counts_by_kind: signal_counts(
                effective_indices
                    .iter()
                    .map(|index| &signals[*index]),
            ),
            signals: effective_indices
                .iter()
                .map(|index| signal_explanation(*index, &signals[*index]))
                .collect(),
        };
        let discarded_signal_count = input_signals.count - effective_signals.count;
        HumanTimeExplanation {
            algorithm_version: "signal-blocks-v1",
            timezone_basis: "UTC ledger timestamps; review-credit edges clamp to host-local calendar midnights",
            input_signals,
            effective_signals,
            same_timestamp_deduplication: HumanTimeDeduplication {
                priority: "prompt > commit > foreground_session_edge",
                equal_priority_tie_break: "first input signal wins",
                discarded_signal_count,
                groups: deduplication_groups,
            },
            blocks: explained_blocks,
            unrounded_block_seconds_total,
            total_rounding_adjustment_seconds: total_seconds - unrounded_block_seconds_total,
            total_seconds,
        }
    });

    HumanTimeCalculation {
        intervals,
        total_seconds,
        explanation,
    }
}

#[cfg(test)]
pub fn build_human_intervals(
    signals: &[HumanSignal],
    idle_threshold: Duration,
    block_credit: Duration,
) -> Vec<Interval> {
    calculate_human_time(signals, idle_threshold, block_credit, None, None, false).intervals
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
    if !matches!(dimension, "day" | "week" | "month") {
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
        } else if dimension == "week" {
            // The next Monday, found on the date rather than by adding hours,
            // so a daylight-saving change inside the week cannot move it.
            (iso_week_label(date), week_monday(date) + Duration::days(7))
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

pub fn local_week(value: DateTime<Utc>) -> String {
    iso_week_label(value.with_timezone(&Local).date_naive())
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
            repo_id: "x".into(),
            root: "root".into(),
            points,
            exact_intervals: vec![],
            human_points: vec![],
            token_events: vec![],
            is_subagent: false,
            branch_source: crate::model::BranchSource::None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
            source_file: std::path::PathBuf::new(),
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
            repo_id: "x".into(),
            root: "root".into(),
            branch: None,
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
            repo_id: "repo".into(),
            root: "root".into(),
            kind: "commit".into(),
            model: "—".into(),
            branch: None,
        };
        let intervals = build_human_intervals(&[signal], Duration::minutes(30), Duration::MAX);
        let date = timestamp.with_timezone(&Local).date_naive();
        assert_eq!(local_midnight(date), intervals[0].start);
        assert_eq!(local_midnight(date.succ_opt().unwrap()), intervals[0].end);
    }

    #[test]
    fn the_branch_travels_with_every_interval_the_pipeline_cuts() {
        let mut value = session(vec![
            point("2026-01-01T10:00:00Z", "m"),
            point("2026-01-01T10:03:00Z", "m"),
            point("2026-01-01T10:20:00Z", "m"),
            point("2026-01-01T10:22:00Z", "m"),
        ]);
        value.branches = vec![
            crate::model::BranchMark {
                from: None,
                branch: "feat/a".into(),
            },
            crate::model::BranchMark {
                from: Some(parse_timestamp("2026-01-01T10:10:00Z").unwrap()),
                branch: "feat/b".into(),
            },
        ];
        let intervals = build_session_intervals(&value, Duration::minutes(5));
        let branches: Vec<_> = intervals
            .iter()
            .map(|item| item.branch.as_deref())
            .collect();
        assert_eq!(vec![Some("feat/a"), Some("feat/b")], branches);

        // Clipping and splitting copy the interval they cut.
        let clipped = clip_interval(
            &intervals[0],
            Some(parse_timestamp("2026-01-01T10:01:00Z").unwrap()),
            None,
        )
        .unwrap();
        assert_eq!(Some("feat/a"), clipped.branch.as_deref());
        assert!(
            split_interval(&clipped, "day")
                .iter()
                .all(|(_, piece)| piece.branch.as_deref() == Some("feat/a"))
        );

        // Human pieces take the branch of the signal nearest to them.
        let signal = |at: &str, branch: &str| HumanSignal {
            timestamp: parse_timestamp(at).unwrap(),
            provider: "codex".into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            repo_id: "x".into(),
            root: "root".into(),
            kind: "codex_prompt".into(),
            model: "m".into(),
            branch: Some(branch.into()),
        };
        let human = build_human_intervals(
            &[
                signal("2026-01-01T10:00:00Z", "feat/a"),
                signal("2026-01-01T10:10:00Z", "feat/b"),
            ],
            Duration::hours(1),
            Duration::zero(),
        );
        let branches: Vec<_> = human.iter().map(|item| item.branch.as_deref()).collect();
        assert_eq!(vec![Some("feat/a"), Some("feat/b")], branches);
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

    fn local_noon(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    /// The ISO week-numbering year is not the calendar year at either edge of
    /// it, and 2026 is a 53-week year, so both edges are covered.
    #[test]
    fn iso_week_labels_follow_the_week_year_across_a_year_boundary() {
        let label = |year, month, day| local_week(local_noon(year, month, day));
        // A Sunday, the last day of 2025-W52.
        assert_eq!("2025-W52", label(2025, 12, 28));
        // Monday 29 December is already 2026-W01 although the calendar says 2025.
        assert_eq!("2026-W01", label(2025, 12, 29));
        assert_eq!("2026-W01", label(2026, 1, 4));
        assert_eq!("2026-W02", label(2026, 1, 5));
        // 2026 has a week 53, and it runs into January 2027.
        assert_eq!("2026-W53", label(2026, 12, 31));
        assert_eq!("2026-W53", label(2027, 1, 3));
        assert_eq!("2027-W01", label(2027, 1, 4));
        // The mirror image: 1 January 2021 belongs to the previous week year.
        assert_eq!("2020-W53", label(2021, 1, 1));
        // Zero-padded, so text order is chronological order.
        assert_eq!("2026-W09", label(2026, 2, 25));
    }

    #[test]
    fn an_interval_across_new_year_splits_at_local_mondays() {
        let interval = Interval {
            start: local_noon(2025, 12, 28),
            end: local_noon(2026, 1, 6),
            provider: "codex".into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            repo_id: "x".into(),
            root: "root".into(),
            model: "m".into(),
            branch: None,
        };
        let pieces = split_interval(&interval, "week");
        assert_eq!(
            vec!["2025-W52", "2026-W01", "2026-W02"],
            pieces
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
        );
        // The cut is the local Monday midnight, and nothing is lost or repeated.
        assert_eq!("2025-12-29", local_date(pieces[0].1.end));
        assert_eq!(pieces[0].1.end, pieces[1].1.start);
        assert_eq!("2026-01-05", local_date(pieces[1].1.end));
        assert_eq!(interval.start, pieces[0].1.start);
        assert_eq!(interval.end, pieces[2].1.end);
        assert_eq!(
            interval.seconds(),
            pieces.iter().map(|(_, piece)| piece.seconds()).sum::<f64>()
        );
    }

    /// Whatever the machine's timezone, a year of weeks is contiguous: no week
    /// overlaps or leaves a gap, even where a daylight-saving change makes one of
    /// them an hour longer or shorter than 168.
    #[test]
    fn a_year_of_weeks_is_contiguous_and_starts_on_mondays() {
        let reference = local_noon(2026, 6, 1);
        let mut previous_end = None;
        for week in 1..=53 {
            let (start, end) = week_span(&format!("2026-W{week:02}"), reference).unwrap();
            let monday = start.with_timezone(&Local).date_naive();
            assert_eq!(Weekday::Mon, monday.weekday(), "week {week}");
            assert_eq!(
                Weekday::Mon,
                end.with_timezone(&Local).date_naive().weekday(),
                "week {week}"
            );
            assert_eq!(
                7,
                (end.with_timezone(&Local).date_naive() - monday).num_days()
            );
            if let Some(previous) = previous_end {
                assert_eq!(previous, start, "week {week}");
            }
            previous_end = Some(end);
        }

        let interval = Interval {
            start: week_span("2026-W01", reference).unwrap().0,
            end: week_span("2026-W53", reference).unwrap().1,
            provider: "codex".into(),
            session_id: "s".into(),
            cwd: "/x".into(),
            repo: "x".into(),
            repo_id: "x".into(),
            root: "root".into(),
            model: "m".into(),
            branch: None,
        };
        let keys: Vec<_> = split_interval(&interval, "week")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(53, keys.len());
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn week_spans_match_the_monday_to_monday_window_they_stand_for() {
        let reference = local_noon(2026, 1, 15); // a Thursday in 2026-W03
        let week = |value: &str| week_span(value, reference).unwrap();
        let bounds = |since: &str, until: &str| {
            (
                parse_bound(Some(since), false).unwrap().unwrap(),
                parse_bound(Some(until), false).unwrap().unwrap(),
            )
        };

        // 2026-W01 starts in December 2025.
        assert_eq!(bounds("2025-12-29", "2026-01-05"), week("2026-W01"));
        assert_eq!(week("2026-W01"), week("2026-w01"));
        assert_eq!(bounds("2026-02-23", "2026-03-02"), week("2026-W09"));
        assert_eq!(bounds("2026-12-28", "2027-01-04"), week("2026-W53"));
        assert_eq!(bounds("2026-01-12", "2026-01-19"), week("current"));
        assert_eq!(week("current"), week("This"));
        assert_eq!(bounds("2026-01-05", "2026-01-12"), week("last"));
        // The week before 2026-W01 is 2025-W52, not week zero.
        let new_year = local_noon(2026, 1, 1);
        assert_eq!(
            bounds("2025-12-22", "2025-12-29"),
            week_span("previous", new_year).unwrap()
        );

        // 2025 has only 52 weeks, and there is no week 00 or 54.
        for value in [
            "", "2026", "2026-W", "2026-W9", "2025-W53", "2026-W00", "2026-W54", "2026-09", "next",
        ] {
            assert!(
                week_span(value, reference).is_err(),
                "--week {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_previous_span_is_the_same_kind_of_window_one_step_back() {
        let reference = local_noon(2026, 6, 15);
        let bounds = |since: &str, until: &str| {
            (
                parse_bound(Some(since), false).unwrap().unwrap(),
                parse_bound(Some(until), false).unwrap().unwrap(),
            )
        };
        let before = |(since, until): CalendarSpan| previous_span(since, until).unwrap();

        // January's predecessor is last year's December, and a year's is a year.
        assert_eq!(
            bounds("2025-12-01", "2026-01-01"),
            before(month_span("2026-01", reference).unwrap())
        );
        assert_eq!(
            bounds("2026-02-01", "2026-03-01"),
            before(month_span("2026-03", reference).unwrap())
        );
        assert_eq!(
            bounds("2025-01-01", "2026-01-01"),
            before(year_span("2026", reference).unwrap())
        );
        // 2026-W01 (from 29 December) is preceded by 2025-W52; 2027-W01 (from
        // 4 January) by 2026-W53.
        assert_eq!(
            week_span("2025-W52", reference).unwrap(),
            before(week_span("2026-W01", reference).unwrap())
        );
        assert_eq!(
            week_span("2026-W53", reference).unwrap(),
            before(week_span("2027-W01", reference).unwrap())
        );
        // Whole months written as a range step back by months, not by days.
        assert_eq!(
            bounds("2025-11-01", "2026-01-01"),
            before(bounds("2026-01-01", "2026-03-01"))
        );
        // Anything else is the same number of days: --since 05-10 --until 05-19
        // is ten days, inclusive of both ends.
        let ten_days = (
            parse_bound(Some("2026-05-10"), false).unwrap().unwrap(),
            parse_bound(Some("2026-05-19"), true).unwrap().unwrap(),
        );
        assert_eq!(bounds("2026-04-30", "2026-05-10"), before(ten_days));

        assert!(previous_span(ten_days.1, ten_days.0).is_err());
    }

    #[test]
    fn a_named_span_is_chosen_by_its_shape() {
        let reference = local_noon(2026, 6, 15);
        assert_eq!(
            month_span("2026-03", reference).unwrap(),
            named_span("2026-03", reference).unwrap()
        );
        assert_eq!(
            week_span("2026-W09", reference).unwrap(),
            named_span("2026-w09", reference).unwrap()
        );
        assert_eq!(
            year_span("2025", reference).unwrap(),
            named_span("2025", reference).unwrap()
        );
        for value in ["", "last", "2026-3", "2026-W9", "2026-13", "2025-W53", "26"] {
            assert!(
                named_span(value, reference).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_window_is_named_the_way_it_was_asked_for() {
        let reference = local_noon(2026, 6, 15);
        let label = |(since, until): CalendarSpan| window_label(since, until);
        assert_eq!("2026-03", label(month_span("2026-03", reference).unwrap()));
        assert_eq!("2026", label(year_span("2026", reference).unwrap()));
        assert_eq!("2026-W01", label(week_span("2026-W01", reference).unwrap()));
        let range = (
            parse_bound(Some("2026-05-10"), false).unwrap().unwrap(),
            parse_bound(Some("2026-05-19"), true).unwrap().unwrap(),
        );
        assert_eq!("2026-05-10 → 2026-05-19", label(range));
    }

    #[test]
    fn human_blocks_are_non_overlapping_and_attributed() {
        let signal = |timestamp: &str, provider: &str, repo: &str, kind: &str| HumanSignal {
            timestamp: parse_timestamp(timestamp).unwrap(),
            provider: provider.into(),
            session_id: repo.into(),
            cwd: format!("/{repo}"),
            repo: repo.into(),
            repo_id: repo.into(),
            root: "root".into(),
            kind: kind.into(),
            model: "model".into(),
            branch: None,
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
    fn human_time_explanation_records_timestamp_selection_and_reconciles() {
        let signal = |provider: &str, repo: &str, kind: &str| HumanSignal {
            timestamp: parse_timestamp("2026-01-01T10:00:00Z").unwrap(),
            provider: provider.into(),
            session_id: repo.into(),
            cwd: format!("/{repo}"),
            repo: repo.into(),
            repo_id: repo.into(),
            root: "root".into(),
            kind: kind.into(),
            model: "model".into(),
            branch: None,
        };
        let calculation = calculate_human_time(
            &[
                signal("claude", "edge", "claude_session_edge"),
                signal("git", "commit", "commit"),
                signal("codex", "prompt", "codex_prompt"),
            ],
            Duration::minutes(30),
            Duration::minutes(10),
            None,
            None,
            true,
        );
        let explanation = calculation.explanation.unwrap();
        assert_eq!(3, explanation.input_signals.count);
        assert_eq!(1, explanation.effective_signals.count);
        assert_eq!("prompt", explanation.effective_signals.signals[0].kind);
        assert_eq!(
            2,
            explanation
                .same_timestamp_deduplication
                .discarded_signal_count
        );
        assert_eq!(1, explanation.same_timestamp_deduplication.groups.len());
        assert_eq!(600.0, explanation.blocks[0].seconds);
        assert_eq!(calculation.total_seconds, explanation.total_seconds);
        assert_eq!(
            explanation.total_seconds,
            explanation.unrounded_block_seconds_total
                + explanation.total_rounding_adjustment_seconds
        );
    }

    #[test]
    fn human_time_uses_one_rounding_boundary_and_reports_its_adjustment() {
        assert_eq!(2000, round_microseconds_to_milliseconds(2_000_500));
        assert_eq!(2002, round_microseconds_to_milliseconds(2_001_500));

        let signal = |timestamp: &str| HumanSignal {
            timestamp: parse_timestamp(timestamp).unwrap(),
            provider: "provider".into(),
            session_id: timestamp.into(),
            cwd: "/repo".into(),
            repo: "repo".into(),
            repo_id: "repo".into(),
            root: "root".into(),
            kind: "provider_prompt".into(),
            model: "model".into(),
            branch: None,
        };
        let calculation = calculate_human_time(
            &[
                signal("2026-01-01T10:00:00Z"),
                signal("2026-01-01T10:00:00.062500Z"),
            ],
            Duration::seconds(1),
            Duration::zero(),
            None,
            None,
            true,
        );
        let explanation = calculation.explanation.unwrap();
        assert_eq!(0.0625, explanation.blocks[0].seconds);
        assert_eq!(0.0625, explanation.unrounded_block_seconds_total);
        assert_eq!(0.062, explanation.total_seconds);
        assert_eq!(calculation.total_seconds, explanation.total_seconds);
        assert_eq!(
            explanation.total_seconds,
            explanation.unrounded_block_seconds_total
                + explanation.total_rounding_adjustment_seconds
        );
    }

    #[test]
    fn human_time_explanation_excludes_sensitive_source_identifiers() {
        let timestamp = parse_timestamp("2026-01-01T10:00:00Z").unwrap();
        let signals = [
            HumanSignal {
                timestamp,
                provider: "provider".into(),
                session_id: "SESSION_ID_SECRET".into(),
                cwd: "/CWD_SECRET/project".into(),
                repo: "safe-repo".into(),
                repo_id: "REPO_ID_SECRET".into(),
                root: "/ROOT_SECRET".into(),
                kind: "provider_prompt".into(),
                model: "MODEL_SECRET".into(),
                branch: None,
            },
            HumanSignal {
                timestamp: timestamp + Duration::seconds(1),
                provider: "git".into(),
                session_id: "COMMIT_HASH_SECRET".into(),
                cwd: "/CWD_SECRET/project".into(),
                repo: "safe-repo".into(),
                repo_id: "REPO_ID_SECRET".into(),
                root: "/ROOT_SECRET".into(),
                kind: "commit".into(),
                model: "MODEL_SECRET".into(),
                branch: None,
            },
        ];
        let explanation = calculate_human_time(
            &signals,
            Duration::minutes(30),
            Duration::minutes(10),
            None,
            None,
            true,
        )
        .explanation
        .unwrap();
        let serialized = serde_json::to_string(&explanation).unwrap();
        for secret in [
            "SESSION_ID_SECRET",
            "CWD_SECRET",
            "REPO_ID_SECRET",
            "ROOT_SECRET",
            "MODEL_SECRET",
            "COMMIT_HASH_SECRET",
        ] {
            assert!(
                !serialized.contains(secret),
                "leaked {secret}: {serialized}"
            );
        }
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
            repo_id: "repo".into(),
            root: "root".into(),
            kind: "commit".into(),
            model: "—".into(),
            branch: None,
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
            repo_id: "x".into(),
            root: "root".into(),
            branch: None,
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
