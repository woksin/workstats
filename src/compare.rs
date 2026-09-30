//! `--compare`: the headline figures of two windows side by side, with the
//! change between them.
//!
//! Only the summary is compared. Two reports whose grouped rows differ cannot
//! be lined up row for row without deciding what a repository that appears on
//! one side only means, and the headline is what a month-over-month question
//! is about. Both sides come from the same `Summary` the plain report prints,
//! so a figure here is never a second calculation of one shown elsewhere.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::model::Summary;
use crate::timeutil::window_label;

/// What a figure measures, which decides how a reader formats it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Unit {
    Seconds,
    Count,
    /// A multiple, such as agent concurrency.
    Ratio,
}

/// One compared figure. `optional` ones are left out of the readable views
/// when both windows are zero, as the plain report omits the same lines; the
/// JSON always carries them so its shape does not depend on the data.
pub(crate) struct Metric {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) unit: Unit,
    optional: bool,
    get: fn(&Figures) -> f64,
}

macro_rules! metric {
    ($key:ident, $label:expr, $unit:expr, $optional:expr) => {
        Metric {
            key: stringify!($key),
            label: $label,
            unit: $unit,
            optional: $optional,
            get: |figures| figures.$key as f64,
        }
    };
}

/// In reading order: the human estimate first, then what was observed around it.
pub(crate) const METRICS: &[Metric] = &[
    metric!(
        human_estimated_seconds,
        "Estimated human work",
        Unit::Seconds,
        false
    ),
    metric!(human_active_days, "Active work days", Unit::Count, false),
    metric!(prompt_signal_count, "Prompts", Unit::Count, false),
    metric!(session_count, "Sessions", Unit::Count, false),
    metric!(
        foreground_session_count,
        "Foreground sessions",
        Unit::Count,
        false
    ),
    metric!(
        subagent_session_count,
        "Subagent sessions",
        Unit::Count,
        false
    ),
    metric!(commit_count, "Git commits", Unit::Count, false),
    metric!(additions, "Lines added", Unit::Count, false),
    metric!(deletions, "Lines removed", Unit::Count, false),
    metric!(
        agent_commit_count,
        "Agent-authored commits",
        Unit::Count,
        true
    ),
    metric!(agent_additions, "Agent lines added", Unit::Count, true),
    metric!(agent_deletions, "Agent lines removed", Unit::Count, true),
    metric!(
        ai_assisted_commit_count,
        "Co-authored by AI",
        Unit::Count,
        true
    ),
    metric!(agent_wall_seconds, "Agent wall clock", Unit::Seconds, true),
    metric!(
        parallel_agent_seconds,
        "Parallel agent work",
        Unit::Seconds,
        true
    ),
    metric!(agent_concurrency, "Agent concurrency", Unit::Ratio, true),
];

const NOTE: &str = "Human work and active days are estimates, not stopwatch times, and so are their changes. Agent figures come from local histories, which can be pruned or cover different tools in the two windows.";

/// The summary figures both sides are compared on.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Figures {
    pub human_estimated_seconds: f64,
    pub human_active_days: usize,
    pub prompt_signal_count: usize,
    pub session_count: usize,
    pub foreground_session_count: usize,
    pub subagent_session_count: usize,
    pub commit_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub agent_commit_count: usize,
    pub agent_additions: u64,
    pub agent_deletions: u64,
    pub ai_assisted_commit_count: usize,
    /// `ai_assisted_commit_count` as a share of `commit_count`; absent when
    /// there were no commits, because the share is undefined rather than zero.
    pub ai_assisted_commit_share: Option<f64>,
    pub agent_wall_seconds: f64,
    pub parallel_agent_seconds: f64,
    /// Agent work for each second any agent was active; zero when none was.
    pub agent_concurrency: f64,
    /// Each file area's share of the changed Git lines.
    pub composition: Vec<AreaShare>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AreaShare {
    pub category: String,
    pub share: f64,
}

impl Figures {
    pub(crate) fn from_summary(summary: &Summary) -> Self {
        Self {
            human_estimated_seconds: summary.human_estimated_seconds,
            human_active_days: summary.human_active_days,
            prompt_signal_count: summary.prompt_signal_count,
            session_count: summary.session_count,
            foreground_session_count: summary.foreground_session_count,
            subagent_session_count: summary.subagent_session_count,
            commit_count: summary.commit_count,
            additions: summary.additions,
            deletions: summary.deletions,
            agent_commit_count: summary.agent_commit_count,
            agent_additions: summary.agent_additions,
            agent_deletions: summary.agent_deletions,
            ai_assisted_commit_count: summary.ai_assisted_commit_count,
            ai_assisted_commit_share: (summary.commit_count != 0)
                .then(|| summary.ai_assisted_commit_count as f64 / summary.commit_count as f64),
            agent_wall_seconds: summary.agent_wall_seconds,
            parallel_agent_seconds: summary.parallel_agent_seconds,
            agent_concurrency: if summary.agent_wall_seconds == 0.0 {
                0.0
            } else {
                summary.parallel_agent_seconds / summary.agent_wall_seconds
            },
            composition: summary
                .composition
                .iter()
                .map(|entry| AreaShare {
                    category: entry.category.clone(),
                    share: entry.share_of_changed_lines,
                })
                .collect(),
        }
    }
}

/// One window and what was measured in it.
#[derive(Debug, Serialize)]
pub(crate) struct Period {
    /// `2026-08`, `2026-W09`, `2026`, or the first and last day.
    pub label: String,
    /// The half-open window, `[since, until)`.
    pub since: String,
    pub until: String,
    pub figures: Figures,
}

impl Period {
    pub(crate) fn new(window: (DateTime<Utc>, DateTime<Utc>), summary: &Summary) -> Self {
        Self {
            label: window_label(window.0, window.1),
            since: window.0.to_rfc3339_opts(SecondsFormat::Secs, true),
            until: window.1.to_rfc3339_opts(SecondsFormat::Secs, true),
            figures: Figures::from_summary(summary),
        }
    }
}

/// One decimal place: the shares a change is computed from are already rounded
/// to three, so more digits would only print float noise.
fn tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// `current - previous`, and that as a percentage of `previous`.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Change {
    pub change: f64,
    /// Absent when `previous` is zero: growth from nothing has no percentage,
    /// and infinity is not a figure a reader can use.
    pub percent: Option<f64>,
}

impl Change {
    fn between(current: f64, previous: f64) -> Self {
        Self {
            change: current - previous,
            percent: (previous != 0.0).then(|| tenth((current - previous) / previous * 100.0)),
        }
    }
}

/// A share in each window and the change between them in percentage points.
/// Absent where a window has nothing to take a share of.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct ShareChange {
    pub current: Option<f64>,
    pub previous: Option<f64>,
    pub change_points: Option<f64>,
}

impl ShareChange {
    fn between(current: Option<f64>, previous: Option<f64>) -> Self {
        Self {
            current,
            previous,
            change_points: current.zip(previous).map(|(c, p)| tenth((c - p) * 100.0)),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AreaChange {
    pub category: String,
    #[serde(flatten)]
    pub share: ShareChange,
}

#[derive(Debug, Serialize)]
pub(crate) struct Delta {
    #[serde(flatten)]
    pub figures: BTreeMap<&'static str, Change>,
    pub ai_assisted_commit_share: ShareChange,
    pub composition: Vec<AreaChange>,
}

/// The `comparison` block of a report: the selected window, the one it is
/// compared to, and the change from the second to the first.
#[derive(Debug, Serialize)]
pub(crate) struct Comparison {
    /// `previous`, or the window `--compare` named.
    pub basis: String,
    pub note: &'static str,
    pub current: Period,
    pub previous: Period,
    pub delta: Delta,
}

/// A compared figure ready to print.
pub(crate) struct Line {
    pub label: &'static str,
    pub unit: Unit,
    pub current: f64,
    pub previous: f64,
    pub change: Change,
}

/// A share ready to print.
pub(crate) struct ShareLine {
    pub label: String,
    pub share: ShareChange,
}

impl Comparison {
    pub(crate) fn new(current: Period, previous: Period, basis: String) -> Self {
        let delta = Delta {
            figures: METRICS
                .iter()
                .map(|metric| {
                    (
                        metric.key,
                        Change::between(
                            (metric.get)(&current.figures),
                            (metric.get)(&previous.figures),
                        ),
                    )
                })
                .collect(),
            ai_assisted_commit_share: ShareChange::between(
                current.figures.ai_assisted_commit_share,
                previous.figures.ai_assisted_commit_share,
            ),
            composition: area_changes(&current.figures, &previous.figures),
        };
        Self {
            basis,
            note: NOTE,
            current,
            previous,
            delta,
        }
    }

    /// The figures a reader is shown, in order. A group that is zero in both
    /// windows is left out, the way the plain report leaves out its line.
    pub(crate) fn lines(&self) -> Vec<Line> {
        METRICS
            .iter()
            .filter_map(|metric| {
                let current = (metric.get)(&self.current.figures);
                let previous = (metric.get)(&self.previous.figures);
                (!metric.optional || current != 0.0 || previous != 0.0).then(|| Line {
                    label: metric.label,
                    unit: metric.unit,
                    current,
                    previous,
                    change: self.delta.figures[metric.key],
                })
            })
            .collect()
    }

    /// The AI-co-authored share, when either window has one, then each file
    /// area's share of changed lines.
    pub(crate) fn share_lines(&self) -> Vec<ShareLine> {
        let ai = self.delta.ai_assisted_commit_share;
        let mut lines = Vec::new();
        if ai.current.is_some_and(|share| share != 0.0)
            || ai.previous.is_some_and(|share| share != 0.0)
        {
            lines.push(ShareLine {
                label: "Commits co-authored by AI".to_string(),
                share: ai,
            });
        }
        lines.extend(self.delta.composition.iter().map(|area| ShareLine {
            label: format!("{} lines", area.category),
            share: area.share,
        }));
        lines
    }
}

/// Every file area either window touched, the current window's order first.
/// An area absent from a window that did change lines is a zero share there;
/// a window that changed no lines has no shares at all.
fn area_changes(current: &Figures, previous: &Figures) -> Vec<AreaChange> {
    let mut categories: Vec<&str> = current
        .composition
        .iter()
        .map(|area| area.category.as_str())
        .collect();
    for area in &previous.composition {
        if !categories.contains(&area.category.as_str()) {
            categories.push(&area.category);
        }
    }
    let share = |figures: &Figures, category: &str| {
        (!figures.composition.is_empty()).then(|| {
            figures
                .composition
                .iter()
                .find(|area| area.category == category)
                .map_or(0.0, |area| area.share)
        })
    };
    categories
        .into_iter()
        .map(|category| AreaChange {
            category: category.to_string(),
            share: ShareChange::between(share(current, category), share(previous, category)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn figures(human: f64, commits: usize, areas: &[(&str, f64)]) -> Figures {
        Figures {
            human_estimated_seconds: human,
            human_active_days: 0,
            prompt_signal_count: 0,
            session_count: 0,
            foreground_session_count: 0,
            subagent_session_count: 0,
            commit_count: commits,
            additions: 0,
            deletions: 0,
            agent_commit_count: 0,
            agent_additions: 0,
            agent_deletions: 0,
            ai_assisted_commit_count: 0,
            ai_assisted_commit_share: None,
            agent_wall_seconds: 0.0,
            parallel_agent_seconds: 0.0,
            agent_concurrency: 0.0,
            composition: areas
                .iter()
                .map(|(category, share)| AreaShare {
                    category: (*category).to_string(),
                    share: *share,
                })
                .collect(),
        }
    }

    fn period(figures: Figures) -> Period {
        Period {
            label: "x".into(),
            since: String::new(),
            until: String::new(),
            figures,
        }
    }

    #[test]
    fn a_change_from_zero_has_no_percentage_rather_than_infinity() {
        let change = Change::between(5.0, 0.0);
        assert_eq!(5.0, change.change);
        assert_eq!(None, change.percent);
        assert_eq!(Some(50.0), Change::between(15.0, 10.0).percent);
        assert_eq!(Some(-100.0), Change::between(0.0, 10.0).percent);
        let json = serde_json::to_value(change).unwrap();
        assert!(json["percent"].is_null());
    }

    #[test]
    fn shares_are_compared_in_percentage_points_per_area() {
        let comparison = Comparison::new(
            period(figures(0.0, 1, &[("source", 0.75), ("test", 0.25)])),
            period(figures(0.0, 1, &[("source", 0.5), ("docs", 0.5)])),
            "previous".into(),
        );
        let areas: Vec<_> = comparison
            .delta
            .composition
            .iter()
            .map(|area| (area.category.as_str(), area.share))
            .collect();
        assert_eq!(
            ["source", "test", "docs"],
            [areas[0].0, areas[1].0, areas[2].0]
        );
        assert_eq!(Some(25.0), areas[0].1.change_points);
        // Absent from the previous window's changed lines: zero there, not unknown.
        assert_eq!(Some(0.0), areas[1].1.previous);
        assert_eq!(Some(25.0), areas[1].1.change_points);
        assert_eq!(Some(-50.0), areas[2].1.change_points);
    }

    #[test]
    fn a_window_with_no_changed_lines_has_no_shares_to_compare() {
        let comparison = Comparison::new(
            period(figures(0.0, 0, &[])),
            period(figures(0.0, 1, &[("source", 1.0)])),
            "previous".into(),
        );
        let source = &comparison.delta.composition[0];
        assert_eq!(None, source.share.current);
        assert_eq!(None, source.share.change_points);
    }

    #[test]
    fn optional_groups_are_hidden_only_when_both_windows_are_zero() {
        let mut current = figures(3600.0, 2, &[]);
        let previous = figures(1800.0, 1, &[]);
        let labels = |comparison: &Comparison| -> Vec<&'static str> {
            comparison.lines().iter().map(|line| line.label).collect()
        };
        let comparison = Comparison::new(
            period(current.clone()),
            period(previous.clone()),
            "previous".into(),
        );
        assert!(labels(&comparison).contains(&"Estimated human work"));
        assert!(!labels(&comparison).contains(&"Agent-authored commits"));
        current.agent_commit_count = 1;
        let comparison = Comparison::new(period(current), period(previous), "previous".into());
        assert!(labels(&comparison).contains(&"Agent-authored commits"));
        // Hidden from the readable views, never from the JSON.
        assert!(serde_json::to_value(&comparison).unwrap()["delta"]["agent_additions"].is_object());
    }
}
