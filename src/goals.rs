//! Weekly-hours and list-value-cap progress.
//!
//! Goals are read from the config's `goals` block. The report carries a
//! `GoalReport` only when goals are configured and not disabled with
//! `--no-goals`, and `workstats now` asks the same rules for the week and month
//! so far. Nothing here is a bill: list value is the public pay-per-token price
//! of the tokens used, the same figure `allocate` weighs plans with.
//!
//! Periods are the local calendar's: ISO weeks (Monday first) and months. A
//! report window rarely lines up with them, so a period that the window only
//! partly covers is flagged `partial` rather than prorated, and its figure is
//! a lower bound.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::ReportWindow;
use crate::model::{Diagnostics, Interval, Session};
use crate::output::{number, safe_text};
use crate::pricing::{self, Family, RateOverrides};
use crate::timeutil::split_interval;

/// Enough periods for a window of a couple of years; past it the oldest are
/// left out and the run says so, because a goal report is not an archive.
const MAX_PERIODS: usize = 120;

/// The share of a cap at which a warning starts when the config gives none.
const DEFAULT_WARN_AT: f64 = 0.8;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Week,
    Month,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    fn adjective(self) -> &'static str {
        match self {
            Self::Week => "weekly",
            Self::Month => "monthly",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGoals {
    weekly_hours: Option<f64>,
    daily_hours: Option<f64>,
    max_weekly_hours: Option<f64>,
    #[serde(default)]
    list_value_caps: Vec<RawCap>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCap {
    pool: String,
    period: Period,
    usd: f64,
    warn_at: Option<f64>,
}

/// One validated list-value cap.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cap {
    pub(crate) pool: String,
    pub(crate) period: Period,
    pub(crate) usd: f64,
    pub(crate) warn_at: f64,
}

/// The validated `goals` block.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Goals {
    pub(crate) weekly_hours: Option<f64>,
    pub(crate) daily_hours: Option<f64>,
    pub(crate) max_weekly_hours: Option<f64>,
    pub(crate) caps: Vec<Cap>,
}

impl Goals {
    /// Reads the raw `goals` value. A misspelt field, a wrong type or a bad
    /// value stops the run with an error naming the entry, the same way
    /// `model_rates` does, instead of quietly measuring against nothing.
    /// `None` means no goal is set (an empty block counts as none).
    pub(crate) fn from_config(value: Option<&serde_json::Value>) -> Result<Option<Self>> {
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        Self::parse(value)
            .context("invalid \"goals\" configuration")
            .map(|goals| (!goals.is_empty()).then_some(goals))
    }

    fn parse(value: &serde_json::Value) -> Result<Self> {
        let raw: RawGoals = serde_json::from_value(value.clone())?;
        let hours = |name: &str, value: Option<f64>, most: f64| -> Result<Option<f64>> {
            match value {
                Some(hours) if !hours.is_finite() || hours <= 0.0 || hours > most => {
                    bail!("{name} must be greater than 0 and at most {most}")
                }
                other => Ok(other),
            }
        };
        let weekly_hours = hours("weekly_hours", raw.weekly_hours, 168.0)?;
        let daily_hours = hours("daily_hours", raw.daily_hours, 24.0)?;
        let max_weekly_hours = hours("max_weekly_hours", raw.max_weekly_hours, 168.0)?;
        if let (Some(target), Some(most)) = (weekly_hours, max_weekly_hours)
            && most < target
        {
            bail!("max_weekly_hours ({most}) must not be below weekly_hours ({target})");
        }
        if raw.list_value_caps.len() > 32 {
            bail!("at most 32 list_value_caps are supported");
        }
        let mut caps: Vec<Cap> = Vec::new();
        for (index, cap) in raw.list_value_caps.iter().enumerate() {
            let at = format!("list_value_caps[{index}]");
            let pool = normalized_pool(&cap.pool).with_context(|| format!("{at}.pool"))?;
            if !cap.usd.is_finite() || cap.usd <= 0.0 {
                bail!("{at}.usd must be greater than 0");
            }
            let warn_at = cap.warn_at.unwrap_or(DEFAULT_WARN_AT);
            if !warn_at.is_finite() || warn_at <= 0.0 || warn_at > 1.0 {
                bail!("{at}.warn_at must be greater than 0 and at most 1 (0.8 is 80%)");
            }
            if caps
                .iter()
                .any(|other| other.pool == pool && other.period == cap.period)
            {
                bail!("{at}: {pool} already has a {} cap", cap.period.adjective());
            }
            caps.push(Cap {
                pool,
                period: cap.period,
                usd: cap.usd,
                warn_at,
            });
        }
        Ok(Self {
            weekly_hours,
            daily_hours,
            max_weekly_hours,
            caps,
        })
    }

    fn is_empty(&self) -> bool {
        self.weekly_hours.is_none()
            && self.daily_hours.is_none()
            && self.max_weekly_hours.is_none()
            && self.caps.is_empty()
    }

    /// Whether anything needs the month so far, which decides how far back
    /// `workstats now` has to look.
    pub(crate) fn has_month_cap(&self) -> bool {
        self.caps.iter().any(|cap| cap.period == Period::Month)
    }

    fn has_hour_goal(&self) -> bool {
        self.weekly_hours.is_some() || self.daily_hours.is_some() || self.max_weekly_hours.is_some()
    }
}

/// The pool names `RateOverrides::pool_for` returns, accepting the aliases
/// `--sub` does (`codex` is the OpenAI pool).
fn normalized_pool(name: &str) -> Result<String> {
    let lowered = name.trim().to_ascii_lowercase();
    if pricing::SEPARATE_PLANS.contains(&lowered.as_str()) {
        return Ok(lowered);
    }
    match Family::parse(&lowered) {
        Some(family) => Ok(family.as_str().to_string()),
        None => bail!(
            "unknown pool {name:?}; use one of {}",
            Family::ALL
                .iter()
                .map(|family| family.as_str())
                .chain(pricing::SEPARATE_PLANS)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// One token event with its list value and the pool it draws on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PricedEvent {
    pub(crate) at: DateTime<Utc>,
    /// `None` when no pool claims the model; it still counts toward totals.
    pub(crate) pool: Option<String>,
    pub(crate) usd: f64,
}

/// Every priced token event, and how many could not be priced at all.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Priced {
    pub(crate) events: Vec<PricedEvent>,
    pub(crate) unpriced: usize,
}

pub(crate) fn price_events(sessions: &[Session], overrides: &RateOverrides) -> Priced {
    let mut priced = Priced::default();
    for session in sessions {
        for event in &session.token_events {
            match pricing::list_value_usd(&event.model, &event.usage, overrides) {
                Some(usd) => priced.events.push(PricedEvent {
                    at: event.timestamp,
                    pool: overrides.pool_for(&session.provider, &event.model),
                    usd,
                }),
                None if event.usage.is_zero() => {}
                None => priced.unpriced += 1,
            }
        }
    }
    priced
}

/// List value per pool for events on or after `from`, local calendar.
pub(crate) fn pool_totals(priced: &Priced, from: NaiveDate) -> BTreeMap<String, f64> {
    let mut totals = BTreeMap::new();
    for event in &priced.events {
        if local_day(event.at) >= from
            && let Some(pool) = &event.pool
        {
            *totals.entry(pool.clone()).or_insert(0.0) += event.usd;
        }
    }
    totals
}

fn local_day(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

pub(crate) fn week_start(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

pub(crate) fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

fn period_start(period: Period, date: NaiveDate) -> NaiveDate {
    match period {
        Period::Week => week_start(date),
        Period::Month => month_start(date),
    }
}

/// The first day after the period that starts on `start`.
fn period_end(period: Period, start: NaiveDate) -> NaiveDate {
    match period {
        Period::Week => start + Duration::days(7),
        Period::Month => start
            .checked_add_months(chrono::Months::new(1))
            .unwrap_or(start + Duration::days(31)),
    }
}

fn period_label(period: Period, start: NaiveDate) -> String {
    match period {
        Period::Week => {
            let week = start.iso_week();
            format!("{:04}-W{:02}", week.year(), week.week())
        }
        Period::Month => format!("{:04}-{:02}", start.year(), start.month()),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WeekProgress {
    /// ISO week, `2026-W34`.
    pub week: String,
    /// The Monday it starts on.
    pub start: NaiveDate,
    pub hours: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_hours: Option<f64>,
    /// Hours as a share of the target, 1.0 being on target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent_of_target: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over_max: Option<bool>,
    /// Days in the week that reached `daily_hours`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_at_daily_target: Option<usize>,
    /// The window covers only part of the week, so the hours are a lower bound.
    pub partial: bool,
    /// The week has not ended yet.
    pub in_progress: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapStatus {
    Ok,
    Warn,
    Reached,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CapProgress {
    pub pool: String,
    pub period: Period,
    /// `2026-08` or `2026-W34`.
    pub label: String,
    pub usd: f64,
    pub value_usd: f64,
    /// Value as a share of the cap, 1.0 being the cap.
    pub fraction: f64,
    pub warn_at: f64,
    pub status: CapStatus,
    pub partial: bool,
    pub in_progress: bool,
}

/// Progress against the configured goals over the report window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GoalReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_weekly_hours: Option<f64>,
    pub weeks: Vec<WeekProgress>,
    pub caps: Vec<CapProgress>,
    /// Token events whose model has no list rate. They are left out of every
    /// cap, so a cap total is a lower bound while this is not zero.
    pub unpriced_events: usize,
}

/// Evaluates the goals for a finished report; the one call `execute` makes.
///
/// Returns `None` when goals are off, unset, or the run is `now`'s own.
/// Warnings go through `diagnostics`, so they print with the others.
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_report(
    enabled: bool,
    config: Option<&serde_json::Value>,
    window: ReportWindow,
    now: DateTime<Utc>,
    sessions: &[Session],
    human_intervals: &[Interval],
    overrides: &RateOverrides,
    diagnostics: &mut Diagnostics,
) -> Result<Option<GoalReport>> {
    if !enabled {
        return Ok(None);
    }
    let Some(goals) = Goals::from_config(config)? else {
        return Ok(None);
    };
    let priced = if goals.caps.is_empty() {
        Priced::default()
    } else {
        price_events(sessions, overrides)
    };
    Ok(Some(evaluate(
        &goals,
        window,
        now,
        human_intervals,
        &priced,
        diagnostics,
    )))
}

/// The pure core of [`for_report`], over already-priced events.
pub(crate) fn evaluate(
    goals: &Goals,
    window: ReportWindow,
    now: DateTime<Utc>,
    human_intervals: &[Interval],
    priced: &Priced,
    diagnostics: &mut Diagnostics,
) -> GoalReport {
    let today = local_day(now);
    // The days the report can have data for. An unbounded start is the earliest
    // day anything happened; a window reaching into the future stops today,
    // because a week that has not started has nothing to report yet.
    let earliest = || {
        human_intervals
            .iter()
            .map(|interval| local_day(interval.start))
            .chain(priced.events.iter().map(|event| local_day(event.at)))
            .min()
    };
    let window_first = window.0.map(local_day);
    let first = window_first.or_else(earliest).unwrap_or(today);
    let window_last = window
        .1
        .map(|until| local_day(until - Duration::milliseconds(1)));
    let last = window_last.unwrap_or(today).min(today);

    let in_window = |at: DateTime<Utc>| {
        window.0.is_none_or(|since| at >= since) && window.1.is_none_or(|until| at < until)
    };
    // How a period relates to the window. A period still running reaches the
    // end of the data without being cut short, so it is in progress, not
    // partial; the window ending before the data does is what cuts it short.
    let coverage = |period: Period, start: NaiveDate| {
        let end = period_end(period, start);
        let partial = window_first.is_some_and(|first| first > start)
            || window_last
                .is_some_and(|window_last| window_last < (end - Duration::days(1)).min(today));
        (partial, end > today)
    };

    let mut report = GoalReport {
        weekly_hours: goals.weekly_hours,
        daily_hours: goals.daily_hours,
        max_weekly_hours: goals.max_weekly_hours,
        weeks: Vec::new(),
        caps: Vec::new(),
        unpriced_events: if goals.caps.is_empty() {
            0
        } else {
            priced.unpriced
        },
    };

    if goals.has_hour_goal() {
        let mut days: BTreeMap<NaiveDate, f64> = BTreeMap::new();
        for interval in human_intervals {
            for (_, piece) in split_interval(interval, "day") {
                *days.entry(local_day(piece.start)).or_insert(0.0) += piece.seconds();
            }
        }
        let starts = periods(Period::Week, first, last, diagnostics);
        for start in starts {
            let end = period_end(Period::Week, start);
            let week_days: Vec<f64> = days
                .range(start..end)
                .map(|(_, seconds)| *seconds)
                .collect();
            let hours = week_days.iter().sum::<f64>() / 3600.0;
            let (partial, in_progress) = coverage(Period::Week, start);
            report.weeks.push(WeekProgress {
                week: period_label(Period::Week, start),
                start,
                hours,
                target_hours: goals.weekly_hours,
                percent_of_target: goals.weekly_hours.map(|target| hours / target),
                over_max: goals.max_weekly_hours.map(|most| hours > most),
                days_at_daily_target: goals.daily_hours.map(|target| {
                    week_days
                        .iter()
                        .filter(|seconds| **seconds / 3600.0 >= target)
                        .count()
                }),
                partial,
                in_progress,
            });
        }
    }

    for cap in &goals.caps {
        let mut totals: BTreeMap<NaiveDate, f64> = BTreeMap::new();
        for event in priced
            .events
            .iter()
            .filter(|event| in_window(event.at) && event.pool.as_deref() == Some(cap.pool.as_str()))
        {
            *totals
                .entry(period_start(cap.period, local_day(event.at)))
                .or_insert(0.0) += event.usd;
        }
        for start in periods(cap.period, first, last, diagnostics) {
            let value = totals.get(&start).copied().unwrap_or(0.0);
            let fraction = value / cap.usd;
            let (partial, in_progress) = coverage(cap.period, start);
            report.caps.push(CapProgress {
                pool: cap.pool.clone(),
                period: cap.period,
                label: period_label(cap.period, start),
                usd: cap.usd,
                value_usd: value,
                fraction,
                warn_at: cap.warn_at,
                status: cap_status(fraction, cap.warn_at),
                partial,
                in_progress,
            });
        }
    }
    report
        .caps
        .sort_by(|a, b| (a.period, &a.pool, &a.label).cmp(&(b.period, &b.pool, &b.label)));

    for week in &report.weeks {
        if week.over_max == Some(true) {
            diagnostics.warn(format!(
                "goals: {} has {} of human time, above max_weekly_hours {}{}",
                week.week,
                hours_text(week.hours * 3600.0),
                trimmed(goals.max_weekly_hours.unwrap_or_default()),
                lower_bound(week.partial)
            ));
        }
    }
    for cap in &report.caps {
        match cap.status {
            CapStatus::Ok => {}
            CapStatus::Warn => diagnostics.warn(format!(
                "goals: {} {} list value is {} of the ${} cap for {} (warns at {}){}",
                cap.pool,
                cap.period.adjective(),
                percent_text(cap.fraction),
                number(cap.usd.round() as u64),
                cap.label,
                percent_text(cap.warn_at),
                lower_bound(cap.partial)
            )),
            CapStatus::Reached => diagnostics.warn(format!(
                "goals: {} {} list value has reached the ${} cap for {} ({}){}",
                cap.pool,
                cap.period.adjective(),
                number(cap.usd.round() as u64),
                cap.label,
                percent_text(cap.fraction),
                lower_bound(cap.partial)
            )),
        }
    }
    if report.unpriced_events > 0 {
        diagnostics.warn(format!(
            "goals: {} token event(s) have no list rate and are not in any cap; set \"model_rates\" for those models",
            report.unpriced_events
        ));
    }
    if !goals.caps.is_empty()
        && let Some(stale) = pricing::stale_rates_warning(today)
    {
        diagnostics.warn(stale);
    }
    report
}

fn lower_bound(partial: bool) -> &'static str {
    if partial {
        "; the window covers only part of the period, so this is a lower bound"
    } else {
        ""
    }
}

fn cap_status(fraction: f64, warn_at: f64) -> CapStatus {
    // Compared with a hair of tolerance so 0.8 of 1,500 dollars is a warning at
    // exactly $1,200 and not at $1,200.0000001 because of float representation.
    if fraction >= 1.0 - 1e-9 {
        CapStatus::Reached
    } else if fraction >= warn_at - 1e-9 {
        CapStatus::Warn
    } else {
        CapStatus::Ok
    }
}

/// The starts of the periods from the one holding `first` through the one
/// holding `last`, newest `MAX_PERIODS` at most.
fn periods(
    period: Period,
    first: NaiveDate,
    last: NaiveDate,
    diagnostics: &mut Diagnostics,
) -> Vec<NaiveDate> {
    let mut starts = Vec::new();
    if last < first {
        return starts;
    }
    let mut start = period_start(period, first);
    while start <= last {
        starts.push(start);
        let next = period_end(period, start);
        if next <= start {
            break;
        }
        start = next;
    }
    if starts.len() > MAX_PERIODS {
        let dropped = starts.len() - MAX_PERIODS;
        starts.drain(..dropped);
        diagnostics.note(format!(
            "goals: {dropped} older {} period(s) left out; only the latest {MAX_PERIODS} are shown",
            period.name()
        ));
    }
    starts
}

fn hours_text(seconds: f64) -> String {
    let minutes = (seconds / 60.0).round().max(0.0) as u64;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

fn percent_text(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

/// `37.5` as `37.5`, `40.0` as `40`.
pub(crate) fn trimmed(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

impl GoalReport {
    /// The report's goal lines, shared by the table and the Markdown and HTML
    /// documents so a figure cannot read differently between them.
    pub(crate) fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for week in &self.weeks {
            let mut line = format!("{}: {} human", week.week, hours_text(week.hours * 3600.0));
            if let (Some(target), Some(share)) = (week.target_hours, week.percent_of_target) {
                line.push_str(&format!(
                    " of {}h target ({})",
                    trimmed(target),
                    percent_text(share)
                ));
            }
            if week.over_max == Some(true) {
                line.push_str(&format!(
                    "; above the {}h maximum",
                    trimmed(self.max_weekly_hours.unwrap_or_default())
                ));
            }
            if let (Some(days), Some(target)) = (week.days_at_daily_target, self.daily_hours) {
                line.push_str(&format!(
                    "; {days} {} at {}h or more",
                    if days == 1 { "day" } else { "days" },
                    trimmed(target)
                ));
            }
            line.push_str(&flags(week.partial, week.in_progress));
            lines.push(line);
        }
        for cap in &self.caps {
            let marker = match cap.status {
                CapStatus::Ok => "",
                CapStatus::Warn => "  ⚠ near the cap",
                CapStatus::Reached => "  ⚠ cap reached",
            };
            lines.push(format!(
                "{} {} {}: ${} of ${} list value ({}){marker}{}",
                safe_text(&cap.pool),
                cap.period.adjective(),
                cap.label,
                number(cap.value_usd.round() as u64),
                number(cap.usd.round() as u64),
                percent_text(cap.fraction),
                flags(cap.partial, cap.in_progress)
            ));
        }
        if self.unpriced_events > 0 {
            lines.push(format!(
                "{} token event(s) have no list rate and are left out of the caps.",
                self.unpriced_events
            ));
        }
        lines
    }
}

fn flags(partial: bool, in_progress: bool) -> String {
    match (partial, in_progress) {
        (true, true) => "  (partial window, in progress)".to_string(),
        (true, false) => "  (partial window)".to_string(),
        (false, true) => "  (in progress)".to_string(),
        (false, false) => String::new(),
    }
}

/// What `workstats now` shows of the goals: the week so far against its
/// target, the highest cap share, and the warnings a prompt should carry.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct NowStatus {
    pub week_target_hours: Option<f64>,
    /// Week hours as a share of the target, 1.0 being on target.
    pub week_fraction: Option<f64>,
    /// The highest share of any cap, 1.0 being the cap.
    pub cap_fraction: Option<f64>,
    /// Short prompt-sized warnings, for example `⚠ claude 88%`.
    pub warnings: Vec<String>,
}

/// Evaluates the current week and month to date. `week_pools` and
/// `month_pools` are list value per pool since each period began.
pub(crate) fn now_status(
    goals: &Goals,
    week_human_seconds: f64,
    week_pools: &BTreeMap<String, f64>,
    month_pools: &BTreeMap<String, f64>,
) -> NowStatus {
    let week_hours = week_human_seconds / 3600.0;
    let mut status = NowStatus {
        week_target_hours: goals.weekly_hours,
        week_fraction: goals.weekly_hours.map(|target| week_hours / target),
        ..NowStatus::default()
    };
    if let Some(most) = goals.max_weekly_hours
        && week_hours > most
    {
        status.warnings.push(format!(
            "⚠ week {}h/{}h",
            trimmed((week_hours * 10.0).round() / 10.0),
            trimmed(most)
        ));
    }
    for cap in &goals.caps {
        let pools = match cap.period {
            Period::Week => week_pools,
            Period::Month => month_pools,
        };
        let fraction = pools.get(&cap.pool).copied().unwrap_or(0.0) / cap.usd;
        status.cap_fraction = Some(
            status
                .cap_fraction
                .map_or(fraction, |most| most.max(fraction)),
        );
        if cap_status(fraction, cap.warn_at) != CapStatus::Ok {
            status
                .warnings
                .push(format!("⚠ {} {}", cap.pool, percent_text(fraction)));
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn local(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("an unambiguous local time")
            .with_timezone(&Utc)
    }

    fn piece(start: DateTime<Utc>, minutes: i64) -> Interval {
        Interval {
            start,
            end: start + Duration::minutes(minutes),
            provider: "claude".to_string(),
            model: "claude-opus-5".to_string(),
            session_id: "work-block:0".to_string(),
            cwd: "/work".to_string(),
            repo: "work".to_string(),
            repo_id: "local:work".to_string(),
            root: "/work".to_string(),
            branch: None,
        }
    }

    fn goals(value: serde_json::Value) -> Goals {
        Goals::from_config(Some(&value)).unwrap().unwrap()
    }

    fn event(at: DateTime<Utc>, pool: &str, usd: f64) -> PricedEvent {
        PricedEvent {
            at,
            pool: Some(pool.to_string()),
            usd,
        }
    }

    fn priced(events: Vec<PricedEvent>) -> Priced {
        Priced {
            events,
            unpriced: 0,
        }
    }

    #[test]
    fn an_absent_or_empty_block_is_no_goal() {
        assert_eq!(None, Goals::from_config(None).unwrap());
        assert_eq!(None, Goals::from_config(Some(&json!(null))).unwrap());
        assert_eq!(None, Goals::from_config(Some(&json!({}))).unwrap());
    }

    #[test]
    fn a_bad_block_is_refused_with_the_entry_named() {
        for (value, expected) in [
            (json!({"weekly_hourz": 30}), "unknown field"),
            (json!({"weekly_hours": 0}), "weekly_hours"),
            (json!({"daily_hours": 25}), "daily_hours"),
            (
                json!({"weekly_hours": 40, "max_weekly_hours": 30}),
                "max_weekly_hours",
            ),
            (
                json!({"list_value_caps": [{"pool": "claude", "period": "year", "usd": 5}]}),
                "year",
            ),
            (
                json!({"list_value_caps": [{"pool": "mistral", "period": "week", "usd": 5}]}),
                "list_value_caps[0].pool",
            ),
            (
                json!({"list_value_caps": [{"pool": "claude", "period": "week", "usd": 0}]}),
                "usd",
            ),
            (
                json!({"list_value_caps": [{"pool": "claude", "period": "week", "usd": 5, "warn_at": 1.5}]}),
                "warn_at",
            ),
            (
                json!({"list_value_caps": [
                    {"pool": "claude", "period": "week", "usd": 5},
                    {"pool": "Anthropic", "period": "week", "usd": 6}]}),
                "already has",
            ),
        ] {
            let error = format!("{:#}", Goals::from_config(Some(&value)).unwrap_err());
            assert!(error.contains("goals"), "{error}");
            assert!(error.contains(expected), "{expected} not in {error}");
        }
    }

    #[test]
    fn pools_accept_the_aliases_allocate_does() {
        let parsed = goals(json!({"list_value_caps": [
            {"pool": "codex", "period": "month", "usd": 400},
            {"pool": "Copilot", "period": "week", "usd": 20, "warn_at": 0.5}]}));
        assert_eq!("openai", parsed.caps[0].pool);
        assert_eq!(DEFAULT_WARN_AT, parsed.caps[0].warn_at);
        assert_eq!("copilot", parsed.caps[1].pool);
        assert_eq!(0.5, parsed.caps[1].warn_at);
    }

    #[test]
    fn weeks_report_hours_against_the_target_and_the_daily_goal() {
        // 2026-08-17 is a Monday. Two 8h days and one 2h day in W34.
        let parsed = goals(json!({"weekly_hours": 20, "daily_hours": 7.5, "max_weekly_hours": 40}));
        let pieces = vec![
            piece(local(2026, 8, 17, 9), 8 * 60),
            piece(local(2026, 8, 18, 9), 8 * 60),
            piece(local(2026, 8, 19, 9), 2 * 60),
        ];
        let window = (Some(local(2026, 8, 17, 0)), Some(local(2026, 8, 24, 0)));
        let mut diagnostics = Diagnostics::default();
        let report = evaluate(
            &parsed,
            window,
            local(2026, 8, 30, 12),
            &pieces,
            &Priced::default(),
            &mut diagnostics,
        );
        assert_eq!(1, report.weeks.len());
        let week = &report.weeks[0];
        assert_eq!("2026-W34", week.week);
        assert!((week.hours - 18.0).abs() < 1e-9);
        assert!((week.percent_of_target.unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(Some(2), week.days_at_daily_target);
        assert_eq!(Some(false), week.over_max);
        assert!(!week.partial && !week.in_progress);
        assert!(
            diagnostics.messages.is_empty(),
            "{:?}",
            diagnostics.messages
        );
    }

    #[test]
    fn exceeding_the_weekly_maximum_warns() {
        let parsed = goals(json!({"max_weekly_hours": 10}));
        let pieces = vec![piece(local(2026, 8, 17, 9), 11 * 60)];
        let mut diagnostics = Diagnostics::default();
        let report = evaluate(
            &parsed,
            (Some(local(2026, 8, 17, 0)), Some(local(2026, 8, 24, 0))),
            local(2026, 8, 30, 12),
            &pieces,
            &Priced::default(),
            &mut diagnostics,
        );
        assert_eq!(Some(true), report.weeks[0].over_max);
        assert_eq!(1, diagnostics.messages.len());
        assert!(diagnostics.messages[0].contains("2026-W34"));
        assert!(diagnostics.messages[0].contains("max_weekly_hours 10"));
    }

    #[test]
    fn a_cap_warns_exactly_at_warn_at_and_not_below_it() {
        let parsed = goals(
            json!({"list_value_caps": [{"pool": "claude", "period": "month", "usd": 1500, "warn_at": 0.8}]}),
        );
        let window = (Some(local(2026, 8, 1, 0)), Some(local(2026, 9, 1, 0)));
        let now = local(2026, 9, 2, 12);
        let run = |usd: f64| {
            let mut diagnostics = Diagnostics::default();
            let report = evaluate(
                &parsed,
                window,
                now,
                &[],
                &priced(vec![event(local(2026, 8, 10, 9), "claude", usd)]),
                &mut diagnostics,
            );
            (report, diagnostics)
        };
        let (below, diagnostics) = run(1199.99);
        assert_eq!(CapStatus::Ok, below.caps[0].status);
        assert!(
            !diagnostics.messages.iter().any(|m| m.contains("cap")),
            "{:?}",
            diagnostics.messages
        );

        let (at, diagnostics) = run(1200.0);
        assert_eq!(CapStatus::Warn, at.caps[0].status);
        assert!(
            diagnostics.messages[0].contains("80%"),
            "{:?}",
            diagnostics.messages
        );

        let (over, diagnostics) = run(1500.0);
        assert_eq!(CapStatus::Reached, over.caps[0].status);
        assert!(diagnostics.messages[0].contains("reached"));
    }

    #[test]
    fn caps_are_kept_per_pool_and_period_and_ignore_other_pools() {
        let parsed = goals(json!({"list_value_caps": [
            {"pool": "claude", "period": "month", "usd": 100},
            {"pool": "openai", "period": "week", "usd": 50}]}));
        let window = (Some(local(2026, 8, 3, 0)), Some(local(2026, 8, 10, 0)));
        let report = evaluate(
            &parsed,
            window,
            local(2026, 8, 12, 12),
            &[],
            &priced(vec![
                event(local(2026, 8, 4, 9), "claude", 30.0),
                event(local(2026, 8, 5, 9), "openai", 10.0),
                event(local(2026, 8, 5, 10), "claude", 5.0),
                // Outside the window: not counted.
                event(local(2026, 8, 20, 9), "claude", 999.0),
            ]),
            &mut Diagnostics::default(),
        );
        let claude = report.caps.iter().find(|c| c.pool == "claude").unwrap();
        let openai = report.caps.iter().find(|c| c.pool == "openai").unwrap();
        assert_eq!(35.0, claude.value_usd);
        assert_eq!("2026-08", claude.label);
        assert_eq!(10.0, openai.value_usd);
        assert_eq!("2026-W32", openai.label);
    }

    #[test]
    fn periods_the_window_only_touches_are_flagged_partial() {
        let parsed =
            goals(json!({"list_value_caps": [{"pool": "claude", "period": "month", "usd": 100}]}));
        // A window that starts mid-July and ends mid-August: both months are cut.
        let window = (Some(local(2026, 7, 15, 0)), Some(local(2026, 8, 15, 0)));
        let report = evaluate(
            &parsed,
            window,
            local(2026, 9, 20, 12),
            &[],
            &Priced::default(),
            &mut Diagnostics::default(),
        );
        assert_eq!(2, report.caps.len());
        assert_eq!("2026-07", report.caps[0].label);
        assert!(report.caps[0].partial);
        assert_eq!("2026-08", report.caps[1].label);
        assert!(report.caps[1].partial);
        assert!(!report.caps[1].in_progress);

        // A whole calendar month that has ended is complete.
        let whole = evaluate(
            &parsed,
            (Some(local(2026, 8, 1, 0)), Some(local(2026, 9, 1, 0))),
            local(2026, 9, 20, 12),
            &[],
            &Priced::default(),
            &mut Diagnostics::default(),
        );
        assert_eq!(1, whole.caps.len());
        assert!(!whole.caps[0].partial && !whole.caps[0].in_progress);
    }

    #[test]
    fn the_current_month_is_in_progress_not_partial() {
        let parsed =
            goals(json!({"list_value_caps": [{"pool": "claude", "period": "month", "usd": 100}]}));
        let now = local(2026, 8, 12, 12);
        // `--month current`: the window ends at the start of next month.
        let report = evaluate(
            &parsed,
            (Some(local(2026, 8, 1, 0)), Some(local(2026, 9, 1, 0))),
            now,
            &[],
            &Priced::default(),
            &mut Diagnostics::default(),
        );
        assert_eq!(1, report.caps.len());
        assert!(!report.caps[0].partial);
        assert!(report.caps[0].in_progress);

        // Ending before today cuts it short.
        let cut = evaluate(
            &parsed,
            (Some(local(2026, 8, 1, 0)), Some(local(2026, 8, 6, 0))),
            now,
            &[],
            &Priced::default(),
            &mut Diagnostics::default(),
        );
        assert!(cut.caps[0].partial);
    }

    #[test]
    fn unpriced_usage_is_reported_not_counted_as_zero() {
        let parsed =
            goals(json!({"list_value_caps": [{"pool": "claude", "period": "week", "usd": 10}]}));
        let mut diagnostics = Diagnostics::default();
        let report = evaluate(
            &parsed,
            (Some(local(2026, 8, 17, 0)), Some(local(2026, 8, 24, 0))),
            local(2026, 8, 30, 12),
            &[],
            &Priced {
                events: Vec::new(),
                unpriced: 3,
            },
            &mut diagnostics,
        );
        assert_eq!(3, report.unpriced_events);
        assert!(
            diagnostics
                .messages
                .iter()
                .any(|m| m.contains("3 token event(s)"))
        );
        assert!(
            report
                .lines()
                .iter()
                .any(|line| line.contains("no list rate"))
        );
    }

    #[test]
    fn disabled_or_unset_goals_produce_no_report() {
        let value = json!({"weekly_hours": 30});
        let mut diagnostics = Diagnostics::default();
        let overrides = RateOverrides::default();
        let call = |enabled: bool, config: Option<&serde_json::Value>, d: &mut Diagnostics| {
            for_report(
                enabled,
                config,
                (None, None),
                local(2026, 8, 12, 12),
                &[],
                &[],
                &overrides,
                d,
            )
            .unwrap()
        };
        assert!(call(false, Some(&value), &mut diagnostics).is_none());
        assert!(call(true, None, &mut diagnostics).is_none());
        assert!(call(true, Some(&value), &mut diagnostics).is_some());
        // A bad block is only an error while goals are on.
        let bad = json!({"weekly_hours": -1});
        assert!(call(false, Some(&bad), &mut diagnostics).is_none());
    }

    #[test]
    fn now_status_warns_on_caps_and_the_weekly_maximum() {
        let parsed = goals(json!({"weekly_hours": 40, "max_weekly_hours": 50,
            "list_value_caps": [
                {"pool": "claude", "period": "month", "usd": 1000, "warn_at": 0.8},
                {"pool": "openai", "period": "week", "usd": 100}]}));
        let week: BTreeMap<String, f64> = [("openai".to_string(), 10.0)].into();
        let month: BTreeMap<String, f64> = [("claude".to_string(), 880.0)].into();
        let status = now_status(&parsed, 20.0 * 3600.0, &week, &month);
        assert_eq!(vec!["⚠ claude 88%".to_string()], status.warnings);
        assert_eq!(Some(0.5), status.week_fraction);
        assert_eq!(Some(0.88), status.cap_fraction);

        let busy = now_status(&parsed, 52.0 * 3600.0, &week, &month);
        assert_eq!("⚠ week 52h/50h", busy.warnings[0]);
    }

    #[test]
    fn lines_describe_weeks_and_caps_in_plain_text() {
        let report = GoalReport {
            weekly_hours: Some(37.5),
            daily_hours: None,
            max_weekly_hours: None,
            weeks: vec![WeekProgress {
                week: "2026-W34".to_string(),
                start: NaiveDate::from_ymd_opt(2026, 8, 17).unwrap(),
                hours: 30.0,
                target_hours: Some(37.5),
                percent_of_target: Some(0.8),
                over_max: None,
                days_at_daily_target: None,
                partial: true,
                in_progress: false,
            }],
            caps: vec![CapProgress {
                pool: "claude".to_string(),
                period: Period::Month,
                label: "2026-08".to_string(),
                usd: 1500.0,
                value_usd: 1320.0,
                fraction: 0.88,
                warn_at: 0.8,
                status: CapStatus::Warn,
                partial: false,
                in_progress: true,
            }],
            unpriced_events: 0,
        };
        let lines = report.lines();
        assert_eq!(
            "2026-W34: 30h 00m human of 37.5h target (80%)  (partial window)",
            lines[0]
        );
        assert_eq!(
            "claude monthly 2026-08: $1,320 of $1,500 list value (88%)  ⚠ near the cap  (in progress)",
            lines[1]
        );
    }

    #[test]
    fn more_periods_than_the_limit_keep_the_latest_and_say_so() {
        let mut diagnostics = Diagnostics::default();
        let first = NaiveDate::from_ymd_opt(2010, 1, 4).unwrap();
        let last = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let starts = periods(Period::Week, first, last, &mut diagnostics);
        assert_eq!(MAX_PERIODS, starts.len());
        assert_eq!(week_start(last), *starts.last().unwrap());
        assert_eq!(1, diagnostics.notes.len());
    }
}
