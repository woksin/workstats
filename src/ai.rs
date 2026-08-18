use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use walkdir::WalkDir;

use crate::cache::{CacheLookup, FileStamp, TranscriptCache, file_stamp};
use crate::model::{
    ActivityPoint, Diagnostics, ExactInterval, RawSession, Session, TokenEvent, TokenUsage,
};
use crate::paths::{PathResolver, lossy_claude_cwd};
use crate::timeutil::{nearest_models, parse_epoch_milliseconds, parse_timestamp};

pub const MAX_JSONL_LINE_BYTES: usize = 8 * 1024 * 1024;

/// A legacy Gemini session is a single JSON document rather than a line per record, so
/// it needs a whole-file bound of its own. Real sessions reach tens of megabytes, hence
/// the far larger limit.
pub const MAX_GEMINI_JSON_BYTES: u64 = 128 * 1024 * 1024;

/// A VS Code chat session is one JSON document too, so the line-bounded discipline the
/// JSONL adapters rely on does not apply and the file needs its own ceiling. The
/// largest session found on the machine this was designed against was 6.4 MB, and a
/// single `chatSessions` directory held 102 of them; this leaves generous headroom
/// while keeping one damaged or runaway file from deciding the process's memory use.
pub const MAX_VSCODE_CHAT_JSON_BYTES: u64 = 16 * 1024 * 1024;

/// The `version` VS Code stamps on its own chat serialization. It has been bumped
/// before — that is why the field exists — and reading a newer layout as if it were
/// this one would report confident nonsense, so a higher version is declined.
const COPILOT_VSCODE_FORMAT_VERSION: u32 = 3;

/// The directory VS Code keeps chat transcripts in, one level below a workspace's
/// storage directory.
const VSCODE_CHAT_SESSION_DIRECTORY: &str = "chatSessions";

/// Nothing waits a week for one turn, so a longer duration is a corrupt field rather
/// than a measurement. The Copilot CLI's subagent durations are bounded the same way.
const MAX_EXACT_DURATION_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;

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

#[derive(Default, Deserialize)]
struct CopilotRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    #[serde(default)]
    data: CopilotData,
}

#[derive(Default, Deserialize)]
struct CopilotData {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    context: Option<CopilotContext>,
    cwd: Option<String>,
    #[serde(rename = "selectedModel")]
    selected_model: Option<String>,
    #[serde(rename = "newModel")]
    new_model: Option<String>,
    model: Option<String>,
    #[serde(
        rename = "durationMs",
        deserialize_with = "deserialize_maybe_number",
        default
    )]
    duration_ms: Option<f64>,
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(rename = "parentAgentTaskId")]
    parent_agent_task_id: Option<String>,
    #[serde(rename = "copilotVersion")]
    copilot_version: Option<String>,
    #[serde(default, rename = "modelMetrics")]
    model_metrics: Option<BTreeMap<String, CopilotModelMetrics>>,
}

#[derive(Deserialize)]
struct CopilotContext {
    cwd: Option<String>,
}

#[derive(Default, Deserialize)]
struct CopilotModelMetrics {
    usage: Option<CopilotUsage>,
}

#[derive(Default, Deserialize)]
struct CopilotUsage {
    #[serde(default, rename = "inputTokens")]
    input_tokens: u64,
    #[serde(default, rename = "outputTokens")]
    output_tokens: u64,
    #[serde(default, rename = "cacheReadTokens")]
    cache_read_tokens: u64,
    #[serde(default, rename = "cacheWriteTokens")]
    cache_write_tokens: u64,
}

/// The structural half of a VS Code Copilot Chat session.
///
/// `message.text`, `response[]`, and `result.metadata.renderedUserMessage` hold the
/// conversation itself, including file excerpts, and they are deliberately absent from
/// this struct: serde skips a field no struct names without materializing its value, so
/// a 6 MB transcript is walked for four fields and the bodies are never read into
/// memory. Adding a field here is therefore a privacy decision, not a parsing one.
#[derive(Default, Deserialize)]
struct VsCodeChatSession {
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    version: Option<f64>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    requests: Vec<VsCodeChatRequest>,
}

#[derive(Default, Deserialize)]
struct VsCodeChatRequest {
    /// Epoch milliseconds: the moment the developer pressed enter.
    #[serde(default, deserialize_with = "deserialize_maybe_number")]
    timestamp: Option<f64>,
    #[serde(default, rename = "modelId")]
    model_id: Option<String>,
    #[serde(default)]
    result: Option<VsCodeChatResult>,
}

#[derive(Default, Deserialize)]
struct VsCodeChatResult {
    #[serde(default)]
    timings: Option<VsCodeChatTimings>,
}

#[derive(Default, Deserialize)]
struct VsCodeChatTimings {
    #[serde(
        default,
        rename = "totalElapsed",
        deserialize_with = "deserialize_maybe_number"
    )]
    total_elapsed: Option<f64>,
}

/// `workspace.json` beside a workspace's `chatSessions` directory, which is the only
/// thing tying a chat session to a place on disk.
#[derive(Default, Deserialize)]
struct VsCodeWorkspace {
    #[serde(default)]
    folder: Option<String>,
    /// A multi-root workspace was never observed on the machine this was designed
    /// against, so this is a tolerant guess: an absent array simply leaves the cwd
    /// approximate, which is the same outcome as not looking.
    #[serde(default)]
    folders: Vec<VsCodeWorkspaceFolder>,
}

#[derive(Default, Deserialize)]
struct VsCodeWorkspaceFolder {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// One row of the Copilot CLI's newer `session-store.db`, restricted to the columns
/// that say *where* a session ran.
///
/// The same database's `turns` table holds every prompt and response body the CLI has
/// seen, with an FTS5 index over them. Nothing here reads it, and the column list in
/// `read_copilot_session_store` is closed on purpose.
#[derive(Clone, Debug, Default)]
pub struct CopilotStoreSession {
    pub cwd: Option<String>,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub host_type: Option<String>,
}

#[derive(Debug, Default)]
pub struct CopilotSessionStore {
    by_id: BTreeMap<String, CopilotStoreSession>,
}

impl CopilotSessionStore {
    fn get(&self, id: &str) -> Option<&CopilotStoreSession> {
        self.by_id.get(id)
    }
}

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

fn unknown_model() -> String {
    "unknown".to_string()
}

fn activity_event() -> String {
    "activity".to_string()
}

fn foreground_role() -> String {
    "foreground".to_string()
}

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
        |_| "claude-v1".to_string(),
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

pub fn discover_gemini_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        let extension = path.extension().and_then(|value| value.to_str());
        matches!(extension, Some("json" | "jsonl"))
            && path
                .components()
                .any(|part| part.as_os_str().eq_ignore_ascii_case("chats"))
    })
}

pub fn discover_copilot_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.file_name()
            .is_some_and(|value| value.eq_ignore_ascii_case("events.jsonl"))
    })
}

/// Finds `<workspace hash>/chatSessions/*.json` under VS Code's `workspaceStorage`.
///
/// The two levels are walked by hand rather than handed to `WalkDir` because
/// `workspaceStorage` holds one directory per workspace — 120 of them on the machine
/// this was designed against, of which ~26 had chat sessions — and each is full of
/// unrelated extension state. A recursive `*.json` walk would read all of it and would
/// also pull other extensions' files into the parser.
pub fn discover_copilot_vscode_files(root: &Path) -> Vec<PathBuf> {
    let Ok(workspaces) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for workspace in workspaces.filter_map(Result::ok) {
        let Ok(files) = fs::read_dir(workspace.path().join(VSCODE_CHAT_SESSION_DIRECTORY)) else {
            continue;
        };
        paths.extend(
            files
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|value| value.eq_ignore_ascii_case("json"))
                }),
        );
    }
    paths.sort();
    paths
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

pub fn read_copilot_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!(
            "GitHub Copilot CLI history not found: {}",
            root.display()
        ));
        return Vec::new();
    }
    // `session-state/` and `session-store.db` are siblings in the CLI's home, so the
    // database is found from the history root rather than through a second flag:
    // `--history copilot=PATH` then keeps both pointing at the same install.
    let store = read_copilot_session_store(&copilot_session_store_path(root), diagnostics);
    load_files(
        discover_copilot_files(root),
        resolver,
        diagnostics,
        cache,
        "copilot",
        |path| copilot_context_fingerprint(&store, path),
        since,
        until,
        |path| parse_copilot_file(path, &store, MAX_JSONL_LINE_BYTES),
    )
}

pub fn read_copilot_vscode_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!(
            "GitHub Copilot Chat history not found: {}",
            root.display()
        ));
        return Vec::new();
    }
    load_files(
        discover_copilot_vscode_files(root),
        resolver,
        diagnostics,
        cache,
        "copilot-vscode",
        copilot_vscode_context_fingerprint,
        since,
        until,
        |path| parse_copilot_vscode_file(path, MAX_VSCODE_CHAT_JSON_BYTES),
    )
}

pub fn read_opencode_sessions_indexed(
    database: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !database.is_file() {
        diagnostics.warn(format!(
            "OpenCode history not found: {}",
            database.display()
        ));
        return Vec::new();
    }
    let wal_path = PathBuf::from(format!("{}-wal", database.to_string_lossy()));
    let context = format!("opencode-v1:{}", crate::cache::file_context(&wal_path));
    load_files(
        vec![database.to_path_buf()],
        resolver,
        diagnostics,
        cache,
        "opencode",
        |_| context.clone(),
        since,
        until,
        parse_opencode_database,
    )
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
        for_json_lines(
            path,
            max_line_bytes,
            &mut result.diagnostics,
            |record: GeminiRecord| {
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
            },
        );
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

/// The event log decides everything about a session; the store only fills gaps it left.
/// See `read_copilot_session_store` for why the database is read at all and what it is
/// never allowed to read.
pub fn parse_copilot_file(
    path: &Path,
    store: &CopilotSessionStore,
    max_line_bytes: usize,
) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut session_id = None;
    let mut version = None;
    let mut cwd = None;
    let mut current_model = "unknown".to_string();
    let mut points_by_cwd: BTreeMap<Option<String>, Vec<ActivityPoint>> = BTreeMap::new();
    let mut human_by_cwd: BTreeMap<Option<String>, Vec<ActivityPoint>> = BTreeMap::new();
    let mut token_events_by_cwd: BTreeMap<Option<String>, Vec<TokenEvent>> = BTreeMap::new();
    let mut subagent_intervals: Vec<(String, String, ExactInterval)> = Vec::new();
    for_json_lines(
        path,
        max_line_bytes,
        &mut result.diagnostics,
        |record: CopilotRecord| {
            let Some(record_type) = record.record_type.as_deref() else {
                return;
            };
            if record_type == "session.start" {
                session_id = record.data.session_id.or(session_id.take());
                cwd = record
                    .data
                    .context
                    .and_then(|context| context.cwd)
                    .or(cwd.take());
                if let Some(model) = record.data.selected_model {
                    current_model = safe_model(&model);
                }
                version = record.data.copilot_version;
                return;
            }
            if record_type == "session.context_changed" {
                cwd = record.data.cwd.or(cwd.take());
            }
            if record_type == "session.model_change"
                && let Some(model) = record.data.new_model
            {
                current_model = safe_model(&model);
            }
            let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp) else {
                return;
            };
            if record_type == "session.shutdown"
                && let Some(metrics) = record.data.model_metrics
            {
                for (model_name, entry) in metrics {
                    let Some(usage) = entry.usage else {
                        continue;
                    };
                    // `cacheReadTokens` is a subset of `inputTokens`, not additional to it.
                    let usage = TokenUsage {
                        input_tokens: usage.input_tokens.saturating_sub(usage.cache_read_tokens),
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_write_tokens,
                    };
                    if usage.is_zero() {
                        continue;
                    }
                    token_events_by_cwd
                        .entry(cwd.clone())
                        .or_default()
                        .push(TokenEvent {
                            timestamp,
                            model: safe_model(&model_name),
                            usage,
                        });
                }
                return;
            }
            if record_type == "subagent.completed"
                && let Some(duration_ms) = record.data.duration_ms
                && duration_ms.is_finite()
                && duration_ms > 0.0
                && duration_ms <= 7.0 * 24.0 * 60.0 * 60.0 * 1000.0
            {
                let duration = chrono::Duration::milliseconds(duration_ms.round() as i64);
                let model = record
                    .data
                    .model
                    .as_deref()
                    .map(safe_model)
                    .unwrap_or_else(|| current_model.clone());
                subagent_intervals.push((
                    record
                        .data
                        .tool_call_id
                        .unwrap_or_else(|| format!("subagent-{}", timestamp.timestamp_micros())),
                    cwd.clone().unwrap_or_default(),
                    ExactInterval {
                        start: timestamp - duration,
                        end: timestamp,
                        model,
                    },
                ));
                return;
            }
            let is_agent_event =
                record.agent_id.is_some() || record.data.parent_agent_task_id.is_some();
            if is_agent_event {
                return;
            }
            // Deliberately below the subagent guard: a subagent record names its own
            // model, and applying it here used to redirect every following foreground
            // point to a model the human never selected.
            if let Some(model) = record.data.model.as_deref() {
                current_model = safe_model(model);
            }
            if matches!(
                record_type,
                "user.message"
                    | "assistant.message"
                    | "assistant.turn_start"
                    | "assistant.turn_end"
                    | "tool.execution_start"
                    | "tool.execution_complete"
            ) {
                let point = ActivityPoint {
                    timestamp,
                    model: current_model.clone(),
                };
                points_by_cwd
                    .entry(cwd.clone())
                    .or_default()
                    .push(point.clone());
                if record_type == "user.message" {
                    human_by_cwd.entry(cwd.clone()).or_default().push(point);
                }
            }
        },
    );
    let directory_id = copilot_session_directory(path);
    let base_id = session_id.unwrap_or_else(|| {
        if directory_id.is_empty() {
            file_stem(path)
        } else {
            directory_id.clone()
        }
    });
    // Keyed by the session directory name rather than by the id inside the file,
    // because `copilot_context_fingerprint` has to reach the same row from the path
    // alone — a cached parse that outlives a change to the row it used is the Gemini
    // `.project_root` bug. The CLI names the directory after the session UUID, so on a
    // real install the two agree.
    let store_entry = store.get(&directory_id);
    let store_cwd = store_entry
        .and_then(|entry| entry.cwd.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    // A `session.context_changed` after the last activity event leaves the shutdown
    // usage under a cwd that has no points, so the sessions come from the union of the
    // maps rather than from the activity map alone — as `parse_codex_file` already does.
    let mut cwd_keys = BTreeSet::new();
    cwd_keys.extend(points_by_cwd.keys().cloned());
    cwd_keys.extend(human_by_cwd.keys().cloned());
    cwd_keys.extend(token_events_by_cwd.keys().cloned());
    let multiple = cwd_keys.len() > 1;
    for cwd_key in cwd_keys {
        let points = points_by_cwd.remove(&cwd_key).unwrap_or_default();
        let human_points = human_by_cwd.remove(&cwd_key).unwrap_or_default();
        let token_events = token_events_by_cwd.remove(&cwd_key).unwrap_or_default();
        if points.is_empty() && token_events.is_empty() {
            continue;
        }
        // A cwd the event log recorded is the directory the CLI actually ran in, so the
        // store never overrides it — only fills its absence.
        let resolved = cwd_key.or_else(|| store_cwd.clone());
        let approximate_cwd = resolved.is_none();
        let resolved_cwd = resolved
            .unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned());
        result.sessions.push(RawSession {
            provider: "copilot".to_string(),
            session_id: if multiple {
                format!("{base_id}:{resolved_cwd}")
            } else {
                base_id.clone()
            },
            source_file: path.to_path_buf(),
            cwd: resolved_cwd,
            points,
            exact_intervals: Vec::new(),
            human_points,
            token_events,
            is_subagent: false,
            approximate_cwd,
            version: version.clone(),
        });
    }
    for (subagent_id, subagent_cwd, interval) in subagent_intervals {
        let resolved = Some(subagent_cwd)
            .filter(|value| !value.is_empty())
            .or_else(|| store_cwd.clone());
        let approximate_cwd = resolved.is_none();
        result.sessions.push(RawSession {
            provider: "copilot".to_string(),
            session_id: format!("{base_id}:subagent:{subagent_id}"),
            source_file: path.to_path_buf(),
            cwd: resolved
                .unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned()),
            points: Vec::new(),
            exact_intervals: vec![interval],
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: true,
            approximate_cwd,
            version: version.clone(),
        });
    }
    report_copilot_repository_disagreement(&mut result, store_entry, &base_id, path);
    if result.sessions.is_empty() {
        result.diagnostics.skipped_sessions += 1;
    }
    result
}

/// `sessions.repository` is a hint, never a verdict: on the machine this was designed
/// against, one row in seven named `Cratis/Chronicle` for a session that ran in
/// `.../cratis/Arc`. The working directory decides where the session is reported, and
/// the disagreement is said out loud so a wrong slug is visible rather than silently
/// preferred or silently dropped.
fn report_copilot_repository_disagreement(
    result: &mut ParsedFile,
    entry: Option<&CopilotStoreSession>,
    session_id: &str,
    path: &Path,
) {
    let Some(entry) = entry else {
        return;
    };
    // Only a GitHub-hosted session carries an `owner/repo` slug; anything else is a
    // string this comparison has no business interpreting.
    let hosted_on_github = entry
        .host_type
        .as_deref()
        .is_none_or(|value| value.eq_ignore_ascii_case("github"));
    if !hosted_on_github {
        return;
    }
    let Some(repository) = entry
        .repository
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(cwd) = result
        .sessions
        .iter()
        .find(|session| !session.approximate_cwd)
        .map(|session| session.cwd.as_str())
    else {
        return;
    };
    if repository_matches_directory(repository, cwd) {
        return;
    }
    let branch = entry
        .branch
        .as_deref()
        .map(|branch| format!(" on branch {branch}"))
        .unwrap_or_default();
    result.diagnostics.warn(format!(
        "GitHub Copilot session {session_id} records repository {repository}{branch} but ran in {cwd}; the working directory decides ({})",
        path.display()
    ));
}

/// Compares the name half of an `owner/repo` slug with the last component of the
/// working directory. A clone legitimately sits in a differently named directory, so
/// this only decides whether a disagreement is worth reporting — never which side wins.
fn repository_matches_directory(repository: &str, cwd: &str) -> bool {
    let name = repository.rsplit('/').next().unwrap_or(repository);
    Path::new(cwd)
        .file_name()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(name))
}

fn copilot_session_directory(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn copilot_session_store_path(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("session-store.db")
}

/// A session's working directory can come from the store, so a change to the row has to
/// invalidate the cached parse of that session — the same reason the Gemini fingerprint
/// follows `.project_root`. Bumped to v3 because a v2 entry was parsed without the
/// store and may hold an approximate cwd the store can now resolve.
fn copilot_context_fingerprint(store: &CopilotSessionStore, path: &Path) -> String {
    match store.get(&copilot_session_directory(path)) {
        Some(entry) => format!(
            "copilot-v3:{}:{}:{}",
            entry.cwd.as_deref().unwrap_or_default(),
            entry.repository.as_deref().unwrap_or_default(),
            entry.branch.as_deref().unwrap_or_default()
        ),
        None => "copilot-v3:none".to_string(),
    }
}

/// Reads the Copilot CLI's SQLite session store for working-directory and repository
/// attribution only.
///
/// The column list is closed on purpose. `turns` holds every prompt and response body
/// the CLI has seen and `search_index*` is an FTS5 index over them; selecting from
/// either would pull message text into the process, which the privacy boundary forbids.
/// The connection is opened read-only for the same reason the Codex and OpenCode
/// readers are — this tool must never be able to alter another program's state.
///
/// The store is a supplement, not a replacement: it held 7 sessions where
/// `session-state/` held 19, because it does not backfill. It therefore adds nothing
/// the event log already knows, and creates no session of its own — without reading
/// `turns` there are no timestamps to place one in time with.
pub fn read_copilot_session_store(
    path: &Path,
    diagnostics: &mut Diagnostics,
) -> CopilotSessionStore {
    if !path.is_file() {
        return CopilotSessionStore::default();
    }
    let result = (|| -> rusqlite::Result<CopilotSessionStore> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if !sqlite_table_exists(&connection, "sessions")? {
            return Ok(CopilotSessionStore::default());
        }
        let columns = sqlite_columns(&connection, "sessions")?;
        if !columns.contains("id") {
            return Ok(CopilotSessionStore::default());
        }
        let column = |name: &str| {
            if columns.contains(name) {
                format!("\"{name}\"")
            } else {
                "NULL".to_string()
            }
        };
        let query = format!(
            "SELECT \"id\", {}, {}, {}, {} FROM sessions",
            column("cwd"),
            column("repository"),
            column("branch"),
            column("host_type")
        );
        let mut statement = connection.prepare(&query)?;
        let mut rows = statement.query([])?;
        let mut store = CopilotSessionStore::default();
        while let Some(row) = rows.next()? {
            // One unreadable cell costs one row, as in the OpenCode reader; a session
            // without an id cannot be joined to anything anyway.
            let Some(id) = sqlite_text(row, 0).filter(|value| !value.is_empty()) else {
                continue;
            };
            store.by_id.insert(
                id,
                CopilotStoreSession {
                    cwd: sqlite_text(row, 1),
                    repository: sqlite_text(row, 2),
                    branch: sqlite_text(row, 3),
                    host_type: sqlite_text(row, 4),
                },
            );
        }
        Ok(store)
    })();
    match result {
        Ok(store) => store,
        Err(error) => {
            // A missing or newer store is not a broken run: the event log stays the
            // primary source and simply keeps whatever it knew on its own.
            diagnostics.warn(format!(
                "GitHub Copilot session store ignored: {}: {error}",
                path.display()
            ));
            CopilotSessionStore::default()
        }
    }
}

/// Reads one VS Code Copilot Chat session.
///
/// Two things make this unlike the JSONL adapters. The file is a single JSON document,
/// so the line bound the others rely on cannot apply and the whole-file cap is what
/// keeps memory bounded. And VS Code records how long each turn actually took, so agent
/// time here is measured rather than estimated: the `--gap-cap` heuristic every other
/// adapter needs is not used, and must not be layered on top of a real duration.
pub fn parse_copilot_vscode_file(path: &Path, max_bytes: u64) -> ParsedFile {
    let mut result = ParsedFile::default();
    let document = File::open(path)
        .map_err(anyhow::Error::from)
        .and_then(|file| {
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if size > max_bytes {
                anyhow::bail!("chat session larger than {max_bytes} bytes");
            }
            // The BufReader is not cosmetic: serde_json's `IoRead` issues one syscall
            // per byte, which measured ~90x slower on a large session.
            serde_json::from_reader::<_, VsCodeChatSession>(BufReader::with_capacity(
                128 * 1024,
                file,
            ))
            .map_err(Into::into)
        });
    let document = match document {
        Ok(document) => document,
        Err(error) => {
            // Best-effort by design: VS Code owns this format and changes it. A session
            // that cannot be read is reported and skipped, never guessed at.
            result.diagnostics.unreadable_files += 1;
            result.diagnostics.warn(format!(
                "invalid Copilot Chat session skipped: {}: {error}",
                path.display()
            ));
            return result;
        }
    };
    if document
        .version
        .is_some_and(|value| value > f64::from(COPILOT_VSCODE_FORMAT_VERSION))
    {
        result.diagnostics.skipped_sessions += 1;
        result.diagnostics.warn(format!(
            "Copilot Chat session format is newer than v{COPILOT_VSCODE_FORMAT_VERSION}, skipped: {}",
            path.display()
        ));
        return result;
    }

    let mut points = Vec::new();
    let mut human_points = Vec::new();
    let mut exact_intervals = Vec::new();
    let mut current_model = "unknown".to_string();
    for request in document.requests {
        if let Some(model) = request.model_id.as_deref() {
            current_model = copilot_vscode_model(model);
        }
        let Some(timestamp) = request.timestamp.and_then(parse_epoch_milliseconds) else {
            continue;
        };
        let point = ActivityPoint {
            timestamp,
            model: current_model.clone(),
        };
        // The instant the developer submitted a prompt, which is the same human
        // evidence the other adapters take from a user message.
        human_points.push(point.clone());
        let elapsed = request
            .result
            .and_then(|value| value.timings)
            .and_then(|timings| timings.total_elapsed);
        match exact_vscode_interval(timestamp, elapsed, &current_model) {
            // A measured turn needs no activity point: a point would add a gap-capped
            // range on top of the exact interval and stretch a 30-second turn to the
            // gap cap.
            Some(interval) => exact_intervals.push(interval),
            // A cancelled or still-running turn has no duration to trust, so it falls
            // back to the ordinary point timeline rather than disappearing.
            None => points.push(point),
        }
    }
    if points.is_empty() && exact_intervals.is_empty() && human_points.is_empty() {
        result.diagnostics.skipped_sessions += 1;
        return result;
    }

    let workspace =
        copilot_vscode_workspace_file(path).and_then(|file| vscode_workspace_folder(&file));
    let approximate_cwd = workspace.is_none();
    let cwd =
        workspace.unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned());
    let session_id = document
        .session_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| file_stem(path));
    // The workspace-storage directory is part of the id because the aggregator keys a
    // session by (provider, id) alone, and storage directories get copied between
    // machines and workspaces — two files carrying one id would otherwise collapse into
    // a single session.
    let workspace_key = copilot_vscode_workspace_key(path);
    result.sessions.push(RawSession {
        provider: "copilot-vscode".to_string(),
        session_id: if workspace_key.is_empty() {
            session_id
        } else {
            format!("{session_id}:{workspace_key}")
        },
        source_file: path.to_path_buf(),
        cwd,
        points,
        exact_intervals,
        human_points,
        // Copilot Chat records a premium-request multiplier ("GPT-5 mini • 1x"), never
        // token counts, so there is nothing honest to report here.
        token_events: Vec::new(),
        is_subagent: false,
        approximate_cwd,
        version: document
            .version
            .map(|value| format!("vscode-chat-v{}", value.round() as i64)),
    });
    result
}

/// `result.timings.totalElapsed` is the measured duration of one turn in milliseconds.
fn exact_vscode_interval(
    start: DateTime<Utc>,
    elapsed_milliseconds: Option<f64>,
    model: &str,
) -> Option<ExactInterval> {
    let elapsed = elapsed_milliseconds?;
    if !elapsed.is_finite() || elapsed <= 0.0 || elapsed > MAX_EXACT_DURATION_MS {
        return None;
    }
    // `checked_add_signed` rather than `+`: chrono panics when a timestamp from a file
    // this tool does not control plus a duration leaves the representable range.
    let end = start.checked_add_signed(chrono::Duration::milliseconds(elapsed.round() as i64))?;
    Some(ExactInterval {
        start,
        end,
        model: model.to_string(),
    })
}

/// `copilot/gpt-5-mini` names the vendor twice once the provider column already says
/// Copilot, so the prefix is dropped — the CLI adapter reports the same models
/// unprefixed, and a model column that spells one product two ways cannot be grouped.
fn copilot_vscode_model(value: &str) -> String {
    safe_model(value.strip_prefix("copilot/").unwrap_or(value))
}

/// `chatSessions/` sits beside `workspace.json` inside one workspace-storage directory.
fn copilot_vscode_workspace_file(path: &Path) -> Option<PathBuf> {
    Some(path.parent()?.parent()?.join("workspace.json"))
}

fn copilot_vscode_workspace_key(path: &Path) -> String {
    path.parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A chat session takes its working directory from an out-of-band `workspace.json`, so
/// the fingerprint has to follow that file: a constant one kept a workspace pinned to a
/// stale directory for as long as the transcript itself was untouched (the Gemini
/// `.project_root` bug).
fn copilot_vscode_context_fingerprint(path: &Path) -> String {
    match copilot_vscode_workspace_file(path) {
        Some(file) => format!("copilot-vscode-v1:{}", crate::cache::file_context(&file)),
        None => "copilot-vscode-v1:none".to_string(),
    }
}

/// Maps a workspace-storage directory to the folder it belongs to. The file holds a
/// handful of bytes — `{"folder": "file:///…"}` — but it is bounded anyway, since
/// nothing about it is under this tool's control.
fn vscode_workspace_folder(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let record: VsCodeWorkspace = serde_json::from_slice(&bytes).ok()?;
    record
        .folder
        .or_else(|| {
            record
                .folders
                .into_iter()
                .find_map(|folder| folder.uri.or(folder.path))
        })
        .as_deref()
        .and_then(file_url_to_path)
}

/// `file:///Volumes/sourcecode/repos/example` is a URL, so it has to be decoded before
/// it names anything on disk. Only a local `file://` URL with an empty authority maps
/// to a path: a UNC share or a `vscode-remote://` URI names a place this machine cannot
/// measure, so it is declined and the session keeps an approximate cwd.
fn file_url_to_path(value: &str) -> Option<String> {
    let rest = value.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest);
    // `file:///c%3A/Users/…` decodes to `/c:/Users/…`; that leading separator belongs to
    // the URL, not to the Windows path.
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(decoded[1..].to_string());
    }
    Some(decoded)
}

/// Decodes `%XX` escapes over bytes rather than characters: a `%` followed by a
/// multi-byte character would panic a `str` slice, and a percent-escape can encode one
/// byte of a UTF-8 sequence, which only reassembles correctly as bytes.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            high.zip(low)
                .and_then(|(high, low)| u8::try_from(high * 16 + low).ok())
        } else {
            None
        };
        match escape {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub fn parse_event_file(path: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    type EventKey = (String, String, String, bool);
    let mut sessions: BTreeMap<EventKey, RawSession> = BTreeMap::new();
    let mut sensitive_records = 0_u64;
    for_json_lines(
        path,
        max_line_bytes,
        &mut result.diagnostics,
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
                points: Vec::new(),
                exact_intervals: Vec::new(),
                human_points: Vec::new(),
                token_events: Vec::new(),
                is_subagent,
                approximate_cwd: false,
                version: Some("workstats-events-v1".to_string()),
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

#[derive(Default)]
struct OpenCodeSession {
    id: String,
    cwd: String,
    model: String,
    version: Option<String>,
    is_subagent: bool,
    points: Vec<ActivityPoint>,
    human_points: Vec<ActivityPoint>,
}

pub fn parse_opencode_database(path: &Path) -> ParsedFile {
    let mut result = ParsedFile::default();
    let parsed = (|| -> rusqlite::Result<(Vec<RawSession>, u64)> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if !sqlite_table_exists(&connection, "session")? {
            return Ok((Vec::new(), 0));
        }
        let columns = sqlite_columns(&connection, "session")?;
        let expression = |name: &str, fallback: &str| {
            if columns.contains(name) {
                format!("\"{name}\"")
            } else {
                fallback.to_string()
            }
        };
        let query = format!(
            "SELECT {}, {}, {}, {}, {} FROM session",
            expression("id", "''"),
            expression("directory", "''"),
            expression("parent_id", "NULL"),
            expression("version", "NULL"),
            expression("model", "NULL")
        );
        let mut statement = connection.prepare(&query)?;
        let mut rows = statement.query([])?;
        let mut sessions: BTreeMap<String, OpenCodeSession> = BTreeMap::new();
        let mut skipped_rows = 0_u64;
        while let Some(row) = rows.next()? {
            // A NULL or unexpectedly typed cell costs one row, never the whole database:
            // `parent_id` below already read tolerantly, and a strict read here threw
            // away every OpenCode session over a single bad cell.
            let (Some(id), Some(cwd)) = (sqlite_text(row, 0), sqlite_text(row, 1)) else {
                skipped_rows += 1;
                continue;
            };
            if id.is_empty() || cwd.is_empty() {
                continue;
            }
            let parent: Option<String> = row.get(2).unwrap_or(None);
            let version: Option<String> = row.get(3).unwrap_or(None);
            let encoded_model: Option<String> = row.get(4).unwrap_or(None);
            sessions.insert(
                id.clone(),
                OpenCodeSession {
                    id,
                    cwd,
                    model: encoded_model
                        .as_deref()
                        .map(json_model)
                        .unwrap_or_else(unknown_model),
                    version,
                    is_subagent: parent.is_some(),
                    ..OpenCodeSession::default()
                },
            );
        }

        let mut current_session_ids = BTreeSet::new();
        if sqlite_table_exists(&connection, "session_message")? {
            let columns = sqlite_columns(&connection, "session_message")?;
            if columns.contains("session_id")
                && columns.contains("type")
                && columns.contains("time_created")
            {
                let data = if columns.contains("data") {
                    "data"
                } else {
                    "NULL"
                };
                let query = format!(
                    "SELECT session_id, type, time_created, {data} FROM session_message ORDER BY time_created"
                );
                let mut statement = connection.prepare(&query)?;
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    let (Some(session_id), Some(message_type), Some(milliseconds)) = (
                        sqlite_text(row, 0),
                        sqlite_text(row, 1),
                        sqlite_number(row, 2),
                    ) else {
                        skipped_rows += 1;
                        continue;
                    };
                    let data: Option<String> = row.get(3).unwrap_or(None);
                    let Some(session) = sessions.get_mut(&session_id) else {
                        continue;
                    };
                    let Some(timestamp) = parse_epoch_milliseconds(milliseconds) else {
                        continue;
                    };
                    current_session_ids.insert(session_id);
                    let model = data
                        .as_deref()
                        .map(json_model)
                        .filter(|value| value != "unknown")
                        .unwrap_or_else(|| session.model.clone());
                    let point = ActivityPoint { timestamp, model };
                    session.points.push(point.clone());
                    if message_type == "user" && !session.is_subagent {
                        session.human_points.push(point);
                    }
                }
            }
        }

        if sqlite_table_exists(&connection, "message")? {
            let columns = sqlite_columns(&connection, "message")?;
            if columns.contains("session_id")
                && columns.contains("time_created")
                && columns.contains("data")
            {
                let mut statement = connection.prepare(
                    "SELECT session_id, time_created, data FROM message ORDER BY time_created",
                )?;
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    // The session check comes before the other two cells on
                    // purpose: this table is the legacy mirror of
                    // `session_message`, so rows already covered there are
                    // skipped by design and a NULL in one of them is not a
                    // damaged database worth warning about.
                    let Some(session_id) = sqlite_text(row, 0) else {
                        skipped_rows += 1;
                        continue;
                    };
                    if current_session_ids.contains(&session_id) {
                        continue;
                    }
                    let (Some(milliseconds), Some(data)) =
                        (sqlite_number(row, 1), sqlite_text(row, 2))
                    else {
                        skipped_rows += 1;
                        continue;
                    };
                    let Some(session) = sessions.get_mut(&session_id) else {
                        continue;
                    };
                    let Some(timestamp) = parse_epoch_milliseconds(milliseconds) else {
                        continue;
                    };
                    let value: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
                    let message_type = value
                        .get("role")
                        .or_else(|| value.get("type"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let parsed_model = json_model(&data);
                    let model = if parsed_model == "unknown" {
                        session.model.clone()
                    } else {
                        parsed_model
                    };
                    let point = ActivityPoint { timestamp, model };
                    session.points.push(point.clone());
                    if message_type == "user" && !session.is_subagent {
                        session.human_points.push(point);
                    }
                }
            }
        }

        Ok((
            sessions
                .into_values()
                .filter(|session| !session.points.is_empty())
                .map(|session| RawSession {
                    provider: "opencode".to_string(),
                    session_id: session.id,
                    source_file: path.to_path_buf(),
                    cwd: session.cwd,
                    points: session.points,
                    exact_intervals: Vec::new(),
                    human_points: session.human_points,
                    token_events: Vec::new(),
                    is_subagent: session.is_subagent,
                    approximate_cwd: false,
                    version: session.version,
                })
                .collect(),
            skipped_rows,
        ))
    })();
    match parsed {
        Ok((sessions, skipped_rows)) => {
            result.sessions = sessions;
            if skipped_rows > 0 {
                result.diagnostics.malformed_lines += skipped_rows;
                result.diagnostics.warn(format!(
                    "{skipped_rows} unreadable OpenCode row(s) skipped: {}",
                    path.display()
                ));
            }
            if result.sessions.is_empty() {
                result.diagnostics.skipped_sessions += 1;
            }
        }
        Err(error) => {
            result.diagnostics.unreadable_files += 1;
            result.diagnostics.warn(format!(
                "OpenCode database ignored: {}: {error}",
                path.display()
            ));
        }
    }
    result
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

fn json_model(encoded: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(encoded) else {
        return safe_model(encoded);
    };
    let provider = value
        .get("providerID")
        .or_else(|| value.get("provider_id"))
        .and_then(serde_json::Value::as_str);
    let model = value
        .get("modelID")
        .or_else(|| value.get("model_id"))
        .or_else(|| value.get("model"))
        .or_else(|| value.get("id"))
        .and_then(serde_json::Value::as_str);
    match (provider, model) {
        (Some(provider), Some(model)) => safe_model(&format!("{provider}/{model}")),
        (_, Some(model)) => safe_model(model),
        _ => "unknown".to_string(),
    }
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
    sessions
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
                        (id, request) => {
                            Some((id.unwrap_or_default(), request.unwrap_or_default()))
                        }
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
        token_events,
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
    let mut token_events_by_cwd: BTreeMap<Option<String>, Vec<TokenEvent>> = BTreeMap::new();
    let mut cwd = None;
    let mut metadata_cwd = None;
    let mut session_id = None;
    let mut current_model = "unknown".to_string();
    let mut is_subagent = false;
    let mut previous_token_total = None;
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
                    if record.record_type.as_deref() == Some("event_msg")
                        && let Some(timestamp) =
                            record.timestamp.as_deref().and_then(parse_timestamp)
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
            points,
            exact_intervals,
            human_points,
            token_events,
            is_subagent,
            approximate_cwd,
            version: None,
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
    fn copilot_events_track_foreground_work_and_exact_subagents() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "type": "session.start",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"sessionId": "copilot-session", "selectedModel": "gpt-test", "context": {"cwd": root.path()}}
            }),
            serde_json::json!({
                "type": "user.message",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"content": "not parsed"}
            }),
            serde_json::json!({
                "type": "assistant.message",
                "timestamp": "2026-01-01T00:01:00Z",
                "data": {"model": "gpt-test", "content": "not parsed"}
            }),
            serde_json::json!({
                "type": "subagent.completed",
                "timestamp": "2026-01-01T00:02:00Z",
                "data": {"toolCallId": "agent-one", "model": "gpt-test", "durationMs": 30000}
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

        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.sessions.len());
        let foreground = parsed
            .sessions
            .iter()
            .find(|session| !session.is_subagent)
            .unwrap();
        assert_eq!(1, foreground.human_points.len());
        let subagent = parsed
            .sessions
            .iter()
            .find(|session| session.is_subagent)
            .unwrap();
        assert_eq!(
            30.0,
            (subagent.exact_intervals[0].end - subagent.exact_intervals[0].start).num_seconds()
                as f64
        );
    }

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
    fn opencode_database_is_read_structurally_and_read_only() {
        let root = tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    parent_id TEXT,
                    version TEXT,
                    model TEXT
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    type TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session(id, directory, version, model) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "session-one",
                    root.path().to_string_lossy(),
                    "test",
                    r#"{"providerID":"openai","id":"gpt-test"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_message(id, session_id, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["message-one", "session-one", "user", 1_767_225_600_000_i64, "{}"],
            )
            .unwrap();
        drop(connection);

        let parsed = parse_opencode_database(&path);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(1, parsed.sessions[0].human_points.len());
        assert_eq!("openai/gpt-test", parsed.sessions[0].points[0].model);
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
    fn copilot_shutdown_model_metrics_become_token_events() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "type": "session.start",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"sessionId": "copilot-session", "selectedModel": "gpt-test", "context": {"cwd": root.path()}}
            }),
            serde_json::json!({
                "type": "user.message",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"content": "not parsed"}
            }),
            serde_json::json!({
                "type": "session.shutdown",
                "timestamp": "2026-01-01T00:05:00Z",
                "data": {
                    "modelMetrics": {
                        "gpt-test": {
                            "usage": {
                                "inputTokens": 100,
                                "outputTokens": 20,
                                "cacheReadTokens": 5,
                                "cacheWriteTokens": 1
                            }
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

        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        let foreground = parsed
            .sessions
            .iter()
            .find(|session| !session.is_subagent)
            .unwrap();
        assert_eq!(1, foreground.token_events.len());
        let event = &foreground.token_events[0];
        assert_eq!("gpt-test", event.model);
        assert_eq!(95, event.usage.input_tokens);
        assert_eq!(20, event.usage.output_tokens);
        assert_eq!(5, event.usage.cache_read_tokens);
        assert_eq!(1, event.usage.cache_creation_tokens);
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
    fn file_time_range_covers_token_events_after_the_last_activity_point() {
        let session = RawSession {
            provider: "copilot".to_string(),
            session_id: "session".to_string(),
            source_file: PathBuf::from("events.jsonl"),
            cwd: "/tmp/repo".to_string(),
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
        };
        let (minimum, maximum) = file_time_range(std::slice::from_ref(&session));
        assert_eq!(parse_timestamp("2026-01-01T23:00:00Z"), minimum);
        assert_eq!(parse_timestamp("2026-01-02T00:30:00Z"), maximum);
    }

    #[test]
    fn copilot_usage_survives_a_context_change_after_the_last_activity() {
        let root = tempdir().unwrap();
        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "type": "session.start",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"sessionId": "copilot-session", "selectedModel": "gpt-test", "context": {"cwd": root.path()}}
            }),
            serde_json::json!({
                "type": "user.message",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {}
            }),
            serde_json::json!({
                "type": "session.context_changed",
                "timestamp": "2026-01-01T00:04:00Z",
                "data": {"cwd": other}
            }),
            serde_json::json!({
                "type": "session.shutdown",
                "timestamp": "2026-01-01T00:05:00Z",
                "data": {
                    "modelMetrics": {
                        "gpt-test": {"usage": {"inputTokens": 100, "outputTokens": 20}}
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

        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.sessions.len());
        let usage_session = parsed
            .sessions
            .iter()
            .find(|session| !session.token_events.is_empty())
            .unwrap();
        assert!(usage_session.points.is_empty());
        assert_eq!(other.to_string_lossy(), usage_session.cwd);
        assert_eq!(120, usage_session.token_events[0].usage.total());
    }

    #[test]
    fn copilot_subagent_model_does_not_replace_the_foreground_model() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let records = [
            serde_json::json!({
                "type": "session.start",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {"sessionId": "copilot-session", "selectedModel": "gpt-foreground", "context": {"cwd": root.path()}}
            }),
            serde_json::json!({
                "type": "user.message",
                "timestamp": "2026-01-01T00:00:00Z",
                "data": {}
            }),
            serde_json::json!({
                "type": "assistant.message",
                "timestamp": "2026-01-01T00:00:30Z",
                "agentId": "agent-one",
                "data": {"model": "gpt-subagent"}
            }),
            serde_json::json!({
                "type": "assistant.message",
                "timestamp": "2026-01-01T00:01:00Z",
                "data": {}
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

        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(2, parsed.sessions[0].points.len());
        assert!(
            parsed.sessions[0]
                .points
                .iter()
                .all(|point| point.model == "gpt-foreground")
        );
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
    fn opencode_skips_only_the_rows_it_cannot_read() {
        let root = tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT,
                    parent_id TEXT,
                    version TEXT,
                    model TEXT
                );
                CREATE TABLE session_message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    type TEXT,
                    time_created REAL,
                    data TEXT
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session(id, directory, version, model) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "session-one",
                    root.path().to_string_lossy(),
                    "test",
                    r#"{"providerID":"openai","id":"gpt-test"}"#
                ],
            )
            .unwrap();
        // A NULL directory used to abort the read of every other session as well.
        connection
            .execute(
                "INSERT INTO session(id, directory) VALUES (?1, NULL)",
                rusqlite::params!["session-two"],
            )
            .unwrap();
        // OpenCode declares `time_created` NUMERIC, and SQLite may hand it back as REAL.
        connection
            .execute(
                "INSERT INTO session_message(id, session_id, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "message-one",
                    "session-one",
                    "user",
                    1_767_225_600_000.0_f64,
                    "{}"
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_message(id, session_id, type, time_created, data) VALUES (?1, ?2, NULL, ?3, ?4)",
                rusqlite::params!["message-two", "session-one", 1_767_225_660_000.0_f64, "{}"],
            )
            .unwrap();
        drop(connection);

        let parsed = parse_opencode_database(&path);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(1, parsed.sessions[0].points.len());
        assert_eq!(1, parsed.sessions[0].human_points.len());
        assert_eq!(2, parsed.diagnostics.malformed_lines);
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

    /// Builds `<root>/<workspace>/chatSessions/<name>.json` and the `workspace.json`
    /// beside it, which is the whole layout the VS Code adapter depends on.
    fn vscode_chat_session(
        root: &Path,
        workspace: &str,
        name: &str,
        folder: Option<&str>,
        document: &serde_json::Value,
    ) -> PathBuf {
        let storage = root.join(workspace);
        let sessions = storage.join("chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        if let Some(folder) = folder {
            fs::write(
                storage.join("workspace.json"),
                serde_json::json!({ "folder": folder }).to_string(),
            )
            .unwrap();
        }
        let path = sessions.join(format!("{name}.json"));
        fs::write(&path, document.to_string()).unwrap();
        path
    }

    /// Renders a directory the way VS Code writes it into `workspace.json`: a URL, not a
    /// path. Interpolating a `Path` into `file:///{}` yields
    /// `file:///C:\Users\…\repos/my example` on Windows — backslashes, a bare drive
    /// colon, an unescaped space — which is no file URL at all, so the parser declines it
    /// and the test proves the fallback instead of what it claims to test.
    fn vscode_folder_url(path: &Path) -> String {
        let text = path
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20");
        let bytes = text.as_bytes();
        // A drive letter is not the root of a URL path: VS Code writes `C:/…` as
        // `/c%3A/…`, lowercased and with the colon escaped.
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return format!(
                "file:///{}%3A{}",
                text[..1].to_ascii_lowercase(),
                &text[2..]
            );
        }
        format!("file://{text}")
    }

    #[test]
    fn copilot_chat_requests_become_prompts_and_an_exact_interval() {
        let root = tempdir().unwrap();
        let storage = root.path().join("workspaceStorage");
        // A space in the project directory arrives percent-escaped, because
        // `workspace.json` stores a URL, not a path, and an undecoded one names no
        // directory on this machine.
        let project = root.path().join("repos/my example");
        fs::create_dir_all(&project).unwrap();
        let folder = vscode_folder_url(&project);
        let document = serde_json::json!({
            "version": 3,
            "sessionId": "chat-one",
            "requests": [
                {
                    "timestamp": 1_767_225_600_000_i64,
                    "modelId": "copilot/gpt-test",
                    "message": {"text": "not parsed"},
                    "response": [{"value": "not parsed"}],
                    "result": {
                        "timings": {"firstProgress": 500, "totalElapsed": 30000},
                        "metadata": {"renderedUserMessage": ["not parsed"]}
                    }
                },
                // Cancelled before VS Code measured anything.
                {
                    "timestamp": 1_767_225_900_000_i64,
                    "modelId": "copilot/gpt-test",
                    "message": {"text": "not parsed"},
                    "isCanceled": true
                }
            ]
        });
        let path = vscode_chat_session(
            &storage,
            "1a2b",
            "session",
            Some(folder.as_str()),
            &document,
        );
        // Neither of these is a chat session, and a looser walk would parse both.
        fs::write(storage.join("1a2b/state.json"), "{}").unwrap();
        fs::write(storage.join("1a2b/chatSessions/notes.txt"), "text").unwrap();

        assert_eq!(vec![path.clone()], discover_copilot_vscode_files(&storage));

        let parsed = parse_copilot_vscode_file(&path, MAX_VSCODE_CHAT_JSON_BYTES);
        assert_eq!(1, parsed.sessions.len());
        let session = &parsed.sessions[0];
        assert_eq!("copilot-vscode", session.provider);
        assert!(!session.is_subagent);
        // The cwd comes out of a URL, so it carries the URL's separators and drive-letter
        // case rather than the platform's (`c:/Users/…` on Windows). Comparing the two
        // spellings as the directory they resolve to keeps the assertion honest —
        // and still fails on a fallback cwd, which resolves elsewhere.
        assert_eq!(
            canonical_string(&project),
            canonical_string(Path::new(&session.cwd))
        );
        assert!(!session.approximate_cwd);
        // Both submissions are human evidence; only the measured turn is agent time.
        assert_eq!(2, session.human_points.len());
        assert_eq!(1, session.exact_intervals.len());
        assert_eq!(1, session.points.len());
        assert_eq!("gpt-test", session.exact_intervals[0].model);
        assert_eq!(
            30,
            (session.exact_intervals[0].end - session.exact_intervals[0].start).num_seconds()
        );
        // Copilot reports no token counts anywhere, so none may be invented.
        assert!(session.token_events.is_empty());
        assert_eq!("chat-one:1a2b", session.session_id);
    }

    #[test]
    fn a_chat_session_without_a_workspace_keeps_an_approximate_directory() {
        let root = tempdir().unwrap();
        let document = serde_json::json!({
            "version": 3,
            "requests": [{"timestamp": 1_767_225_600_000_i64, "modelId": "copilot/gpt-test"}]
        });
        let path = vscode_chat_session(root.path(), "3c4d", "session", None, &document);
        let parsed = parse_copilot_vscode_file(&path, MAX_VSCODE_CHAT_JSON_BYTES);
        assert!(parsed.sessions[0].approximate_cwd);
        // A remote workspace names a place this machine cannot measure.
        assert_eq!(None, file_url_to_path("vscode-remote://ssh-remote/work"));
        assert_eq!(
            Some("C:/Users/test/project".to_string()),
            file_url_to_path("file:///C%3A/Users/test/project")
        );
    }

    /// A `workspace.json` written verbatim as VS Code writes it on Windows: an escaped
    /// drive colon, forward slashes, a lowercase drive letter. Every developer on Windows
    /// hits this shape, and the tempdir-driven test above can only exercise the shape of
    /// whichever platform runs it.
    #[test]
    fn a_windows_workspace_url_resolves_to_a_drive_path() {
        let root = tempdir().unwrap();
        let document = serde_json::json!({
            "version": 3,
            "sessionId": "chat-windows",
            "requests": [{"timestamp": 1_767_225_600_000_i64, "modelId": "copilot/gpt-test"}]
        });
        let path = vscode_chat_session(
            root.path(),
            "5e6f",
            "session",
            Some("file:///c%3A/Users/test/repos/my%20project"),
            &document,
        );
        let parsed = parse_copilot_vscode_file(&path, MAX_VSCODE_CHAT_JSON_BYTES);
        let session = &parsed.sessions[0];
        // A fixed URL rather than a temp path, so one expectation holds everywhere.
        assert_eq!("c:/Users/test/repos/my project", session.cwd);
        assert!(!session.approximate_cwd);
    }

    #[test]
    fn an_oversized_chat_session_is_declined_rather_than_read() {
        let root = tempdir().unwrap();
        let document = serde_json::json!({
            "version": 3,
            "sessionId": "chat-one",
            "requests": [{"timestamp": 1_767_225_600_000_i64, "modelId": "copilot/gpt-test"}]
        });
        let path = vscode_chat_session(root.path(), "1a2b", "session", None, &document);
        // Sessions reach 6.4 MB and arrive in directories of a hundred; the cap is what
        // keeps one file from deciding the whole run's memory use.
        let parsed = parse_copilot_vscode_file(&path, 16);
        assert!(parsed.sessions.is_empty());
        assert_eq!(1, parsed.diagnostics.unreadable_files);
    }

    #[test]
    fn an_unreadable_or_newer_chat_session_degrades_to_a_diagnostic() {
        let root = tempdir().unwrap();
        let sessions = root.path().join("1a2b/chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        let truncated = sessions.join("truncated.json");
        fs::write(&truncated, r#"{"version": 3, "requests": ["#).unwrap();
        let parsed = parse_copilot_vscode_file(&truncated, MAX_VSCODE_CHAT_JSON_BYTES);
        assert!(parsed.sessions.is_empty());
        assert_eq!(1, parsed.diagnostics.unreadable_files);
        assert!(
            parsed.diagnostics.messages[0].contains("Copilot Chat"),
            "unexpected diagnostic {:?}",
            parsed.diagnostics.messages
        );

        // A format VS Code has moved on from is skipped rather than mis-parsed: the
        // fields would still deserialize, and would quietly mean something else.
        let document = serde_json::json!({
            "version": 4,
            "requests": [{"timestamp": 1_767_225_600_000_i64, "modelId": "copilot/gpt-test"}]
        });
        let newer = vscode_chat_session(root.path(), "3c4d", "session", None, &document);
        let parsed = parse_copilot_vscode_file(&newer, MAX_VSCODE_CHAT_JSON_BYTES);
        assert!(parsed.sessions.is_empty());
        assert_eq!(1, parsed.diagnostics.skipped_sessions);
        assert_eq!(0, parsed.diagnostics.unreadable_files);
    }

    #[test]
    fn the_copilot_session_store_fills_a_missing_directory_without_reading_messages() {
        let root = tempdir().unwrap();
        let arc = root.path().join("repos/Arc");
        let chronicle = root.path().join("repos/Chronicle");
        fs::create_dir_all(&arc).unwrap();
        fs::create_dir_all(&chronicle).unwrap();
        let database = root.path().join("session-store.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    cwd TEXT,
                    repository TEXT,
                    host_type TEXT,
                    branch TEXT,
                    summary TEXT,
                    created_at TEXT,
                    updated_at TEXT
                );
                CREATE TABLE turns (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT,
                    user_message TEXT,
                    assistant_response TEXT
                );",
            )
            .unwrap();
        for (id, cwd, repository) in [
            ("68c65742", &arc, "Cratis/Chronicle"),
            ("780e0e2d", &chronicle, "Cratis/Chronicle"),
        ] {
            connection
                .execute(
                    "INSERT INTO sessions(id, cwd, repository, host_type, branch) VALUES (?1, ?2, ?3, 'github', 'main')",
                    rusqlite::params![id, cwd.to_string_lossy(), repository],
                )
                .unwrap();
        }
        // Present exactly so a widened column list would be caught: these bodies are
        // what the reader must never select.
        connection
            .execute(
                "INSERT INTO turns(session_id, user_message, assistant_response) VALUES (?1, ?2, ?3)",
                rusqlite::params!["68c65742", "SECRET PROMPT", "SECRET RESPONSE"],
            )
            .unwrap();
        drop(connection);

        let mut diagnostics = Diagnostics::default();
        let store = read_copilot_session_store(&database, &mut diagnostics);
        assert!(
            diagnostics.messages.is_empty(),
            "{:?}",
            diagnostics.messages
        );

        // A session whose event log never recorded a working directory: without the
        // store it lands under the transcript's own directory, marked approximate.
        let records = |id: &str| {
            [
                serde_json::json!({
                    "type": "session.start",
                    "timestamp": "2026-01-01T00:00:00Z",
                    "data": {"sessionId": id, "selectedModel": "gpt-test"}
                }),
                serde_json::json!({
                    "type": "user.message",
                    "timestamp": "2026-01-01T00:00:00Z",
                    "data": {}
                }),
                serde_json::json!({
                    "type": "assistant.message",
                    "timestamp": "2026-01-01T00:01:00Z",
                    "data": {}
                }),
            ]
            .into_iter()
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n")
        };
        let disagreeing = root.path().join("session-state/68c65742/events.jsonl");
        fs::create_dir_all(disagreeing.parent().unwrap()).unwrap();
        fs::write(&disagreeing, records("68c65742")).unwrap();

        let parsed = parse_copilot_file(&disagreeing, &store, MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(arc.to_string_lossy(), parsed.sessions[0].cwd);
        assert!(!parsed.sessions[0].approximate_cwd);
        // `repository` was wrong in one row of seven on the machine this was designed
        // against: the directory decides, and the disagreement is reported.
        let message = parsed
            .diagnostics
            .messages
            .iter()
            .find(|message| message.contains("Cratis/Chronicle"))
            .expect("the repository disagreement is reported");
        assert!(message.contains("Arc"), "unexpected diagnostic {message}");
        assert!(
            !parsed
                .diagnostics
                .messages
                .iter()
                .any(|message| message.contains("SECRET")),
            "message bodies must never leave the database"
        );

        // The agreeing row says nothing, because there is nothing to warn about.
        let agreeing = root.path().join("session-state/780e0e2d/events.jsonl");
        fs::create_dir_all(agreeing.parent().unwrap()).unwrap();
        fs::write(&agreeing, records("780e0e2d")).unwrap();
        let parsed = parse_copilot_file(&agreeing, &store, MAX_JSONL_LINE_BYTES);
        assert_eq!(chronicle.to_string_lossy(), parsed.sessions[0].cwd);
        assert!(
            parsed.diagnostics.messages.is_empty(),
            "{:?}",
            parsed.diagnostics.messages
        );

        // Without the store the same transcript cannot say where it ran, which is the
        // gap the database closes.
        let parsed = parse_copilot_file(
            &agreeing,
            &CopilotSessionStore::default(),
            MAX_JSONL_LINE_BYTES,
        );
        assert!(parsed.sessions[0].approximate_cwd);
    }
}
