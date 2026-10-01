//! The ledger of manual entries, overrides and locks that sits beside the
//! computed timesheet.
//!
//! It is a file the user's own hours live in, so it follows different rules
//! from the cache and the saved views: it sits beside the config, it is
//! written atomically, and **a ledger that cannot be read is a hard error**.
//! Treating an unreadable ledger as empty would silently change hours that may
//! already have been submitted. It is never the cache: `--no-cache` and
//! `--rebuild-cache` leave it alone.
//!
//! `apply` is the single point the computation calls: overrides replace an
//! estimate, manual entries are added on top, the daily cap is enforced again
//! now that those are known, and finally locked periods are replaced by their
//! snapshots (see `lock.rs`).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Local, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::compute::{self, span_text};
use super::lock::{self, Lock, LockSettings};
use super::model::{
    DroppedEntry, EntryStatus, Evidence, Timesheet, TimesheetEntry, TimesheetWindow,
};
use super::round;
use crate::engagement::Engagements;
use crate::paths::{default_config_path, home_dir};

pub(crate) const VERSION: u32 = 1;
/// Manual entries and overrides together.
pub(crate) const MAX_ITEMS: usize = 10_000;
pub(crate) const MAX_LOCKS: usize = 500;
pub(crate) const MAX_SECONDS: u64 = 24 * 3600;
const MAX_NOTE_BYTES: usize = 1024;
/// Forced writes are evidence for the drift report, not data worth growing
/// without bound; the oldest go first.
const MAX_FORCED_WRITES: usize = 10_000;

/// The detail a manual entry gets when its billing differs from its
/// engagement's, so it stays a row of its own instead of changing the
/// estimate's billing.
const MANUAL_BILLABLE: &str = "manual, billable";
const MANUAL_NON_BILLABLE: &str = "manual, non-billable";

/// Hours entered by hand: added to a day and engagement on top of whatever the
/// activity suggests.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManualEntry {
    pub(crate) id: String,
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    pub(crate) seconds: u64,
    #[serde(default)]
    pub(crate) note: Option<String>,
    /// `None` follows the engagement.
    #[serde(default)]
    pub(crate) billable: Option<bool>,
    /// Local `HH:MM`, for the CSV exports.
    #[serde(default)]
    pub(crate) start: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

/// A value that replaces the estimate for one day and engagement. Zero
/// suppresses the estimate.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Override {
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    pub(crate) seconds: u64,
    #[serde(default)]
    pub(crate) note: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

/// A write into a locked day that was made anyway with `--force`. Kept so the
/// drift report can say "the ledger was edited" instead of guessing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForcedWrite {
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    /// `add`, `set`, `unset` or `rm`.
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) id: Option<String>,
    pub(crate) at: DateTime<Utc>,
}

fn current_version() -> u32 {
    VERSION
}

/// `deny_unknown_fields` on purpose: a field this binary does not know would
/// be dropped on the next save, which is the data loss this file's rules
/// exist to prevent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    #[serde(default = "current_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) entries: Vec<ManualEntry>,
    #[serde(default)]
    pub(crate) overrides: Vec<Override>,
    #[serde(default)]
    pub(crate) locks: Vec<Lock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) forced_writes: Vec<ForcedWrite>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
            overrides: Vec::new(),
            locks: Vec::new(),
            forced_writes: Vec::new(),
        }
    }
}

/// Where the ledger lives: `WORKSTATS_TIMESHEET`, else `timesheet.json` beside
/// the config file. Like the saved views it ignores `--config`, so every
/// command in a session reads and writes the same file.
pub(crate) fn default_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_TIMESHEET") {
        return PathBuf::from(path);
    }
    default_config_path()
        .parent()
        .map(|parent| parent.join("timesheet.json"))
        .unwrap_or_else(|| home_dir().join(".config/workstats/timesheet.json"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `1h30m`, for messages.
pub(crate) fn span(seconds: u64) -> String {
    span_text(seconds)
}

/// A note is one printable line: it is shown in tables and written to CSV.
pub(crate) fn check_note(note: &str) -> Result<()> {
    if note.len() > MAX_NOTE_BYTES {
        bail!("a note may be at most {MAX_NOTE_BYTES} bytes");
    }
    if note.chars().any(char::is_control) {
        bail!("a note must be a single line without control characters");
    }
    Ok(())
}

pub(crate) fn check_seconds(seconds: u64) -> Result<()> {
    if seconds > MAX_SECONDS {
        bail!("a day has at most 24h; {} is more", span(seconds));
    }
    Ok(())
}

pub(crate) fn check_start(start: &str) -> Result<()> {
    NaiveTime::parse_from_str(start, "%H:%M")
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("invalid start time {start:?}: use HH:MM, e.g. 09:00"))
}

impl Ledger {
    /// A missing file is an empty ledger. Anything else that goes wrong is an
    /// error naming the file: see the module comment.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| unreadable(path));
            }
        };
        let ledger: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a valid timesheet ledger", path.display()))
            .with_context(|| unreadable(path))?;
        ledger
            .validate()
            .with_context(|| format!("{} is not a valid timesheet ledger", path.display()))
            .with_context(|| unreadable(path))?;
        Ok(ledger)
    }

    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!(
                "it has version {}, but this workstats reads version {VERSION}; a newer workstats wrote it",
                self.version
            );
        }
        if self.entries.len() + self.overrides.len() > MAX_ITEMS {
            bail!("it holds more than {MAX_ITEMS} entries and overrides");
        }
        if self.locks.len() > MAX_LOCKS {
            bail!("it holds more than {MAX_LOCKS} locks");
        }
        for entry in &self.entries {
            check_seconds(entry.seconds).with_context(|| format!("entry {}", entry.id))?;
            if let Some(note) = &entry.note {
                check_note(note).with_context(|| format!("entry {}", entry.id))?;
            }
            if let Some(start) = &entry.start {
                check_start(start).with_context(|| format!("entry {}", entry.id))?;
            }
        }
        for item in &self.overrides {
            check_seconds(item.seconds)
                .with_context(|| format!("override {} {}", item.date, item.engagement))?;
            if let Some(note) = &item.note {
                check_note(note)
                    .with_context(|| format!("override {} {}", item.date, item.engagement))?;
            }
        }
        for lock in &self.locks {
            lock.validate()
                .with_context(|| format!("lock {}", lock.period))?;
        }
        Ok(())
    }

    /// Written through a temporary file in the same directory, so an
    /// interrupted save cannot leave a half-written ledger behind. There is no
    /// cross-process locking: two commands writing at once, the last one wins.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let mut stored = self.clone();
        stored.version = VERSION;
        let encoded = serde_json::to_vec_pretty(&stored)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot write to {}", parent.display()))?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.flush()?;
        crate::durable::persist(file, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    /// A digest of the hours the ledger contributes: entries and overrides,
    /// in a fixed order. Locks and forced writes are left out, so taking a
    /// lock does not itself look like an edit.
    pub(crate) fn fingerprint(&self) -> String {
        let mut entries: Vec<&ManualEntry> = self.entries.iter().collect();
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let mut overrides: Vec<&Override> = self.overrides.iter().collect();
        overrides.sort_by(|left, right| {
            (left.date, &left.engagement).cmp(&(right.date, &right.engagement))
        });
        let canonical = serde_json::to_string(&(entries, overrides)).unwrap_or_default();
        format!("sha256:{}", hex(&Sha256::digest(canonical.as_bytes())))
    }

    /// The lock whose period contains the day, if any.
    pub(crate) fn locked_period(&self, date: NaiveDate) -> Option<&Lock> {
        self.locks.iter().find(|lock| lock.covers(date))
    }

    fn room(&self) -> Result<()> {
        if self.entries.len() + self.overrides.len() >= MAX_ITEMS {
            bail!(
                "the ledger already holds {MAX_ITEMS} entries and overrides, the most it supports"
            );
        }
        Ok(())
    }

    /// Adds a manual entry and returns its id: eight hex characters of the
    /// sha256 of its content and creation time, extended on the rare clash.
    pub(crate) fn add_entry(&mut self, mut entry: ManualEntry) -> Result<String> {
        self.room()?;
        check_seconds(entry.seconds)?;
        let content = format!(
            "{}|{}|{}|{}|{:?}|{:?}|{}",
            entry.date,
            entry.engagement,
            entry.seconds,
            entry.note.as_deref().unwrap_or(""),
            entry.billable,
            entry.start,
            entry.created_at.to_rfc3339()
        );
        let mut salt = 0u32;
        let id = loop {
            let digest = Sha256::digest(format!("{content}|{salt}").as_bytes());
            let id = hex(&digest)[..8].to_string();
            if !self.entries.iter().any(|existing| existing.id == id) {
                break id;
            }
            salt += 1;
        };
        entry.id = id.clone();
        self.entries.push(entry);
        Ok(id)
    }

    /// Sets (or replaces) the override for a day and engagement, returning the
    /// seconds it replaced.
    pub(crate) fn set_override(&mut self, item: Override) -> Result<Option<u64>> {
        check_seconds(item.seconds)?;
        if let Some(existing) = self
            .overrides
            .iter_mut()
            .find(|existing| existing.date == item.date && existing.engagement == item.engagement)
        {
            let previous = existing.seconds;
            *existing = item;
            return Ok(Some(previous));
        }
        self.room()?;
        self.overrides.push(item);
        Ok(None)
    }

    pub(crate) fn unset_override(&mut self, date: NaiveDate, engagement: &str) -> Result<Override> {
        let position = self
            .overrides
            .iter()
            .position(|item| item.date == date && item.engagement == engagement)
            .ok_or_else(|| anyhow::anyhow!("no override for {date} {engagement}"))?;
        Ok(self.overrides.remove(position))
    }

    pub(crate) fn find_entry(&self, id: &str) -> Result<&ManualEntry> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no manual entry with id {id:?}; `workstats timesheet entries` lists them"
                )
            })
    }

    pub(crate) fn remove_entry(&mut self, id: &str) -> Result<ManualEntry> {
        self.find_entry(id)?;
        let position = self
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .expect("found just above");
        Ok(self.entries.remove(position))
    }

    pub(crate) fn record_forced(
        &mut self,
        date: NaiveDate,
        engagement: &str,
        action: &str,
        id: Option<String>,
        at: DateTime<Utc>,
    ) {
        self.forced_writes.push(ForcedWrite {
            date,
            engagement: engagement.to_string(),
            action: action.to_string(),
            id,
            at,
        });
        if self.forced_writes.len() > MAX_FORCED_WRITES {
            let excess = self.forced_writes.len() - MAX_FORCED_WRITES;
            self.forced_writes.drain(..excess);
        }
    }

    /// Adds a lock, replacing one of the same period.
    pub(crate) fn put_lock(&mut self, lock: Lock) -> Result<()> {
        if let Some(existing) = self
            .locks
            .iter_mut()
            .find(|existing| existing.period == lock.period)
        {
            *existing = lock;
            return Ok(());
        }
        if self.locks.len() >= MAX_LOCKS {
            bail!("the ledger already holds {MAX_LOCKS} locks, the most it supports");
        }
        self.locks.push(lock);
        Ok(())
    }
}

fn unreadable(path: &Path) -> String {
    format!(
        "cannot use the timesheet ledger {}: it holds hours that may already have been submitted, so it is never ignored; fix or move the file, or point WORKSTATS_TIMESHEET elsewhere",
        path.display()
    )
}

// ------------------------------------------------------------------ days

/// The first instant of a local day. Built the way the report's window bounds
/// are, so a day is "in" the window exactly when the report counts it.
pub(crate) fn day_start(date: NaiveDate) -> DateTime<Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight exists");
    let local = match Local.from_local_datetime(&midnight) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => {
            let noon = date.and_hms_opt(12, 0, 0).expect("noon exists");
            Local
                .from_local_datetime(&noon)
                .earliest()
                .expect("noon exists")
        }
    };
    local.with_timezone(&Utc)
}

/// Whether a local day falls inside the (half-open) window.
pub(crate) fn in_window(window: &TimesheetWindow, date: NaiveDate) -> bool {
    let start = day_start(date);
    window.since.is_none_or(|since| start >= since)
        && window.until.is_none_or(|until| start < until)
}

// ----------------------------------------------------------------- apply

/// What `apply` needs besides the timesheet.
pub(crate) struct Context<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) engagements: &'a Engagements,
    /// Show the live computation for locked periods.
    pub(crate) ignore_locks: bool,
    /// The settings of this run, to tell a lock why it differs.
    pub(crate) current: LockSettings,
}

/// Applies overrides, manual entries and locks to a computed timesheet.
pub(crate) fn apply(timesheet: &mut Timesheet, context: &Context<'_>) -> Result<()> {
    apply_items(timesheet, context);
    if !context.ignore_locks {
        lock::apply_locks(timesheet, context);
    }
    Ok(())
}

/// A new, empty entry for a day and engagement the computation had nothing
/// for: hours entered by hand, or an override on a day with no activity.
fn blank_entry(
    engagements: &Engagements,
    date: NaiveDate,
    engagement: &str,
    detail: Option<String>,
    billable: bool,
) -> TimesheetEntry {
    let configured = engagements.get(engagement);
    TimesheetEntry {
        date,
        engagement: engagement.to_string(),
        detail,
        label: configured.map_or_else(|| engagement.to_string(), |item| item.label.clone()),
        client: configured.and_then(|item| item.client.clone()),
        billable,
        raw_seconds: 0.0,
        estimated_seconds: 0,
        manual_seconds: 0,
        override_seconds: None,
        final_seconds: 0,
        first_start: None,
        last_end: None,
        evidence: Evidence::default(),
        rate: configured.filter(|_| billable).and_then(|item| item.rate),
        currency: configured
            .filter(|_| billable)
            .and_then(|item| item.currency.clone()),
        amount: None,
        notes: Vec::new(),
        description: None,
        status: EntryStatus::Suggested,
        adjustments: Vec::new(),
        lock_drift_seconds: None,
    }
}

/// The local `HH:MM` of a manual entry on its day, as an instant. A time the
/// clock skips (a daylight-saving gap) has no instant, and the entry then
/// simply has no start.
fn start_instant(date: NaiveDate, start: &str) -> Option<DateTime<Utc>> {
    let time = NaiveTime::parse_from_str(start, "%H:%M").ok()?;
    Local
        .from_local_datetime(&date.and_time(time))
        .earliest()
        .map(|moment| moment.with_timezone(&Utc))
}

fn apply_items(timesheet: &mut Timesheet, context: &Context<'_>) {
    let ledger = context.ledger;
    let window = timesheet.window.clone();
    let overrides: Vec<&Override> = ledger
        .overrides
        .iter()
        .filter(|item| in_window(&window, item.date))
        .collect();
    let manuals: Vec<&ManualEntry> = ledger
        .entries
        .iter()
        .filter(|item| in_window(&window, item.date))
        .collect();
    if overrides.is_empty() && manuals.is_empty() {
        return;
    }
    let mut touched: BTreeSet<NaiveDate> = BTreeSet::new();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    let mut note_unknown = |engagement: &str| {
        if context.engagements.get(engagement).is_none() {
            unknown.insert(engagement.to_string());
        }
    };

    for item in &overrides {
        touched.insert(item.date);
        note_unknown(&item.engagement);
        let positions: Vec<usize> = timesheet
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.date == item.date && entry.engagement == item.engagement)
            .map(|(index, _)| index)
            .collect();
        let first = match positions.split_first() {
            Some((&first, rest)) => {
                // With --detail an engagement has several rows on a day; the
                // override is for the engagement's day, so it sits on the
                // first row and the others are zeroed, not left to add up.
                for &index in rest {
                    let entry = &mut timesheet.entries[index];
                    entry.override_seconds = Some(0);
                    entry
                        .notes
                        .push("folded into the override on this day's first row".to_string());
                }
                first
            }
            // An override of zero on a day with no estimate suppresses
            // nothing, so there is nothing to show.
            None if item.seconds == 0 => continue,
            None => {
                let billable = context
                    .engagements
                    .get(&item.engagement)
                    .is_some_and(|engagement| engagement.billable);
                timesheet.entries.push(blank_entry(
                    context.engagements,
                    item.date,
                    &item.engagement,
                    None,
                    billable,
                ));
                timesheet.entries.len() - 1
            }
        };
        let entry = &mut timesheet.entries[first];
        entry.override_seconds = Some(item.seconds);
        match &item.note {
            Some(note) => entry.notes.push(format!("override: {note}")),
            None if item.seconds == 0 => entry.notes.push("estimate suppressed".to_string()),
            None => {}
        }
    }

    for item in &manuals {
        touched.insert(item.date);
        note_unknown(&item.engagement);
        let default_billable = context
            .engagements
            .get(&item.engagement)
            .is_some_and(|engagement| engagement.billable);
        let billable = item.billable.unwrap_or(default_billable);
        // Billing that differs from the engagement's cannot share a row with
        // the estimate, whose billing is the engagement's.
        let detail = (billable != default_billable).then(|| {
            if billable {
                MANUAL_BILLABLE.to_string()
            } else {
                MANUAL_NON_BILLABLE.to_string()
            }
        });
        let position = timesheet.entries.iter().position(|entry| {
            entry.date == item.date && entry.engagement == item.engagement && entry.detail == detail
        });
        let index = position.unwrap_or_else(|| {
            timesheet.entries.push(blank_entry(
                context.engagements,
                item.date,
                &item.engagement,
                detail,
                billable,
            ));
            timesheet.entries.len() - 1
        });
        let entry = &mut timesheet.entries[index];
        entry.manual_seconds += item.seconds;
        entry.notes.push(match &item.note {
            Some(note) => format!("manual {}: {note} ({})", span(item.seconds), item.id),
            None => format!("manual {} ({})", span(item.seconds), item.id),
        });
        // A start time only matters for an entry with no activity of its own.
        if let Some(start) = item
            .start
            .as_deref()
            .and_then(|start| start_instant(item.date, start))
            && entry.evidence == Evidence::default()
            && entry.estimated_seconds == 0
        {
            let first = entry.first_start.map_or(start, |first| first.min(start));
            entry.first_start = Some(first);
            entry.last_end = Some(first + chrono::Duration::seconds(entry.manual_seconds as i64));
        }
    }

    timesheet
        .entries
        .sort_by(|left, right| compute::entry_order(left).cmp(&compute::entry_order(right)));

    // The cap was enforced over estimates alone. Now that manual hours and
    // overrides are known they come off the remaining room, and the estimates
    // are capped again. A cap already applied is not undone: an override that
    // frees room does not give an estimate back its capped increment.
    if let Some(cap) = timesheet.settings.daily_cap_seconds {
        let increment = timesheet.settings.increment_seconds;
        for date in &touched {
            let start = timesheet
                .entries
                .partition_point(|entry| entry.date < *date);
            let end = timesheet
                .entries
                .partition_point(|entry| entry.date <= *date);
            if let Some(warning) =
                round::enforce_daily_cap(&mut timesheet.entries[start..end], increment, cap)
            {
                timesheet.warnings.push(warning);
            }
        }
    }

    // An estimate the cap took down to nothing is dropped like any other.
    let entries = std::mem::take(&mut timesheet.entries);
    for entry in entries {
        let gone = touched.contains(&entry.date)
            && entry.estimated_seconds == 0
            && entry.override_seconds.is_none()
            && entry.manual_seconds == 0;
        if gone {
            timesheet.dropped.push(DroppedEntry {
                date: entry.date,
                engagement: entry.engagement.clone(),
                detail: entry.detail.clone(),
                raw_seconds: entry.raw_seconds,
            });
        } else {
            timesheet.entries.push(entry);
        }
    }

    for entry in &mut timesheet.entries {
        if !touched.contains(&entry.date) {
            continue;
        }
        entry.status = if entry.override_seconds.is_some() {
            EntryStatus::Overridden
        } else if entry.estimated_seconds == 0 && entry.manual_seconds > 0 {
            EntryStatus::Manual
        } else {
            EntryStatus::Suggested
        };
    }
    for engagement in unknown {
        timesheet.warnings.push(format!(
            "the ledger names engagement {engagement:?}, which is not configured (any more); its hours are shown under that key"
        ));
    }
    compute::finalize(&mut timesheet.entries);
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;
    use crate::timesheet::model::{Adjustment, TimesheetMethodology, TimesheetSettings};

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, day).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap()
    }

    fn engagements() -> Engagements {
        Engagements::compile(
            Some(&json!({
                "acme": {"label": "ACME", "client": "ACME AS", "rate": 1000, "currency": "NOK",
                         "paths": ["/work/acme"]},
                "internal": {"label": "Internal", "billable": false, "paths": ["/work/internal"]},
            })),
            &std::collections::BTreeMap::new(),
            Path::new("/"),
        )
        .unwrap()
    }

    fn manual(id: &str, day: u32, engagement: &str, seconds: u64) -> ManualEntry {
        ManualEntry {
            id: id.to_string(),
            date: date(day),
            engagement: engagement.to_string(),
            seconds,
            note: None,
            billable: None,
            start: None,
            created_at: now(),
        }
    }

    fn overriding(day: u32, engagement: &str, seconds: u64) -> Override {
        Override {
            date: date(day),
            engagement: engagement.to_string(),
            seconds,
            note: None,
            created_at: now(),
        }
    }

    fn estimate(day: u32, engagement: &str, seconds: u64, billable: bool) -> TimesheetEntry {
        let configured = engagements();
        let mut entry = blank_entry(&configured, date(day), engagement, None, billable);
        entry.raw_seconds = seconds as f64;
        entry.estimated_seconds = seconds;
        entry.final_seconds = seconds;
        entry.evidence.prompts = 3;
        entry
    }

    fn sheet(entries: Vec<TimesheetEntry>) -> Timesheet {
        let mut sheet = Timesheet {
            window: TimesheetWindow::default(),
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

    fn applied(ledger: &Ledger, timesheet: &mut Timesheet) {
        let engagements = engagements();
        let context = Context {
            ledger,
            engagements: &engagements,
            ignore_locks: false,
            current: settings(),
        };
        apply(timesheet, &context).unwrap();
        compute::finalize(&mut timesheet.entries);
    }

    #[test]
    fn applying_the_empty_ledger_changes_nothing() {
        let mut timesheet = sheet(vec![estimate(12, "acme", 3600, true)]);
        let before = timesheet.clone();
        applied(&Ledger::default(), &mut timesheet);
        assert_eq!(before, timesheet);
    }

    #[test]
    fn an_override_replaces_the_estimate_and_manual_hours_add_on_top() {
        let mut ledger = Ledger::default();
        ledger.overrides.push(overriding(12, "acme", 5400));
        ledger.entries.push(manual("aaaa1111", 12, "acme", 1800));
        let mut timesheet = sheet(vec![estimate(12, "acme", 3600, true)]);
        applied(&ledger, &mut timesheet);
        let entry = &timesheet.entries[0];
        assert_eq!(1, timesheet.entries.len());
        assert_eq!(Some(5400), entry.override_seconds);
        assert_eq!(1800, entry.manual_seconds);
        assert_eq!(3600, entry.estimated_seconds, "the estimate stays visible");
        assert_eq!(7200, entry.final_seconds);
        assert_eq!(EntryStatus::Overridden, entry.status);
        // 2h at 1000/h, computed after the ledger.
        assert_eq!(Some(2000.0), entry.amount);
        assert!(entry.notes.iter().any(|note| note.contains("aaaa1111")));
    }

    #[test]
    fn zero_suppresses_the_estimate_but_keeps_the_row() {
        let mut ledger = Ledger::default();
        ledger.overrides.push(overriding(12, "acme", 0));
        let mut timesheet = sheet(vec![estimate(12, "acme", 3600, true)]);
        applied(&ledger, &mut timesheet);
        let entry = &timesheet.entries[0];
        assert_eq!(0, entry.final_seconds);
        assert_eq!(EntryStatus::Overridden, entry.status);
        assert!(entry.notes.iter().any(|note| note.contains("suppressed")));
        // And a zero override with nothing to suppress adds no row.
        ledger.overrides[0].date = date(13);
        let mut timesheet = sheet(Vec::new());
        applied(&ledger, &mut timesheet);
        assert!(timesheet.entries.is_empty());
    }

    #[test]
    fn manual_hours_on_a_quiet_day_make_a_manual_entry_with_the_engagements_billing() {
        let mut ledger = Ledger::default();
        let mut item = manual("bbbb2222", 14, "acme", 3600);
        item.note = Some("Steering meeting".into());
        item.start = Some("09:00".into());
        ledger.entries.push(item);
        let mut timesheet = sheet(Vec::new());
        applied(&ledger, &mut timesheet);
        let entry = &timesheet.entries[0];
        assert_eq!(EntryStatus::Manual, entry.status);
        assert_eq!(3600, entry.final_seconds);
        assert!(entry.billable);
        assert_eq!(Some("NOK"), entry.currency.as_deref());
        assert_eq!(Some(1000.0), entry.amount);
        assert_eq!("ACME", entry.label);
        assert!(entry.first_start.is_some());
        assert!(entry.notes[0].contains("Steering meeting"));
    }

    #[test]
    fn explicit_billing_that_differs_gets_its_own_row() {
        let mut ledger = Ledger::default();
        let mut item = manual("cccc3333", 12, "acme", 1800);
        item.billable = Some(false);
        ledger.entries.push(item);
        let mut timesheet = sheet(vec![estimate(12, "acme", 3600, true)]);
        applied(&ledger, &mut timesheet);
        assert_eq!(2, timesheet.entries.len());
        let extra = timesheet
            .entries
            .iter()
            .find(|entry| entry.detail.is_some())
            .unwrap();
        assert!(!extra.billable);
        assert_eq!(None, extra.amount);
        assert_eq!(1800, extra.final_seconds);
        let base = timesheet
            .entries
            .iter()
            .find(|entry| entry.detail.is_none())
            .unwrap();
        assert!(base.billable);
        assert_eq!(
            0, base.manual_seconds,
            "the estimate's billing is untouched"
        );
    }

    #[test]
    fn with_detail_the_override_covers_the_engagements_day() {
        let mut first = estimate(12, "acme", 3600, true);
        first.detail = Some("ACME-1".into());
        let mut second = estimate(12, "acme", 1800, true);
        second.detail = Some("ACME-2".into());
        let mut ledger = Ledger::default();
        ledger.overrides.push(overriding(12, "acme", 7200));
        let mut timesheet = sheet(vec![first, second]);
        applied(&ledger, &mut timesheet);
        let total: u64 = timesheet
            .entries
            .iter()
            .map(|entry| entry.final_seconds)
            .sum();
        assert_eq!(7200, total);
    }

    #[test]
    fn the_daily_cap_is_enforced_again_with_manual_hours_counted_first() {
        let mut timesheet = sheet(vec![
            estimate(12, "acme", 4 * 3600, true),
            estimate(12, "internal", 3 * 3600, false),
        ]);
        timesheet.settings.daily_cap_seconds = Some(8 * 3600);
        let mut ledger = Ledger::default();
        ledger.entries.push(manual("dddd4444", 12, "acme", 3600));
        applied(&ledger, &mut timesheet);
        let day: u64 = timesheet
            .entries
            .iter()
            .map(|entry| entry.final_seconds)
            .sum();
        assert_eq!(
            8 * 3600,
            day,
            "7h estimated + 1h manual fits the cap exactly"
        );
        // 4h + 3h + 1h = 8h: nothing capped yet; add another hour.
        ledger.entries.push(manual("eeee5555", 12, "acme", 3600));
        let mut timesheet = sheet(vec![
            estimate(12, "acme", 4 * 3600, true),
            estimate(12, "internal", 3 * 3600, false),
        ]);
        timesheet.settings.daily_cap_seconds = Some(8 * 3600);
        applied(&ledger, &mut timesheet);
        let day: u64 = timesheet
            .entries
            .iter()
            .map(|entry| entry.final_seconds)
            .sum();
        assert_eq!(8 * 3600, day);
        let manual_total: u64 = timesheet
            .entries
            .iter()
            .map(|entry| entry.manual_seconds)
            .sum();
        assert_eq!(7200, manual_total, "manual hours are never reduced");
        assert!(
            timesheet
                .entries
                .iter()
                .any(|entry| entry.adjustments.contains(&Adjustment::Capped))
        );
    }

    #[test]
    fn manual_hours_beyond_the_cap_are_kept_and_warned_about() {
        let mut timesheet = sheet(vec![estimate(12, "internal", 3600, false)]);
        timesheet.settings.daily_cap_seconds = Some(3600);
        let mut ledger = Ledger::default();
        ledger
            .entries
            .push(manual("ffff6666", 12, "acme", 2 * 3600));
        applied(&ledger, &mut timesheet);
        assert!(timesheet.warnings.iter().any(|w| w.contains("daily cap")));
        let acme = timesheet
            .entries
            .iter()
            .find(|e| e.engagement == "acme")
            .unwrap();
        assert_eq!(7200, acme.final_seconds);
        // The estimate was taken to nothing and is listed, not lost.
        assert_eq!(1, timesheet.dropped.len());
    }

    #[test]
    fn items_outside_the_window_are_left_alone() {
        let mut ledger = Ledger::default();
        ledger.entries.push(manual("aaaa0000", 12, "acme", 3600));
        let mut timesheet = sheet(Vec::new());
        timesheet.window = TimesheetWindow {
            since: Some(day_start(date(1))),
            until: Some(day_start(date(10))),
        };
        applied(&ledger, &mut timesheet);
        assert!(timesheet.entries.is_empty());
    }

    #[test]
    fn a_ledger_naming_a_vanished_engagement_still_counts_and_warns() {
        let mut ledger = Ledger::default();
        ledger.entries.push(manual("aaaa7777", 12, "gone", 3600));
        let mut timesheet = sheet(Vec::new());
        applied(&ledger, &mut timesheet);
        assert_eq!(3600, timesheet.entries[0].final_seconds);
        assert!(timesheet.warnings.iter().any(|w| w.contains("gone")));
    }

    // ------------------------------------------------------------ storage

    #[test]
    fn a_missing_ledger_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger::load(&directory.path().join("timesheet.json")).unwrap();
        assert_eq!(Ledger::default(), ledger);
    }

    #[test]
    fn saving_round_trips_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("timesheet.json");
        let mut ledger = Ledger::default();
        let id = ledger.add_entry(manual("", 12, "acme", 3600)).unwrap();
        ledger.set_override(overriding(12, "acme", 900)).unwrap();
        ledger.save(&path).unwrap();
        assert_eq!(ledger, Ledger::load(&path).unwrap());
        assert_eq!(8, id.len());
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(1, leftovers.len(), "{leftovers:?}");
        // The file has the documented shape.
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(1, value["version"]);
        assert_eq!("acme", value["entries"][0]["engagement"]);
        assert_eq!("2026-08-12", value["overrides"][0]["date"]);
    }

    #[test]
    fn an_unreadable_ledger_is_a_hard_error_naming_the_file() {
        let directory = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("garbage", "this is not json".to_string()),
            ("future", r#"{"version": 2}"#.to_string()),
            ("unknown", r#"{"version": 1, "entires": []}"#.to_string()),
            (
                "too long a day",
                r#"{"version":1,"entries":[{"id":"a","date":"2026-08-12","engagement":"acme","seconds":90000,"created_at":"2026-08-12T00:00:00Z"}]}"#.to_string(),
            ),
        ] {
            let path = directory.path().join(format!("{name}.json"));
            fs::write(&path, body).unwrap();
            let error = format!("{:#}", Ledger::load(&path).unwrap_err());
            assert!(
                error.contains(path.to_str().unwrap()) && error.contains("never ignored"),
                "{name}: {error}"
            );
        }
        // A directory where the file should be cannot be read either.
        let error = Ledger::load(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("never ignored"));
    }

    #[test]
    fn ids_come_from_the_content_and_never_clash() {
        let mut ledger = Ledger::default();
        let first = ledger.add_entry(manual("", 12, "acme", 3600)).unwrap();
        let second = ledger.add_entry(manual("", 12, "acme", 3600)).unwrap();
        assert_ne!(first, second, "identical content still gets distinct ids");
    }

    #[test]
    fn limits_are_enforced() {
        let mut ledger = Ledger::default();
        assert!(
            ledger
                .add_entry(manual("", 12, "acme", MAX_SECONDS + 1))
                .is_err()
        );
        assert!(check_note("one\nline").is_err());
        assert!(check_note(&"x".repeat(MAX_NOTE_BYTES + 1)).is_err());
        assert!(check_start("25:00").is_err());
        assert!(check_start("9:30").is_ok());
        for index in 0..MAX_ITEMS {
            ledger
                .entries
                .push(manual(&format!("{index:08x}"), 12, "acme", 60));
        }
        assert!(ledger.add_entry(manual("", 12, "acme", 60)).is_err());
        assert!(ledger.set_override(overriding(13, "acme", 60)).is_err());
        // Replacing an existing override needs no room.
        ledger.entries.pop();
        ledger.set_override(overriding(13, "acme", 60)).unwrap();
        assert_eq!(
            Some(60),
            ledger.set_override(overriding(13, "acme", 120)).unwrap()
        );
    }

    #[test]
    fn the_fingerprint_follows_entries_and_overrides_only() {
        let mut ledger = Ledger::default();
        let empty = ledger.fingerprint();
        ledger.entries.push(manual("aaaa1111", 12, "acme", 60));
        let one = ledger.fingerprint();
        assert_ne!(empty, one);
        ledger.record_forced(date(12), "acme", "add", None, now());
        assert_eq!(
            one,
            ledger.fingerprint(),
            "forced writes are evidence, not hours"
        );
        ledger.overrides.push(overriding(12, "acme", 60));
        assert_ne!(one, ledger.fingerprint());
        assert!(one.starts_with("sha256:"));
    }

    #[test]
    fn removing_and_unsetting_report_what_was_not_there() {
        let mut ledger = Ledger::default();
        assert!(ledger.remove_entry("nope").is_err());
        assert!(ledger.unset_override(date(12), "acme").is_err());
        let id = ledger.add_entry(manual("", 12, "acme", 60)).unwrap();
        assert_eq!(60, ledger.remove_entry(&id).unwrap().seconds);
        ledger.set_override(overriding(12, "acme", 60)).unwrap();
        assert_eq!(60, ledger.unset_override(date(12), "acme").unwrap().seconds);
    }

    #[test]
    fn a_day_is_in_the_window_by_its_local_start() {
        let window = TimesheetWindow {
            since: Some(day_start(date(1))),
            until: Some(day_start(date(10))),
        };
        assert!(in_window(&window, date(1)));
        assert!(in_window(&window, date(9)));
        assert!(!in_window(&window, date(10)));
        assert!(!in_window(
            &window,
            NaiveDate::from_ymd_opt(2026, 7, 31).unwrap()
        ));
        assert!(in_window(&TimesheetWindow::default(), date(10)));
        assert!(day_start(date(2)) - day_start(date(1)) >= Duration::hours(23));
    }
}
