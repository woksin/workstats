//! The generic workstats events format: JSONL any tool can write to be counted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};

use super::{
    MAX_JSONL_LINE_BYTES, ParsedFile, discover_files, for_json_lines, load_files, safe_model,
    unknown_model,
};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, ExactInterval, RawSession, Session};
use crate::paths::PathResolver;
use crate::timeutil::parse_timestamp;

#[derive(Deserialize)]
struct WorkstatsEvent {
    timestamp: String,
    provider: String,
    session_id: String,
    cwd: String,
    #[serde(default = "unknown_model")]
    model: String,
    #[serde(default = "activity_event")]
    event: String,
    #[serde(default = "foreground_role")]
    role: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    // One field each rather than six aliases of a single field. Aliases make
    // serde raise "duplicate field" as soon as a record carries two of them —
    // and `response` together with `output` is the ordinary shape of an
    // API-wrapper log — which reported a privacy rejection as a malformed line
    // and sent the author hunting a JSON syntax error that did not exist.
    // Every one deserializes through `IgnoredAny`, so the value is recognised
    // without ever being read into memory.
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    content: bool,
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    prompt: bool,
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    response: bool,
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    input: bool,
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    output: bool,
    #[serde(default, deserialize_with = "deserialize_sensitive_payload")]
    api_key: bool,
}

impl WorkstatsEvent {
    fn carries_sensitive_payload(&self) -> bool {
        self.content || self.prompt || self.response || self.input || self.output || self.api_key
    }
}

fn deserialize_sensitive_payload<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

fn activity_event() -> String {
    "activity".to_string()
}

fn foreground_role() -> String {
    "foreground".to_string()
}

pub fn discover_event_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    discover_files(path, |candidate| {
        candidate
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
    })
}

pub fn read_event_sessions_indexed(
    path: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    let files = discover_event_files(path);
    if files.is_empty() {
        diagnostics.warn(format!("event history not found: {}", path.display()));
        return Vec::new();
    }
    load_files(
        files,
        resolver,
        diagnostics,
        cache,
        "events",
        |_| "workstats-events-v1".to_string(),
        since,
        until,
        |file| parse_event_file(file, MAX_JSONL_LINE_BYTES),
    )
}

pub fn parse_event_file(path: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    type EventKey = (String, String, String, bool);
    let mut sessions: BTreeMap<EventKey, RawSession> = BTreeMap::new();
    let mut sensitive_records = 0_u64;
    for_json_lines(
        path,
        max_line_bytes,
        &mut result,
        |record: WorkstatsEvent| {
            if record.carries_sensitive_payload() {
                sensitive_records += 1;
                return;
            }
            let Some(timestamp) = parse_timestamp(&record.timestamp) else {
                return;
            };
            let provider = safe_provider(&record.provider);
            if provider == "unknown"
                || record.session_id.trim().is_empty()
                || record.cwd.trim().is_empty()
            {
                return;
            }
            let is_subagent = record.role.eq_ignore_ascii_case("subagent");
            let model = safe_model(&record.model);
            let key = (
                provider.clone(),
                record.session_id.clone(),
                record.cwd.clone(),
                is_subagent,
            );
            let session = sessions.entry(key).or_insert_with(|| RawSession {
                provider,
                session_id: record.session_id,
                source_file: path.to_path_buf(),
                cwd: record.cwd,
                repository_hint_cwd: None,
                points: Vec::new(),
                exact_intervals: Vec::new(),
                human_points: Vec::new(),
                token_events: Vec::new(),
                is_subagent,
                approximate_cwd: false,
                version: Some("workstats-events-v1".to_string()),
                branches: Vec::new(),
                pull_requests: Vec::new(),
            });
            let point = ActivityPoint {
                timestamp,
                model: model.clone(),
            };
            session.points.push(point.clone());
            if record.event.eq_ignore_ascii_case("prompt") && !is_subagent {
                session.human_points.push(point);
            }
            if let (Some(start), Some(end)) = (
                record.started_at.as_deref().and_then(parse_timestamp),
                record.completed_at.as_deref().and_then(parse_timestamp),
            ) && end > start
            {
                session
                    .exact_intervals
                    .push(ExactInterval { start, end, model });
            }
        },
    );
    // The events format is a published schema written by the user's own tooling, and a
    // record that does not fit it is already reported as a malformed line or a content
    // rejection. Leaving `records_read` at zero keeps those from being second-guessed as
    // an upstream format change.
    result.records_read = 0;
    if sensitive_records > 0 {
        result.diagnostics.content_rejections += sensitive_records;
        result.diagnostics.warn(format!(
            "{sensitive_records} content-bearing event record(s) skipped: {}",
            path.display()
        ));
    }
    // The aggregator keys a session by (provider, session_id) alone, so one id reused
    // across working directories or roles would collapse to whichever record came last
    // — reporting foreground work as a subagent. Codex and Copilot suffix the cwd for
    // the same reason.
    //
    // The suffix is unconditional rather than applied only when one file holds
    // several variants: a rotated or split log puts the same stream in two
    // files, and suffixing per file would give the same session two different
    // ids and count it twice. Deriving the id from the record alone keeps it
    // stable however the records are distributed.
    result.sessions = sessions
        .into_iter()
        .map(|((_, _, cwd, is_subagent), mut session)| {
            session.session_id = if is_subagent {
                format!("{}:{cwd}:subagent", session.session_id)
            } else {
                format!("{}:{cwd}", session.session_id)
            };
            session
        })
        .collect();
    if result.sessions.is_empty() {
        result.diagnostics.skipped_sessions += 1;
    }
    result
}

fn safe_provider(value: &str) -> String {
    let normalized = crate::sources::normalize_provider(value);
    let valid = !normalized.is_empty()
        && normalized != "all"
        && normalized.len() <= 64
        && normalized.as_bytes()[0].is_ascii_alphanumeric()
        && normalized
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'/' | b'-'));
    if valid {
        normalized
    } else {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{fixture, utc};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn open_events_accept_arbitrary_providers_and_exact_intervals() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "provider": "cursor",
                "session_id": "task-one",
                "cwd": root.path(),
                "model": "model-a",
                "event": "prompt",
                "role": "foreground"
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:01:00Z",
                "provider": "cursor",
                "session_id": "task-one",
                "cwd": root.path(),
                "model": "model-a",
                "event": "activity",
                "started_at": "2026-01-01T00:00:30Z",
                "completed_at": "2026-01-01T00:01:00Z"
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:02:00Z",
                "provider": "unsafe-export",
                "session_id": "must-be-skipped",
                "cwd": root.path(),
                "content": "a prompt body does not belong in Workstats Events"
            }),
        ];
        fs::write(
            &path,
            records
                .into_iter()
                .map(|record| record.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let parsed = parse_event_file(&path, MAX_JSONL_LINE_BYTES);
        assert_eq!("cursor", parsed.sessions[0].provider);
        assert_eq!(1, parsed.sessions.len());
        // The cwd is always appended, so the same stream keeps one id however
        // the records are split across files.
        assert!(
            parsed.sessions[0].session_id.starts_with("task-one:"),
            "unexpected id {}",
            parsed.sessions[0].session_id
        );
        assert_eq!(1, parsed.sessions[0].human_points.len());
        assert_eq!(1, parsed.sessions[0].exact_intervals.len());
        assert_eq!(1, parsed.diagnostics.content_rejections);
    }

    /// A wrapper logging an exchange naturally writes several of these at once.
    /// While they were aliases of one field, serde raised "duplicate field"
    /// before the privacy guard ran, and the record was reported as malformed —
    /// sending the author after a JSON syntax error that did not exist.
    #[test]
    fn a_record_carrying_several_sensitive_fields_is_a_privacy_rejection() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let project = root.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let lines = [
            serde_json::json!({
                "timestamp": "2026-01-01T10:00:00Z",
                "provider": "wrapper",
                "session_id": "s1",
                "cwd": project,
                "event": "prompt",
                "response": "SECRET",
                "output": "ALSO SECRET"
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T10:05:00Z",
                "provider": "wrapper",
                "session_id": "s2",
                "cwd": project,
                "event": "prompt"
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let parsed = parse_event_file(&path, MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.diagnostics.content_rejections);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        // The clean record still lands, and the rejected one is gone entirely.
        assert_eq!(1, parsed.sessions.len());
        assert!(parsed.sessions[0].session_id.starts_with("s2:"));
    }

    #[test]
    fn event_sessions_keep_one_id_apart_across_directories_and_roles() {
        let root = tempdir().unwrap();
        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "provider": "cursor",
                "session_id": "task-one",
                "cwd": root.path(),
                "model": "model-a",
                "event": "prompt",
                "role": "foreground"
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:01:00Z",
                "provider": "cursor",
                "session_id": "task-one",
                "cwd": other,
                "model": "model-a",
                "role": "subagent"
            }),
        ];
        fs::write(
            &path,
            records
                .into_iter()
                .map(|record| record.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let parsed = parse_event_file(&path, MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.sessions.len());
        let foreground = parsed
            .sessions
            .iter()
            .find(|session| !session.is_subagent)
            .unwrap();
        let subagent = parsed
            .sessions
            .iter()
            .find(|session| session.is_subagent)
            .unwrap();
        assert_ne!(foreground.session_id, subagent.session_id);
        assert!(foreground.session_id.starts_with("task-one:"));
        assert!(subagent.session_id.ends_with(":subagent"));
    }

    #[test]
    fn the_events_fixture_parses_to_its_documented_timestamps() {
        let root = fixture("events");
        let files = discover_event_files(&root);
        assert_eq!(vec![root.join("events.jsonl")], files);

        let parsed = parse_event_file(&files[0], MAX_JSONL_LINE_BYTES);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        assert_eq!(0, parsed.diagnostics.content_rejections);
        assert_eq!(2, parsed.sessions.len());
        let foreground = parsed
            .sessions
            .iter()
            .find(|session| !session.is_subagent)
            .unwrap();
        assert_eq!("fixture-tool", foreground.provider);
        assert_eq!("task-1:/home/example/project", foreground.session_id);
        assert_eq!(3, foreground.points.len());
        assert_eq!(
            vec![utc("2026-01-01T14:00:00Z"), utc("2026-01-01T14:05:00Z")],
            foreground
                .human_points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(1, foreground.exact_intervals.len());
        assert_eq!(
            utc("2026-01-01T14:00:05Z"),
            foreground.exact_intervals[0].start
        );
        assert_eq!(
            utc("2026-01-01T14:00:30Z"),
            foreground.exact_intervals[0].end
        );
        let subagent = parsed
            .sessions
            .iter()
            .find(|session| session.is_subagent)
            .unwrap();
        assert_eq!("task-1:/home/example/project:subagent", subagent.session_id);
        assert_eq!(1, subagent.points.len());
        assert!(subagent.human_points.is_empty());
        // Events carry no token counts.
        assert!(foreground.token_events.is_empty());
    }
}
