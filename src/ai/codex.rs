//! Codex CLI: `rollout-*.jsonl` files under `sessions/YYYY/MM/DD`, plus the optional SQLite
//! `threads` table that knows where each rollout ran.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use walkdir::WalkDir;

use super::{
    MAX_JSONL_LINE_BYTES, ParsedFile, canonical_string, deserialize_maybe_number, file_stem,
    for_json_lines, load_files, safe_model,
};
use crate::cache::TranscriptCache;
use crate::model::{
    ActivityPoint, Diagnostics, ExactInterval, RawSession, Session, TokenEvent, TokenUsage,
};
use crate::paths::PathResolver;
use crate::timeutil::{parse_epoch_milliseconds, parse_timestamp};

#[derive(Debug, Default, Clone)]
pub struct CodexMetadata {
    pub id: Option<String>,
    pub rollout_path: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default)]
pub struct CodexMetadataIndex {
    pub by_path: HashMap<String, CodexMetadata>,
    pub by_id: HashMap<String, CodexMetadata>,
}

#[derive(Deserialize)]
struct CodexRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    payload: CodexPayload,
}

#[derive(Default, Deserialize)]
struct CodexPayload {
    id: Option<String>,
    session_id: Option<String>,
    parent_thread_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_is_object")]
    source: bool,
    cwd: Option<String>,
    model: Option<String>,
    #[serde(rename = "type")]
    payload_type: Option<String>,
    role: Option<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    started_at_ms: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    completed_at_ms: Option<f64>,
    item: Option<ExactCandidate>,
    result: Option<ExactCandidate>,
    task: Option<ExactCandidate>,
    info: Option<CodexTokenInfo>,
}

#[derive(Deserialize)]
struct ExactCandidate {
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    started_at_ms: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    completed_at_ms: Option<f64>,
}

#[derive(Deserialize)]
struct CodexTokenInfo {
    total_token_usage: Option<CodexTokenUsage>,
    last_token_usage: Option<CodexTokenUsage>,
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
struct CodexTokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn deserialize_is_object<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    struct ObjectVisitor;
    impl<'de> Visitor<'de> for ObjectVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("any JSON value")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(true)
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(false)
        }
        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            Ok(false)
        }
    }
    deserializer.deserialize_any(ObjectVisitor)
}

pub fn discover_codex_files_bounded(root: &Path, until: Option<DateTime<Utc>>) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let until_date = until.map(|value| value.with_timezone(&Local).date_naive());
    let mut paths: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_dir()
                || codex_directory_date(entry.path(), root)
                    .is_none_or(|date| until_date.is_none_or(|bound| date < bound))
        })
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().file_name().is_some_and(|value| {
                    value.to_string_lossy().starts_with("rollout-")
                        && entry.path().extension().is_some_and(|ext| ext == "jsonl")
                })
        })
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();
    paths
}

fn codex_directory_date(path: &Path, root: &Path) -> Option<NaiveDate> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = relative.iter().collect();
    if parts.len() != 3 {
        return None;
    }
    let year = parts[0].to_string_lossy().parse().ok()?;
    let month = parts[1].to_string_lossy().parse().ok()?;
    let day = parts[2].to_string_lossy().parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

#[allow(clippy::too_many_arguments)]
pub fn read_codex_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    sqlite_path: Option<&Path>,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!("Codex history not found: {}", root.display()));
        return Vec::new();
    }
    let metadata = sqlite_path
        .map(|path| read_codex_sqlite_metadata(path, diagnostics))
        .unwrap_or_default();
    load_files(
        discover_codex_files_bounded(root, until),
        resolver,
        diagnostics,
        cache,
        "codex",
        |path| codex_context_fingerprint(&metadata, path),
        since,
        until,
        |path| parse_codex_file(path, &metadata, MAX_JSONL_LINE_BYTES),
    )
}

pub fn parse_codex_file(
    path: &Path,
    metadata: &CodexMetadataIndex,
    max_line_bytes: usize,
) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut points_by_cwd: BTreeMap<Option<String>, Vec<ActivityPoint>> = BTreeMap::new();
    let mut exact_by_cwd: BTreeMap<Option<String>, Vec<ExactInterval>> = BTreeMap::new();
    let mut human_by_cwd: BTreeMap<Option<String>, Vec<ActivityPoint>> = BTreeMap::new();
    let mut token_events_by_cwd: BTreeMap<Option<String>, Vec<TokenEvent>> = BTreeMap::new();
    let mut cwd = None;
    let mut metadata_cwd = None;
    let mut session_id = None;
    let mut current_model = "unknown".to_string();
    let mut is_subagent = false;
    let mut previous_token_total = None;
    for_json_lines(path, max_line_bytes, &mut result, |record: CodexRecord| {
        let payload = record.payload;
        match record.record_type.as_deref() {
            Some("session_meta") => {
                if let Some(id) = payload.id.or(payload.session_id) {
                    session_id = Some(id);
                }
                is_subagent |= payload.parent_thread_id.is_some() || payload.source;
                if let Some(value) = payload.cwd {
                    metadata_cwd = Some(value.clone());
                    cwd = Some(value);
                }
                if let Some(model) = payload.model {
                    current_model = safe_model(&model);
                }
            }
            Some("turn_context") => {
                if let Some(value) = payload.cwd {
                    cwd = Some(value);
                }
                if let Some(model) = payload.model {
                    current_model = safe_model(&model);
                }
            }
            Some("response_item" | "event_msg") => {
                if let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp) {
                    points_by_cwd
                        .entry(cwd.clone())
                        .or_default()
                        .push(ActivityPoint {
                            timestamp,
                            model: current_model.clone(),
                        });
                    if record.record_type.as_deref() == Some("response_item")
                        && payload.payload_type.as_deref() == Some("message")
                        && payload.role.as_deref() == Some("user")
                        && !is_subagent
                    {
                        human_by_cwd
                            .entry(cwd.clone())
                            .or_default()
                            .push(ActivityPoint {
                                timestamp,
                                model: current_model.clone(),
                            });
                    }
                }
                if record.record_type.as_deref() == Some("event_msg")
                    && let Some(interval) = exact_codex_interval(&payload, &current_model)
                {
                    exact_by_cwd.entry(cwd.clone()).or_default().push(interval);
                }
                if record.record_type.as_deref() == Some("event_msg")
                    && let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp)
                    && let Some(event) = codex_token_event(
                        &payload,
                        timestamp,
                        &current_model,
                        &mut previous_token_total,
                    )
                {
                    token_events_by_cwd
                        .entry(cwd.clone())
                        .or_default()
                        .push(event);
                }
            }
            _ => {}
        }
    });

    let resolved_path = canonical_string(path);
    let meta = metadata
        .by_path
        .get(&resolved_path)
        .or_else(|| session_id.as_ref().and_then(|id| metadata.by_id.get(id)));
    if session_id.is_none() {
        session_id = meta
            .and_then(|item| item.id.clone())
            .or_else(|| Some(file_stem(path).trim_start_matches("rollout-").to_string()));
    }
    if metadata_cwd.is_none() {
        metadata_cwd = meta.and_then(|item| item.cwd.clone());
    }
    let fallback_model = safe_model(
        meta.and_then(|item| item.model.as_deref())
            .unwrap_or("unknown"),
    );
    let mut cwd_keys = BTreeSet::new();
    cwd_keys.extend(points_by_cwd.keys().cloned());
    cwd_keys.extend(exact_by_cwd.keys().cloned());
    cwd_keys.extend(human_by_cwd.keys().cloned());
    cwd_keys.extend(token_events_by_cwd.keys().cloned());
    if cwd_keys.is_empty() {
        result.diagnostics.skipped_sessions += 1;
        return result;
    }
    let multiple = cwd_keys.len() > 1;
    for cwd_key in cwd_keys {
        let mut points = points_by_cwd.remove(&cwd_key).unwrap_or_default();
        let mut exact_intervals = exact_by_cwd.remove(&cwd_key).unwrap_or_default();
        let human_points = human_by_cwd.remove(&cwd_key).unwrap_or_default();
        let mut token_events = token_events_by_cwd.remove(&cwd_key).unwrap_or_default();
        if fallback_model != "unknown" {
            for point in &mut points {
                if point.model == "unknown" {
                    point.model.clone_from(&fallback_model);
                }
            }
            for interval in &mut exact_intervals {
                if interval.model == "unknown" {
                    interval.model.clone_from(&fallback_model);
                }
            }
            for event in &mut token_events {
                if event.model == "unknown" {
                    event.model.clone_from(&fallback_model);
                }
            }
        }
        let resolved_cwd = cwd_key.clone().or_else(|| metadata_cwd.clone());
        let approximate_cwd = resolved_cwd.is_none();
        let resolved_cwd = resolved_cwd
            .unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned());
        let base_id = session_id.clone().unwrap_or_else(|| file_stem(path));
        let split_id = if multiple {
            format!("{base_id}:{resolved_cwd}")
        } else {
            base_id
        };
        result.sessions.push(RawSession {
            provider: "codex".to_string(),
            session_id: split_id,
            source_file: path.to_path_buf(),
            cwd: resolved_cwd,
            repository_hint_cwd: None,
            points,
            exact_intervals,
            human_points,
            token_events,
            is_subagent,
            approximate_cwd,
            version: None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
        });
    }
    result
}

/// `parse_codex_file` falls back from the path index to the id index, so the fingerprint
/// has to follow it — a rollout matched only by id used to carry a constant fingerprint
/// and never invalidate when its metadata changed. The file is not read here, so the id
/// comes from the `rollout-<timestamp>-<thread id>.jsonl` name.
fn codex_context_fingerprint(metadata: &CodexMetadataIndex, path: &Path) -> String {
    metadata
        .by_path
        .get(&canonical_string(path))
        .or_else(|| codex_rollout_id(path).and_then(|id| metadata.by_id.get(&id)))
        .map(|item| {
            format!(
                "codex-v2:{}:{}:{}",
                item.id.as_deref().unwrap_or_default(),
                item.cwd.as_deref().unwrap_or_default(),
                item.model.as_deref().unwrap_or_default()
            )
        })
        .unwrap_or_else(|| "codex-v2:none".to_string())
}

fn codex_rollout_id(path: &Path) -> Option<String> {
    let stem = file_stem(path);
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    let uuid_shaped = candidate
        .as_bytes()
        .iter()
        .enumerate()
        .all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    uuid_shaped.then(|| candidate.to_string())
}

fn exact_codex_interval(payload: &CodexPayload, model: &str) -> Option<ExactInterval> {
    let direct = (payload.started_at_ms, payload.completed_at_ms);
    let nested = [
        payload.item.as_ref(),
        payload.result.as_ref(),
        payload.task.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|item| (item.started_at_ms, item.completed_at_ms));
    std::iter::once(direct)
        .chain(nested)
        .find_map(|(start, end)| {
            let start = parse_epoch_milliseconds(start?)?;
            let end = parse_epoch_milliseconds(end?)?;
            (end > start).then(|| ExactInterval {
                start,
                end,
                model: model.to_string(),
            })
        })
}

fn codex_token_event(
    payload: &CodexPayload,
    timestamp: DateTime<Utc>,
    model: &str,
    previous_total: &mut Option<CodexTokenUsage>,
) -> Option<TokenEvent> {
    if payload.payload_type.as_deref() != Some("token_count") {
        return None;
    }
    let info = payload.info.as_ref()?;
    // Codex re-emits a `token_count` event that still carries the previous turn's
    // `last_token_usage` while the cumulative total has not moved. Only the total tells
    // the two apart, so an unmoved total means the turn was already counted.
    if let Some(total) = info.total_token_usage.as_ref() {
        if previous_total.as_ref() == Some(total) {
            return None;
        }
        *previous_total = Some(total.clone());
    }
    let last = info.last_token_usage.as_ref()?;
    // `cached_input_tokens` is a subset of `input_tokens` (OpenAI-style accounting), not
    // additional to it, so it is split out here rather than summed on top.
    let usage = TokenUsage {
        input_tokens: last.input_tokens.saturating_sub(last.cached_input_tokens),
        output_tokens: last.output_tokens,
        cache_read_tokens: last.cached_input_tokens,
        cache_creation_tokens: last.cache_write_input_tokens,
    };
    if usage.is_zero() {
        return None;
    }
    Some(TokenEvent {
        timestamp,
        model: model.to_string(),
        usage,
    })
}

pub fn read_codex_sqlite_metadata(
    path: &Path,
    diagnostics: &mut Diagnostics,
) -> CodexMetadataIndex {
    if !path.is_file() {
        return CodexMetadataIndex::default();
    }
    let result = (|| -> rusqlite::Result<CodexMetadataIndex> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let has_threads: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads')",
            [],
            |row| row.get(0),
        )?;
        if !has_threads {
            return Ok(CodexMetadataIndex::default());
        }
        let mut statement = connection.prepare("PRAGMA table_info(threads)")?;
        let columns: BTreeSet<String> = statement
            .query_map([], |row| row.get(1))?
            .filter_map(Result::ok)
            .collect();
        let selected: Vec<_> = ["id", "rollout_path", "cwd", "model"]
            .into_iter()
            .filter(|name| columns.contains(*name))
            .collect();
        if selected.is_empty() {
            return Ok(CodexMetadataIndex::default());
        }
        let query = format!(
            "SELECT {} FROM threads",
            selected
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut statement = connection.prepare(&query)?;
        let mut rows = statement.query([])?;
        let mut index = CodexMetadataIndex::default();
        while let Some(row) = rows.next()? {
            let mut item = CodexMetadata::default();
            for (column, name) in selected.iter().enumerate() {
                let value: Option<String> = row.get(column).unwrap_or(None);
                match *name {
                    "id" => item.id = value,
                    "rollout_path" => item.rollout_path = value,
                    "cwd" => item.cwd = value,
                    "model" => item.model = value,
                    _ => {}
                }
            }
            if let Some(rollout_path) = &item.rollout_path {
                index
                    .by_path
                    .insert(canonical_string(Path::new(rollout_path)), item.clone());
            }
            if let Some(id) = &item.id {
                index.by_id.insert(id.clone(), item);
            }
        }
        Ok(index)
    })();
    match result {
        Ok(index) => index,
        Err(error) => {
            diagnostics.warn(format!(
                "Codex metadata database ignored: {}: {error}",
                path.display()
            ));
            CodexMetadataIndex::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{fixture, utc};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn bounded_codex_discovery_skips_future_date_directories() {
        let root = tempdir().unwrap();
        let january = root.path().join("2026/01/31");
        let february = root.path().join("2026/02/01");
        fs::create_dir_all(&january).unwrap();
        fs::create_dir_all(&february).unwrap();
        fs::write(january.join("rollout-a.jsonl"), "{}\n").unwrap();
        fs::write(february.join("rollout-b.jsonl"), "{}\n").unwrap();
        let until = crate::timeutil::parse_bound(Some("2026-01-31"), true)
            .unwrap()
            .unwrap();
        let files = discover_codex_files_bounded(root.path(), Some(until));
        assert_eq!(1, files.len());
        assert!(files[0].ends_with("rollout-a.jsonl"));
    }

    #[test]
    fn codex_model_changes_and_exact_intervals_match_reference() {
        let root = tempdir().unwrap();
        let second = root.path().join("second");
        fs::create_dir(&second).unwrap();
        let path = root.path().join("rollout-test.jsonl");
        let records = [
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": "s", "cwd": root.path()}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "turn_context",
                "payload": {"model": "gpt-a", "cwd": root.path()}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "response_item",
                "payload": {"type": "reasoning"}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:01:00Z",
                "type": "response_item",
                "payload": {"type": "message"}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:02:00Z",
                "type": "turn_context",
                "payload": {"model": "gpt-b", "cwd": second}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:02:00Z",
                "type": "response_item",
                "payload": {"type": "reasoning"}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:03:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "started_at_ms": 1767225720000_i64,
                    "completed_at_ms": 1767225780000_i64
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
        let parsed = parse_codex_file(&path, &CodexMetadataIndex::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.sessions.len());
        let mut resolver = PathResolver::with_home(Vec::new(), root.path().to_path_buf());
        let intervals: Vec<_> = parsed
            .sessions
            .into_iter()
            .flat_map(|raw| {
                crate::timeutil::build_session_intervals(
                    &resolver.resolve_session(raw),
                    chrono::Duration::minutes(5),
                )
            })
            .collect();
        assert_eq!(
            BTreeSet::from(["gpt-a".to_string(), "gpt-b".to_string()]),
            intervals.iter().map(|item| item.model.clone()).collect()
        );
        assert_eq!(
            120.0,
            intervals
                .iter()
                .map(crate::model::Interval::seconds)
                .sum::<f64>()
        );
    }

    #[test]
    fn codex_token_count_events_report_per_turn_deltas() {
        let root = tempdir().unwrap();
        let path = root.path().join("rollout-test.jsonl");
        let records = [
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": "s", "cwd": root.path(), "model": "gpt-a"}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:10Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 100, "cached_input_tokens": 10,
                            "cache_write_input_tokens": 0, "output_tokens": 20,
                            "reasoning_output_tokens": 5, "total_tokens": 120
                        },
                        "last_token_usage": {
                            "input_tokens": 100, "cached_input_tokens": 10,
                            "cache_write_input_tokens": 0, "output_tokens": 20,
                            "reasoning_output_tokens": 5, "total_tokens": 120
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:20Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 260, "cached_input_tokens": 90,
                            "cache_write_input_tokens": 0, "output_tokens": 45,
                            "reasoning_output_tokens": 8, "total_tokens": 305
                        },
                        "last_token_usage": {
                            "input_tokens": 160, "cached_input_tokens": 80,
                            "cache_write_input_tokens": 0, "output_tokens": 25,
                            "reasoning_output_tokens": 3, "total_tokens": 185
                        }
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
        let parsed = parse_codex_file(&path, &CodexMetadataIndex::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions.len());
        let events = &parsed.sessions[0].token_events;
        assert_eq!(2, events.len());
        assert_eq!(90, events[0].usage.input_tokens);
        assert_eq!(80, events[1].usage.input_tokens);
        assert_eq!(80, events[1].usage.cache_read_tokens);
        let total: u64 = events.iter().map(|event| event.usage.total()).sum();
        assert_eq!(120 + 185, total);
    }

    #[test]
    fn codex_token_count_repeated_at_an_unchanged_total_is_counted_once() {
        let root = tempdir().unwrap();
        let path = root.path().join("rollout-test.jsonl");
        let total = serde_json::json!({
            "input_tokens": 100, "cached_input_tokens": 10,
            "cache_write_input_tokens": 0, "output_tokens": 20,
            "reasoning_output_tokens": 5, "total_tokens": 120
        });
        let last = serde_json::json!({
            "input_tokens": 100, "cached_input_tokens": 10,
            "cache_write_input_tokens": 0, "output_tokens": 20,
            "reasoning_output_tokens": 5, "total_tokens": 120
        });
        let records = [
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {"id": "s", "cwd": root.path(), "model": "gpt-a"}
            }),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:10Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": total.clone(), "last_token_usage": last.clone()}
                }
            }),
            // Re-emitted with the previous turn's last usage and an unmoved total.
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:15Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": total.clone(), "last_token_usage": last.clone()}
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
        let parsed = parse_codex_file(&path, &CodexMetadataIndex::default(), MAX_JSONL_LINE_BYTES);
        let events = &parsed.sessions[0].token_events;
        assert_eq!(1, events.len());
        assert_eq!(120, events[0].usage.total());
    }

    #[test]
    fn the_codex_fixture_parses_to_its_documented_timestamps_and_tokens() {
        let root = fixture("codex");
        let files = discover_codex_files_bounded(&root, None);
        assert_eq!(1, files.len());

        let parsed = parse_codex_file(
            &files[0],
            &CodexMetadataIndex::default(),
            MAX_JSONL_LINE_BYTES,
        );
        assert_eq!(8, parsed.records_read);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        assert_eq!(1, parsed.sessions.len());
        let session = &parsed.sessions[0];
        assert_eq!("codex-fixture", session.session_id);
        assert_eq!("/home/example/project", session.cwd);
        assert!(!session.approximate_cwd);
        assert!(!session.is_subagent);
        // Metadata and context records carry no activity; the rest do.
        assert_eq!(
            vec![
                utc("2026-01-01T10:00:02Z"),
                utc("2026-01-01T10:00:10Z"),
                utc("2026-01-01T10:00:20Z"),
                utc("2026-01-01T10:01:00Z"),
                utc("2026-01-01T10:05:00Z"),
                utc("2026-01-01T10:05:30Z"),
            ],
            session
                .points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert!(
            session
                .points
                .iter()
                .all(|point| point.model == "gpt-fixture")
        );
        assert_eq!(
            vec![utc("2026-01-01T10:00:02Z"), utc("2026-01-01T10:05:00Z")],
            session
                .human_points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(1, session.exact_intervals.len());
        assert_eq!(
            utc("2026-01-01T10:00:30Z"),
            session.exact_intervals[0].start
        );
        assert_eq!(utc("2026-01-01T10:01:00Z"), session.exact_intervals[0].end);
        // The second count is reported as a delta against the first, with cached input
        // taken out of the input it is a subset of.
        let tokens: Vec<_> = session
            .token_events
            .iter()
            .map(|event| {
                (
                    event.timestamp,
                    event.usage.input_tokens,
                    event.usage.output_tokens,
                    event.usage.cache_read_tokens,
                )
            })
            .collect();
        assert_eq!(
            vec![
                (utc("2026-01-01T10:00:20Z"), 60, 20, 40),
                (utc("2026-01-01T10:05:30Z"), 100, 30, 100),
            ],
            tokens
        );
    }
}
