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
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Diagnostics {
    pub malformed_lines: u64,
    pub unreadable_files: u64,
    pub approximate_cwds: u64,
    pub skipped_sessions: u64,
    pub git_errors: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_writes: u64,
    pub pruned_files: u64,
    pub messages: Vec<String>,
}

impl Diagnostics {
    pub fn warn(&mut self, message: impl Into<String>) {
        if self.messages.len() < 100 {
            self.messages.push(message.into());
        }
    }

    pub fn merge(&mut self, other: &Diagnostics) {
        self.malformed_lines += other.malformed_lines;
        self.unreadable_files += other.unreadable_files;
        self.approximate_cwds += other.approximate_cwds;
        self.skipped_sessions += other.skipped_sessions;
        self.git_errors += other.git_errors;
        self.cache_hits += other.cache_hits;
        self.cache_misses += other.cache_misses;
        self.cache_writes += other.cache_writes;
        self.pruned_files += other.pruned_files;
        for message in &other.messages {
            self.warn(message.clone());
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
    pub composition: &'static str,
    pub change_shapes: &'static str,
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
