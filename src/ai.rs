use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use walkdir::WalkDir;

use crate::cache::{CacheLookup, FileStamp, TranscriptCache, file_stamp};
use crate::model::{ActivityPoint, Diagnostics, ExactInterval, RawSession, Session};
use crate::paths::{PathResolver, lossy_claude_cwd};
use crate::timeutil::{nearest_models, parse_epoch_milliseconds, parse_timestamp};

pub const MAX_JSONL_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default, Deserialize, serde::Serialize)]
pub struct ParsedFile {
    pub sessions: Vec<RawSession>,
    pub diagnostics: Diagnostics,
}

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
struct ClaudeRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
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
    model: Option<String>,
    human_content: bool,
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
                    "model" => message.model = map.next_value::<Option<String>>()?,
                    "content" => message.human_content = map.next_value::<HumanContent>()?.0,
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
}

#[derive(Deserialize)]
struct ExactCandidate {
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    started_at_ms: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    completed_at_ms: Option<f64>,
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

pub fn discover_claude_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.extension().is_some_and(|value| value == "jsonl")
    })
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
        "claude-v1",
        since,
        until,
        |path| parse_claude_file(path, root, MAX_JSONL_LINE_BYTES),
    )
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
    let context = format!(
        "codex-v1:{}",
        sqlite_path
            .map(crate::cache::file_context)
            .unwrap_or_else(|| "none".to_string())
    );
    load_files(
        discover_codex_files_bounded(root, until),
        resolver,
        diagnostics,
        cache,
        "codex",
        &context,
        since,
        until,
        |path| parse_codex_file(path, &metadata, MAX_JSONL_LINE_BYTES),
    )
}

#[allow(clippy::too_many_arguments)]
fn load_files<F>(
    paths: Vec<PathBuf>,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    mut cache: Option<&mut TranscriptCache>,
    provider: &str,
    context: &str,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    parser: F,
) -> Vec<Session>
where
    F: Fn(&Path) -> ParsedFile + Sync,
{
    let mut slots: Vec<Option<ParsedFile>> = (0..paths.len()).map(|_| None).collect();
    let mut pending_stamps: Vec<Option<FileStamp>> = vec![None; paths.len()];
    let mut misses = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let stamp = file_stamp(path);
        let lookup = cache.as_deref_mut().and_then(|cache| {
            stamp.map(|stamp| cache.lookup(path, provider, context, stamp, since, until))
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
    for (index, parsed) in parsed_misses {
        slots[index] = Some(parsed);
    }

    let mut sessions = Vec::new();
    for (index, item) in slots.into_iter().enumerate() {
        let Some(item) = item else {
            continue;
        };
        if let (Some(cache), Some(stamp)) = (cache.as_deref_mut(), pending_stamps[index])
            && item.diagnostics.unreadable_files == 0
        {
            match cache.put(&paths[index], provider, context, stamp, &item) {
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
    sessions
}

pub fn parse_claude_file(path: &Path, root: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut points = Vec::new();
    let mut human_points = Vec::new();
    let mut cwd = None;
    let mut session_id = None;
    let mut version = None;
    let mut current_model = "unknown".to_string();
    for_json_lines(
        path,
        max_line_bytes,
        &mut result.diagnostics,
        |record: ClaudeRecord| {
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
            let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp) else {
                return;
            };
            points.push(ActivityPoint {
                timestamp,
                model: current_model.clone(),
            });
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
        },
    );
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
        points,
        exact_intervals: Vec::new(),
        human_points,
        is_subagent: path
            .components()
            .any(|part| part.as_os_str() == "subagents"),
        approximate_cwd,
        version,
    });
    result
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
    let mut cwd = None;
    let mut metadata_cwd = None;
    let mut session_id = None;
    let mut current_model = "unknown".to_string();
    let mut is_subagent = false;
    for_json_lines(
        path,
        max_line_bytes,
        &mut result.diagnostics,
        |record: CodexRecord| {
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
                }
                _ => {}
            }
        },
    );

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
    if cwd_keys.is_empty() {
        result.diagnostics.skipped_sessions += 1;
        return result;
    }
    let multiple = cwd_keys.len() > 1;
    for cwd_key in cwd_keys {
        let mut points = points_by_cwd.remove(&cwd_key).unwrap_or_default();
        let mut exact_intervals = exact_by_cwd.remove(&cwd_key).unwrap_or_default();
        let human_points = human_by_cwd.remove(&cwd_key).unwrap_or_default();
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
            points,
            exact_intervals,
            human_points,
            is_subagent,
            approximate_cwd,
            version: None,
        });
    }
    result
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

fn for_json_lines<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_line_bytes: usize,
    diagnostics: &mut Diagnostics,
    mut consume: impl FnMut(T),
) {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            diagnostics.unreadable_files += 1;
            diagnostics.warn(format!(
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
                    diagnostics.malformed_lines += 1;
                    diagnostics.warn(format!(
                        "oversized JSONL line skipped: {}:{line_number}",
                        path.display()
                    ));
                    continue;
                }
                match serde_json::from_slice(&line) {
                    Ok(record) => consume(record),
                    Err(_) => {
                        diagnostics.malformed_lines += 1;
                        if diagnostics.malformed_lines <= 20 {
                            diagnostics.warn(format!(
                                "malformed JSONL skipped: {}:{line_number}",
                                path.display()
                            ));
                        }
                    }
                }
            }
            Err(error) => {
                diagnostics.unreadable_files += 1;
                diagnostics.warn(format!(
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
    let oversized = line.len() > maximum;
    let complete = line.ends_with(b"\n");
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
        {
            minimum = Some(minimum.map_or(timestamp, |value: DateTime<Utc>| value.min(timestamp)));
            maximum = Some(maximum.map_or(timestamp, |value: DateTime<Utc>| value.max(timestamp)));
        }
    }
    (minimum, maximum)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn model_validation_rejects_control_text() {
        assert_eq!("claude-test", safe_model("claude-test"));
        assert_eq!("unknown", safe_model("prompt text\n"));
    }

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
    fn claude_meta_messages_are_not_human_evidence() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let path = project.join("session.jsonl");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{}\",\"message\":{{\"content\":\"real\"}}}}\n{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:01:00Z\",\"cwd\":\"{}\",\"isMeta\":true,\"message\":{{\"content\":\"automatic\"}}}}\n",
                project.display(),
                project.display()
            ),
        )
        .unwrap();
        let parsed = parse_claude_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions[0].human_points.len());
    }

    #[test]
    fn codex_model_changes_and_exact_intervals_match_reference() {
        let root = tempdir().unwrap();
        let second = root.path().join("second");
        fs::create_dir(&second).unwrap();
        let path = root.path().join("rollout-test.jsonl");
        let records = [
            format!(
                "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"s\",\"cwd\":\"{}\"}}}}",
                root.path().display()
            ),
            format!(
                "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-a\",\"cwd\":\"{}\"}}}}",
                root.path().display()
            ),
            "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}".into(),
            "{\"timestamp\":\"2026-01-01T00:01:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}".into(),
            format!(
                "{{\"timestamp\":\"2026-01-01T00:02:00Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-b\",\"cwd\":\"{}\"}}}}",
                second.display()
            ),
            "{\"timestamp\":\"2026-01-01T00:02:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}".into(),
            "{\"timestamp\":\"2026-01-01T00:03:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"started_at_ms\":1767225720000,\"completed_at_ms\":1767225780000}}".into(),
        ];
        fs::write(&path, records.join("\n")).unwrap();
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
}
