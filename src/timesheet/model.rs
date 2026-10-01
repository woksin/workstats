//! The timesheet data contract: what an entry, a day and a whole timesheet
//! are, and the settings that produced them. Computation, rendering and the
//! ledger all speak these types, so they are defined once and before any of
//! them exist.

use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use serde::Serialize;

/// How a raw duration becomes a whole number of increments.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rounding {
    /// Half-up to the nearest increment.
    #[default]
    Nearest,
    Up,
    Down,
    /// Per day: the day total is rounded, and the leftover increments go to
    /// the entries with the largest remainders.
    Balanced,
}

/// How a work block's time is shared between the engagements inside it. The
/// block total never changes; only who it is attributed to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SplitRule {
    /// Each moment goes to the engagement of the nearest signal.
    #[default]
    Nearest,
    /// In proportion to the effective signals per engagement.
    Signals,
    /// In proportion to each engagement's agent time inside the block.
    Agent,
}

/// What an engagement is broken down by beneath it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Detail {
    Issue,
    Feature,
    Branch,
    Repo,
}

/// Whether work matching no engagement is listed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnassignedMode {
    #[default]
    Show,
    Hide,
}

/// How the totals are grouped for display.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TotalsBy {
    #[default]
    Day,
    Week,
}

/// A vendor CSV layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportPreset {
    Toggl,
    Harvest,
    Clockify,
    Generic,
}

/// What stands behind an entry: counts of the signals attributed to it and the
/// names they carried.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Evidence {
    pub(crate) prompts: usize,
    pub(crate) commits: usize,
    /// Distinct foreground `(provider, session_id)` pairs.
    pub(crate) sessions: usize,
    pub(crate) blocks: usize,
    pub(crate) repos: Vec<String>,
    pub(crate) branches: Vec<String>,
    pub(crate) issues: Vec<String>,
}

/// Where an entry's final value came from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryStatus {
    /// Estimated from activity and rounded.
    #[default]
    Suggested,
    /// An override replaced the estimate.
    Overridden,
    /// Entered by hand; no activity behind it.
    Manual,
    /// Taken from a lock snapshot.
    Locked,
}

/// A change made to an entry after its raw value was known, kept so the
/// reader can see why the final figure differs from the estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Adjustment {
    /// Reduced to fit the daily cap.
    Capped,
    /// Raised to the minimum entry.
    RaisedToMinimum,
    /// Received a leftover increment while balancing the day.
    Balanced,
}

/// One day, one engagement (and optionally one detail key).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TimesheetEntry {
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    pub(crate) detail: Option<String>,
    pub(crate) label: String,
    pub(crate) client: Option<String>,
    pub(crate) billable: bool,
    /// The unrounded estimate.
    pub(crate) raw_seconds: f64,
    /// The estimate after rounding, minimum and cap.
    pub(crate) estimated_seconds: u64,
    /// Hours added by hand for this day and engagement.
    pub(crate) manual_seconds: u64,
    /// A value that replaced the estimate.
    pub(crate) override_seconds: Option<u64>,
    /// What is submitted: the override or the estimate, plus manual hours.
    pub(crate) final_seconds: u64,
    pub(crate) first_start: Option<DateTime<Utc>>,
    pub(crate) last_end: Option<DateTime<Utc>>,
    pub(crate) evidence: Evidence,
    pub(crate) rate: Option<f64>,
    pub(crate) currency: Option<String>,
    pub(crate) amount: Option<f64>,
    pub(crate) notes: Vec<String>,
    pub(crate) description: Option<String>,
    pub(crate) status: EntryStatus,
    pub(crate) adjustments: Vec<Adjustment>,
    /// Against a lock snapshot: current minus locked, in seconds.
    pub(crate) lock_drift_seconds: Option<i64>,
}

/// The window the timesheet covers.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct TimesheetWindow {
    pub(crate) since: Option<DateTime<Utc>>,
    pub(crate) until: Option<DateTime<Utc>>,
}

/// Every setting that shapes the figures. Recorded on the output, and in a
/// lock, so a later difference can be traced to what changed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TimesheetSettings {
    pub(crate) increment_seconds: u64,
    pub(crate) rounding: Rounding,
    pub(crate) min_entry_seconds: u64,
    pub(crate) drop_below_seconds: u64,
    pub(crate) daily_cap_seconds: Option<u64>,
    pub(crate) split: SplitRule,
    pub(crate) unassigned: UnassignedMode,
    pub(crate) detail: Option<Detail>,
}

impl Default for TimesheetSettings {
    fn default() -> Self {
        Self {
            increment_seconds: 15 * 60,
            rounding: Rounding::default(),
            min_entry_seconds: 0,
            drop_below_seconds: 0,
            daily_cap_seconds: None,
            split: SplitRule::default(),
            unassigned: UnassignedMode::default(),
            detail: None,
        }
    }
}

/// An entry that rounding removed, listed so nothing disappears silently.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DroppedEntry {
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    pub(crate) detail: Option<String>,
    pub(crate) raw_seconds: f64,
}

/// One engagement's total under one split rule, for the cross-check table.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct CrossCheckRow {
    pub(crate) engagement: String,
    pub(crate) nearest_seconds: f64,
    pub(crate) signals_seconds: f64,
    pub(crate) agent_seconds: f64,
}

/// One day and engagement where a lock's snapshot and the current
/// computation disagree, with the most likely reason.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DriftRow {
    /// The locked period the snapshot belongs to.
    pub(crate) period: String,
    pub(crate) date: NaiveDate,
    pub(crate) engagement: String,
    pub(crate) detail: Option<String>,
    /// What was submitted: zero when the entry did not exist at lock time.
    pub(crate) locked_seconds: u64,
    /// What the computation says now: zero when the entry no longer exists.
    pub(crate) current_seconds: u64,
    pub(crate) difference_seconds: i64,
    pub(crate) cause: String,
}

/// How the figures were arrived at, stated on every output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TimesheetMethodology {
    /// Always `suggested`: these are estimates for review, not a stopwatch.
    pub(crate) status: &'static str,
    pub(crate) split_rule: String,
    pub(crate) rounding: String,
}

/// The whole result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Timesheet {
    pub(crate) window: TimesheetWindow,
    pub(crate) settings: TimesheetSettings,
    pub(crate) entries: Vec<TimesheetEntry>,
    pub(crate) dropped: Vec<DroppedEntry>,
    pub(crate) cross_check: Vec<CrossCheckRow>,
    pub(crate) warnings: Vec<String>,
    pub(crate) methodology: TimesheetMethodology,
    /// Where locked periods and the current computation disagree.
    pub(crate) drift: Vec<DriftRow>,
    /// The locks whose snapshots stand in for the computation, one line each.
    pub(crate) applied_locks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let settings = TimesheetSettings::default();
        assert_eq!(900, settings.increment_seconds);
        assert_eq!(Rounding::Nearest, settings.rounding);
        assert_eq!(SplitRule::Nearest, settings.split);
        assert_eq!(UnassignedMode::Show, settings.unassigned);
        assert_eq!(None, settings.daily_cap_seconds);
    }

    #[test]
    fn enums_serialize_as_the_words_users_type() {
        assert_eq!(
            r#""balanced""#,
            serde_json::to_string(&Rounding::Balanced).unwrap()
        );
        assert_eq!(
            r#""raised_to_minimum""#,
            serde_json::to_string(&Adjustment::RaisedToMinimum).unwrap()
        );
        assert_eq!(
            r#""suggested""#,
            serde_json::to_string(&EntryStatus::Suggested).unwrap()
        );
    }
}
