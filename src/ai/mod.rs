//! Readers for each supported assistant's local history.
//!
//! One module per provider keeps a format's quirks, its tests and the reasons behind
//! them together, so a change to (say) the Pi transcript layout touches one file. What
//! every provider shares lives here: the `ParsedFile` a parser returns, the bounded
//! line reader, file discovery, and `load_files`, the cache-aware driver that turns a
//! provider's parser into sessions.

mod claude;
mod codex;
mod copilot_cli;
mod copilot_vscode;
mod events;
mod gemini;
mod opencode;
mod pi;
pub(crate) mod titles;

pub use claude::read_claude_sessions_indexed;
pub use codex::read_codex_sessions_indexed;
pub use copilot_cli::read_copilot_sessions_indexed;
pub use copilot_vscode::read_copilot_vscode_sessions_indexed;
pub use events::read_event_sessions_indexed;
pub use gemini::read_gemini_sessions_indexed;
pub use opencode::read_opencode_sessions_indexed;
pub use pi::read_pi_sessions_indexed;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rusqlite::Connection;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use walkdir::WalkDir;

use crate::cache::{CacheLookup, FileStamp, TranscriptCache, file_stamp};
use crate::model::{BranchMark, Diagnostics, PrLink, RawSession, Session};
use crate::paths::PathResolver;

pub const MAX_JSONL_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default, Deserialize, serde::Serialize)]
pub struct ParsedFile {
    pub sessions: Vec<RawSession>,
    pub diagnostics: Diagnostics,
    /// How many records the parser took in as well-formed input, whether or not any of
    /// them turned out to describe activity. A parser for a format the user does not
    /// control leaves this at zero only when the file was empty, unreadable or declined
    /// for another reported reason.
    ///
    /// It is what separates an empty file from one whose records the parser no longer
    /// understands: both yield no timestamps, but only the second has records.
    #[serde(default)]
    pub records_read: u64,
    /// Set by `load_files` once per parse: records were read but not a single timestamp
    /// came out of them. Stored rather than recomputed because a range-pruned cache hit
    /// has had its timestamps cleared, and would otherwise look like drift.
    #[serde(default)]
    pub unrecognized: bool,
}

fn deserialize_maybe_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct NumberVisitor;
    impl<'de> Visitor<'de> for NumberVisitor {
        type Value = Option<f64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a number, numeric string, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
            Ok(Some(value))
        }
        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }
        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(value.parse().ok())
        }
    }
    deserializer.deserialize_any(NumberVisitor)
}

fn discover_files(root: &Path, predicate: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut paths: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && predicate(entry.path()))
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();
    paths
}

fn sqlite_table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )
}

/// Reads a cell through its stored value instead of a fixed Rust type. SQLite columns
/// are typed per value, and OpenCode declares `time_created` NUMERIC, which it is free
/// to store as REAL, so a strict `i64` read fails on rows a previous version wrote.
fn sqlite_number(row: &rusqlite::Row<'_>, index: usize) -> Option<f64> {
    match row.get_ref(index).ok()? {
        rusqlite::types::ValueRef::Integer(value) => Some(value as f64),
        rusqlite::types::ValueRef::Real(value) => Some(value),
        rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok()?.parse().ok(),
        _ => None,
    }
}

fn sqlite_text(row: &rusqlite::Row<'_>, index: usize) -> Option<String> {
    match row.get_ref(index).ok()? {
        rusqlite::types::ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        rusqlite::types::ValueRef::Integer(value) => Some(value.to_string()),
        rusqlite::types::ValueRef::Real(value) => Some(value.to_string()),
        _ => None,
    }
}

fn sqlite_columns(connection: &Connection, table: &str) -> rusqlite::Result<BTreeSet<String>> {
    let safe_table = table.replace('"', "\"\"");
    let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{safe_table}\")"))?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .filter_map(Result::ok)
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn load_files<F, C>(
    paths: Vec<PathBuf>,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    mut cache: Option<&mut TranscriptCache>,
    provider: &str,
    context_for: C,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    parser: F,
) -> Vec<Session>
where
    F: Fn(&Path) -> ParsedFile + Sync,
    C: Fn(&Path) -> String,
{
    let contexts: Vec<_> = paths.iter().map(|path| context_for(path)).collect();
    let mut slots: Vec<Option<ParsedFile>> = (0..paths.len()).map(|_| None).collect();
    let mut pending_stamps: Vec<Option<FileStamp>> = vec![None; paths.len()];
    let mut misses = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let stamp = file_stamp(path);
        let lookup = cache.as_deref_mut().and_then(|cache| {
            stamp.map(|stamp| cache.lookup(path, provider, &contexts[index], stamp, since, until))
        });
        match lookup {
            Some(Ok(CacheLookup::Hit(parsed))) => {
                diagnostics.cache_hits += 1;
                slots[index] = Some(parsed);
            }
            Some(Ok(CacheLookup::Pruned(parsed))) => {
                diagnostics.cache_hits += 1;
                diagnostics.pruned_files += 1;
                slots[index] = Some(parsed);
            }
            Some(Ok(CacheLookup::Miss)) | None => {
                if cache.is_some() {
                    diagnostics.cache_misses += 1;
                }
                pending_stamps[index] = stamp;
                misses.push((index, path.clone()));
            }
            Some(Err(error)) => {
                diagnostics.cache_misses += 1;
                diagnostics.warn(format!(
                    "transcript cache entry ignored for {}: {error}",
                    path.display()
                ));
                pending_stamps[index] = stamp;
                misses.push((index, path.clone()));
            }
        }
    }
    let parsed_misses: Vec<_> = misses
        .par_iter()
        .map(|(index, path)| (*index, parser(path)))
        .collect();
    for (index, mut parsed) in parsed_misses {
        parsed.unrecognized = records_without_activity(&parsed);
        slots[index] = Some(parsed);
    }

    let mut sessions = Vec::new();
    let mut unrecognized_files = 0_usize;
    let mut recognized_files = 0_usize;
    for (index, item) in slots.into_iter().enumerate() {
        let Some(item) = item else {
            continue;
        };
        if item.unrecognized {
            unrecognized_files += 1;
        } else if item.records_read > 0 {
            recognized_files += 1;
        }
        if let (Some(cache), Some(stamp)) = (cache.as_deref_mut(), pending_stamps[index])
            && item.diagnostics.unreadable_files == 0
        {
            match cache.put(&paths[index], provider, &contexts[index], stamp, &item) {
                Ok(()) => diagnostics.cache_writes += 1,
                Err(error) => diagnostics.warn(format!(
                    "transcript cache write ignored for {}: {error}",
                    paths[index].display()
                )),
            }
        }
        diagnostics.merge(&item.diagnostics);
        for raw in item.sessions {
            if raw.approximate_cwd {
                diagnostics.approximate_cwds += 1;
            }
            sessions.push(resolver.resolve_session(raw));
        }
    }
    warn_if_format_drifted(provider, unrecognized_files, recognized_files, diagnostics);
    sessions
}

/// Whether a parse read records yet recovered no timestamp from any of them.
///
/// A timestamp of any kind counts (activity, prompt, exact interval or token event), so
/// this flags only a file from which nothing at all could be used. A file with no records
/// is not flagged: an empty or zero-length transcript is ordinary, and only records the
/// parser could not make sense of are evidence that the format moved.
fn records_without_activity(parsed: &ParsedFile) -> bool {
    parsed.records_read > 0 && file_time_range(&parsed.sessions).0.is_none()
}

/// Says so when a provider's history was read but nothing in it was understood.
///
/// Upstream tools change their transcript formats without notice. A record that still
/// parses as JSON but no longer carries the fields a parser looks for is dropped without
/// a trace, so the provider would quietly report zero and look like a quiet week. The
/// warning is raised only when *no* file of the provider yielded anything, and at least
/// one had records: a session that holds only metadata (Claude's file snapshots, a Gemini
/// header written before the first prompt) is ordinary, but a whole provider made of
/// them is not. A provider where some files still work is left alone; its unusable files
/// are then most likely just such sessions, and the threshold has to stay high enough
/// that the warning means something when it appears.
fn warn_if_format_drifted(
    provider: &str,
    unrecognized_files: usize,
    recognized_files: usize,
    diagnostics: &mut Diagnostics,
) {
    if unrecognized_files == 0 || recognized_files > 0 {
        return;
    }
    let files = if unrecognized_files == 1 {
        "1 file".to_string()
    } else {
        format!("{unrecognized_files} files")
    };
    diagnostics.warn(format!(
        "{provider}: read {files} but found no usable activity records in them; the history format may have changed"
    ));
}

fn for_json_lines<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_line_bytes: usize,
    parsed: &mut ParsedFile,
    mut consume: impl FnMut(T),
) {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            parsed.diagnostics.unreadable_files += 1;
            parsed.diagnostics.warn(format!(
                "unreadable transcript skipped: {}: {error}",
                path.display()
            ));
            return;
        }
    };
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut line_number = 0_u64;
    loop {
        match read_bounded_line(&mut reader, max_line_bytes) {
            Ok(None) => break,
            Ok(Some((line, oversized))) => {
                line_number += 1;
                if oversized {
                    parsed.diagnostics.malformed_lines += 1;
                    parsed.diagnostics.warn(format!(
                        "oversized JSONL line skipped: {}:{line_number}",
                        path.display()
                    ));
                    continue;
                }
                match serde_json::from_slice(&line) {
                    Ok(record) => {
                        parsed.records_read += 1;
                        consume(record);
                    }
                    Err(_) => {
                        parsed.diagnostics.malformed_lines += 1;
                        if parsed.diagnostics.malformed_lines <= 20 {
                            parsed.diagnostics.warn(format!(
                                "malformed JSONL skipped: {}:{line_number}",
                                path.display()
                            ));
                        }
                    }
                }
            }
            Err(error) => {
                parsed.diagnostics.unreadable_files += 1;
                parsed.diagnostics.warn(format!(
                    "unreadable transcript skipped: {}: {error}",
                    path.display()
                ));
                break;
            }
        }
    }
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    maximum: usize,
) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let (read, remaining) = {
        let mut limited = reader.by_ref().take(maximum as u64 + 1);
        let read = limited.read_until(b'\n', &mut line)?;
        (read, limited.limit())
    };
    if read == 0 {
        return Ok(None);
    }
    let complete = line.ends_with(b"\n");
    // The terminator is not part of the record, so counting it made the effective limit
    // one byte short of `maximum`.
    let oversized = line.len() - usize::from(complete) > maximum;
    let truncated = !complete && remaining == 0;
    if truncated {
        discard_to_newline(reader)?;
    }
    Ok(Some((line, oversized || truncated)))
}

fn discard_to_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(());
        }
        let length = available.len();
        reader.consume(length);
    }
}

fn unknown_model() -> String {
    "unknown".to_string()
}

fn safe_model(value: &str) -> String {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(*byte, b'.' | b'_' | b':' | b'/' | b'+' | b'<' | b'>' | b'-')
        });
    if valid || value == "<synthetic>" {
        value.to_string()
    } else {
        "unknown".to_string()
    }
}

/// Most branch marks one session keeps. A session that hops between branches more often
/// than this is almost certainly a tool scripting checkouts; the later hops are counted
/// and reported rather than stored without bound in the cache.
pub(crate) const MAX_BRANCH_MARKS: usize = 256;
pub(crate) const MAX_BRANCH_BYTES: usize = 256;
/// Most pull requests one session keeps.
pub(crate) const MAX_PR_LINKS: usize = 64;

/// A branch name as a provider recorded it, or `None` when it cannot be stored.
///
/// Branch names reach reports, the cache and (later) bundles, so anything that is not a
/// plain, bounded name is refused here: a control character could repaint a terminal and
/// a very long value is not a branch. Git itself forbids both, so a real name is never
/// refused.
pub(crate) fn safe_branch(value: &str) -> Option<&str> {
    let valid = !value.trim().is_empty()
        && value.len() <= MAX_BRANCH_BYTES
        && !value.chars().any(char::is_control);
    valid.then_some(value)
}

/// Turns a stream of "the branch was X at time T" observations into marks written only
/// where the branch changes.
#[derive(Default)]
pub(crate) struct BranchTracker {
    marks: Vec<BranchMark>,
    /// When the newest mark took effect, whether or not it is written as `from: None`.
    latest: Option<DateTime<Utc>>,
    refused: u64,
    over_limit: u64,
}

impl BranchTracker {
    pub(crate) fn observe(&mut self, at: DateTime<Utc>, branch: &str) {
        let Some(branch) = safe_branch(branch) else {
            self.refused += 1;
            return;
        };
        if self.marks.last().is_some_and(|last| last.branch == branch) {
            return;
        }
        if self.marks.len() >= MAX_BRANCH_MARKS {
            self.over_limit += 1;
            return;
        }
        // `branch_at` needs sorted marks. A log is written in order, so this only
        // matters for a clock that stepped back, which must not unsort the marks.
        let at = self.latest.map_or(at, |latest| latest.max(at));
        let from = self.marks.last().map(|_| at);
        self.latest = Some(at);
        self.marks.push(BranchMark {
            from,
            branch: branch.to_string(),
        });
    }

    /// Hands back the marks and says, once, what was left out.
    pub(crate) fn finish(self, diagnostics: &mut Diagnostics, path: &Path) -> Vec<BranchMark> {
        if self.refused > 0 {
            diagnostics.note(format!(
                "{} branch name(s) that were empty, too long or contained control characters were not stored: {}",
                self.refused,
                path.display()
            ));
        }
        if self.over_limit > 0 {
            diagnostics.note(format!(
                "{} branch change(s) beyond the {MAX_BRANCH_MARKS} kept per session were not stored: {}",
                self.over_limit,
                path.display()
            ));
        }
        self.marks
    }
}

/// The marks for a session whose provider names one branch for its whole length (Codex's
/// `session_meta`, the Copilot store): a single mark from the session start.
pub(crate) fn whole_session_branch(branch: Option<&str>) -> Vec<BranchMark> {
    branch
        .and_then(safe_branch)
        .map(|branch| {
            vec![BranchMark {
                from: None,
                branch: branch.to_string(),
            }]
        })
        .unwrap_or_default()
}

/// Collects the pull requests a session mentioned, by number and repository only.
#[derive(Default)]
pub(crate) struct PrCollector {
    links: Vec<PrLink>,
    refused: u64,
}

impl PrCollector {
    pub(crate) fn observe(&mut self, number: u64, repository: &str, at: Option<DateTime<Utc>>) {
        let repository_ok =
            repository.len() <= MAX_BRANCH_BYTES && !repository.chars().any(char::is_control);
        if number == 0 || !repository_ok {
            self.refused += 1;
            return;
        }
        if let Some(existing) = self
            .links
            .iter_mut()
            .find(|link| link.number == number && link.repository == repository)
        {
            existing.at = existing.at.into_iter().chain(at).min();
            return;
        }
        if self.links.len() >= MAX_PR_LINKS {
            self.refused += 1;
            return;
        }
        self.links.push(PrLink {
            number,
            repository: repository.to_string(),
            at,
        });
    }

    pub(crate) fn finish(self, diagnostics: &mut Diagnostics, path: &Path) -> Vec<PrLink> {
        if self.refused > 0 {
            diagnostics.note(format!(
                "{} pull-request reference(s) that were malformed or beyond the {MAX_PR_LINKS} kept per session were not stored: {}",
                self.refused,
                path.display()
            ));
        }
        self.links
    }
}

/// A whole, positive number out of a lenient numeric field.
fn whole_number(value: Option<f64>) -> Option<u64> {
    let value = value?;
    (value.is_finite() && value >= 1.0 && value.fract() == 0.0 && value <= 9_007_199_254_740_992.0)
        .then_some(value as u64)
}

fn canonical_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn file_time_range(sessions: &[RawSession]) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut minimum = None;
    let mut maximum = None;
    for session in sessions {
        for timestamp in session
            .points
            .iter()
            .map(|point| point.timestamp)
            .chain(session.human_points.iter().map(|point| point.timestamp))
            .chain(
                session
                    .exact_intervals
                    .iter()
                    .flat_map(|item| [item.start, item.end]),
            )
            // Copilot reports a whole session's usage from `session.shutdown`, which can
            // land more than an hour after the last activity point. Leaving those out of
            // the cached range lets a range-pruned hit answer with zero tokens for a day
            // the cold read counts.
            .chain(session.token_events.iter().map(|event| event.timestamp))
        {
            minimum = Some(minimum.map_or(timestamp, |value: DateTime<Utc>| value.min(timestamp)));
            maximum = Some(maximum.map_or(timestamp, |value: DateTime<Utc>| value.max(timestamp)));
        }
    }
    (minimum, maximum)
}

/// The directory of synthetic sessions each provider's regression test parses. They are
/// written to look like what the tools emit, with invented paths and text, so a change to
/// a parser that misreads a real layout fails here instead of only on a developer's own
/// history.
#[cfg(test)]
fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}

#[cfg(test)]
fn utc(value: &str) -> DateTime<Utc> {
    crate::timeutil::parse_timestamp(value).expect("fixture timestamps are valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ActivityPoint, TokenEvent, TokenUsage};
    use crate::timeutil::parse_timestamp;
    use std::fs;
    use tempfile::tempdir;

    /// A record that is valid JSON and valid for every provider's permissive record
    /// struct, yet names nothing any of them looks for: what a renamed-field upstream
    /// release looks like to the parsers.
    const UNRECOGNIZED_RECORD: &str = r#"{"schema":"v9","at":"2026-01-01T00:00:00Z"}"#;

    fn resolver(home: &Path) -> PathResolver {
        PathResolver::with_home(Vec::new(), home.to_path_buf())
    }

    fn drift_warnings(diagnostics: &Diagnostics) -> Vec<&String> {
        diagnostics
            .messages
            .iter()
            .filter(|message| message.contains("format may have changed"))
            .collect()
    }

    #[test]
    fn model_validation_rejects_control_text() {
        assert_eq!("claude-test", safe_model("claude-test"));
        assert_eq!("unknown", safe_model("prompt text\n"));
    }

    #[test]
    fn file_time_range_covers_token_events_after_the_last_activity_point() {
        let session = RawSession {
            provider: "copilot".to_string(),
            session_id: "session".to_string(),
            source_file: PathBuf::from("events.jsonl"),
            cwd: "/tmp/repo".to_string(),
            repository_hint_cwd: None,
            points: vec![ActivityPoint {
                timestamp: parse_timestamp("2026-01-01T23:00:00Z").unwrap(),
                model: "gpt-test".to_string(),
            }],
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: vec![TokenEvent {
                timestamp: parse_timestamp("2026-01-02T00:30:00Z").unwrap(),
                model: "gpt-test".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
            }],
            is_subagent: false,
            approximate_cwd: false,
            version: None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
        };
        let (minimum, maximum) = file_time_range(std::slice::from_ref(&session));
        assert_eq!(parse_timestamp("2026-01-01T23:00:00Z"), minimum);
        assert_eq!(parse_timestamp("2026-01-02T00:30:00Z"), maximum);
    }

    #[test]
    fn jsonl_parsers_count_records_they_read_but_cannot_use() {
        let root = tempdir().unwrap();
        let drifted = root.path().join("drifted.jsonl");
        fs::write(
            &drifted,
            format!("{UNRECOGNIZED_RECORD}\n{UNRECOGNIZED_RECORD}\n"),
        )
        .unwrap();
        let empty = root.path().join("empty.jsonl");
        fs::write(&empty, "").unwrap();
        let limit = MAX_JSONL_LINE_BYTES;
        let store = copilot_cli::CopilotSessionStore::default();
        let metadata = codex::CodexMetadataIndex::default();

        type Parser<'a> = Box<dyn Fn(&Path) -> ParsedFile + 'a>;
        let parsers: [(&str, Parser<'_>); 5] = [
            (
                "claude",
                Box::new(|path| claude::parse_claude_file(path, root.path(), limit)),
            ),
            (
                "codex",
                Box::new(|path| codex::parse_codex_file(path, &metadata, limit)),
            ),
            (
                "copilot",
                Box::new(|path| copilot_cli::parse_copilot_file(path, &store, limit)),
            ),
            (
                "gemini",
                Box::new(|path| gemini::parse_gemini_file(path, root.path(), limit)),
            ),
            (
                "pi",
                Box::new(|path| pi::parse_pi_file(path, root.path(), limit)),
            ),
        ];
        for (name, parse) in &parsers {
            let unusable = parse(&drifted);
            assert_eq!(2, unusable.records_read, "{name}");
            assert!(records_without_activity(&unusable), "{name}");
            // A zero-length transcript is ordinary, not a format change.
            let nothing = parse(&empty);
            assert_eq!(0, nothing.records_read, "{name}");
            assert!(!records_without_activity(&nothing), "{name}");
        }
    }

    #[test]
    fn a_provider_whose_every_record_is_unrecognized_warns_once() {
        let root = tempdir().unwrap();
        let history = root.path().join("history");
        let project = history.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("one.jsonl"),
            format!("{UNRECOGNIZED_RECORD}\n"),
        )
        .unwrap();
        fs::write(
            project.join("two.jsonl"),
            format!("{UNRECOGNIZED_RECORD}\n{UNRECOGNIZED_RECORD}\n"),
        )
        .unwrap();
        // An empty file beside them must not count towards the number of files.
        fs::write(project.join("empty.jsonl"), "").unwrap();
        let mut diagnostics = Diagnostics::default();

        let sessions = read_claude_sessions_indexed(
            &history,
            &mut resolver(root.path()),
            &mut diagnostics,
            None,
            None,
            None,
        );

        assert!(sessions.is_empty());
        let warnings = drift_warnings(&diagnostics);
        assert_eq!(1, warnings.len(), "{:?}", diagnostics.messages);
        assert!(
            warnings[0].starts_with("claude: read 2 files"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn empty_or_metadata_only_histories_next_to_a_working_one_do_not_warn() {
        let root = tempdir().unwrap();
        let history = root.path().join("history");
        let project = history.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("empty.jsonl"), "").unwrap();
        let mut diagnostics = Diagnostics::default();
        read_claude_sessions_indexed(
            &history,
            &mut resolver(root.path()),
            &mut diagnostics,
            None,
            None,
            None,
        );
        assert!(drift_warnings(&diagnostics).is_empty());

        // A session that never got past its metadata is ordinary while another file in
        // the same history still parses, so the warning is held back.
        fs::write(
            project.join("snapshot-only.jsonl"),
            format!("{UNRECOGNIZED_RECORD}\n"),
        )
        .unwrap();
        fs::write(
            project.join("real.jsonl"),
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{},\"message\":{{\"content\":\"hello\"}}}}\n",
                serde_json::to_string(&project).unwrap()
            ),
        )
        .unwrap();
        let mut diagnostics = Diagnostics::default();
        let sessions = read_claude_sessions_indexed(
            &history,
            &mut resolver(root.path()),
            &mut diagnostics,
            None,
            None,
            None,
        );
        assert_eq!(1, sessions.len());
        assert!(drift_warnings(&diagnostics).is_empty());
    }

    #[test]
    fn the_verdict_survives_the_cache_including_a_range_pruned_hit() {
        let root = tempdir().unwrap();
        let history = root.path().join("history");
        let project = history.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("real.jsonl"),
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{},\"message\":{{\"content\":\"hello\"}}}}\n",
                serde_json::to_string(&project).unwrap()
            ),
        )
        .unwrap();
        let mut cache = TranscriptCache::open(&root.path().join("cache.sqlite3"), false).unwrap();
        let mut read = |since: Option<DateTime<Utc>>| {
            let mut diagnostics = Diagnostics::default();
            read_claude_sessions_indexed(
                &history,
                &mut resolver(root.path()),
                &mut diagnostics,
                Some(&mut cache),
                since,
                None,
            );
            diagnostics
        };
        assert!(drift_warnings(&read(None)).is_empty());
        // Everything in the file is before the range, so the hit is pruned and its
        // timestamps are gone; it must not be mistaken for a file nothing came out of.
        let pruned = read(parse_timestamp("2026-06-01T00:00:00Z"));
        assert_eq!(1, pruned.pruned_files);
        assert!(drift_warnings(&pruned).is_empty());

        // And a drifted file warns on the cold read and again when served from the cache.
        fs::remove_file(project.join("real.jsonl")).unwrap();
        fs::write(
            project.join("drifted.jsonl"),
            format!("{UNRECOGNIZED_RECORD}\n"),
        )
        .unwrap();
        let cold = read(None);
        assert_eq!(1, drift_warnings(&cold).len());
        let warm = read(None);
        assert_eq!(1, warm.cache_hits);
        assert_eq!(1, drift_warnings(&warm).len());
    }

    #[test]
    fn opencode_messages_without_a_usable_timestamp_are_drift() {
        let root = tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
                 CREATE TABLE session_message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    type TEXT NOT NULL,
                    time_created TEXT,
                    data TEXT
                 );
                 INSERT INTO session VALUES ('s', '/tmp/project');
                 INSERT INTO session_message VALUES ('m', 's', 'user', NULL, '{}');",
            )
            .unwrap();
        drop(connection);
        let mut diagnostics = Diagnostics::default();

        read_opencode_sessions_indexed(
            &path,
            &mut resolver(root.path()),
            &mut diagnostics,
            None,
            None,
            None,
        );

        assert_eq!(
            1,
            drift_warnings(&diagnostics).len(),
            "{:?}",
            diagnostics.messages
        );
        assert!(drift_warnings(&diagnostics)[0].starts_with("opencode: read 1 file "));
    }

    #[test]
    fn an_opencode_session_without_messages_is_not_drift() {
        let root = tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
                 CREATE TABLE session_message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    type TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT
                 );
                 INSERT INTO session VALUES ('s', '/tmp/project');",
            )
            .unwrap();
        drop(connection);
        let mut diagnostics = Diagnostics::default();

        read_opencode_sessions_indexed(
            &path,
            &mut resolver(root.path()),
            &mut diagnostics,
            None,
            None,
            None,
        );

        assert!(drift_warnings(&diagnostics).is_empty());
    }

    #[test]
    fn copilot_chat_requests_without_timestamps_are_drift_but_no_requests_is_not() {
        let root = tempdir().unwrap();
        let drifted = root.path().join("drifted.json");
        fs::write(
            &drifted,
            r#"{"version":3,"sessionId":"a","requests":[{"sentAt":1767225600000}]}"#,
        )
        .unwrap();
        let parsed = copilot_vscode::parse_copilot_vscode_file(
            &drifted,
            copilot_vscode::MAX_VSCODE_CHAT_JSON_BYTES,
        );
        assert_eq!(1, parsed.records_read);
        assert!(records_without_activity(&parsed));

        let unused = root.path().join("unused.json");
        fs::write(&unused, r#"{"version":3,"sessionId":"b","requests":[]}"#).unwrap();
        let parsed = copilot_vscode::parse_copilot_vscode_file(
            &unused,
            copilot_vscode::MAX_VSCODE_CHAT_JSON_BYTES,
        );
        assert_eq!(0, parsed.records_read);
        assert!(!records_without_activity(&parsed));
    }

    #[test]
    fn branch_names_must_be_plain_and_short() {
        assert_eq!(Some("feat/a-1"), safe_branch("feat/a-1"));
        assert_eq!(Some("users/ada/x y"), safe_branch("users/ada/x y"));
        for refused in ["", "  ", "a\nb", "a\u{1b}[0m", "\u{7f}"] {
            assert_eq!(None, safe_branch(refused), "{refused:?}");
        }
        assert!(safe_branch(&"b".repeat(256)).is_some());
        assert_eq!(None, safe_branch(&"b".repeat(257)));
        // Bytes, not characters: 129 two-byte characters are 258 bytes.
        assert_eq!(None, safe_branch(&"é".repeat(129)));
    }

    #[test]
    fn a_branch_tracker_never_unsorts_its_marks() {
        let mut tracker = BranchTracker::default();
        tracker.observe(utc("2026-01-01T00:10:00Z"), "a");
        // A clock that stepped back: the mark is placed no earlier than the last one.
        tracker.observe(utc("2026-01-01T00:05:00Z"), "b");
        tracker.observe(utc("2026-01-01T00:06:00Z"), "b");
        let marks = tracker.finish(&mut Diagnostics::default(), Path::new("x"));
        assert_eq!(2, marks.len());
        assert_eq!(None, marks[0].from);
        assert_eq!(Some(utc("2026-01-01T00:10:00Z")), marks[1].from);
    }

    #[test]
    fn pull_request_collection_dedupes_and_is_bounded() {
        let mut collector = PrCollector::default();
        collector.observe(1, "a/b", Some(utc("2026-01-01T00:09:00Z")));
        collector.observe(1, "a/b", Some(utc("2026-01-01T00:01:00Z")));
        collector.observe(1, "c/d", None);
        for number in 2..200 {
            collector.observe(number, "a/b", None);
        }
        let mut diagnostics = Diagnostics::default();
        let links = collector.finish(&mut diagnostics, Path::new("x"));
        assert_eq!(MAX_PR_LINKS, links.len());
        assert_eq!(Some(utc("2026-01-01T00:01:00Z")), links[0].at);
        assert_eq!(1, diagnostics.note_count);
    }
}
