//! Locks: a period's figures frozen as submitted.
//!
//! A lock stores the entries and totals a period had when it was locked, the
//! settings that produced them and fingerprints of the engagement
//! configuration and the ledger. Viewing a locked period shows the snapshot's
//! entries (status `Locked`), never a recomputation, so what was submitted
//! stays what is displayed. The current computation is still made, and where
//! it disagrees with the snapshot the difference is listed as drift with the
//! most likely cause.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::compute::{self, round_to_cents, span_text};
use super::ledger::{Context, in_window};
use super::model::{
    DriftRow, EntryStatus, Timesheet, TimesheetEntry, TimesheetSettings, TimesheetWindow,
};
use crate::timeutil::{month_span, parse_bound, week_span};

/// Every setting that shapes the figures, as text, plus fingerprints of what
/// else they depend on. Text rather than numbers so the file reads the way the
/// flags are typed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct LockSettings {
    pub(crate) increment: String,
    pub(crate) rounding: String,
    pub(crate) split: String,
    pub(crate) min_entry: String,
    #[serde(default)]
    pub(crate) drop_below: String,
    pub(crate) daily_cap: Option<String>,
    pub(crate) human_idle: String,
    pub(crate) review_credit: String,
    pub(crate) gap_cap: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    pub(crate) engagements_fingerprint: String,
    pub(crate) ledger_fingerprint: String,
}

fn word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl LockSettings {
    /// The settings of a run. `unassigned` is left out on purpose: it hides
    /// entries from the display and changes no figure.
    pub(crate) fn current(
        settings: &TimesheetSettings,
        gap_cap: chrono::Duration,
        human_idle: chrono::Duration,
        review_credit: chrono::Duration,
        engagements_fingerprint: &str,
        ledger_fingerprint: &str,
    ) -> Self {
        let span = |duration: chrono::Duration| span_text(duration.num_seconds().max(0) as u64);
        Self {
            increment: span_text(settings.increment_seconds),
            rounding: word(&settings.rounding),
            split: word(&settings.split),
            min_entry: span_text(settings.min_entry_seconds),
            drop_below: span_text(settings.drop_below_seconds),
            daily_cap: settings.daily_cap_seconds.map(span_text),
            human_idle: span(human_idle),
            review_credit: span(review_credit),
            gap_cap: span(gap_cap),
            detail: settings.detail.as_ref().map(word),
            engagements_fingerprint: engagements_fingerprint.to_string(),
            ledger_fingerprint: ledger_fingerprint.to_string(),
        }
    }

    /// The names of the settings that differ, leaving the two fingerprints to
    /// their own causes.
    pub(crate) fn differences(&self, other: &Self) -> Vec<&'static str> {
        let mut names = Vec::new();
        let mut check = |name: &'static str, same: bool| {
            if !same {
                names.push(name);
            }
        };
        check("increment", self.increment == other.increment);
        check("rounding", self.rounding == other.rounding);
        check("split", self.split == other.split);
        check("minimum entry", self.min_entry == other.min_entry);
        check("drop-below", self.drop_below == other.drop_below);
        check("daily cap", self.daily_cap == other.daily_cap);
        check("human idle", self.human_idle == other.human_idle);
        check("review credit", self.review_credit == other.review_credit);
        check("gap cap", self.gap_cap == other.gap_cap);
        check("detail", self.detail == other.detail);
        names
    }

    /// One line for `locks` and the lock summary.
    pub(crate) fn summary(&self) -> String {
        let mut text = format!(
            "{} to {}, split {}",
            self.rounding, self.increment, self.split
        );
        if self.min_entry != "0m" {
            text.push_str(&format!(", minimum entry {}", self.min_entry));
        }
        if let Some(cap) = &self.daily_cap {
            text.push_str(&format!(", daily cap {cap}"));
        }
        text.push_str(&format!(
            "; human idle {}, review credit {}, gap cap {}",
            self.human_idle, self.review_credit, self.gap_cap
        ));
        text
    }
}

/// One entry as it was submitted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct LockEntry {
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client: Option<String>,
    pub(crate) final_seconds: u64,
    pub(crate) billable: bool,
    pub(crate) rate: Option<f64>,
    pub(crate) currency: Option<String>,
    pub(crate) amount: Option<f64>,
    #[serde(default)]
    pub(crate) notes: Vec<String>,
    /// What the entry's description was when the lock was taken, because that
    /// is what was submitted. Present only when one was asked for.
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct LockTotals {
    pub(crate) seconds: u64,
    pub(crate) amounts: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Lock {
    /// `YYYY-MM`, `YYYY-Www` or `A..B`.
    pub(crate) period: String,
    /// Half-open, as local instants.
    pub(crate) since: DateTime<FixedOffset>,
    pub(crate) until: DateTime<FixedOffset>,
    pub(crate) locked_at: DateTime<Utc>,
    pub(crate) workstats_version: String,
    pub(crate) settings: LockSettings,
    pub(crate) entries: Vec<LockEntry>,
    pub(crate) totals: LockTotals,
}

impl Lock {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.period.trim().is_empty() {
            bail!("it has no period");
        }
        if self.since >= self.until {
            bail!("it ends before it starts");
        }
        for entry in &self.entries {
            super::ledger::check_seconds(entry.final_seconds)?;
        }
        Ok(())
    }

    /// Whether a local day is inside the locked period.
    pub(crate) fn covers(&self, date: NaiveDate) -> bool {
        let start = super::ledger::day_start(date);
        start >= self.since && start < self.until
    }

    /// Whether the period shares any time with a window.
    fn overlaps(&self, window: &TimesheetWindow) -> bool {
        window.until.is_none_or(|until| self.since < until)
            && window.since.is_none_or(|since| self.until > since)
    }

    pub(crate) fn overlaps_period(&self, other: &Period) -> bool {
        self.since < other.until && self.until > other.since
    }
}

// ---------------------------------------------------------------- period

/// How a period was spelled, so the report flags that give the same window
/// can be set from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeriodKind {
    Month,
    Week,
    Range(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Period {
    pub(crate) label: String,
    pub(crate) kind: PeriodKind,
    pub(crate) since: DateTime<Utc>,
    pub(crate) until: DateTime<Utc>,
}

const PERIOD_FORMS: &str =
    "a period is YYYY-MM, YYYY-Www (ISO week) or A..B with A and B each YYYY-MM-DD or YYYY-MM";

/// `YYYY-MM`, `YYYY-Www` or `A..B`. Relative words (`current`, `last`) are
/// refused: a lock is named by what it covers, and "last" would mean a
/// different period tomorrow.
pub(crate) fn parse_period(value: &str) -> Result<Period> {
    let value = value.trim();
    let reference = Utc::now();
    let month = Regex::new(r"^\d{4}-\d{2}$").expect("static regex");
    let week = Regex::new(r"^\d{4}-[Ww]\d{2}$").expect("static regex");
    if month.is_match(value) {
        let (since, until) = month_span(value, reference)?;
        return Ok(Period {
            label: value.to_string(),
            kind: PeriodKind::Month,
            since,
            until,
        });
    }
    if week.is_match(value) {
        let (since, until) = week_span(value, reference)?;
        return Ok(Period {
            label: format!("{}-W{}", &value[..4], &value[6..]),
            kind: PeriodKind::Week,
            since,
            until,
        });
    }
    if let Some((first, last)) = value.split_once("..") {
        let (Some(since), Some(until)) = (
            parse_bound(Some(first), false).ok().flatten(),
            parse_bound(Some(last), true).ok().flatten(),
        ) else {
            bail!("invalid period {value:?}: {PERIOD_FORMS}");
        };
        if since >= until {
            bail!("invalid period {value:?}: it ends before it starts");
        }
        return Ok(Period {
            label: format!("{first}..{last}"),
            kind: PeriodKind::Range(first.to_string(), last.to_string()),
            since,
            until,
        });
    }
    bail!("invalid period {value:?}: {PERIOD_FORMS}")
}

// -------------------------------------------------------------- snapshot

/// Freezes a computed timesheet's entries.
pub(crate) fn snapshot(
    period: &Period,
    entries: &[TimesheetEntry],
    settings: LockSettings,
    now: DateTime<Utc>,
) -> Lock {
    let mut cents: BTreeMap<String, i64> = BTreeMap::new();
    let mut seconds = 0;
    let stored: Vec<LockEntry> = entries
        .iter()
        .map(|entry| {
            seconds += entry.final_seconds;
            if let (Some(amount), Some(currency)) = (entry.amount, &entry.currency) {
                *cents.entry(currency.clone()).or_default() += (amount * 100.0).round() as i64;
            }
            LockEntry {
                date: entry.date,
                engagement: entry.engagement.clone(),
                detail: entry.detail.clone(),
                label: Some(entry.label.clone()),
                client: entry.client.clone(),
                final_seconds: entry.final_seconds,
                billable: entry.billable,
                rate: entry.rate,
                currency: entry.currency.clone(),
                amount: entry.amount,
                notes: entry.notes.clone(),
                description: entry.description.clone(),
            }
        })
        .collect();
    Lock {
        period: period.label.clone(),
        since: period.since.with_timezone(&Local).fixed_offset(),
        until: period.until.with_timezone(&Local).fixed_offset(),
        locked_at: now,
        workstats_version: env!("CARGO_PKG_VERSION").to_string(),
        settings,
        entries: stored,
        totals: LockTotals {
            seconds,
            amounts: cents
                .into_iter()
                .map(|(currency, cents)| (currency, round_to_cents(cents as f64 / 100.0)))
                .collect(),
        },
    }
}

// ----------------------------------------------------------------- apply

type Key = (NaiveDate, String, Option<String>);

fn key_of(entry: &TimesheetEntry) -> Key {
    (entry.date, entry.engagement.clone(), entry.detail.clone())
}

/// Replaces the days of every lock overlapping the window with the lock's
/// snapshot, and lists where the live computation disagrees.
pub(crate) fn apply_locks(timesheet: &mut Timesheet, context: &Context<'_>) {
    let window = timesheet.window.clone();
    let locks: Vec<&Lock> = context
        .ledger
        .locks
        .iter()
        .filter(|lock| lock.overlaps(&window))
        .collect();
    if locks.is_empty() {
        return;
    }
    // The live values to compare against are the finished ones.
    compute::finalize(&mut timesheet.entries);

    let mut drifted_periods: Vec<String> = Vec::new();
    let mut net: i64 = 0;
    for lock in locks {
        let covered = |date: NaiveDate| in_window(&window, date) && lock.covers(date);
        let entries = std::mem::take(&mut timesheet.entries);
        let (inside, outside): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|entry| covered(entry.date));
        let live: BTreeMap<Key, TimesheetEntry> = inside
            .into_iter()
            .map(|entry| (key_of(&entry), entry))
            .collect();
        let snapshot: BTreeMap<Key, &LockEntry> = lock
            .entries
            .iter()
            .filter(|entry| covered(entry.date))
            .map(|entry| {
                (
                    (entry.date, entry.engagement.clone(), entry.detail.clone()),
                    entry,
                )
            })
            .collect();

        let causes = Causes::new(lock, context);
        let keys: BTreeSet<&Key> = live.keys().chain(snapshot.keys()).collect();
        let mut kept = outside;
        let mut drifted = false;
        for key in keys {
            let locked = snapshot.get(key).map_or(0, |entry| entry.final_seconds);
            let current = live.get(key).map_or(0, |entry| entry.final_seconds);
            let difference = current as i64 - locked as i64;
            if difference != 0 {
                drifted = true;
                net += difference;
                timesheet.drift.push(DriftRow {
                    period: lock.period.clone(),
                    date: key.0,
                    engagement: key.1.clone(),
                    detail: key.2.clone(),
                    locked_seconds: locked,
                    current_seconds: current,
                    difference_seconds: difference,
                    cause: causes.of(key),
                });
            }
            if let Some(stored) = snapshot.get(key) {
                kept.push(locked_entry(stored, live.get(key), difference, context));
            }
        }
        timesheet.entries = kept;
        timesheet.dropped.retain(|entry| !covered(entry.date));
        if drifted {
            drifted_periods.push(lock.period.clone());
        }
        timesheet.applied_locks.push(format!(
            "{} (locked {})",
            lock.period,
            lock.locked_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
        ));
    }
    timesheet
        .entries
        .sort_by(|left, right| compute::entry_order(left).cmp(&compute::entry_order(right)));
    if !timesheet.drift.is_empty() {
        let one = timesheet.drift.len() == 1;
        timesheet.warnings.push(format!(
            "{} entr{} in locked period {} {} from the current computation ({}); the locked figures are shown, see the DRIFT section, or use --ignore-locks for the live ones",
            timesheet.drift.len(),
            if one { "y" } else { "ies" },
            drifted_periods.join(", "),
            if one { "differs" } else { "differ" },
            signed_span(net)
        ));
    }
}

fn signed_span(seconds: i64) -> String {
    format!(
        "{}{} net",
        if seconds < 0 { "-" } else { "+" },
        span_text(seconds.unsigned_abs())
    )
}

/// An entry from the snapshot, keeping what the live computation can still say
/// about it (the evidence, the first start) without changing what was
/// submitted.
fn locked_entry(
    stored: &LockEntry,
    live: Option<&TimesheetEntry>,
    drift: i64,
    context: &Context<'_>,
) -> TimesheetEntry {
    let configured = context.engagements.get(&stored.engagement);
    TimesheetEntry {
        date: stored.date,
        engagement: stored.engagement.clone(),
        detail: stored.detail.clone(),
        label: stored
            .label
            .clone()
            .or_else(|| configured.map(|engagement| engagement.label.clone()))
            .unwrap_or_else(|| stored.engagement.clone()),
        client: stored
            .client
            .clone()
            .or_else(|| configured.and_then(|engagement| engagement.client.clone())),
        billable: stored.billable,
        raw_seconds: live.map_or(0.0, |entry| entry.raw_seconds),
        estimated_seconds: stored.final_seconds,
        manual_seconds: 0,
        override_seconds: None,
        final_seconds: stored.final_seconds,
        first_start: live.and_then(|entry| entry.first_start),
        last_end: live.and_then(|entry| entry.last_end),
        evidence: live.map(|entry| entry.evidence.clone()).unwrap_or_default(),
        rate: stored.rate,
        currency: stored.currency.clone(),
        amount: stored.amount,
        notes: stored.notes.clone(),
        description: stored.description.clone(),
        status: EntryStatus::Locked,
        adjustments: Vec::new(),
        lock_drift_seconds: Some(drift),
    }
}

/// Why a locked period may differ from now, decided once per lock.
struct Causes<'a> {
    lock: &'a Lock,
    settings: Vec<&'static str>,
    engagements: bool,
    ledger: bool,
    context: &'a Context<'a>,
}

impl<'a> Causes<'a> {
    fn new(lock: &'a Lock, context: &'a Context<'a>) -> Self {
        Self {
            lock,
            settings: lock.settings.differences(&context.current),
            engagements: lock.settings.engagements_fingerprint
                != context.current.engagements_fingerprint,
            ledger: lock.settings.ledger_fingerprint != context.current.ledger_fingerprint,
            context,
        }
    }

    /// Whether the ledger holds a change to this day and engagement made after
    /// the lock: a forced write, or an item created since.
    fn ledger_touched(&self, key: &Key) -> bool {
        let ledger = self.context.ledger;
        let after = self.lock.locked_at;
        ledger
            .forced_writes
            .iter()
            .any(|write| write.date == key.0 && write.engagement == key.1 && write.at > after)
            || ledger.entries.iter().any(|entry| {
                entry.date == key.0 && entry.engagement == key.1 && entry.created_at > after
            })
            || ledger.overrides.iter().any(|item| {
                item.date == key.0 && item.engagement == key.1 && item.created_at > after
            })
    }

    /// The most likely cause, in order: settings, engagement configuration,
    /// the ledger, and otherwise the history itself (new sessions, or old ones
    /// the tools have since pruned).
    fn of(&self, key: &Key) -> String {
        if !self.settings.is_empty() {
            format!("settings changed ({})", self.settings.join(", "))
        } else if self.engagements {
            "engagement config changed".to_string()
        } else if self.ledger && self.ledger_touched(key) {
            "ledger edited after lock".to_string()
        } else {
            "new or pruned history".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::engagement::Engagements;
    use crate::timesheet::ledger::{Ledger, ManualEntry, day_start};
    use crate::timesheet::model::{Evidence, TimesheetMethodology};

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
    }

    fn engagements() -> Engagements {
        Engagements::compile(
            Some(&json!({
                "acme": {"label": "ACME", "rate": 1000, "currency": "NOK", "paths": ["/work/acme"]},
                "internal": {"label": "Internal", "billable": false, "paths": ["/work/internal"]},
            })),
            &BTreeMap::new(),
            std::path::Path::new("/"),
        )
        .unwrap()
    }

    fn settings() -> LockSettings {
        LockSettings {
            increment: "15m".into(),
            rounding: "nearest".into(),
            split: "nearest".into(),
            min_entry: "0m".into(),
            drop_below: "0m".into(),
            daily_cap: None,
            human_idle: "1h".into(),
            review_credit: "30m".into(),
            gap_cap: "5m".into(),
            detail: None,
            engagements_fingerprint: "sha256:e".into(),
            ledger_fingerprint: "sha256:l".into(),
        }
    }

    fn entry(day: u32, engagement: &str, seconds: u64) -> TimesheetEntry {
        let billable = engagement == "acme";
        TimesheetEntry {
            date: date(day),
            engagement: engagement.to_string(),
            detail: None,
            label: engagement.to_string(),
            client: None,
            billable,
            raw_seconds: seconds as f64,
            estimated_seconds: seconds,
            manual_seconds: 0,
            override_seconds: None,
            final_seconds: seconds,
            first_start: None,
            last_end: None,
            evidence: Evidence::default(),
            rate: billable.then_some(1000.0),
            currency: billable.then(|| "NOK".to_string()),
            amount: None,
            notes: Vec::new(),
            description: None,
            status: EntryStatus::Suggested,
            adjustments: Vec::new(),
            lock_drift_seconds: None,
        }
    }

    fn sheet(entries: Vec<TimesheetEntry>) -> Timesheet {
        let mut sheet = Timesheet {
            window: TimesheetWindow {
                since: Some(day_start(date(1))),
                until: Some(day_start(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap())),
            },
            settings: TimesheetSettings::default(),
            entries,
            dropped: Vec::new(),
            cross_check: Vec::new(),
            warnings: Vec::new(),
            methodology: TimesheetMethodology {
                status: "suggested",
                split_rule: String::new(),
                rounding: String::new(),
            },
            drift: Vec::new(),
            applied_locks: Vec::new(),
        };
        compute::finalize(&mut sheet.entries);
        sheet
    }

    fn locked_ledger(entries: Vec<TimesheetEntry>, at: DateTime<Utc>) -> Ledger {
        let period = parse_period("2026-08").unwrap();
        let mut ledger = Ledger::default();
        let mut frozen = entries;
        compute::finalize(&mut frozen);
        ledger
            .put_lock(snapshot(&period, &frozen, settings(), at))
            .unwrap();
        ledger
    }

    fn apply_with(ledger: &Ledger, current: LockSettings, timesheet: &mut Timesheet) {
        let engagements = engagements();
        let context = Context {
            ledger,
            engagements: &engagements,
            ignore_locks: false,
            current,
        };
        crate::timesheet::ledger::apply(timesheet, &context).unwrap();
        compute::finalize(&mut timesheet.entries);
    }

    fn locked_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 10, 0, 0).unwrap()
    }

    #[test]
    fn periods_parse_in_their_three_forms_and_refuse_relative_words() {
        let month = parse_period("2026-08").unwrap();
        assert_eq!(PeriodKind::Month, month.kind);
        assert_eq!("2026-08", month.label);
        assert_eq!(day_start(date(1)), month.since);

        let week = parse_period("2026-w33").unwrap();
        assert_eq!("2026-W33", week.label);
        assert_eq!(PeriodKind::Week, week.kind);
        assert_eq!(
            day_start(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()),
            week.since
        );

        let range = parse_period("2026-08-03..2026-08-09").unwrap();
        assert_eq!(day_start(date(3)), range.since);
        assert_eq!(day_start(date(10)), range.until, "the end day is inclusive");
        assert_eq!(
            PeriodKind::Range("2026-08-03".into(), "2026-08-09".into()),
            range.kind
        );

        for bad in [
            "current",
            "last",
            "2026",
            "2026-13",
            "2026-W54",
            "2026-08-09..2026-08-03",
            "x..y",
            "",
        ] {
            assert!(parse_period(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_lock_covers_the_days_of_its_period() {
        let lock = locked_ledger(Vec::new(), locked_at()).locks.remove(0);
        assert!(lock.covers(date(1)));
        assert!(lock.covers(date(31)));
        assert!(!lock.covers(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()));
        assert!(!lock.covers(NaiveDate::from_ymd_opt(2026, 7, 31).unwrap()));
    }

    #[test]
    fn the_snapshot_totals_are_the_sum_of_its_entries() {
        let mut entries = vec![entry(12, "acme", 5400), entry(12, "internal", 900)];
        compute::finalize(&mut entries);
        let lock = snapshot(
            &parse_period("2026-08").unwrap(),
            &entries,
            settings(),
            locked_at(),
        );
        assert_eq!(6300, lock.totals.seconds);
        assert_eq!(Some(&1500.0), lock.totals.amounts.get("NOK"));
        assert_eq!(2, lock.entries.len());
        assert_eq!(env!("CARGO_PKG_VERSION"), lock.workstats_version);
        // It survives the file.
        let text = serde_json::to_string(&lock).unwrap();
        assert_eq!(lock, serde_json::from_str(&text).unwrap());
    }

    #[test]
    fn a_locked_period_shows_the_snapshot_even_when_history_changed() {
        let ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        // The live computation now says two hours.
        let mut timesheet = sheet(vec![entry(12, "acme", 7200)]);
        apply_with(&ledger, settings(), &mut timesheet);
        assert_eq!(1, timesheet.entries.len());
        let shown = &timesheet.entries[0];
        assert_eq!(EntryStatus::Locked, shown.status);
        assert_eq!(3600, shown.final_seconds);
        assert_eq!(Some(3600), shown.lock_drift_seconds);
        assert_eq!(
            Some(1000.0),
            shown.amount,
            "the locked amount, not a new one"
        );
        assert_eq!(1, timesheet.drift.len());
        let row = &timesheet.drift[0];
        assert_eq!(
            (3600, 7200, 3600),
            (
                row.locked_seconds,
                row.current_seconds,
                row.difference_seconds
            )
        );
        assert_eq!("new or pruned history", row.cause);
        assert_eq!(1, timesheet.warnings.len(), "one warning for all the drift");
        assert_eq!(1, timesheet.applied_locks.len());
    }

    #[test]
    fn work_that_appeared_or_vanished_since_the_lock_is_drift_too() {
        let ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        let mut timesheet = sheet(vec![entry(13, "internal", 1800)]);
        apply_with(&ledger, settings(), &mut timesheet);
        // Only the snapshot is shown; the new day is in the drift list.
        assert_eq!(
            vec![(date(12), "acme".to_string())],
            timesheet
                .entries
                .iter()
                .map(|e| (e.date, e.engagement.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(2, timesheet.drift.len());
        assert_eq!(0, timesheet.drift[0].current_seconds);
        assert_eq!(0, timesheet.drift[1].locked_seconds);
    }

    #[test]
    fn no_drift_means_no_warning() {
        let ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        let mut timesheet = sheet(vec![entry(12, "acme", 3600)]);
        apply_with(&ledger, settings(), &mut timesheet);
        assert!(timesheet.drift.is_empty());
        assert!(timesheet.warnings.is_empty());
        assert_eq!(Some(0), timesheet.entries[0].lock_drift_seconds);
    }

    #[test]
    fn the_cause_is_chosen_in_order_settings_engagements_ledger_history() {
        let ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        let live = || sheet(vec![entry(12, "acme", 7200)]);

        let mut changed = settings();
        changed.increment = "30m".into();
        changed.engagements_fingerprint = "sha256:other".into();
        let mut timesheet = live();
        apply_with(&ledger, changed, &mut timesheet);
        assert_eq!(
            "settings changed (increment)", timesheet.drift[0].cause,
            "settings win over a changed engagement config"
        );

        let mut changed = settings();
        changed.engagements_fingerprint = "sha256:other".into();
        let mut timesheet = live();
        apply_with(&ledger, changed, &mut timesheet);
        assert_eq!("engagement config changed", timesheet.drift[0].cause);

        // A ledger change on this day and engagement, made after the lock.
        let mut edited = ledger.clone();
        edited.entries.push(ManualEntry {
            id: "aaaa1111".into(),
            date: date(12),
            engagement: "acme".into(),
            seconds: 3600,
            note: None,
            billable: None,
            start: None,
            created_at: locked_at() + chrono::Duration::hours(1),
        });
        let mut changed = settings();
        changed.ledger_fingerprint = edited.fingerprint();
        let mut timesheet = live();
        apply_with(&edited, changed.clone(), &mut timesheet);
        assert_eq!("ledger edited after lock", timesheet.drift[0].cause);

        // The ledger changed, but not here: the history is the explanation.
        let mut timesheet = sheet(vec![entry(12, "acme", 7200), entry(20, "internal", 600)]);
        let mut other_day = edited.clone();
        other_day.entries[0].date = date(20);
        other_day.entries[0].engagement = "internal".into();
        apply_with(&other_day, changed, &mut timesheet);
        let on_acme = timesheet
            .drift
            .iter()
            .find(|row| row.engagement == "acme")
            .unwrap();
        assert_eq!("new or pruned history", on_acme.cause);
    }

    #[test]
    fn a_forced_write_is_named_as_a_ledger_edit() {
        let mut ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        ledger.record_forced(
            date(12),
            "acme",
            "rm",
            None,
            locked_at() + chrono::Duration::hours(2),
        );
        let mut changed = settings();
        changed.ledger_fingerprint = "sha256:different".into();
        let mut timesheet = sheet(vec![entry(12, "acme", 0)]);
        apply_with(&ledger, changed, &mut timesheet);
        assert_eq!("ledger edited after lock", timesheet.drift[0].cause);
    }

    #[test]
    fn ignoring_locks_shows_the_live_computation() {
        let ledger = locked_ledger(vec![entry(12, "acme", 3600)], locked_at());
        let engagements = engagements();
        let context = Context {
            ledger: &ledger,
            engagements: &engagements,
            ignore_locks: true,
            current: settings(),
        };
        let mut timesheet = sheet(vec![entry(12, "acme", 7200)]);
        crate::timesheet::ledger::apply(&mut timesheet, &context).unwrap();
        assert_eq!(7200, timesheet.entries[0].final_seconds);
        assert_ne!(EntryStatus::Locked, timesheet.entries[0].status);
        assert!(timesheet.drift.is_empty());
    }

    #[test]
    fn days_outside_the_lock_stay_live_and_a_window_inside_it_shows_only_its_days() {
        let period = parse_period("2026-08-10..2026-08-14").unwrap();
        let mut ledger = Ledger::default();
        let mut frozen = vec![entry(11, "acme", 3600), entry(13, "acme", 1800)];
        compute::finalize(&mut frozen);
        ledger
            .put_lock(snapshot(&period, &frozen, settings(), locked_at()))
            .unwrap();
        let mut timesheet = sheet(vec![entry(11, "acme", 900), entry(13, "acme", 1800)]);
        // A window of just the 12th and 13th.
        timesheet.window = TimesheetWindow {
            since: Some(day_start(date(12))),
            until: Some(day_start(date(14))),
        };
        timesheet
            .entries
            .retain(|entry| in_window(&timesheet.window, entry.date));
        apply_with(&ledger, settings(), &mut timesheet);
        let shown: Vec<_> = timesheet
            .entries
            .iter()
            .map(|e| (e.date, e.status))
            .collect();
        assert_eq!(vec![(date(13), EntryStatus::Locked)], shown);
        assert!(timesheet.drift.is_empty(), "the 11th is outside the window");
    }

    #[test]
    fn settings_differences_name_what_changed() {
        let base = settings();
        let mut other = base.clone();
        assert!(base.differences(&other).is_empty());
        other.rounding = "up".into();
        other.daily_cap = Some("8h".into());
        other.ledger_fingerprint = "sha256:z".into();
        assert_eq!(vec!["rounding", "daily cap"], base.differences(&other));
    }

    #[test]
    fn current_settings_are_written_the_way_they_are_typed() {
        let settings = TimesheetSettings {
            daily_cap_seconds: Some(8 * 3600),
            ..TimesheetSettings::default()
        };
        let current = LockSettings::current(
            &settings,
            chrono::Duration::minutes(5),
            chrono::Duration::hours(1),
            chrono::Duration::minutes(30),
            "sha256:e",
            "sha256:l",
        );
        assert_eq!("15m", current.increment);
        assert_eq!("nearest", current.rounding);
        assert_eq!("nearest", current.split);
        assert_eq!(Some("8h".to_string()), current.daily_cap);
        assert_eq!("1h", current.human_idle);
        assert_eq!("30m", current.review_credit);
        assert_eq!("5m", current.gap_cap);
    }
}
