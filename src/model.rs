use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::classify::CategoryTally;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ActivityPoint {
    pub timestamp: DateTime<Utc>,
    pub model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExactInterval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_creation_tokens == 0
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenEvent {
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawSession {
    pub provider: String,
    pub session_id: String,
    pub source_file: PathBuf,
    pub cwd: String,
    pub points: Vec<ActivityPoint>,
    pub exact_intervals: Vec<ExactInterval>,
    pub human_points: Vec<ActivityPoint>,
    #[serde(default)]
    pub token_events: Vec<TokenEvent>,
    pub is_subagent: bool,
    pub approximate_cwd: bool,
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub provider: String,
    pub session_id: String,
    pub cwd: String,
    pub repo: String,
    pub root: String,
    pub points: Vec<ActivityPoint>,
    pub exact_intervals: Vec<ExactInterval>,
    pub human_points: Vec<ActivityPoint>,
    pub token_events: Vec<TokenEvent>,
    pub is_subagent: bool,
}

impl Session {
    pub fn first_seen(&self) -> Option<DateTime<Utc>> {
        self.points
            .iter()
            .map(|point| point.timestamp)
            .chain(self.exact_intervals.iter().map(|item| item.start))
            .chain(self.human_points.iter().map(|point| point.timestamp))
            .min()
    }

    pub fn last_seen(&self) -> Option<DateTime<Utc>> {
        self.points
            .iter()
            .map(|point| point.timestamp)
            .chain(self.exact_intervals.iter().map(|item| item.end))
            .chain(self.human_points.iter().map(|point| point.timestamp))
            .max()
    }
}

#[derive(Clone, Debug)]
pub struct HumanSignal {
    pub timestamp: DateTime<Utc>,
    pub provider: String,
    pub session_id: String,
    pub cwd: String,
    pub repo: String,
    pub root: String,
    pub kind: String,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub cwd: String,
    pub repo: String,
    pub root: String,
}

impl Interval {
    pub fn seconds(&self) -> f64 {
        (self.end - self.start)
            .num_microseconds()
            .unwrap_or(0)
            .max(0) as f64
            / 1_000_000.0
    }
}

/// Co-author identities that mean an AI agent helped write the commit.
///
/// Matched against the *value* of a `Co-authored-by:` trailer, lowercased. The
/// e-mail suffix is the only stable part: GitHub has issued at least two
/// numeric ids for the one Copilot identity — `198982749+Copilot@` and
/// `223556219+Copilot@` both occur in this machine's history — so a matcher
/// keyed on the number silently stops finding half of them the day GitHub
/// issues a third.
const AGENT_CO_AUTHORS: &[&str] = &[
    "+copilot@users.noreply.github.com",
    "<copilot@github.com>",
    "copilot-swe-agent[bot]",
    "+claude[bot]@users.noreply.github.com",
    "<noreply@anthropic.com>",
];

/// Copilot Autofix is GitHub's code-scanning remediation, not an interactive
/// assistant, and it is counted apart from one. Its display name is literally
/// "Copilot Autofix powered by AI", so it has to be tested for *first*: any
/// rule looking for the word Copilot claims it, and then every security fix in
/// the report reads as assisted development.
const AUTOFIX_CO_AUTHORS: &[&str] = &[
    "github-code-quality[bot]@",
    "github-advanced-security[bot]@",
];

/// Who Git records as having written a commit, and which AI identities the
/// commit names beside them.
///
/// The distinction is the reason this tool exists. A commit a coding agent
/// authored is landed code the developer never typed, so treating it as
/// evidence that someone was at the keyboard would inflate the single number
/// the report exists to keep honest. A commit the developer wrote *with* an
/// agent is the opposite case: it is already counted, once, and the co-author
/// trailer only records how it was written. That is why assistance is a flag on
/// a commit rather than a signal in its own right — nothing here can turn a
/// trailer into a second piece of work, because nothing here builds a commit
/// out of one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Authorship {
    agent_authored: bool,
    agent_assisted: bool,
    autofix_assisted: bool,
}

impl Authorship {
    /// A commit a coding agent authored. `Authorship::default()` is the other
    /// case — the configured author wrote it — so a commit is human unless a
    /// pass says otherwise.
    pub const fn agent() -> Self {
        Self {
            agent_authored: true,
            agent_assisted: false,
            autofix_assisted: false,
        }
    }

    /// Records one `Co-authored-by:` value against the commit that carries it.
    /// Only the identity is looked at; the trailer is the sole part of a commit
    /// message this tool ever reads, and it is read for *who*, never for what.
    pub fn note_co_author(&mut self, value: &str) {
        let value = value.to_ascii_lowercase();
        if AUTOFIX_CO_AUTHORS.iter().any(|name| value.contains(name)) {
            self.autofix_assisted = true;
        } else if AGENT_CO_AUTHORS.iter().any(|name| value.contains(name)) {
            self.agent_assisted = true;
        }
    }

    pub fn is_agent_authored(&self) -> bool {
        self.agent_authored
    }

    pub fn is_agent_assisted(&self) -> bool {
        self.agent_assisted
    }

    pub fn is_autofix_assisted(&self) -> bool {
        self.autofix_assisted
    }
}

#[derive(Clone, Debug)]
pub struct GitCommit {
    pub sha: String,
    pub timestamp: DateTime<Utc>,
    pub repo: String,
    pub cwd: String,
    pub root: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<String>,
    pub ignored_additions: u64,
    pub ignored_deletions: u64,
    pub categories: CategoryTally,
    pub authorship: Authorship,
}

impl GitCommit {
    /// What this commit is evidence of, on the human timeline — or `None` when
    /// a coding agent authored it.
    ///
    /// Every route from a commit into that timeline runs through here. There is
    /// no other way to build a `HumanSignal` from a commit, so agent-authored
    /// work cannot reach the human estimate by being overlooked in a `map`: the
    /// caller is handed an `Option` it has to deal with, and the only thing it
    /// can do with `None` is drop it.
    pub fn human_signal(&self) -> Option<HumanSignal> {
        if self.authorship.is_agent_authored() {
            return None;
        }
        Some(HumanSignal {
            timestamp: self.timestamp,
            provider: "git".to_string(),
            session_id: self.sha.clone(),
            cwd: self.cwd.clone(),
            repo: self.repo.clone(),
            root: self.root.clone(),
            kind: "commit".to_string(),
            model: "—".to_string(),
        })
    }
}

/// Enough stored warnings to see what went wrong without the report becoming
/// the log. Past it `warn` keeps counting but stops keeping, so `messages` is
/// a sample and `warning_count` is the total. `note` is bounded by the same
/// number for the same reason.
pub const MAX_STORED_MESSAGES: usize = 100;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Diagnostics {
    pub malformed_lines: u64,
    /// Records refused because they carried prompt or response text. That is
    /// the privacy boundary working, not a broken file, so it is counted and
    /// reported apart from `malformed_lines`.
    #[serde(default)]
    pub content_rejections: u64,
    pub unreadable_files: u64,
    pub approximate_cwds: u64,
    pub skipped_sessions: u64,
    pub git_errors: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_writes: u64,
    pub pruned_files: u64,
    /// Copilot sessions whose store row named a repository the session's own
    /// working directory contradicts. Counted rather than warned about: the
    /// directory already decided, so nothing is lost and nothing is actionable
    /// — see `note`.
    #[serde(default)]
    pub repository_conflicts: u64,
    /// Every warning raised, including the ones past `MAX_STORED_MESSAGES`
    /// that were never stored. `messages.len()` is a floor, so a report that
    /// wants the real number has to read this one.
    #[serde(default)]
    pub warning_count: u64,
    pub messages: Vec<String>,
    /// The same floor-and-total pair as `messages`/`warning_count`, for notes.
    #[serde(default)]
    pub note_count: u64,
    /// Facts the run resolved by itself, kept out of `messages` so they never
    /// print as warnings. See `note`.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Diagnostics {
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warning_count += 1;
        if self.messages.len() < MAX_STORED_MESSAGES {
            self.messages.push(message.into());
        }
    }

    /// Something the run noticed, resolved correctly, and has no work to hand
    /// the reader: the report is right and there is nothing they could change.
    ///
    /// It is a separate channel from `warn` because history does not change.
    /// A resolved fact about a session that was written months ago reappears on
    /// every single run, forever, and a `Warning:` line the reader can only ever
    /// ignore is how they learn to ignore the ones that mean something. Notes
    /// are counted for the summary and carried in `--format json`, so anyone
    /// who wants the detail can still have it.
    pub fn note(&mut self, message: impl Into<String>) {
        self.note_count += 1;
        if self.notes.len() < MAX_STORED_MESSAGES {
            self.notes.push(message.into());
        }
    }

    pub fn merge(&mut self, other: &Diagnostics) {
        self.malformed_lines += other.malformed_lines;
        self.content_rejections += other.content_rejections;
        self.unreadable_files += other.unreadable_files;
        self.approximate_cwds += other.approximate_cwds;
        self.skipped_sessions += other.skipped_sessions;
        self.git_errors += other.git_errors;
        self.cache_hits += other.cache_hits;
        self.cache_misses += other.cache_misses;
        self.cache_writes += other.cache_writes;
        self.pruned_files += other.pruned_files;
        self.repository_conflicts += other.repository_conflicts;
        // Only the warnings `other` could not store are added here: the loop
        // below goes through `warn`, which counts every message it replays, so
        // adding `other.warning_count` as well would count those twice. The
        // subtraction saturates for a `Diagnostics` whose messages did not come
        // through `warn` — deserialising one without the field defaults it to
        // zero — where the stored messages are the only total there is.
        self.warning_count += other
            .warning_count
            .saturating_sub(other.messages.len() as u64);
        for message in &other.messages {
            self.warn(message.clone());
        }
        // Notes merge by the same rule, and for the same reason.
        self.note_count += other.note_count.saturating_sub(other.notes.len() as u64);
        for note in &other.notes {
            self.note(note.clone());
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Methodology {
    pub human_work: &'static str,
    pub human_idle_threshold_seconds: f64,
    pub review_credit_seconds: f64,
    pub human_estimate_caveat: &'static str,
    pub ai_time: &'static str,
    pub deduplication: &'static str,
    pub gap_cap_seconds: f64,
    /// Names the configured categories, which are not known at compile time.
    pub composition: String,
    pub change_shapes: &'static str,
    /// How commits a coding agent authored are treated. Stated in the report
    /// itself because the number a reader is most likely to misread is the one
    /// that looks like their own output but is not.
    pub agent_output: &'static str,
    pub scope: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Observed {
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

/// Changed Git lines attributed to one file area. `files` counts distinct
/// paths, so it deduplicates a file touched by several commits.
#[derive(Clone, Debug, Serialize)]
pub struct CompositionEntry {
    pub category: String,
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
    pub share_of_changed_lines: f64,
}

/// Commits counted by the observable shape of their diff.
#[derive(Clone, Debug, Serialize)]
pub struct ShapeEntry {
    pub shape: String,
    pub commits: usize,
    pub share_of_classified_commits: f64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub human_estimated_seconds: f64,
    pub human_active_days: usize,
    pub average_human_seconds_per_active_day: f64,
    pub work_block_count: usize,
    pub human_signal_count: usize,
    pub prompt_signal_count: usize,
    pub foreground_session_edge_signal_count: usize,
    pub commit_signal_count: usize,
    pub deduplicated_active_seconds: f64,
    pub attributed_active_seconds: f64,
    pub agent_wall_seconds: f64,
    pub parallel_agent_seconds: f64,
    pub session_count: usize,
    pub foreground_session_count: usize,
    pub subagent_session_count: usize,
    pub foreground_sessions_with_commits: usize,
    pub foreground_sessions_without_commits: usize,
    pub commit_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub ignored_additions: u64,
    pub ignored_deletions: u64,
    /// Commits a coding agent authored, and their churn. Kept beside the
    /// figures above rather than added into them: `--author` is this tool's
    /// promise about whose work is being measured, and code an agent pushed to
    /// a branch is output, not evidence of anyone's day.
    pub agent_commit_count: usize,
    pub agent_additions: u64,
    pub agent_deletions: u64,
    /// Commits already counted above that name an AI agent as co-author. A
    /// share of `commit_count`, never an addition to it.
    pub ai_assisted_commit_count: usize,
    /// Commits already counted above that carry a Copilot Autofix trailer.
    /// Separate from `ai_assisted_commit_count` because code scanning and
    /// assisted development are different activities.
    pub autofix_assisted_commit_count: usize,
    pub composition: Vec<CompositionEntry>,
    pub change_shapes: Vec<ShapeEntry>,
    pub active_days: usize,
    pub provider_seconds: BTreeMap<String, f64>,
    pub model_seconds: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
    pub provider_tokens: BTreeMap<String, u64>,
    pub model_tokens: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub struct ReportRow {
    pub key: BTreeMap<String, String>,
    pub active_seconds: f64,
    pub parallel_agent_seconds: f64,
    pub ai_wall_seconds: f64,
    pub human_estimated_seconds: f64,
    pub human_signal_count: usize,
    pub work_block_count: usize,
    pub session_count: usize,
    pub commit_count: usize,
    pub foreground_session_count: usize,
    pub subagent_session_count: usize,
    pub file_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub ignored_additions: u64,
    pub ignored_deletions: u64,
    pub net_lines: i64,
    /// The same split as on `Summary`: agent-authored output beside the row's
    /// own, never inside it.
    pub agent_commit_count: usize,
    pub agent_additions: u64,
    pub agent_deletions: u64,
    pub ai_assisted_commit_count: usize,
    pub autofix_assisted_commit_count: usize,
    pub composition: Vec<CompositionEntry>,
    pub change_shapes: Vec<ShapeEntry>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
    pub active_days: usize,
    pub human_active_days: usize,
    pub calendar_days: usize,
    pub average_human_seconds_per_active_day: f64,
    pub average_active_seconds_per_active_day: f64,
    pub average_active_seconds_per_calendar_day: f64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub providers: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Inputs {
    pub git_root: String,
    pub git_scan_roots: Vec<String>,
    pub history_sources: BTreeMap<String, Vec<String>>,
    pub included_providers: Vec<String>,
    pub excluded_providers: Vec<String>,
    pub author: String,
    /// The `--author` patterns the second, agent-identity pass ran with; empty
    /// when the run did not ask for one. Recorded because the patterns are
    /// overridable, so a report is only reproducible if it says which ones it
    /// matched.
    pub agent_authors: Vec<String>,
    /// Whether `Co-authored-by:` trailers were read at all.
    pub co_authors: bool,
    pub repo_filter: Option<String>,
    pub repo_exact_filter: Option<String>,
    pub human_idle: String,
    pub review_credit: String,
    pub cache: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub methodology: Methodology,
    pub observed: Observed,
    pub summary: Summary,
    pub group_by: Vec<String>,
    pub rows: Vec<ReportRow>,
    pub diagnostics: Diagnostics,
    pub inputs: Inputs,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warned(count: usize, prefix: &str) -> Diagnostics {
        let mut diagnostics = Diagnostics::default();
        for index in 0..count {
            diagnostics.warn(format!("{prefix} {index}"));
        }
        diagnostics
    }

    #[test]
    fn warnings_are_counted_past_the_point_where_they_stop_being_stored() {
        let diagnostics = warned(MAX_STORED_MESSAGES + 30, "left");
        assert_eq!(MAX_STORED_MESSAGES, diagnostics.messages.len());
        assert_eq!(MAX_STORED_MESSAGES as u64 + 30, diagnostics.warning_count);
    }

    #[test]
    fn merging_counts_every_warning_exactly_once() {
        // Both sides are over the cap, which is where replaying `other`'s
        // stored messages through `warn` could double count them.
        let mut left = warned(MAX_STORED_MESSAGES + 30, "left");
        let right = warned(MAX_STORED_MESSAGES + 12, "right");
        left.merge(&right);
        assert_eq!(
            2 * MAX_STORED_MESSAGES as u64 + 42,
            left.warning_count,
            "a merged total must be the warnings raised, not the messages kept"
        );
        assert_eq!(MAX_STORED_MESSAGES, left.messages.len());
    }

    fn dated(sha: &str, authorship: Authorship) -> GitCommit {
        GitCommit {
            sha: sha.to_string(),
            timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
            repo: "repo".into(),
            cwd: "/repo".into(),
            root: "root".into(),
            additions: 10,
            deletions: 1,
            files: vec!["src/lib.rs".into()],
            ignored_additions: 0,
            ignored_deletions: 0,
            categories: CategoryTally::default(),
            authorship,
        }
    }

    /// The one lever that keeps the estimate honest. A commit a coding agent
    /// wrote is real output and zero evidence that anybody was present, and
    /// there is deliberately no second way to ask.
    #[test]
    fn an_agent_authored_commit_is_never_evidence_a_human_was_there() {
        assert!(dated("a", Authorship::default()).human_signal().is_some());
        assert!(dated("b", Authorship::agent()).human_signal().is_none());

        // Assistance never changes the answer: the developer still wrote it.
        let mut assisted = Authorship::default();
        assisted.note_co_author("Copilot <223556219+Copilot@users.noreply.github.com>");
        let commit = dated("c", assisted);
        assert!(commit.human_signal().is_some());
        assert!(commit.authorship.is_agent_assisted());
        assert!(!commit.authorship.is_agent_authored());
    }

    /// GitHub has issued more than one numeric id for the same Copilot
    /// identity, so anything matching on the number rots quietly. Both ids in
    /// this machine's history are pinned here.
    #[test]
    fn copilot_is_recognised_by_its_address_and_not_by_its_number() {
        for value in [
            "Copilot <198982749+Copilot@users.noreply.github.com>",
            "Copilot <223556219+Copilot@users.noreply.github.com>",
            "copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>",
            "Copilot <copilot@github.com>",
            "Claude Opus 5 (1M context) <noreply@anthropic.com>",
        ] {
            let mut authorship = Authorship::default();
            authorship.note_co_author(value);
            assert!(authorship.is_agent_assisted(), "{value} went unrecognised");
            assert!(!authorship.is_autofix_assisted(), "{value} is not Autofix");
        }

        // "Copilot Autofix powered by AI" contains the word every rule above
        // looks for, so testing for it second would misfile every security fix
        // as assisted development.
        let mut autofix = Authorship::default();
        autofix.note_co_author(
            "Copilot Autofix powered by AI <223894421+github-code-quality[bot]@users.noreply.github.com>",
        );
        assert!(autofix.is_autofix_assisted());
        assert!(!autofix.is_agent_assisted());

        // Automation that is not an AI agent stays out of both counts.
        let mut human = Authorship::default();
        human.note_co_author("dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>");
        human.note_co_author("Colleague <colleague@example.com>");
        assert_eq!(Authorship::default(), human);
    }

    /// The point of the second channel: a note is recorded, but it is never a
    /// warning, so a run whose only finding is one it resolved itself has
    /// nothing to warn about.
    #[test]
    fn a_note_is_recorded_without_ever_becoming_a_warning() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.note("repository metadata disagreed; the directory decided");
        assert_eq!(1, diagnostics.note_count);
        assert_eq!(1, diagnostics.notes.len());
        assert_eq!(0, diagnostics.warning_count);
        assert!(diagnostics.messages.is_empty());

        // And the reverse, so neither channel can drift into the other.
        diagnostics.warn("history not found");
        assert_eq!(1, diagnostics.note_count);
        assert_eq!(1, diagnostics.warning_count);
    }

    #[test]
    fn notes_are_bounded_and_merged_on_the_same_terms_as_warnings() {
        let mut left = Diagnostics::default();
        let mut right = Diagnostics::default();
        for (target, prefix) in [(&mut left, "left"), (&mut right, "right")] {
            for index in 0..MAX_STORED_MESSAGES + 7 {
                target.note(format!("{prefix} {index}"));
            }
            target.repository_conflicts = 3;
        }
        assert_eq!(MAX_STORED_MESSAGES, left.notes.len());
        left.merge(&right);
        assert_eq!(
            2 * MAX_STORED_MESSAGES as u64 + 14,
            left.note_count,
            "a merged total must be the notes raised, not the notes kept"
        );
        assert_eq!(MAX_STORED_MESSAGES, left.notes.len());
        assert_eq!(6, left.repository_conflicts);
        // Merging notes must not manufacture a warning out of nothing.
        assert_eq!(0, left.warning_count);
    }

    #[test]
    fn merging_a_countless_diagnostics_falls_back_to_its_stored_messages() {
        // Messages that never went through `warn`, which is what deserialising
        // a `Diagnostics` without the field gives: they are all that is known
        // about it, and they must not be lost from the total.
        let older = Diagnostics {
            messages: vec!["stale".to_string()],
            notes: vec!["stale note".to_string()],
            ..Diagnostics::default()
        };
        let mut current = warned(2, "current");
        current.note("current note");
        current.merge(&older);
        assert_eq!(3, current.warning_count);
        assert_eq!(3, current.messages.len());
        assert_eq!(2, current.note_count);
        assert_eq!(2, current.notes.len());
    }
}
