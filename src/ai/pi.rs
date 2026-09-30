//! Pi: `--<encoded cwd>--/<timestamp>_<uuid>.jsonl` session trees.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::{
    MAX_JSONL_LINE_BYTES, ParsedFile, deserialize_maybe_number, discover_files, file_stem,
    for_json_lines, load_files, read_bounded_line, safe_model, unknown_model,
};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, RawSession, Session, TokenEvent, TokenUsage};
use crate::paths::{PathResolver, lossy_pi_cwd};
use crate::timeutil::{nearest_models, parse_epoch_milliseconds, parse_timestamp};

/// One line of a Pi session transcript.
///
/// Pi writes a tree, not a list: every entry carries an `id` and a `parentId`, and
/// `/tree` can move the leaf back so later entries branch off an earlier one. None of
/// that is read here. Every entry that was ever written happened — a branch that was
/// explored and abandoned still cost the time it took — and this tool measures activity
/// rather than reconstructing the conversation the model finally saw. So the file is
/// read as the append-only log of events it physically is, and `parentId` is ignored.
#[derive(Deserialize)]
struct PiRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    /// Only the session header carries `cwd`, and it is the authoritative one: it is
    /// `resolvePath(cwd)` at session creation, not the lossy directory-name encoding.
    cwd: Option<String>,
    /// The header's session UUID.
    id: Option<String>,
    /// The session-format version stamped on the header (1, 2, or 3).
    version: Option<u32>,
    /// Present only on a header whose session was created from another one — a subagent,
    /// a `/fork`, or a `/clone`.
    #[serde(rename = "parentSession")]
    parent_session: Option<String>,
    /// `model_change` names the model the user switched to.
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    /// A `compaction` or `branch_summary` entry records what generating its summary
    /// cost. Absent on older sessions and on summaries an extension produced.
    usage: Option<PiUsage>,
    #[serde(default, deserialize_with = "deserialize_pi_message")]
    message: Option<PiMessage>,
}

#[derive(Default)]
struct PiMessage {
    role: Option<String>,
    model: Option<String>,
    /// Unix milliseconds. Pi stamps the message itself as well as the entry, and for a
    /// user message the entry is written when the turn is submitted while the message
    /// timestamp is when the text was composed.
    timestamp: Option<f64>,
    /// Whether the content is text or an image a person could have supplied. A user
    /// message whose content is neither is not evidence that anyone typed.
    human_content: bool,
    /// Text of a `user` message, kept only long enough to test it against
    /// `PI_INJECTED_USER_PREFIXES`. Bounded at `MAX_PI_PROMPT_PREFIX_BYTES`.
    prefix: String,
    usage: Option<PiUsage>,
}

/// Pi's `Usage`, which counts each class of token separately.
///
/// `totalTokens` is their sum and `cost` is priced in dollars; both are derived, so
/// neither is read. `cacheWrite1h` is a *breakdown* of `cacheWrite` — the portion
/// written to the 1-hour cache, never an addition to it — so adding it would
/// double-count every extended-cache write.
#[derive(Clone, Default, Deserialize)]
struct PiUsage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: u64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: u64,
}

/// Reads a Pi `AgentMessage`, keeping only the fields that describe *when* and *by whom*.
///
/// Written by hand rather than derived because `content` must be classified without
/// being retained: an assistant message holds the whole reply and a tool result holds
/// whole files, and this tool never reads either. The visitor records whether a block is
/// text or an image and keeps at most `MAX_PI_PROMPT_PREFIX_BYTES` of a *user* message,
/// which is the only role whose text is inspected at all, and only to recognise the
/// machine-authored turns described on `PI_INJECTED_USER_PREFIXES`.
fn deserialize_pi_message<'de, D>(deserializer: D) -> Result<Option<PiMessage>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MessageVisitor;
    impl<'de> Visitor<'de> for MessageVisitor {
        type Value = Option<PiMessage>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a message object or another JSON value")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut message = PiMessage::default();
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "role" => message.role = map.next_value()?,
                    "model" => message.model = map.next_value()?,
                    "timestamp" => {
                        message.timestamp = map.next_value::<MaybeNumber>()?.0;
                    }
                    "content" => {
                        let content = map.next_value::<PiContent>()?;
                        message.human_content = content.human;
                        message.prefix = content.prefix;
                    }
                    "usage" => message.usage = map.next_value()?,
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

/// A number that tolerates the field being absent, null, or a numeric string, so the
/// reader used by the Codex adapter can be applied to a single named field.
struct MaybeNumber(Option<f64>);

impl<'de> Deserialize<'de> for MaybeNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_maybe_number(deserializer).map(MaybeNumber)
    }
}

/// How much of a user message is kept to test it against `PI_INJECTED_USER_PREFIXES`.
///
/// The longest marker is well under this, and nothing longer is ever retained, so a
/// multi-megabyte pasted prompt costs this many bytes rather than its own size.
const MAX_PI_PROMPT_PREFIX_BYTES: usize = 64;

/// The classification of a message's `content`: whether a person could have written it,
/// and a bounded prefix of its text.
#[derive(Default)]
struct PiContent {
    human: bool,
    prefix: String,
}

impl PiContent {
    fn push(&mut self, text: &str) {
        let remaining = MAX_PI_PROMPT_PREFIX_BYTES.saturating_sub(self.prefix.len());
        if remaining == 0 {
            return;
        }
        let trimmed = if self.prefix.is_empty() {
            text.trim_start()
        } else {
            text
        };
        // A character boundary, so a multi-byte character straddling the limit is
        // dropped rather than panicking the parser on a slice.
        let mut end = trimmed.len().min(remaining);
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        self.prefix.push_str(&trimmed[..end]);
    }
}

impl<'de> Deserialize<'de> for PiContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = PiContent;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("message content")
            }

            /// `UserMessage.content` is `string | (TextContent | ImageContent)[]`, so a
            /// bare string is ordinary typed text.
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                let mut content = PiContent {
                    human: true,
                    ..PiContent::default()
                };
                content.push(value);
                Ok(content)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                let mut content = PiContent {
                    human: true,
                    ..PiContent::default()
                };
                content.push(&value);
                Ok(content)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut content = PiContent::default();
                while let Some(block) = sequence.next_element::<PiContentBlock>()? {
                    match block.block_type.as_deref() {
                        Some("text") => {
                            content.human = true;
                            content.push(&block.text.unwrap_or_default());
                        }
                        // An image is something a person supplied, but it carries no text
                        // to match a marker against.
                        Some("image") => content.human = true,
                        // `thinking` and `toolCall` are the model's own work.
                        _ => {}
                    }
                }
                Ok(content)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(PiContent::default())
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(PiContent::default())
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(PiContent::default())
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

/// One block of a Pi message's content.
///
/// `text` is read only for a `text` block, and only the first
/// `MAX_PI_PROMPT_PREFIX_BYTES` of a user message survive `PiContent::push`. serde still
/// materializes the field for the block being visited, which is why nothing downstream
/// keeps it.
#[derive(Deserialize)]
struct PiContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// Finds `--<encoded cwd>--/<timestamp>_<uuid>.jsonl` under Pi's session root.
///
/// Pi also leaves `*.jsonl.bak.<epoch>` files beside the sessions it rewrites during a
/// format migration. A backup is a byte-for-byte copy of a session that is still there,
/// so reading both would count every one of its turns twice; only `.jsonl` is taken.
pub fn discover_pi_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.extension()
            .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
    })
}

pub fn read_pi_sessions_indexed(
    root: &Path,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    cache: Option<&mut TranscriptCache>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Vec<Session> {
    if !root.is_dir() {
        diagnostics.warn(format!("Pi history not found: {}", root.display()));
        return Vec::new();
    }
    load_files(
        discover_pi_files(root),
        resolver,
        diagnostics,
        cache,
        "pi",
        // A delegated session's repository hint comes from its parent header,
        // so the cache context must change when that parent changes, appears,
        // or disappears even if the child transcript itself is untouched.
        |path| pi_context_fingerprint(path, root),
        since,
        until,
        |path| parse_pi_file(path, root, MAX_JSONL_LINE_BYTES),
    )
}

/// Openings of a `user` message that a machine wrote.
///
/// Pi has one `user` role and several things arrive in it. A person typing a prompt is
/// one. The others are turns the harness or an extension injects to steer the agent:
/// pi-task's watchdog posts `[SYSTEM] Your \`bash\` call ran longer than …` after it
/// kills a stuck tool call, and background tasks, subagents, and search results are
/// delivered as tagged blocks. Every one of them is written while the machine is working
/// and nobody is at the keyboard, so counting them as prompts would manufacture human
/// involvement out of automation — exactly the confusion this tool exists to prevent.
///
/// Matched against the start of the message, after leading whitespace. The list is
/// deliberately short and anchored: a marker matched anywhere in the body would let a
/// person quoting one in a genuine question erase their own prompt.
const PI_INJECTED_USER_PREFIXES: &[&str] = &[
    "[SYSTEM]",
    "<background-task-notification",
    "<subagent-notification",
    "<system-reminder",
    "<web-search-results",
];

fn pi_injected_prompt(prefix: &str) -> bool {
    let prefix = prefix.trim_start();
    PI_INJECTED_USER_PREFIXES
        .iter()
        .any(|marker| prefix.starts_with(marker))
}

/// Reads only the bounded session header of a Pi parent transcript.
///
/// A delegated Pi session records its parent transcript path. That is enough to
/// retain the logical repository after a temporary worktree is deleted, but it
/// is also transcript-controlled input: only a JSONL file below the configured
/// Pi session root may be opened, and no message line is ever inspected.
const MAX_PI_HEADER_BYTES: usize = 64 * 1024;

fn pi_session_header(path: &Path) -> Option<PiRecord> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let (line, oversized) = read_bounded_line(&mut reader, MAX_PI_HEADER_BYTES).ok()??;
    if oversized {
        return None;
    }
    let record: PiRecord = serde_json::from_slice(&line).ok()?;
    (record.record_type.as_deref() == Some("session")).then_some(record)
}

fn pi_parent_path(parent_session: &str, root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let parent = Path::new(parent_session).canonicalize().ok()?;
    (parent.starts_with(root)
        && parent.extension().and_then(|value| value.to_str()) == Some("jsonl"))
    .then_some(parent)
}

fn pi_context_fingerprint(path: &Path, root: &Path) -> String {
    let parent = pi_session_header(path)
        .and_then(|record| record.parent_session)
        .and_then(|parent| pi_parent_path(&parent, root));
    match parent {
        Some(parent) => format!("pi-v2:parent:{}", crate::cache::file_context(&parent)),
        None => "pi-v2:no-parent".to_string(),
    }
}

fn pi_parent_cwd(parent_session: &str, root: &Path) -> Option<String> {
    let parent = pi_parent_path(parent_session, root)?;
    pi_session_header(&parent)?
        .cwd
        .filter(|cwd| !cwd.is_empty())
}

/// Reads one Pi session transcript.
///
/// Pi writes a file per session under `~/.pi/agent/sessions/--<encoded cwd>--/`, and a
/// session created from another one — a subagent, a `/fork`, or a `/clone` — records the
/// parent's path in its header. `is_subagent` is set from that, which keeps delegated
/// work out of the human-involvement estimate and reports it as agent activity, matching
/// how the OpenCode and Copilot adapters treat a session with a parent.
///
/// A `/fork` or `/clone` also copies the parent's entries into the new file verbatim,
/// keeping their original timestamps. Those copies are real duplicates — the same turns,
/// logged twice — so anything at or before the child's own header timestamp is dropped;
/// see `PiSessionState::inherited`. Because the copy is what makes them duplicates,
/// dropping them costs nothing: the parent file still reports every one of those turns.
pub fn parse_pi_file(path: &Path, root: &Path, max_line_bytes: usize) -> ParsedFile {
    let mut result = ParsedFile::default();
    let mut state = PiSessionState::default();
    for_json_lines(
        path,
        max_line_bytes,
        &mut result.diagnostics,
        |record: PiRecord| state.record(record),
    );
    state.finish(path, root, &mut result);
    result
}

struct PiSessionState {
    session_id: Option<String>,
    cwd: Option<String>,
    version: Option<String>,
    parent_session: Option<String>,
    /// The header's own timestamp, which bounds the inherited prefix a fork carries.
    started: Option<DateTime<Utc>>,
    /// Whether a header has been seen. A file whose first entry is not a session header
    /// is not a Pi session, and Pi's own loader rejects it.
    header: bool,
    current_model: String,
    points: Vec<ActivityPoint>,
    human_points: Vec<ActivityPoint>,
    token_events: Vec<TokenEvent>,
    inherited: u64,
}

impl Default for PiSessionState {
    fn default() -> Self {
        Self {
            session_id: None,
            cwd: None,
            version: None,
            parent_session: None,
            started: None,
            header: false,
            current_model: unknown_model(),
            points: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            inherited: 0,
        }
    }
}

impl PiSessionState {
    fn record(&mut self, record: PiRecord) {
        match record.record_type.as_deref() {
            Some("session") => {
                // A `/fork` copies the source file's entries in after the new header, and
                // a malformed file could hold more than one header; the first one is this
                // session's own.
                if self.header {
                    return;
                }
                self.header = true;
                self.session_id = record.id;
                self.cwd = record.cwd.filter(|value| !value.is_empty());
                self.version = record.version.map(|value| value.to_string());
                self.parent_session = record.parent_session.filter(|value| !value.is_empty());
                self.started = record.timestamp.as_deref().and_then(parse_timestamp);
            }
            // The model in force from here on, until the next change or an assistant
            // message names its own.
            Some("model_change") => {
                if let Some(model) = record.model_id.as_deref() {
                    self.current_model = safe_model(model);
                }
            }
            Some("message") => self.message(record),
            // Compacting a context and summarizing an abandoned branch are both real
            // model calls: they take time and Pi bills them, recording the cost as a
            // `usage` on the entry. Counted as agent activity for that reason, and never
            // as a prompt — the summary is written by the model, not by a person.
            Some("compaction" | "branch_summary") => self.summary(record),
            // `custom` is extension state and `label`, `session_info`, and
            // `thinking_level_change` are bookkeeping: none of them is a turn.
            _ => {}
        }
    }

    fn summary(&mut self, record: PiRecord) {
        let Some(timestamp) = record.timestamp.as_deref().and_then(parse_timestamp) else {
            return;
        };
        if self.is_inherited(Some(timestamp)) {
            self.inherited += 1;
            return;
        }
        self.points.push(ActivityPoint {
            timestamp,
            model: self.current_model.clone(),
        });
        // Older sessions, and summaries an extension generated, carry no usage.
        let Some(usage) = record.usage else {
            return;
        };
        let usage = TokenUsage {
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: usage.cache_read,
            cache_creation_tokens: usage.cache_write,
        };
        if !usage.is_zero() {
            self.token_events.push(TokenEvent {
                timestamp,
                model: self.current_model.clone(),
                usage,
            });
        }
    }

    fn message(&mut self, record: PiRecord) {
        let Some(message) = record.message else {
            return;
        };
        let role = message.role.as_deref().unwrap_or_default();
        // `toolResult` is the harness reporting a tool it just ran, and it is written
        // between an assistant message and the next one. It is bracketed by activity
        // that is already counted, so it adds no interval of its own.
        //
        // `bashExecution` is the opposite case and is counted below: it is a `!command`
        // typed into Pi's own prompt, which is a person at the keyboard rather than a
        // tool the model chose to call.
        if role != "user" && role != "assistant" && role != "bashExecution" {
            return;
        }
        if role == "assistant"
            && let Some(model) = message.model.as_deref()
        {
            self.current_model = safe_model(model);
        }
        // The entry timestamp is when the line was appended. A user message also carries
        // its own, stamped when the text was submitted; it is preferred because it is the
        // moment the person acted.
        let entry_time = record.timestamp.as_deref().and_then(parse_timestamp);
        let message_time = message.timestamp.and_then(parse_epoch_milliseconds);
        let Some(timestamp) = message_time.or(entry_time) else {
            return;
        };
        if self.is_inherited(entry_time.or(message_time)) {
            self.inherited += 1;
            return;
        }
        self.points.push(ActivityPoint {
            timestamp,
            model: self.current_model.clone(),
        });
        if role == "assistant"
            && let Some(usage) = message.usage
        {
            let usage = TokenUsage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cache_read_tokens: usage.cache_read,
                cache_creation_tokens: usage.cache_write,
            };
            if !usage.is_zero() {
                self.token_events.push(TokenEvent {
                    timestamp,
                    model: self.current_model.clone(),
                    usage,
                });
            }
        }
        // A subagent's prompt was written by the agent that delegated to it, so a
        // delegated session has no human prompts at all.
        //
        // A `bashExecution` is a command someone typed at Pi's prompt with `!` (or `!!`
        // to keep the output out of the model's context). The model cannot produce one —
        // it reaches the shell through the `bash` *tool*, which is recorded as a
        // toolResult — so it is direct keyboard evidence, and it is counted whatever its
        // content looks like, without the text ever being read.
        let human = match role {
            "user" => {
                message.human_content
                    && self.parent_session.is_none()
                    && !pi_injected_prompt(&message.prefix)
            }
            "bashExecution" => self.parent_session.is_none(),
            _ => false,
        };
        if human {
            self.human_points.push(ActivityPoint {
                timestamp,
                model: self.current_model.clone(),
            });
        }
    }

    /// Whether an entry was copied in from a parent session by `/fork` or `/clone`.
    ///
    /// Those entries keep the timestamps they had in the source file, so they predate the
    /// header of the session that now holds them. A subagent's header names a parent too
    /// but copies nothing, and its own entries all follow its header, so this costs it
    /// nothing.
    fn is_inherited(&self, timestamp: Option<DateTime<Utc>>) -> bool {
        let (Some(started), Some(timestamp)) = (self.started, timestamp) else {
            return false;
        };
        self.parent_session.is_some() && timestamp < started
    }

    fn finish(self, path: &Path, root: &Path, result: &mut ParsedFile) {
        if self.inherited > 0 {
            result.diagnostics.note(format!(
                "{} inherited entr(ies) from a forked Pi session counted once, in the session they were recorded in: {}",
                self.inherited,
                path.display()
            ));
        }
        if self.points.is_empty() {
            result.diagnostics.skipped_sessions += 1;
            return;
        }
        // A model named after the fact still applies to the turns before it: the user
        // messages that opened a session precede the first assistant reply that reports
        // which model answered.
        let models_at: BTreeMap<_, _> = nearest_models(&self.points)
            .into_iter()
            .map(|point| (point.timestamp, point.model))
            .collect();
        let mut human_points = self.human_points;
        for point in &mut human_points {
            if let Some(model) = models_at.get(&point.timestamp) {
                point.model.clone_from(model);
            }
        }
        let approximate_cwd = self.cwd.is_none();
        let repository_hint_cwd = self
            .parent_session
            .as_deref()
            .and_then(|parent| pi_parent_cwd(parent, root));
        // The header's `cwd` is the resolved absolute path. Only when it is missing does
        // the directory name have to be decoded, and that is lossy: Pi replaces both
        // separators and `:` with `-`, so a path that contained a dash cannot be told
        // apart from one that did not.
        let cwd = self
            .cwd
            .unwrap_or_else(|| lossy_pi_cwd(path.parent().unwrap_or(root)));
        let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
        result.sessions.push(RawSession {
            provider: "pi".to_string(),
            // Pi's session id is a UUID, but a file copied between machines or restored
            // from a backup can repeat one; the path keeps sessions distinct, as it does
            // for Claude Code.
            session_id: format!(
                "{}:{relative}",
                self.session_id.unwrap_or_else(|| file_stem(path))
            ),
            source_file: path.to_path_buf(),
            cwd,
            repository_hint_cwd,
            points: self.points,
            exact_intervals: Vec::new(),
            human_points,
            token_events: self.token_events,
            is_subagent: self.parent_session.is_some(),
            approximate_cwd,
            version: self.version,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Builds a Pi session file: a header followed by the given entries.
    fn pi_session(
        directory: &Path,
        name: &str,
        header: serde_json::Value,
        entries: &[serde_json::Value],
    ) -> PathBuf {
        let path = directory.join(name);
        let mut lines = vec![header.to_string()];
        lines.extend(entries.iter().map(std::string::ToString::to_string));
        fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn pi_message(id: &str, timestamp: &str, message: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": serde_json::Value::Null,
            "timestamp": timestamp,
            "message": message,
        })
    }

    fn pi_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> serde_json::Value {
        serde_json::json!({
            "input": input,
            "output": output,
            "cacheRead": cache_read,
            "cacheWrite": cache_write,
            "cacheWrite1h": cache_write,
            "totalTokens": input + output + cache_read + cache_write,
            "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.3},
        })
    }

    #[test]
    fn pi_cache_context_tracks_the_parent_header() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let parent = pi_session(
            &directory,
            "parent.jsonl",
            serde_json::json!({
                "type": "session", "id": "parent", "cwd": "/tmp/one"
            }),
            &[],
        );
        let child = pi_session(
            &directory,
            "child.jsonl",
            serde_json::json!({
                "type": "session", "id": "child", "cwd": "/tmp/child",
                "parentSession": parent
            }),
            &[],
        );

        let original = pi_context_fingerprint(&child, root.path());
        fs::write(
            &parent,
            serde_json::json!({
                "type": "session", "id": "parent", "cwd": "/tmp/different-parent"
            })
            .to_string(),
        )
        .unwrap();
        let changed = pi_context_fingerprint(&child, root.path());
        assert_ne!(original, changed);

        fs::remove_file(parent).unwrap();
        assert_eq!(
            "pi-v2:no-parent",
            pi_context_fingerprint(&child, root.path())
        );
    }

    /// Pi has one `user` role, and a person typing into it is only one of the things that
    /// arrive there. An extension's watchdog notice and a background-task notification are
    /// written while the machine works and nobody is at the keyboard, and a `toolCall` is
    /// not typed at all.
    #[test]
    fn pi_machine_written_user_turns_are_not_human_evidence() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T00:00:10.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "a genuine question"}
                    ]}),
                ),
                pi_message(
                    "b",
                    "2026-01-01T00:00:20.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "[SYSTEM] Your `bash` call ran longer than 15 minutes"}
                    ]}),
                ),
                pi_message(
                    "c",
                    "2026-01-01T00:00:30.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "<background-task-notification>done</background-task-notification>"}
                    ]}),
                ),
                pi_message(
                    "d",
                    "2026-01-01T00:00:40.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "toolCall", "id": "t1", "name": "bash", "arguments": {}}
                    ]}),
                ),
                // A tool result is bracketed by activity that is already counted.
                pi_message(
                    "e",
                    "2026-01-01T00:00:50.000Z",
                    serde_json::json!({"role": "toolResult", "toolCallId": "t1",
                        "toolName": "bash", "isError": false,
                        "content": [{"type": "text", "text": "output"}]}),
                ),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert_eq!(1, session.human_points.len());
        assert_eq!(4, session.points.len());
        assert_eq!("/tmp/project", session.cwd);
        assert!(!session.is_subagent);
        assert_eq!(Some("3".to_string()), session.version);
    }

    /// A session Pi created from another one records the parent's path. For a subagent
    /// that means the work was delegated: its one prompt was written by the agent that
    /// delegated it, so it is agent activity and never human involvement.
    #[test]
    fn pi_subagent_sessions_carry_no_human_prompts() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T01-00-00-000Z_child.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "child",
                "timestamp": "2026-01-01T01:00:00.000Z", "cwd": "/tmp/project",
                "parentSession": "/somewhere/parent.jsonl"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T01:00:10.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "You are investigating X. Report back."}
                    ]}),
                ),
                pi_message(
                    "b",
                    "2026-01-01T01:00:20.000Z",
                    serde_json::json!({"role": "assistant", "model": "claude-opus-5",
                        "provider": "anthropic", "stopReason": "stop",
                        "content": [{"type": "text", "text": "done"}],
                        "usage": pi_usage(5, 7, 11, 13)}),
                ),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert!(session.is_subagent);
        assert!(session.human_points.is_empty());
        assert_eq!(2, session.points.len());
        // `cacheWrite1h` is a breakdown of `cacheWrite`, so it is never added to it.
        assert_eq!(13, session.token_events[0].usage.cache_creation_tokens);
        assert_eq!(36, session.token_events[0].usage.total());
        assert_eq!("claude-opus-5", session.token_events[0].model);
        // A transcript cannot make the reader open an arbitrary path outside
        // the configured Pi history root.
        assert!(session.repository_hint_cwd.is_none());
    }

    #[test]
    fn pi_subagent_retains_its_parent_cwd_as_a_repository_hint() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let parent = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_parent.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "parent",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/repos/product"
            }),
            &[],
        );
        let child = pi_session(
            &directory,
            "2026-01-01T01-00-00-000Z_child.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "child",
                "timestamp": "2026-01-01T01:00:00.000Z", "cwd": "/tmp/pi-agent-deleted",
                "parentSession": parent
            }),
            &[pi_message(
                "a",
                "2026-01-01T01:00:10.000Z",
                serde_json::json!({"role": "assistant", "model": "test-model",
                    "content": [{"type": "text", "text": "done"}]}),
            )],
        );

        let parsed = parse_pi_file(&child, root.path(), MAX_JSONL_LINE_BYTES);

        assert_eq!(
            Some("/repos/product"),
            parsed.sessions[0].repository_hint_cwd.as_deref()
        );
        assert_eq!("/tmp/pi-agent-deleted", parsed.sessions[0].cwd);
    }

    /// `/fork` and `/clone` also record a parent, but unlike a subagent they copy the
    /// source session's entries into the new file keeping their original timestamps. Those
    /// are the same turns logged twice, and the parent file still reports every one of
    /// them, so the copies must not be counted a second time.
    #[test]
    fn pi_entries_inherited_by_a_fork_are_counted_once() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let inherited = [
            pi_message(
                "a",
                "2026-01-01T00:00:10.000Z",
                serde_json::json!({"role": "user", "content": [
                    {"type": "text", "text": "the original prompt"}
                ]}),
            ),
            pi_message(
                "b",
                "2026-01-01T00:00:20.000Z",
                serde_json::json!({"role": "assistant", "model": "claude-opus-5",
                    "provider": "anthropic", "stopReason": "stop",
                    "content": [{"type": "text", "text": "reply"}],
                    "usage": pi_usage(10, 20, 0, 0)}),
            ),
        ];
        let source = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_source.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "source",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &inherited,
        );
        let mut forked = inherited.to_vec();
        forked.push(pi_message(
            "c",
            "2026-01-02T00:00:30.000Z",
            serde_json::json!({"role": "assistant", "model": "claude-opus-5",
                "provider": "anthropic", "stopReason": "stop",
                "content": [{"type": "text", "text": "continued"}],
                "usage": pi_usage(1, 2, 0, 0)}),
        ));
        let fork = pi_session(
            &directory,
            "2026-01-02T00-00-00-000Z_fork.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "fork",
                "timestamp": "2026-01-02T00:00:00.000Z", "cwd": "/tmp/project",
                "parentSession": source.to_string_lossy()
            }),
            &forked,
        );

        let original = parse_pi_file(&source, root.path(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, original.sessions[0].human_points.len());
        assert_eq!(30, original.sessions[0].token_events[0].usage.total());

        let parsed = parse_pi_file(&fork, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        // Only the turn the fork actually added.
        assert_eq!(1, session.points.len());
        assert_eq!(1, session.token_events.len());
        assert_eq!(3, session.token_events[0].usage.total());
        assert!(session.human_points.is_empty());
        assert_eq!(1, parsed.diagnostics.note_count);
        assert!(parsed.diagnostics.messages.is_empty());
    }

    /// A `.jsonl.bak.<epoch>` file left by a format migration is a copy of a session that
    /// is still there, so reading it would count every one of its turns twice.
    #[test]
    fn pi_discovery_skips_migration_backups() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("session.jsonl"), "{}").unwrap();
        fs::write(directory.join("session.jsonl.bak.1787125134"), "{}").unwrap();

        let found = discover_pi_files(root.path());
        assert_eq!(1, found.len());
        assert_eq!(
            Some("session.jsonl"),
            found[0].file_name().unwrap().to_str()
        );
    }

    /// The model is named by the assistant message that used it, and by a `model_change`
    /// the user made. A prompt typed before the first reply is still credited to the model
    /// that answered it rather than to `unknown`.
    #[test]
    fn pi_model_changes_apply_to_the_turns_around_them() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T00:00:10.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "first"}
                    ]}),
                ),
                pi_message(
                    "b",
                    "2026-01-01T00:00:20.000Z",
                    serde_json::json!({"role": "assistant", "model": "claude-opus-5",
                        "provider": "anthropic", "stopReason": "stop",
                        "content": [{"type": "text", "text": "reply"}],
                        "usage": pi_usage(1, 1, 0, 0)}),
                ),
                serde_json::json!({
                    "type": "model_change", "id": "c", "parentId": "b",
                    "timestamp": "2026-01-01T00:00:30.000Z",
                    "provider": "openai", "modelId": "gpt-5"
                }),
                pi_message(
                    "d",
                    "2026-01-01T00:00:40.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "second"}
                    ]}),
                ),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert_eq!("claude-opus-5", session.human_points[0].model);
        assert_eq!("gpt-5", session.human_points[1].model);
    }

    /// Recognising a machine-written turn is the one reason this adapter looks at message
    /// text at all, and it must not become a way for prompts to leak into a report. The
    /// bound is what makes that safe: a marker is matched against the opening of a *user*
    /// message and nothing longer than `MAX_PI_PROMPT_PREFIX_BYTES` is ever held, so a
    /// pasted prompt or a whole file in a tool result costs a fixed few bytes.
    #[test]
    fn pi_message_text_is_bounded_and_never_reaches_a_session() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let secret = "attack at dawn ".repeat(4096);
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T00:00:10.000Z",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": secret}
                    ]}),
                ),
                pi_message(
                    "b",
                    "2026-01-01T00:00:20.000Z",
                    serde_json::json!({"role": "assistant", "model": "pi-test",
                        "provider": "anthropic", "stopReason": "stop",
                        "content": [{"type": "text", "text": secret}],
                        "usage": pi_usage(1, 1, 0, 0)}),
                ),
                pi_message(
                    "c",
                    "2026-01-01T00:00:30.000Z",
                    serde_json::json!({"role": "toolResult", "toolCallId": "t1",
                        "toolName": "read", "isError": false,
                        "content": [{"type": "text", "text": secret}]}),
                ),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        // The prompt still counts as involvement — it was read, not ignored.
        assert_eq!(1, parsed.sessions[0].human_points.len());
        let serialized = serde_json::to_string(&parsed).unwrap();
        assert!(!serialized.contains("attack at dawn"));

        // The bound holds a marker that a long injected turn opens with, without holding
        // the turn.
        let mut content = PiContent::default();
        content.push(&format!("   [SYSTEM] {secret}"));
        assert!(content.prefix.len() <= MAX_PI_PROMPT_PREFIX_BYTES);
        assert!(pi_injected_prompt(&content.prefix));

        // A multi-byte character straddling the limit is dropped rather than panicking.
        let mut wide = PiContent::default();
        wide.push(&"\u{1f600}".repeat(64));
        assert!(wide.prefix.len() <= MAX_PI_PROMPT_PREFIX_BYTES);
    }

    /// A `!command` typed at Pi's own prompt is a person at the keyboard. The model has no
    /// way to produce one — it reaches the shell through the `bash` tool, which is logged
    /// as a toolResult — so it is direct evidence of involvement, and the command itself
    /// is never read.
    #[test]
    fn pi_shell_commands_typed_by_a_person_are_human_evidence() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T00:00:10.000Z",
                    serde_json::json!({"role": "bashExecution",
                        "command": "deploy --token hunter2",
                        "output": "secret-output", "exitCode": 0,
                        "cancelled": false, "truncated": false}),
                ),
                // `!!` keeps the output out of the model's context; someone still typed it.
                pi_message(
                    "b",
                    "2026-01-01T00:00:20.000Z",
                    serde_json::json!({"role": "bashExecution", "command": "git status",
                        "output": "clean", "exitCode": 0, "cancelled": false,
                        "truncated": false, "excludeFromContext": true}),
                ),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert_eq!(2, session.human_points.len());
        assert_eq!(2, session.points.len());
        let serialized = serde_json::to_string(&parsed).unwrap();
        assert!(!serialized.contains("hunter2"));
        assert!(!serialized.contains("secret-output"));
    }

    /// A subagent runs unattended, so a shell command inside one was issued by the agent
    /// driving it rather than typed by anyone.
    #[test]
    fn pi_shell_commands_in_a_delegated_session_are_not_human_evidence() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T01-00-00-000Z_child.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "child",
                "timestamp": "2026-01-01T01:00:00.000Z", "cwd": "/tmp/project",
                "parentSession": "/somewhere/parent.jsonl"
            }),
            &[pi_message(
                "a",
                "2026-01-01T01:00:10.000Z",
                serde_json::json!({"role": "bashExecution", "command": "cargo test",
                    "output": "ok", "exitCode": 0, "cancelled": false,
                    "truncated": false}),
            )],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert!(session.human_points.is_empty());
        assert_eq!(1, session.points.len());
    }

    /// Compacting a context and summarizing an abandoned branch are model calls Pi bills,
    /// so they are agent activity. Neither is a prompt: the summary is written by the
    /// model, not typed by anyone.
    #[test]
    fn pi_compaction_and_branch_summaries_are_agent_work_not_prompts() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp/project"
            }),
            &[
                pi_message(
                    "a",
                    "2026-01-01T00:00:10.000Z",
                    serde_json::json!({"role": "assistant", "model": "pi-test",
                        "provider": "anthropic", "stopReason": "stop",
                        "content": [{"type": "text", "text": "reply"}],
                        "usage": pi_usage(1, 1, 0, 0)}),
                ),
                serde_json::json!({
                    "type": "compaction", "id": "b", "parentId": "a",
                    "timestamp": "2026-01-01T00:00:20.000Z",
                    "summary": "the user asked about X", "tokensBefore": 50000,
                    "usage": pi_usage(100, 200, 0, 0)
                }),
                // Older sessions record no usage for a summary; it is still activity.
                serde_json::json!({
                    "type": "branch_summary", "id": "c", "parentId": "b",
                    "timestamp": "2026-01-01T00:00:30.000Z",
                    "fromId": "a", "summary": "branch explored Y"
                }),
            ],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert_eq!(3, session.points.len());
        assert!(session.human_points.is_empty());
        assert_eq!(2, session.token_events.len());
        assert_eq!(300, session.token_events[1].usage.total());
        // The summary text itself is never read.
        let serialized = serde_json::to_string(&parsed).unwrap();
        assert!(!serialized.contains("the user asked about X"));
        assert!(!serialized.contains("branch explored Y"));
    }

    /// A session whose header has no `cwd` still has to be placed somewhere, and the
    /// directory name is the only thing left to read it from.
    #[test]
    fn a_pi_session_without_a_working_directory_is_marked_approximate() {
        let root = tempdir().unwrap();
        let directory = root.path().join("--tmp-project--");
        fs::create_dir(&directory).unwrap();
        let path = pi_session(
            &directory,
            "2026-01-01T00-00-00-000Z_session.jsonl",
            serde_json::json!({
                "type": "session", "version": 3, "id": "session-one",
                "timestamp": "2026-01-01T00:00:00.000Z"
            }),
            &[pi_message(
                "a",
                "2026-01-01T00:00:10.000Z",
                serde_json::json!({"role": "user", "content": "typed as a bare string"}),
            )],
        );

        let parsed = parse_pi_file(&path, root.path(), MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        assert!(session.approximate_cwd);
        assert_eq!("/tmp/project", session.cwd);
        // `UserMessage.content` may be a bare string, which is ordinary typed text.
        assert_eq!(1, session.human_points.len());
    }
}
