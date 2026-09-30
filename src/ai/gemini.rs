//! Gemini CLI: `chats/*.json` (legacy single document) and `chats/*.jsonl` (one record per
//! line), with the working directory taken from a `.project_root` marker.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{
    MAX_JSONL_LINE_BYTES, ParsedFile, discover_files, file_stem, for_json_lines, load_files,
    safe_model,
};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, RawSession, Session, TokenEvent, TokenUsage};
use crate::paths::PathResolver;
use crate::timeutil::{nearest_models, parse_timestamp};

/// A legacy Gemini session is a single JSON document rather than a line per record, so
/// it needs a whole-file bound of its own. Real sessions reach tens of megabytes, hence
/// the far larger limit.
pub const MAX_GEMINI_JSON_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct GeminiRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    model: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "startTime")]
    start_time: Option<String>,
    #[serde(rename = "lastUpdated")]
    last_updated: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    messages: Vec<GeminiMessage>,
    #[serde(default)]
    tokens: Option<GeminiTokens>,
}

#[derive(Deserialize)]
struct GeminiMessage {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    model: Option<String>,
    #[serde(default)]
    tokens: Option<GeminiTokens>,
}

#[derive(Default, Deserialize)]
struct GeminiTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cached: u64,
}

pub fn discover_gemini_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        let extension = path.extension().and_then(|value| value.to_str());
        matches!(extension, Some("json" | "jsonl"))
            && path
                .components()
                .any(|part| part.as_os_str().eq_ignore_ascii_case("chats"))
    })
}

pub fn read_gemini_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!("Gemini CLI history not found: {}", root.display()));
        return Vec::new();
    }
    load_files(
        discover_gemini_files(root),
        resolver,
        diagnostics,
        cache,
        "gemini",
        gemini_context_fingerprint,
        since,
        until,
        |path| parse_gemini_file(path, root, MAX_JSONL_LINE_BYTES),
    )
}

pub fn parse_gemini_file(path: &Path, root: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut session_id = None;
    let mut kind = None;
    let mut version = None;
    let mut messages = Vec::new();
    if path
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
    {
        for_json_lines(path, max_line_bytes, &mut result, |record: GeminiRecord| {
            if session_id.is_none() {
                session_id = record.session_id;
            }
            if kind.is_none() {
                kind = record.kind;
            }
            if record.record_type.is_some() {
                messages.push(GeminiMessage {
                    record_type: record.record_type,
                    timestamp: record.timestamp,
                    model: record.model,
                    tokens: record.tokens,
                });
            }
        });
    } else {
        // A legacy session is one JSON document, so `max_line_bytes` cannot bound it and
        // the whole file would otherwise be read into memory unbounded. The BufReader is
        // not cosmetic: serde_json's `IoRead` issues one syscall per byte, which measured
        // ~90x slower on a 19 MB session.
        let parsed = File::open(path)
            .map_err(anyhow::Error::from)
            .and_then(|file| {
                let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                if size > MAX_GEMINI_JSON_BYTES {
                    anyhow::bail!("session larger than {MAX_GEMINI_JSON_BYTES} bytes");
                }
                serde_json::from_reader::<_, GeminiRecord>(BufReader::with_capacity(
                    128 * 1024,
                    file,
                ))
                .map_err(Into::into)
            });
        match parsed {
            Ok(record) => {
                session_id = record.session_id;
                kind = record.kind;
                version = record
                    .last_updated
                    .or(record.start_time)
                    .map(|_| "legacy-json".to_string());
                result.records_read = record.messages.len() as u64;
                messages = record.messages;
            }
            Err(error) => {
                result.diagnostics.unreadable_files += 1;
                result.diagnostics.warn(format!(
                    "invalid Gemini session skipped: {}: {error}",
                    path.display()
                ));
                return result;
            }
        }
    }
    let mut current_model = "unknown".to_string();
    let mut points = Vec::new();
    let mut human_points = Vec::new();
    let mut token_events = Vec::new();
    let is_subagent = kind.as_deref() == Some("subagent")
        || path
            .file_name()
            .is_some_and(|name| !name.to_string_lossy().starts_with("session-"));
    for message in messages {
        let Some(message_type) = message.record_type.as_deref() else {
            continue;
        };
        if !matches!(message_type, "user" | "gemini") {
            continue;
        }
        if let Some(model) = message.model {
            current_model = safe_model(&model);
        }
        let Some(timestamp) = message.timestamp.as_deref().and_then(parse_timestamp) else {
            continue;
        };
        let point = ActivityPoint {
            timestamp,
            model: current_model.clone(),
        };
        points.push(point.clone());
        if message_type == "user" && !is_subagent {
            human_points.push(point);
        }
        if let Some(tokens) = message.tokens {
            // `cached` is a subset of `input`, not additional to it.
            let usage = TokenUsage {
                input_tokens: tokens.input.saturating_sub(tokens.cached),
                output_tokens: tokens.output,
                cache_read_tokens: tokens.cached,
                cache_creation_tokens: 0,
            };
            if !usage.is_zero() {
                token_events.push(TokenEvent {
                    timestamp,
                    model: current_model.clone(),
                    usage,
                });
            }
        }
    }
    if points.is_empty() {
        result.diagnostics.skipped_sessions += 1;
        return result;
    }
    let nearest = nearest_models(&points);
    points = nearest.clone();
    let models_at: BTreeMap<_, _> = nearest
        .into_iter()
        .map(|point| (point.timestamp, point.model))
        .collect();
    for point in &mut human_points {
        if let Some(model) = models_at.get(&point.timestamp) {
            point.clone_from(&ActivityPoint {
                timestamp: point.timestamp,
                model: model.clone(),
            });
        }
    }
    let project_root = gemini_project_root(path);
    let approximate_cwd = project_root.is_none();
    let cwd = project_root
        .unwrap_or_else(|| path.parent().unwrap_or(root).to_string_lossy().into_owned());
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    result.sessions.push(RawSession {
        provider: "gemini".to_string(),
        session_id: format!(
            "{}:{relative}",
            session_id.unwrap_or_else(|| file_stem(path))
        ),
        source_file: path.to_path_buf(),
        cwd,
        repository_hint_cwd: None,
        points,
        exact_intervals: Vec::new(),
        human_points,
        token_events,
        is_subagent,
        approximate_cwd,
        version,
    });
    result
}

fn gemini_project_root(path: &Path) -> Option<String> {
    gemini_project_marker(path).map(|(_, root)| root)
}

fn gemini_project_marker(path: &Path) -> Option<(PathBuf, String)> {
    for ancestor in path.ancestors() {
        let marker = ancestor.join(".project_root");
        if !marker.is_file() {
            continue;
        }
        let bytes = fs::read(&marker).ok()?;
        if bytes.len() > 32 * 1024 {
            return None;
        }
        let value = String::from_utf8(bytes).ok()?;
        let value = value.trim();
        if !value.is_empty() {
            return Some((marker, value.to_string()));
        }
    }
    None
}

/// A Gemini session takes its cwd from an out-of-band `.project_root` marker, so the
/// marker belongs in the fingerprint: a constant one kept a moved project pinned to its
/// stale directory for as long as the transcript itself was untouched.
fn gemini_context_fingerprint(path: &Path) -> String {
    match gemini_project_marker(path) {
        Some((marker, root)) => {
            format!("gemini-v2:{}:{root}", crate::cache::file_context(&marker))
        }
        None => "gemini-v2:none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{fixture, utc};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn gemini_jsonl_uses_project_marker_without_reading_message_content() {
        let root = tempdir().unwrap();
        let project = root.path().join("workspace/example");
        let storage = root.path().join("hash");
        let chats = storage.join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            storage.join(".project_root"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let path = chats.join("session-2026-test.jsonl");
        let records = [
            serde_json::json!({
                "sessionId": "gemini-session",
                "projectHash": "hash",
                "startTime": "2026-01-01T00:00:00Z",
                "lastUpdated": "2026-01-01T00:01:00Z",
                "kind": "main"
            }),
            serde_json::json!({
                "id": "one",
                "type": "user",
                "timestamp": "2026-01-01T00:00:00Z",
                "content": "not parsed"
            }),
            serde_json::json!({
                "id": "two",
                "type": "gemini",
                "timestamp": "2026-01-01T00:01:00Z",
                "model": "gemini-test",
                "content": "not parsed"
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

        let parsed = parse_gemini_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(project.to_string_lossy(), parsed.sessions[0].cwd);
        assert_eq!(2, parsed.sessions[0].points.len());
        assert_eq!(1, parsed.sessions[0].human_points.len());
        assert_eq!("gemini-test", parsed.sessions[0].human_points[0].model);
    }

    #[test]
    fn gemini_message_tokens_become_a_token_event() {
        let root = tempdir().unwrap();
        let project = root.path().join("workspace/example");
        let storage = root.path().join("hash");
        let chats = storage.join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            storage.join(".project_root"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let path = chats.join("session-2026-test.jsonl");
        let records = [
            serde_json::json!({
                "id": "one",
                "type": "user",
                "timestamp": "2026-01-01T00:00:00Z",
                "content": "not parsed"
            }),
            serde_json::json!({
                "id": "two",
                "type": "gemini",
                "timestamp": "2026-01-01T00:01:00Z",
                "model": "gemini-test",
                "tokens": {"input": 10, "output": 5, "cached": 2}
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
        let parsed = parse_gemini_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions[0].token_events.len());
        let event = &parsed.sessions[0].token_events[0];
        assert_eq!("gemini-test", event.model);
        assert_eq!(8, event.usage.input_tokens);
        assert_eq!(5, event.usage.output_tokens);
        assert_eq!(2, event.usage.cache_read_tokens);
    }

    #[test]
    fn the_gemini_fixtures_parse_to_their_documented_timestamps_and_tokens() {
        let root = fixture("gemini");
        let files = discover_gemini_files(&root);
        assert_eq!(2, files.len());
        let (legacy, lines) = if files[0].extension().is_some_and(|value| value == "json") {
            (&files[0], &files[1])
        } else {
            (&files[1], &files[0])
        };

        let parsed = parse_gemini_file(lines, &root, MAX_JSONL_LINE_BYTES);
        // The header line parses too; it just is not a message.
        assert_eq!(6, parsed.records_read);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        let session = &parsed.sessions[0];
        assert!(session.session_id.starts_with("gemini-fixture:"));
        assert_eq!("/home/example/project", session.cwd);
        assert!(!session.approximate_cwd);
        assert!(!session.is_subagent);
        // The `info` notice is neither a prompt nor a reply.
        assert_eq!(
            vec![
                utc("2026-01-01T11:00:01Z"),
                utc("2026-01-01T11:00:20Z"),
                utc("2026-01-01T11:04:00Z"),
                utc("2026-01-01T11:04:10Z"),
            ],
            session
                .points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec![utc("2026-01-01T11:00:01Z"), utc("2026-01-01T11:04:00Z")],
            session
                .human_points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        let tokens: Vec<_> = session
            .token_events
            .iter()
            .map(|event| {
                (
                    event.timestamp,
                    event.model.as_str(),
                    event.usage.input_tokens,
                    event.usage.output_tokens,
                    event.usage.cache_read_tokens,
                )
            })
            .collect();
        assert_eq!(
            vec![
                (
                    utc("2026-01-01T11:00:20Z"),
                    "gemini-fixture-model",
                    70,
                    40,
                    30
                ),
                (
                    utc("2026-01-01T11:04:10Z"),
                    "gemini-fixture-model",
                    150,
                    20,
                    0
                ),
            ],
            tokens
        );

        // The legacy layout is one JSON document holding the same kind of messages.
        let parsed = parse_gemini_file(legacy, &root, MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.records_read);
        let session = &parsed.sessions[0];
        assert_eq!(Some("legacy-json"), session.version.as_deref());
        assert_eq!(
            vec![utc("2026-01-02T11:00:05Z"), utc("2026-01-02T11:00:30Z")],
            session
                .points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(1, session.human_points.len());
        assert_eq!(1, session.token_events.len());
        assert_eq!(60, session.token_events[0].usage.input_tokens);
        assert_eq!(10, session.token_events[0].usage.output_tokens);
    }
}
