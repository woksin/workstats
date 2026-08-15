use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawSession {
    pub provider: String,
    pub session_id: String,
    pub source_file: PathBuf,
    pub cwd: String,
    pub points: Vec<ActivityPoint>,
    pub exact_intervals: Vec<ExactInterval>,
    pub human_points: Vec<ActivityPoint>,
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
    pub isolated_signal_credit_seconds: f64,
    pub human_estimate_caveat: &'static str,
    pub ai_time: &'static str,
    pub deduplication: &'static str,
    pub gap_cap_seconds: f64,
    pub scope: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Observed {
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub human_estimated_seconds: f64,
    pub human_active_days: usize,
    pub average_human_seconds_per_active_day: f64,
    pub work_block_count: usize,
    pub human_signal_count: usize,
    pub prompt_signal_count: usize,
    pub commit_signal_count: usize,
    pub deduplicated_active_seconds: f64,
    pub attributed_active_seconds: f64,
    pub agent_wall_seconds: f64,
    pub parallel_agent_seconds: f64,
    pub session_count: usize,
    pub foreground_session_count: usize,
    pub subagent_session_count: usize,
    pub commit_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub ignored_additions: u64,
    pub ignored_deletions: u64,
    pub active_days: usize,
    pub provider_seconds: BTreeMap<String, f64>,
    pub model_seconds: BTreeMap<String, f64>,
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
    pub claude_root: String,
    pub codex_root: String,
    pub author: String,
    pub repo_filter: Option<String>,
    pub repo_exact_filter: Option<String>,
    pub human_idle: String,
    pub isolated_credit: String,
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
