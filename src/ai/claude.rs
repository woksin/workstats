//! Claude Code: `~/.claude/projects/<encoded cwd>/<session>.jsonl`, one JSON record per line.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::{
    MAX_JSONL_LINE_BYTES, ParsedFile, discover_files, file_stem, for_json_lines, load_files,
    safe_model,
};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, RawSession, Session, TokenEvent, TokenUsage};
use crate::paths::{PathResolver, lossy_claude_cwd};
use crate::timeutil::{nearest_models, parse_timestamp};

#[derive(Deserialize)]
struct ClaudeRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    version: Option<String>,
    #[serde(default, deserialize_with = "deserialize_message")]
    message: Option<ClaudeMessage>,
    #[serde(default, rename = "isMeta")]
    is_meta: bool,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    #[serde(default, rename = "isCompactSummary")]
    is_compact_summary: bool,
    #[serde(default, rename = "isVisibleInTranscriptOnly")]
    visible_only: bool,
    #[serde(default, rename = "sourceToolUseID")]
    source_tool_use_id: Option<IgnoredAny>,
}

#[derive(Default)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    human_content: bool,
    usage: Option<ClaudeUsage>,
}

#[derive(Clone, Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn deserialize_message<'de, D>(deserializer: D) -> Result<Option<ClaudeMessage>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MessageVisitor;
    impl<'de> Visitor<'de> for MessageVisitor {
        type Value = Option<ClaudeMessage>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a message object or another JSON value")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut message = ClaudeMessage::default();
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "id" => message.id = map.next_value::<Option<String>>()?,
                    "model" => message.model = map.next_value::<Option<String>>()?,
                    "content" => message.human_content = map.next_value::<HumanContent>()?.0,
                    "usage" => message.usage = map.next_value::<Option<ClaudeUsage>>()?,
                    _ => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(Some(message))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }
    deserializer.deserialize_any(MessageVisitor)
}

struct HumanContent(bool);

impl<'de> Deserialize<'de> for HumanContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = HumanContent;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("message content")
            }

            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(HumanContent(true))
            }

            fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
                Ok(HumanContent(true))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut human = false;
                while let Some(item) = sequence.next_element::<ContentItem>()? {
                    human |= matches!(item.item_type.as_deref(), Some("text" | "image"));
                }
                Ok(HumanContent(human))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(HumanContent(false))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }

            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }

            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }

            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(HumanContent(false))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

#[derive(Deserialize)]
struct ContentItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
}

pub fn discover_claude_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.extension().is_some_and(|value| value == "jsonl")
    })
}

pub fn read_claude_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!("Claude history not found: {}", root.display()));
        return Vec::new();
    }
    load_files(
        discover_claude_files(root),
        resolver,
        diagnostics,
        cache,
        "claude",
        |_| "claude-v1".to_string(),
        since,
        until,
        |path| parse_claude_file(path, root, MAX_JSONL_LINE_BYTES),
    )
}

pub fn parse_claude_file(path: &Path, root: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut points = Vec::new();
    let mut human_points = Vec::new();
    let mut token_events: Vec<TokenEvent> = Vec::new();
    let mut counted_responses: HashMap<(String, String), usize> = HashMap::new();
    let mut cwd = None;
    let mut session_id = None;
    let mut version = None;
    let mut current_model = "unknown".to_string();
    for_json_lines(path, max_line_bytes, &mut result, |record: ClaudeRecord| {
        let Some(record_type) = record.record_type.as_deref() else {
            return;
        };
        if record_type != "user" && record_type != "assistant" {
            return;
        }
        if cwd.is_none() {
            cwd = record.cwd;
        }
        if session_id.is_none() {
            session_id = record.session_id;
        }
        if version.is_none() {
            version = record.version;
        }
        if record_type == "assistant"
            && let Some(model) = record
                .message
                .as_ref()
                .and_then(|message| message.model.as_deref())
        {
            current_model = safe_model(model);
        }
        let usage = record
            .message
            .as_ref()
            .and_then(|message| message.usage.clone());
        let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp) else {
            return;
        };
        points.push(ActivityPoint {
            timestamp,
            model: current_model.clone(),
        });
        if record_type == "assistant"
            && let Some(usage) = usage
        {
            let usage = TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_input_tokens,
                cache_creation_tokens: usage.cache_creation_input_tokens,
            };
            if !usage.is_zero() {
                let event = TokenEvent {
                    timestamp,
                    model: current_model.clone(),
                    usage,
                };
                // Claude Code writes one record per content block of a single API
                // response — a text block, then one per tool call — and every one of
                // them repeats the same ids and a byte-identical usage. The response,
                // not the record, is what may be counted.
                let response_key = match (
                    record
                        .message
                        .as_ref()
                        .and_then(|message| message.id.clone()),
                    record.request_id.clone(),
                ) {
                    (None, None) => None,
                    (id, request) => Some((id.unwrap_or_default(), request.unwrap_or_default())),
                };
                let counted = response_key
                    .as_ref()
                    .and_then(|key| counted_responses.get(key).copied());
                match (response_key, counted) {
                    // Last occurrence wins. The repeats are identical apart from the
                    // timestamp, so this only moves the response to the moment its
                    // final block was written.
                    (_, Some(index)) => token_events[index] = event,
                    (Some(key), None) => {
                        counted_responses.insert(key, token_events.len());
                        token_events.push(event);
                    }
                    // A record carrying neither id cannot be matched to a sibling, so
                    // it is counted rather than dropped.
                    (None, None) => token_events.push(event),
                }
            }
        }
        let human = record_type == "user"
            && record.message.is_some_and(|message| message.human_content)
            && !record.is_meta
            && !record.is_sidechain
            && !record.is_compact_summary
            && !record.visible_only
            && record.source_tool_use_id.is_none();
        if human {
            human_points.push(ActivityPoint {
                timestamp,
                model: current_model.clone(),
            });
        }
    });
    if points.is_empty() {
        result.diagnostics.skipped_sessions += 1;
        return result;
    }
    let models_at: BTreeMap<_, _> = nearest_models(&points)
        .into_iter()
        .map(|point| (point.timestamp, point.model))
        .collect();
    for point in &mut human_points {
        if let Some(model) = models_at.get(&point.timestamp) {
            point.model.clone_from(model);
        }
    }
    let approximate_cwd = cwd.is_none();
    let cwd = cwd.unwrap_or_else(|| lossy_claude_cwd(path.parent().unwrap_or(root)));
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    result.sessions.push(RawSession {
        provider: "claude".to_string(),
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
        is_subagent: path
            .components()
            .any(|part| part.as_os_str() == "subagents"),
        approximate_cwd,
        version,
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{file_time_range, fixture, utc};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn oversized_line_is_skipped_without_losing_next_record() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"message\":{{\"content\":\"hello\"}}}}\n",
                "x".repeat(1024)
            ),
        )
        .unwrap();
        let result = parse_claude_file(&path, root.path(), 512);
        assert_eq!(1, result.diagnostics.malformed_lines);
        assert_eq!(1, result.sessions.len());
    }

    #[test]
    fn claude_meta_messages_are_not_human_evidence() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        let records = [
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": project,
                "message": {"content": "real"}
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-01-01T00:01:00Z",
                "cwd": project,
                "isMeta": true,
                "message": {"content": "automatic"}
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
        let parsed = parse_claude_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions[0].human_points.len());
    }

    #[test]
    fn claude_assistant_usage_becomes_a_token_event() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        let records = [
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": project,
                "message": {"content": "hello"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-01-01T00:00:05Z",
                "cwd": project,
                "message": {
                    "model": "claude-test",
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 34,
                        "cache_creation_input_tokens": 5,
                        "cache_read_input_tokens": 6
                    }
                }
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
        let parsed = parse_claude_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions[0].token_events.len());
        let event = &parsed.sessions[0].token_events[0];
        assert_eq!("claude-test", event.model);
        assert_eq!(12, event.usage.input_tokens);
        assert_eq!(34, event.usage.output_tokens);
        assert_eq!(5, event.usage.cache_creation_tokens);
        assert_eq!(6, event.usage.cache_read_tokens);
    }

    #[test]
    fn claude_repeated_content_block_records_count_one_response() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        let usage = serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        });
        let records = [
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-01-01T00:00:05Z",
                "cwd": project,
                "requestId": "req-one",
                "message": {"id": "msg-one", "model": "claude-test", "usage": usage.clone()}
            }),
            // The same API response written again for its tool_use block.
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-01-01T00:00:06Z",
                "cwd": project,
                "requestId": "req-one",
                "message": {"id": "msg-one", "model": "claude-test", "usage": usage.clone()}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-01-01T00:00:09Z",
                "cwd": project,
                "requestId": "req-two",
                "message": {"id": "msg-two", "model": "claude-test", "usage": usage.clone()}
            }),
            // Neither id present: nothing can pair it with a sibling, so it is counted.
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-01-01T00:00:12Z",
                "cwd": project,
                "message": {"model": "claude-test", "usage": usage.clone()}
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
        let parsed = parse_claude_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let events = &parsed.sessions[0].token_events;
        assert_eq!(4, parsed.sessions[0].points.len());
        assert_eq!(3, events.len());
        assert_eq!(
            90,
            events.iter().map(|event| event.usage.total()).sum::<u64>()
        );
        assert_eq!(
            parse_timestamp("2026-01-01T00:00:06Z").unwrap(),
            events[0].timestamp
        );
    }

    #[test]
    fn the_claude_fixture_parses_to_its_documented_timestamps_and_tokens() {
        let root = fixture("claude");
        let files = discover_claude_files(&root);
        assert_eq!(
            vec![root.join("-home-example-project/session.jsonl")],
            files
        );

        let parsed = parse_claude_file(&files[0], &root, MAX_JSONL_LINE_BYTES);
        assert_eq!(9, parsed.records_read);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        assert_eq!(1, parsed.sessions.len());
        let session = &parsed.sessions[0];
        // The id carries the path relative to the history root, written with
        // the platform's own separator.
        assert_eq!(
            format!(
                "claude-fixture:-home-example-project{}session.jsonl",
                std::path::MAIN_SEPARATOR
            ),
            session.session_id
        );
        assert_eq!("/home/example/project", session.cwd);
        assert!(!session.approximate_cwd);
        assert_eq!(Some("2.0.0"), session.version.as_deref());
        // Every user and assistant record is activity; the file-history snapshot is not.
        assert_eq!(8, session.points.len());
        // A tool result and a meta caveat are user records nobody typed. A prompt takes
        // the model of the nearest reply, since it is sent before any reply names one.
        assert!(
            session
                .human_points
                .iter()
                .all(|point| point.model == "claude-fixture-model")
        );
        assert_eq!(
            vec![utc("2026-01-01T09:00:00Z"), utc("2026-01-01T09:05:00Z")],
            session
                .human_points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        // msg-1 is written twice (a text block, then a tool call) and counts once, at
        // the moment of its last block.
        let tokens: Vec<_> = session
            .token_events
            .iter()
            .map(|event| {
                (
                    event.timestamp,
                    event.usage.input_tokens,
                    event.usage.output_tokens,
                    event.usage.cache_read_tokens,
                    event.usage.cache_creation_tokens,
                )
            })
            .collect();
        assert_eq!(
            vec![
                (utc("2026-01-01T09:00:12Z"), 100, 50, 10, 20),
                (utc("2026-01-01T09:00:30Z"), 200, 30, 120, 0),
                (utc("2026-01-01T09:05:10Z"), 10, 5, 0, 0),
            ],
            tokens
        );
        assert_eq!(
            (
                Some(utc("2026-01-01T09:00:00Z")),
                Some(utc("2026-01-01T09:05:10Z"))
            ),
            file_time_range(&parsed.sessions)
        );
    }
}
