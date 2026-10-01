//! The `timesheet` subcommands that change or list the ledger: `add`, `set`,
//! `unset`, `rm`, `entries`, `lock`, `unlock` and `locks`.
//!
//! Each action validates everything before it writes, writes the ledger
//! atomically, and returns the text to print, so the behaviour is testable
//! without a terminal. The reference time is a parameter for the same reason:
//! `yesterday` and `mon` are decided here, from the time they are given.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc, Weekday};

use super::compute::span_text;
use super::ledger::{self, Ledger, ManualEntry, Override};
use super::lock::{self, Lock, Period, PeriodKind};
use super::render::duration_text;
use super::{
    AddArguments, EntriesArguments, LockArguments, RemoveArguments, SetArguments, TimesheetAction,
    UnlockArguments, UnsetArguments, span_seconds,
};
use crate::cli::ReportWindow;
use crate::engagement::Engagements;
use crate::model::Diagnostics;
use crate::output::safe_text;
use crate::paths::{home_dir, load_config};
use crate::timeutil::{month_span, parse_bound, week_span};

/// Runs one action against the real ledger and prints its result.
pub(crate) fn run(action: TimesheetAction) -> Result<()> {
    let now = Utc::now();
    let path = ledger::default_path();
    let text = match action {
        TimesheetAction::Add(arguments) => add(&arguments, &path, &configured()?, now)?,
        TimesheetAction::Set(arguments) => set(&arguments, &path, &configured()?, now)?,
        TimesheetAction::Unset(arguments) => unset(&arguments, &path, &configured()?, now)?,
        TimesheetAction::Rm(arguments) => remove(&arguments, &path, now)?,
        TimesheetAction::Entries(arguments) => entries(&arguments, &path, now)?,
        TimesheetAction::Lock(arguments) => return lock(*arguments),
        TimesheetAction::Unlock(arguments) => unlock(&arguments, &path)?,
        TimesheetAction::Locks => locks(&path)?,
    };
    print!("{text}");
    Ok(())
}

/// The engagements an entry may name, read from the default config. Only the
/// actions that name an engagement need them, so a config problem never
/// blocks listing or unlocking.
fn configured() -> Result<Engagements> {
    let mut diagnostics = Diagnostics::default();
    let config = load_config(None, &mut diagnostics);
    Engagements::compile(
        config.engagements.as_ref(),
        &config.project_aliases,
        &home_dir(),
    )
}

// ----------------------------------------------------------------- input

fn today(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&Local).date_naive()
}

/// `YYYY-MM-DD`, `today`, `yesterday`, or a weekday name (`mon`..`sun`, or in
/// full): the most recent such day, today included. `today` is the local day
/// of the reference time.
pub(crate) fn parse_date(value: &str, today: NaiveDate) -> Result<NaiveDate> {
    let trimmed = value.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "today" => return Ok(today),
        "yesterday" => return Ok(today - Duration::days(1)),
        _ => {}
    }
    if trimmed.len() == 10
        && let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
    {
        return Ok(date);
    }
    if let Ok(weekday) = trimmed.parse::<Weekday>() {
        let back =
            (today.weekday().num_days_from_monday() + 7 - weekday.num_days_from_monday()) % 7;
        return Ok(today - Duration::days(i64::from(back)));
    }
    bail!("invalid date {value:?}: use YYYY-MM-DD, today, yesterday, or mon..sun (the most recent)")
}

fn parse_duration(value: &str, allow_zero: bool) -> Result<u64> {
    let seconds = span_seconds("the duration", value)?;
    if seconds == 0 && !allow_zero {
        bail!("the duration must be more than zero");
    }
    ledger::check_seconds(seconds)?;
    Ok(seconds)
}

fn check_engagement(engagements: &Engagements, key: &str) -> Result<()> {
    if engagements.get(key).is_some() {
        return Ok(());
    }
    let known: Vec<_> = engagements.keys().collect();
    bail!(
        "unknown engagement {key:?}; configured: {}",
        if known.is_empty() {
            "none (add an \"engagements\" block to the config)".to_string()
        } else {
            known.join(", ")
        }
    )
}

fn check_optional_note(note: Option<&str>) -> Result<Option<String>> {
    note.map(|note| {
        ledger::check_note(note)?;
        Ok(note.to_string())
    })
    .transpose()
}

/// Refuses a write into a locked day unless `--force`. Returns the period that
/// was forced into, so the caller records it.
fn guard(ledger: &Ledger, date: NaiveDate, force: bool) -> Result<Option<String>> {
    let Some(lock) = ledger.locked_period(date) else {
        return Ok(None);
    };
    if !force {
        bail!(
            "{date} is in the locked period {}; `workstats timesheet unlock {}` first, or pass --force to write anyway (it is recorded and shown as drift)",
            lock.period,
            lock.period
        );
    }
    Ok(Some(lock.period.clone()))
}

fn forced_note(forced: &Option<String>) -> String {
    forced
        .as_ref()
        .map(|period| format!("\nForced into the locked period {period}; it shows as drift there."))
        .unwrap_or_default()
}

// --------------------------------------------------------------- actions

pub(crate) fn add(
    arguments: &AddArguments,
    path: &Path,
    engagements: &Engagements,
    now: DateTime<Utc>,
) -> Result<String> {
    let date = parse_date(&arguments.date, today(now))?;
    check_engagement(engagements, &arguments.engagement)?;
    let seconds = parse_duration(&arguments.duration, false)?;
    let note = check_optional_note(arguments.note.as_deref())?;
    if let Some(start) = &arguments.start {
        ledger::check_start(start)?;
    }
    let billable = match (arguments.billable, arguments.non_billable) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    let mut ledger = Ledger::load(path)?;
    let forced = guard(&ledger, date, arguments.force)?;
    let id = ledger.add_entry(ManualEntry {
        id: String::new(),
        date,
        engagement: arguments.engagement.clone(),
        seconds,
        note: note.clone(),
        billable,
        start: arguments.start.clone(),
        created_at: now,
    })?;
    if forced.is_some() {
        ledger.record_forced(date, &arguments.engagement, "add", Some(id.clone()), now);
    }
    ledger.save(path)?;
    Ok(format!(
        "Added {id}: {date} {} {}{}{}\n",
        arguments.engagement,
        span_text(seconds),
        note.map(|note| format!(" ({})", safe_text(&note)))
            .unwrap_or_default(),
        forced_note(&forced)
    ))
}

pub(crate) fn set(
    arguments: &SetArguments,
    path: &Path,
    engagements: &Engagements,
    now: DateTime<Utc>,
) -> Result<String> {
    let date = parse_date(&arguments.date, today(now))?;
    check_engagement(engagements, &arguments.engagement)?;
    let seconds = parse_duration(&arguments.duration, true)?;
    let note = check_optional_note(arguments.note.as_deref())?;
    let mut ledger = Ledger::load(path)?;
    let forced = guard(&ledger, date, arguments.force)?;
    let previous = ledger.set_override(Override {
        date,
        engagement: arguments.engagement.clone(),
        seconds,
        note,
        created_at: now,
    })?;
    if forced.is_some() {
        ledger.record_forced(date, &arguments.engagement, "set", None, now);
    }
    ledger.save(path)?;
    let mut text = format!(
        "Override set: {date} {} {}",
        arguments.engagement,
        span_text(seconds)
    );
    if seconds == 0 {
        text.push_str(" (the estimate is suppressed)");
    }
    if let Some(previous) = previous {
        let _ = write!(text, ", replacing {}", span_text(previous));
    }
    text.push('\n');
    text.push_str(&forced_note(&forced));
    Ok(text)
}

pub(crate) fn unset(
    arguments: &UnsetArguments,
    path: &Path,
    engagements: &Engagements,
    now: DateTime<Utc>,
) -> Result<String> {
    let date = parse_date(&arguments.date, today(now))?;
    check_engagement(engagements, &arguments.engagement)?;
    let mut ledger = Ledger::load(path)?;
    let forced = guard(&ledger, date, arguments.force)?;
    let removed = ledger.unset_override(date, &arguments.engagement)?;
    if forced.is_some() {
        ledger.record_forced(date, &arguments.engagement, "unset", None, now);
    }
    ledger.save(path)?;
    Ok(format!(
        "Override removed: {date} {} (it was {}); the estimate applies again\n{}",
        arguments.engagement,
        span_text(removed.seconds),
        forced_note(&forced)
    ))
}

pub(crate) fn remove(
    arguments: &RemoveArguments,
    path: &Path,
    now: DateTime<Utc>,
) -> Result<String> {
    let mut ledger = Ledger::load(path)?;
    let (date, engagement) = {
        let entry = ledger.find_entry(&arguments.id)?;
        (entry.date, entry.engagement.clone())
    };
    let forced = guard(&ledger, date, arguments.force)?;
    let removed = ledger.remove_entry(&arguments.id)?;
    if forced.is_some() {
        ledger.record_forced(date, &engagement, "rm", Some(removed.id.clone()), now);
    }
    ledger.save(path)?;
    Ok(format!(
        "Removed {}: {} {} {}\n{}",
        removed.id,
        removed.date,
        removed.engagement,
        span_text(removed.seconds),
        forced_note(&forced)
    ))
}

// --------------------------------------------------------------- listing

/// A left-aligned table, for the plain-text listings.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut text = String::new();
        for (index, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            if index + 1 == widths.len() {
                text.push_str(cell);
            } else {
                let _ = write!(text, "{cell:<width$}  ");
            }
        }
        format!("{}\n", text.trim_end())
    };
    let mut output = line(headers.to_vec());
    for row in rows {
        output.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    output
}

fn entries_window(arguments: &EntriesArguments, now: DateTime<Utc>) -> Result<ReportWindow> {
    if let Some(value) = &arguments.month {
        let (since, until) =
            month_span(value, now).with_context(|| format!("invalid --month {value:?}"))?;
        return Ok((Some(since), Some(until)));
    }
    if let Some(value) = &arguments.week {
        let (since, until) =
            week_span(value, now).with_context(|| format!("invalid --week {value:?}"))?;
        return Ok((Some(since), Some(until)));
    }
    let bound = |flag: &str, value: &Option<String>, until: bool| {
        parse_bound(value.as_deref(), until)
            .with_context(|| format!("invalid {flag} {:?}", value.as_deref().unwrap_or_default()))
    };
    Ok((
        bound("--since", &arguments.since, false)?,
        bound("--until", &arguments.until, true)?,
    ))
}

pub(crate) fn entries(
    arguments: &EntriesArguments,
    default_path: &Path,
    now: DateTime<Utc>,
) -> Result<String> {
    let path = arguments.ledger.as_deref().unwrap_or(default_path);
    let ledger = Ledger::load(path)?;
    let (since, until) = entries_window(arguments, now)?;
    let window = super::model::TimesheetWindow { since, until };
    let shown = |date: NaiveDate| ledger::in_window(&window, date);

    let mut manual: Vec<&ManualEntry> = ledger
        .entries
        .iter()
        .filter(|entry| shown(entry.date))
        .collect();
    manual.sort_by(|left, right| {
        (left.date, &left.engagement, &left.id).cmp(&(right.date, &right.engagement, &right.id))
    });
    let mut overrides: Vec<&Override> = ledger
        .overrides
        .iter()
        .filter(|item| shown(item.date))
        .collect();
    overrides
        .sort_by(|left, right| (left.date, &left.engagement).cmp(&(right.date, &right.engagement)));
    let outside = ledger.entries.len() + ledger.overrides.len() - manual.len() - overrides.len();

    let mark = |date: NaiveDate| {
        ledger
            .locked_period(date)
            .map(|lock| format!("locked {}", lock.period))
            .unwrap_or_default()
    };
    let mut output = format!(
        "Ledger {}: {} manual entr{}, {} override{}, {} lock{}\n",
        path.display(),
        ledger.entries.len(),
        if ledger.entries.len() == 1 {
            "y"
        } else {
            "ies"
        },
        ledger.overrides.len(),
        if ledger.overrides.len() == 1 { "" } else { "s" },
        ledger.locks.len(),
        if ledger.locks.len() == 1 { "" } else { "s" },
    );
    if since.is_some() || until.is_some() {
        let _ = writeln!(
            output,
            "Showing only the selected window; {outside} item{} outside it not shown.",
            if outside == 1 { " is" } else { "s are" }
        );
    }
    output.push_str("\nManual entries\n");
    if manual.is_empty() {
        output.push_str("none\n");
    } else {
        let rows: Vec<Vec<String>> = manual
            .iter()
            .map(|entry| {
                vec![
                    entry.id.clone(),
                    entry.date.to_string(),
                    safe_text(&entry.engagement),
                    duration_text(entry.seconds),
                    match entry.billable {
                        Some(true) => "billable",
                        Some(false) => "non-billable",
                        None => "per engagement",
                    }
                    .to_string(),
                    entry.start.clone().unwrap_or_default(),
                    mark(entry.date),
                    safe_text(entry.note.as_deref().unwrap_or("")),
                ]
            })
            .collect();
        output.push_str(&table(
            &[
                "ID",
                "Date",
                "Engagement",
                "Time",
                "Billing",
                "Start",
                "Lock",
                "Note",
            ],
            &rows,
        ));
    }
    output.push_str("\nOverrides\n");
    if overrides.is_empty() {
        output.push_str("none\n");
    } else {
        let rows: Vec<Vec<String>> = overrides
            .iter()
            .map(|item| {
                vec![
                    item.date.to_string(),
                    safe_text(&item.engagement),
                    duration_text(item.seconds),
                    mark(item.date),
                    safe_text(item.note.as_deref().unwrap_or("")),
                ]
            })
            .collect();
        output.push_str(&table(
            &["Date", "Engagement", "Time", "Lock", "Note"],
            &rows,
        ));
    }
    Ok(output)
}

// ----------------------------------------------------------------- locks

/// `2026-08-01 to 2026-08-31 (local days)`.
fn days_text(lock: &Lock) -> String {
    format!(
        "{} to {} (local days)",
        lock.since.with_timezone(&Local).date_naive(),
        (lock.until - Duration::microseconds(1))
            .with_timezone(&Local)
            .date_naive()
    )
}

fn amounts_text(lock: &Lock) -> String {
    lock.totals
        .amounts
        .iter()
        .map(|(currency, amount)| format!("{amount:.2} {currency}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn locks(path: &Path) -> Result<String> {
    let ledger = Ledger::load(path)?;
    if ledger.locks.is_empty() {
        return Ok(format!("No locks in {}.\n", path.display()));
    }
    let mut locks: Vec<&Lock> = ledger.locks.iter().collect();
    locks.sort_by_key(|lock| (lock.since, lock.period.clone()));
    let mut output = format!("Locks in {}\n\n", path.display());
    for lock in locks {
        let _ = writeln!(
            output,
            "{}  {}  locked {} by workstats {}",
            lock.period,
            days_text(lock),
            lock.locked_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M"),
            lock.workstats_version
        );
        let amounts = amounts_text(lock);
        let _ = writeln!(
            output,
            "  {} entr{}, {}{}",
            lock.entries.len(),
            if lock.entries.len() == 1 { "y" } else { "ies" },
            duration_text(lock.totals.seconds),
            if amounts.is_empty() {
                String::new()
            } else {
                format!(", {amounts}")
            }
        );
        let _ = writeln!(output, "  {}", lock.settings.summary());
    }
    output.push_str(
        "\nA timesheet over a locked period shows these figures and lists any drift from the current computation.\n",
    );
    Ok(output)
}

pub(crate) fn unlock(arguments: &UnlockArguments, path: &Path) -> Result<String> {
    let period = lock::parse_period(&arguments.period)?;
    let mut ledger = Ledger::load(path)?;
    let Some(position) = ledger
        .locks
        .iter()
        .position(|lock| lock.period == period.label)
    else {
        let held: Vec<&str> = ledger
            .locks
            .iter()
            .map(|lock| lock.period.as_str())
            .collect();
        bail!(
            "no lock for {}; locked periods: {}",
            period.label,
            if held.is_empty() {
                "none".to_string()
            } else {
                held.join(", ")
            }
        );
    };
    let removed = ledger.locks.remove(position);
    ledger.save(path)?;
    Ok(format!(
        "Unlocked {} ({}, {}); its days are computed live again\n",
        removed.period,
        days_text(&removed),
        duration_text(removed.totals.seconds)
    ))
}

/// A lock may not be replaced without `--force`, and may never overlap a lock
/// of another period: a day covered by two snapshots has no single answer.
fn check_lockable(ledger: &Ledger, period: &Period, force: bool) -> Result<()> {
    for lock in &ledger.locks {
        if lock.period == period.label {
            if !force {
                bail!(
                    "{} is already locked (since {}); `workstats timesheet unlock {}` first, or pass --force to replace the snapshot with the current figures",
                    lock.period,
                    lock.locked_at
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M"),
                    lock.period
                );
            }
        } else if lock.overlaps_period(period) {
            bail!(
                "{} overlaps the lock {}; unlock one of them first",
                period.label,
                lock.period
            );
        }
    }
    Ok(())
}

fn lock(arguments: LockArguments) -> Result<()> {
    let LockArguments {
        target,
        force,
        options,
        mut report,
    } = arguments;
    let period = lock::parse_period(&target)?;
    for (given, flag) in [
        (!options.engagement.is_empty(), "--engagement"),
        (options.billable_only, "--billable-only"),
        (options.export.is_some(), "--export"),
        (options.ignore_locks, "--ignore-locks"),
        (options.digest, "--digest"),
    ] {
        if given {
            bail!(
                "{flag} does not apply to `workstats timesheet lock`; a lock freezes every entry of the period"
            );
        }
    }
    if super::has_window(&report) {
        bail!(
            "the window of a lock is its PERIOD ({}); drop --month, --year, --week, --since and --until",
            period.label
        );
    }
    match &period.kind {
        PeriodKind::Month => report.month = Some(period.label.clone()),
        PeriodKind::Week => report.week = Some(period.label.clone()),
        PeriodKind::Range(first, last) => {
            report.since = Some(first.clone());
            report.until = Some(last.clone());
        }
    }
    // Refused before the scan, which can take a while.
    let path = ledger::default_path();
    check_lockable(&Ledger::load(&path)?, &period, force)?;

    // Live figures with the ledger applied: what the report shows is what is
    // frozen. Other locks are ignored, so re-locking after a drift freezes
    // the current figures.
    let live = super::compute_live(&options, report, true)?;
    if live.collected.window != (Some(period.since), Some(period.until)) {
        bail!(
            "internal error: the scanned window does not match the period {}; please report this",
            period.label
        );
    }
    for warning in &live.computation.timesheet.warnings {
        eprintln!("workstats: {warning}");
    }
    let now = Utc::now();
    let current = lock::LockSettings::current(
        &live.resolved.settings,
        live.collected.settings.gap_cap,
        live.collected.settings.human_idle,
        live.collected.settings.review_credit,
        crate::engagement::active().fingerprint(),
        &live.ledger.fingerprint(),
    );
    let snapshot = lock::snapshot(&period, &live.computation.timesheet.entries, current, now);
    let mut ledger = live.ledger;
    let replaced = ledger
        .locks
        .iter()
        .any(|held| held.period == snapshot.period);
    let summary = format!(
        "{} {}: {}, {} entr{}, {}{}\n  {}\n",
        if replaced { "Re-locked" } else { "Locked" },
        snapshot.period,
        days_text(&snapshot),
        snapshot.entries.len(),
        if snapshot.entries.len() == 1 {
            "y"
        } else {
            "ies"
        },
        duration_text(snapshot.totals.seconds),
        {
            let amounts = amounts_text(&snapshot);
            if amounts.is_empty() {
                String::new()
            } else {
                format!(", {amounts}")
            }
        },
        snapshot.settings.summary()
    );
    ledger.put_lock(snapshot)?;
    ledger.save(&path)?;
    println!(
        "{summary}`workstats timesheet --ignore-locks` shows the live figures; `workstats timesheet unlock {}` removes the lock.",
        period.label
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use clap::Parser;
    use serde_json::json;

    use super::*;
    use crate::cli::{Arguments, Command};
    use crate::timesheet::lock::{LockSettings, snapshot};

    /// Wednesday 2026-08-12, local noon.
    fn now() -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 8, 12, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn day(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
    }

    fn engagements() -> Engagements {
        Engagements::compile(
            Some(&json!({
                "acme": {"label": "ACME", "rate": 1000, "currency": "NOK", "paths": ["/work/acme"]},
                "internal": {"label": "Internal", "billable": false, "paths": ["/work/internal"]},
            })),
            &std::collections::BTreeMap::new(),
            Path::new("/"),
        )
        .unwrap()
    }

    fn action(arguments: &[&str]) -> TimesheetAction {
        let mut full = vec!["workstats", "timesheet"];
        full.extend_from_slice(arguments);
        match Arguments::try_parse_from(full).unwrap().command {
            Some(Command::Timesheet(timesheet)) => timesheet.action.expect("an action"),
            other => panic!("expected timesheet, got {other:?}"),
        }
    }

    fn path() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("timesheet.json");
        (directory, path)
    }

    fn do_add(path: &Path, arguments: &[&str]) -> Result<String> {
        let mut full = vec!["add"];
        full.extend_from_slice(arguments);
        let TimesheetAction::Add(arguments) = action(&full) else {
            unreachable!()
        };
        add(&arguments, path, &engagements(), now())
    }

    fn do_set(path: &Path, arguments: &[&str]) -> Result<String> {
        let mut full = vec!["set"];
        full.extend_from_slice(arguments);
        let TimesheetAction::Set(arguments) = action(&full) else {
            unreachable!()
        };
        set(&arguments, path, &engagements(), now())
    }

    fn lock_august(path: &Path) {
        let mut ledger = Ledger::load(path).unwrap();
        let period = lock::parse_period("2026-08").unwrap();
        let settings = LockSettings::current(
            &super::super::model::TimesheetSettings::default(),
            Duration::minutes(5),
            Duration::hours(1),
            Duration::minutes(30),
            "sha256:e",
            &ledger.fingerprint(),
        );
        ledger
            .put_lock(snapshot(&period, &[], settings, now()))
            .unwrap();
        ledger.save(path).unwrap();
    }

    // -------------------------------------------------------------- dates

    #[test]
    fn dates_parse_against_an_injected_reference() {
        let wednesday = day(12);
        for (value, expected) in [
            ("2026-08-01", day(1)),
            ("today", day(12)),
            ("Today", day(12)),
            ("yesterday", day(11)),
            ("mon", day(10)),
            ("monday", day(10)),
            ("TUE", day(11)),
            ("wed", day(12)),
            ("thu", day(6)),
            ("fri", day(7)),
            ("sat", day(8)),
            ("sun", day(9)),
        ] {
            assert_eq!(expected, parse_date(value, wednesday).unwrap(), "{value}");
        }
        // Across a month boundary.
        assert_eq!(
            NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
            parse_date("fri", day(2)).unwrap()
        );
        assert_eq!(
            NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            parse_date("yesterday", NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()).unwrap()
        );
    }

    #[test]
    fn bad_dates_are_refused_with_the_accepted_forms() {
        for value in [
            "",
            "2026-13-01",
            "2026-02-30",
            "2026-8-1",
            "tomorrow",
            "last monday",
            "12/08/2026",
        ] {
            let error = parse_date(value, day(12)).unwrap_err().to_string();
            assert!(error.contains("YYYY-MM-DD"), "{value}: {error}");
        }
    }

    // ------------------------------------------------------------ writing

    #[test]
    fn add_writes_an_entry_and_says_what_it_wrote() {
        let (_directory, path) = path();
        let text = do_add(
            &path,
            &[
                "yesterday",
                "acme",
                "1h30m",
                "Steering",
                "--start",
                "09:00",
                "--billable",
            ],
        )
        .unwrap();
        assert!(text.contains("2026-08-11 acme 1h30m (Steering)"), "{text}");
        let ledger = Ledger::load(&path).unwrap();
        let entry = &ledger.entries[0];
        assert_eq!((day(11), 5400), (entry.date, entry.seconds));
        assert_eq!(Some(true), entry.billable);
        assert_eq!(Some("09:00"), entry.start.as_deref());
        assert!(text.contains(&entry.id));
        assert_eq!(now(), entry.created_at);
    }

    #[test]
    fn add_validates_before_it_writes() {
        let (_directory, path) = path();
        for (arguments, expected) in [
            (vec!["today", "nobody", "1h"], "unknown engagement"),
            (vec!["today", "acme", "0"], "more than zero"),
            (vec!["today", "acme", "soon"], "duration"),
            (vec!["today", "acme", "25h"], "24h"),
            (vec!["today", "acme", "1h", "two\nlines"], "single line"),
            (vec!["today", "acme", "1h", "--start", "9am"], "HH:MM"),
            (vec!["someday", "acme", "1h"], "YYYY-MM-DD"),
        ] {
            let error = do_add(&path, &arguments).unwrap_err().to_string();
            assert!(error.contains(expected), "{arguments:?}: {error}");
        }
        assert!(!path.exists(), "nothing was written");
        // The unknown-engagement message lists what is configured.
        let error = do_add(&path, &["today", "nobody", "1h"])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("acme") && error.contains("internal"),
            "{error}"
        );
    }

    #[test]
    fn set_replaces_and_reports_the_value_it_replaced() {
        let (_directory, path) = path();
        let first = do_set(&path, &["2026-08-12", "acme", "2h", "after review"]).unwrap();
        assert!(
            first.contains("Override set: 2026-08-12 acme 2h"),
            "{first}"
        );
        let second = do_set(&path, &["2026-08-12", "acme", "0"]).unwrap();
        assert!(
            second.contains("suppressed") && second.contains("replacing 2h"),
            "{second}"
        );
        let ledger = Ledger::load(&path).unwrap();
        assert_eq!(1, ledger.overrides.len());
        assert_eq!(0, ledger.overrides[0].seconds);
    }

    #[test]
    fn unset_and_rm_remove_what_exists_and_refuse_what_does_not() {
        let (_directory, path) = path();
        let message = |result: Result<String>| result.unwrap_err().to_string();
        let TimesheetAction::Unset(unset_missing) = action(&["unset", "2026-08-12", "acme"]) else {
            unreachable!()
        };
        assert!(
            message(unset(&unset_missing, &path, &engagements(), now())).contains("no override")
        );
        let TimesheetAction::Rm(rm_missing) = action(&["rm", "deadbeef"]) else {
            unreachable!()
        };
        assert!(message(remove(&rm_missing, &path, now())).contains("no manual entry"));

        do_set(&path, &["2026-08-12", "acme", "2h"]).unwrap();
        let TimesheetAction::Unset(arguments) = action(&["unset", "2026-08-12", "acme"]) else {
            unreachable!()
        };
        let text = unset(&arguments, &path, &engagements(), now()).unwrap();
        assert!(text.contains("estimate applies again"), "{text}");
        assert!(Ledger::load(&path).unwrap().overrides.is_empty());

        do_add(&path, &["2026-08-12", "acme", "1h"]).unwrap();
        let id = Ledger::load(&path).unwrap().entries[0].id.clone();
        let TimesheetAction::Rm(arguments) = action(&["rm", &id]) else {
            unreachable!()
        };
        remove(&arguments, &path, now()).unwrap();
        assert!(Ledger::load(&path).unwrap().entries.is_empty());
    }

    #[test]
    fn a_locked_day_refuses_every_write_until_forced_and_the_force_is_recorded() {
        let (_directory, path) = path();
        do_add(&path, &["2026-08-05", "acme", "1h"]).unwrap();
        let id = Ledger::load(&path).unwrap().entries[0].id.clone();
        lock_august(&path);

        for result in [
            do_add(&path, &["2026-08-12", "acme", "1h"]),
            do_set(&path, &["2026-08-12", "acme", "1h"]),
        ] {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("locked period 2026-08") && error.contains("--force"),
                "{error}"
            );
        }
        let TimesheetAction::Rm(refused) = action(&["rm", &id]) else {
            unreachable!()
        };
        assert!(remove(&refused, &path, now()).is_err());
        let TimesheetAction::Unset(refused) = action(&["unset", "2026-08-12", "acme"]) else {
            unreachable!()
        };
        assert!(unset(&refused, &path, &engagements(), now()).is_err());
        assert_eq!(
            1,
            Ledger::load(&path).unwrap().entries.len(),
            "nothing changed"
        );
        assert!(Ledger::load(&path).unwrap().forced_writes.is_empty());

        // Outside the lock is still writable.
        do_add(&path, &["2026-09-02", "acme", "1h"]).unwrap();

        let text = do_add(&path, &["2026-08-12", "acme", "1h", "--force"]).unwrap();
        assert!(
            text.contains("Forced into the locked period 2026-08"),
            "{text}"
        );
        let TimesheetAction::Rm(forced) = action(&["rm", &id, "--force"]) else {
            unreachable!()
        };
        remove(&forced, &path, now()).unwrap();
        let ledger = Ledger::load(&path).unwrap();
        let actions: Vec<&str> = ledger
            .forced_writes
            .iter()
            .map(|w| w.action.as_str())
            .collect();
        assert_eq!(vec!["add", "rm"], actions);
    }

    // ------------------------------------------------------------ listing

    #[test]
    fn entries_lists_items_marks_locked_days_and_filters_by_window() {
        let (_directory, path) = path();
        do_add(&path, &["2026-07-30", "acme", "1h", "July work"]).unwrap();
        do_add(
            &path,
            &[
                "2026-08-05",
                "internal",
                "30m",
                "Planning",
                "--non-billable",
            ],
        )
        .unwrap();
        do_set(&path, &["2026-08-05", "acme", "0", "Out sick"]).unwrap();
        lock_august(&path);

        let TimesheetAction::Entries(all) = action(&["entries"]) else {
            unreachable!()
        };
        let text = entries(&all, &path, now()).unwrap();
        for expected in [
            "July work",
            "Planning",
            "Out sick",
            "non-billable",
            "locked 2026-08",
        ] {
            assert!(text.contains(expected), "{expected:?} in\n{text}");
        }
        assert!(
            text.contains("2 manual entries, 1 override, 1 lock"),
            "{text}"
        );

        let TimesheetAction::Entries(august) = action(&["entries", "--month", "2026-08"]) else {
            unreachable!()
        };
        let text = entries(&august, &path, now()).unwrap();
        assert!(
            text.contains("Planning") && !text.contains("July work"),
            "{text}"
        );
        assert!(text.contains("1 item is outside it not shown"), "{text}");
    }

    #[test]
    fn locks_lists_each_lock_and_unlock_removes_one() {
        let (_directory, path) = path();
        assert!(locks(&path).unwrap().contains("No locks"));
        lock_august(&path);
        let text = locks(&path).unwrap();
        assert!(
            text.contains("2026-08") && text.contains("2026-08-01 to 2026-08-31"),
            "{text}"
        );
        assert!(text.contains("nearest to 15m"), "{text}");

        let unlock_arguments = |period: &str| {
            let TimesheetAction::Unlock(arguments) = action(&["unlock", period]) else {
                unreachable!()
            };
            arguments
        };
        let error = unlock(&unlock_arguments("2026-07"), &path)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no lock for 2026-07") && error.contains("2026-08"),
            "{error}"
        );
        let text = unlock(&unlock_arguments("2026-08"), &path).unwrap();
        assert!(text.contains("Unlocked 2026-08"), "{text}");
        assert!(Ledger::load(&path).unwrap().locks.is_empty());
        // And the day is writable again.
        do_add(&path, &["2026-08-12", "acme", "1h"]).unwrap();
    }

    #[test]
    fn locking_refuses_a_held_period_without_force_and_an_overlap_always() {
        let (_directory, path) = path();
        lock_august(&path);
        let ledger = Ledger::load(&path).unwrap();
        let august = lock::parse_period("2026-08").unwrap();
        assert!(check_lockable(&ledger, &august, false).is_err());
        assert!(check_lockable(&ledger, &august, true).is_ok());
        let week = lock::parse_period("2026-W33").unwrap();
        let error = check_lockable(&ledger, &week, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps"), "{error}");
        let september = lock::parse_period("2026-09").unwrap();
        assert!(check_lockable(&ledger, &september, false).is_ok());
    }

    #[test]
    fn a_corrupt_ledger_stops_every_action() {
        let (_directory, path) = path();
        std::fs::write(&path, "{ nope").unwrap();
        assert!(do_add(&path, &["today", "acme", "1h"]).is_err());
        assert!(locks(&path).is_err());
        assert_eq!(
            "{ nope",
            std::fs::read_to_string(&path).unwrap(),
            "left as it was"
        );
    }
}
