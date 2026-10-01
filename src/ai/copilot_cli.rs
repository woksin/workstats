//! GitHub Copilot CLI: `session-state/**/events.jsonl`, with the newer `session-store.db`
//! supplying where a session ran.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use super::{
    BranchTracker, MAX_JSONL_LINE_BYTES, ParsedFile, PrCollector, deserialize_maybe_number,
    discover_files, file_stem, for_json_lines, load_files, safe_model, sqlite_columns,
    sqlite_table_exists, sqlite_text, whole_session_branch,
};
use crate::cache::TranscriptCache;
use crate::model::{
    ActivityPoint, Diagnostics, ExactInterval, RawSession, Session, TokenEvent, TokenUsage,
};
use crate::paths::PathResolver;
use crate::timeutil::parse_timestamp;

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
    /// `session.context_changed` may carry the branch beside `cwd` rather than inside
    /// `context`; both spellings are the same fact.
    branch: Option<String>,
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
    branch: Option<String>,
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
    /// `session_refs` rows with `ref_type = 'pr'`: the number, and the repository when
    /// the value itself named one. Never a URL.
    pub pull_requests: Vec<(u64, Option<String>)>,
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

pub fn discover_copilot_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.file_name()
            .is_some_and(|value| value.eq_ignore_ascii_case("events.jsonl"))
    })
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
    // Kept per working directory like the points, so a branch recorded in one checkout
    // is never read as the branch of another the session moved to.
    let mut branches_by_cwd: BTreeMap<Option<String>, BranchTracker> = BTreeMap::new();
    for_json_lines(
        path,
        max_line_bytes,
        &mut result,
        |record: CopilotRecord| {
            let Some(record_type) = record.record_type.as_deref() else {
                return;
            };
            if record_type == "session.start" {
                session_id = record.data.session_id.or(session_id.take());
                let context_branch = record
                    .data
                    .context
                    .as_ref()
                    .and_then(|context| context.branch.clone());
                cwd = record
                    .data
                    .context
                    .and_then(|context| context.cwd)
                    .or(cwd.take());
                if let (Some(branch), Some(timestamp)) = (
                    context_branch,
                    record.timestamp.as_deref().and_then(parse_timestamp),
                ) {
                    branches_by_cwd
                        .entry(cwd.clone())
                        .or_default()
                        .observe(timestamp, &branch);
                }
                if let Some(model) = record.data.selected_model {
                    current_model = safe_model(&model);
                }
                version = record.data.copilot_version;
                return;
            }
            if record_type == "session.context_changed" {
                let context_branch = record
                    .data
                    .context
                    .and_then(|context| context.branch)
                    .or(record.data.branch);
                cwd = record.data.cwd.or(cwd.take());
                if let (Some(branch), Some(timestamp)) = (
                    context_branch,
                    record.timestamp.as_deref().and_then(parse_timestamp),
                ) {
                    branches_by_cwd
                        .entry(cwd.clone())
                        .or_default()
                        .observe(timestamp, &branch);
                }
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
    let store_branch = store_entry
        .and_then(|entry| entry.branch.as_deref())
        .map(str::trim);
    let mut pull_requests = PrCollector::default();
    if let Some(entry) = store_entry {
        for (number, repository) in &entry.pull_requests {
            pull_requests.observe(
                *number,
                repository
                    .as_deref()
                    .or(entry.repository.as_deref())
                    .unwrap_or_default(),
                None,
            );
        }
    }
    let pull_requests = pull_requests.finish(&mut result.diagnostics, path);
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
        // What the event log recorded for this directory wins; the store's single
        // branch only fills the absence, and only for the directory the store names (or
        // for the whole session when it never left one).
        let tracked = branches_by_cwd.remove(&cwd_key);
        let resolved = cwd_key.or_else(|| store_cwd.clone());
        let approximate_cwd = resolved.is_none();
        let resolved_cwd = resolved
            .unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned());
        let mut branches = tracked.map_or_else(Vec::new, |tracker| {
            tracker.finish(&mut result.diagnostics, path)
        });
        if branches.is_empty() && (!multiple || store_cwd.as_deref() == Some(resolved_cwd.as_str()))
        {
            branches = whole_session_branch(store_branch);
        }
        result.sessions.push(RawSession {
            provider: "copilot".to_string(),
            session_id: if multiple {
                format!("{base_id}:{resolved_cwd}")
            } else {
                base_id.clone()
            },
            source_file: path.to_path_buf(),
            cwd: resolved_cwd,
            repository_hint_cwd: None,
            points,
            exact_intervals: Vec::new(),
            human_points,
            token_events,
            is_subagent: false,
            approximate_cwd,
            version: version.clone(),
            branches,
            pull_requests: pull_requests.clone(),
        });
    }
    for (subagent_id, subagent_cwd, interval) in subagent_intervals {
        let resolved = Some(subagent_cwd)
            .filter(|value| !value.is_empty())
            .or_else(|| store_cwd.clone());
        let approximate_cwd = resolved.is_none();
        // A subagent runs in the checkout it was started from. Without events of its
        // own for that directory it takes the store's branch, as the session does.
        let branches = whole_session_branch(store_branch);
        result.sessions.push(RawSession {
            provider: "copilot".to_string(),
            session_id: format!("{base_id}:subagent:{subagent_id}"),
            source_file: path.to_path_buf(),
            cwd: resolved
                .unwrap_or_else(|| path.parent().unwrap_or(path).to_string_lossy().into_owned()),
            repository_hint_cwd: None,
            points: Vec::new(),
            exact_intervals: vec![interval],
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: true,
            approximate_cwd,
            version: version.clone(),
            branches,
            pull_requests: Vec::new(),
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
/// the disagreement is recorded so a wrong slug is visible rather than silently
/// preferred or silently dropped.
///
/// Recorded as a note, not a warning. The slug is Copilot's own metadata about a session
/// that was written and closed long ago: the user cannot correct it, nothing was lost,
/// and the report is already right — so the only thing a `Warning:` line achieves is to
/// reappear on every run until the reader stops looking at warnings altogether. The
/// count reaches the summary and the detail reaches `--format json`.
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
    result.diagnostics.repository_conflicts += 1;
    result.diagnostics.note(format!(
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
/// store and may hold an approximate cwd the store can now resolve, and to v4 because a
/// v3 entry recorded the repository disagreement as a warning: transcripts do not change
/// after a session ends, so without this the cache would keep replaying the warning this
/// release exists to stop, on every run, forever. Bumped to v5 because the store's branch
/// and pull-request rows now reach the parse.
fn copilot_context_fingerprint(store: &CopilotSessionStore, path: &Path) -> String {
    match store.get(&copilot_session_directory(path)) {
        Some(entry) => format!(
            "copilot-v5:{}:{}:{}:{}",
            entry.cwd.as_deref().unwrap_or_default(),
            entry.repository.as_deref().unwrap_or_default(),
            entry.branch.as_deref().unwrap_or_default(),
            entry
                .pull_requests
                .iter()
                .map(|(number, repository)| format!(
                    "{number}@{}",
                    repository.as_deref().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => "copilot-v5:none".to_string(),
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
                    pull_requests: Vec::new(),
                },
            );
        }
        drop(rows);
        drop(statement);
        read_pull_request_refs(&connection, &mut store, diagnostics);
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

/// Adds `session_refs` pull-request rows to the sessions already read. The query names
/// three columns and filters on the type, so the `commit` rows (and anything else the
/// table later holds) are never delivered. A failure costs the pull requests only.
fn read_pull_request_refs(
    connection: &Connection,
    store: &mut CopilotSessionStore,
    diagnostics: &mut Diagnostics,
) {
    let result = (|| -> rusqlite::Result<u64> {
        if !sqlite_table_exists(connection, "session_refs")? {
            return Ok(0);
        }
        let columns = sqlite_columns(connection, "session_refs")?;
        if !["session_id", "ref_type", "ref_value"]
            .iter()
            .all(|name| columns.contains(*name))
        {
            return Ok(0);
        }
        let mut statement = connection.prepare(
            "SELECT \"session_id\", \"ref_value\" FROM session_refs WHERE \"ref_type\" = 'pr'",
        )?;
        let mut rows = statement.query([])?;
        let mut unreadable = 0_u64;
        while let Some(row) = rows.next()? {
            let (Some(id), Some(value)) = (sqlite_text(row, 0), sqlite_text(row, 1)) else {
                unreadable += 1;
                continue;
            };
            let Some(entry) = store.by_id.get_mut(&id) else {
                continue;
            };
            match parse_pull_request_ref(&value) {
                Some(parsed) if !entry.pull_requests.contains(&parsed) => {
                    entry.pull_requests.push(parsed);
                }
                Some(_) => {}
                None => unreadable += 1,
            }
        }
        for entry in store.by_id.values_mut() {
            entry.pull_requests.sort();
        }
        Ok(unreadable)
    })();
    match result {
        Ok(0) => {}
        Ok(unreadable) => diagnostics.note(format!(
            "{unreadable} GitHub Copilot pull-request reference(s) were not a number, owner/repo#number or pull-request URL and were not stored"
        )),
        Err(error) => diagnostics.warn(format!(
            "GitHub Copilot pull-request references ignored: {error}"
        )),
    }
}

/// A pull request out of a `session_refs` value: a bare number, `owner/repo#number`, or
/// a `.../owner/repo/pull/number` URL. Only the number and the `owner/repo` pair are
/// kept; the URL itself, its host and anything after the number are discarded.
fn parse_pull_request_ref(value: &str) -> Option<(u64, Option<String>)> {
    let value = value.trim();
    if value.is_empty() || value.len() > 512 {
        return None;
    }
    let digits = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
            .filter(|number| *number > 0)
    };
    if let Some(number) = digits(value) {
        return Some((number, None));
    }
    if let Some((head, tail)) = value.split_once("/pull/") {
        let number = digits(tail.split(['/', '?', '#']).next()?)?;
        let mut segments = head.rsplit('/');
        let repository = segments.next().filter(|segment| !segment.is_empty())?;
        let owner = segments.next().filter(|segment| !segment.is_empty())?;
        return Some((number, Some(format!("{owner}/{repository}"))));
    }
    let (repository, number) = value.rsplit_once('#')?;
    Some((digits(number)?, Some(repository.to_string()))).filter(|(_, repository)| {
        repository
            .as_deref()
            .is_some_and(|name| !name.is_empty() && !name.chars().any(char::is_control))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{fixture, utc};
    use std::fs;
    use tempfile::tempdir;

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
        // against: the directory decides, and the disagreement is recorded — as a note,
        // never a warning, because the tool already resolved it and the reader has
        // nothing to do about a slug Copilot wrote months ago.
        assert_eq!(1, parsed.diagnostics.repository_conflicts);
        assert_eq!(
            0, parsed.diagnostics.warning_count,
            "a resolved disagreement must not interrupt a clean run: {:?}",
            parsed.diagnostics.messages
        );
        let note = parsed
            .diagnostics
            .notes
            .iter()
            .find(|note| note.contains("Cratis/Chronicle"))
            .expect("the repository disagreement is recorded");
        assert!(note.contains("Arc"), "unexpected diagnostic {note}");
        assert!(
            !parsed
                .diagnostics
                .notes
                .iter()
                .chain(&parsed.diagnostics.messages)
                .any(|message| message.contains("SECRET")),
            "message bodies must never leave the database"
        );

        // The agreeing row says nothing at all, because there is nothing to record.
        let agreeing = root.path().join("session-state/780e0e2d/events.jsonl");
        fs::create_dir_all(agreeing.parent().unwrap()).unwrap();
        fs::write(&agreeing, records("780e0e2d")).unwrap();
        let parsed = parse_copilot_file(&agreeing, &store, MAX_JSONL_LINE_BYTES);
        assert_eq!(chronicle.to_string_lossy(), parsed.sessions[0].cwd);
        assert_eq!(0, parsed.diagnostics.repository_conflicts);
        assert!(
            parsed.diagnostics.messages.is_empty() && parsed.diagnostics.notes.is_empty(),
            "{:?} {:?}",
            parsed.diagnostics.messages,
            parsed.diagnostics.notes
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

    #[test]
    fn the_copilot_cli_fixture_parses_to_its_documented_timestamps_and_tokens() {
        let root = fixture("copilot/session-state");
        let files = discover_copilot_files(&root);
        assert_eq!(1, files.len());

        let parsed = parse_copilot_file(
            &files[0],
            &CopilotSessionStore::default(),
            MAX_JSONL_LINE_BYTES,
        );
        assert_eq!(6, parsed.records_read);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        assert_eq!(2, parsed.sessions.len());
        let foreground = parsed
            .sessions
            .iter()
            .find(|session| !session.is_subagent)
            .unwrap();
        assert_eq!("/home/example/project", foreground.cwd);
        assert!(!foreground.approximate_cwd);
        assert_eq!(
            vec![utc("2026-01-01T12:00:05Z"), utc("2026-01-01T12:06:00Z")],
            foreground
                .human_points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        // Usage arrives once, at shutdown, for the whole session.
        assert_eq!(1, foreground.token_events.len());
        let event = &foreground.token_events[0];
        assert_eq!(utc("2026-01-01T12:07:00Z"), event.timestamp);
        assert_eq!("gpt-fixture", event.model);
        assert_eq!(450, event.usage.input_tokens);
        assert_eq!(120, event.usage.output_tokens);
        assert_eq!(50, event.usage.cache_read_tokens);
        assert_eq!(10, event.usage.cache_creation_tokens);
        let subagent = parsed
            .sessions
            .iter()
            .find(|session| session.is_subagent)
            .unwrap();
        assert!(subagent.human_points.is_empty());
        assert_eq!(1, subagent.exact_intervals.len());
        assert_eq!(
            utc("2026-01-01T12:00:30Z"),
            subagent.exact_intervals[0].start
        );
        assert_eq!(utc("2026-01-01T12:01:00Z"), subagent.exact_intervals[0].end);
    }

    fn write_lines(path: &Path, records: &[serde_json::Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    #[test]
    fn copilot_context_events_give_branch_marks_at_changes_only() {
        let root = tempdir().unwrap();
        let path = root.path().join("s1/events.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "session.start", "timestamp": "2026-01-01T00:00:00Z",
                    "data": {"sessionId": "s1", "context": {"cwd": "/work", "branch": "main"}}}),
                serde_json::json!({"type": "user.message", "timestamp": "2026-01-01T00:00:10Z", "data": {}}),
                // Unchanged branch: no new mark.
                serde_json::json!({"type": "session.context_changed", "timestamp": "2026-01-01T00:01:00Z",
                    "data": {"cwd": "/work", "context": {"branch": "main"}}}),
                serde_json::json!({"type": "session.context_changed", "timestamp": "2026-01-01T00:02:00Z",
                    "data": {"cwd": "/work", "context": {"branch": "feat/x"}}}),
                serde_json::json!({"type": "assistant.message", "timestamp": "2026-01-01T00:03:00Z", "data": {}}),
            ],
        );
        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(1, parsed.sessions.len());
        assert_eq!(
            vec![
                (None, "main"),
                (Some(utc("2026-01-01T00:02:00Z")), "feat/x")
            ],
            parsed.sessions[0]
                .branches
                .iter()
                .map(|mark| (mark.from, mark.branch.as_str()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn copilot_branches_stay_with_the_directory_they_were_recorded_in() {
        let root = tempdir().unwrap();
        let path = root.path().join("s1/events.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "session.start", "timestamp": "2026-01-01T00:00:00Z",
                    "data": {"sessionId": "s1", "context": {"cwd": "/one", "branch": "main"}}}),
                serde_json::json!({"type": "user.message", "timestamp": "2026-01-01T00:00:10Z", "data": {}}),
                serde_json::json!({"type": "session.context_changed", "timestamp": "2026-01-01T00:02:00Z",
                    "data": {"cwd": "/two", "context": {"branch": "feat/y"}}}),
                serde_json::json!({"type": "user.message", "timestamp": "2026-01-01T00:03:00Z", "data": {}}),
            ],
        );
        let parsed =
            parse_copilot_file(&path, &CopilotSessionStore::default(), MAX_JSONL_LINE_BYTES);
        assert_eq!(2, parsed.sessions.len());
        for session in &parsed.sessions {
            let expected = if session.cwd == "/one" {
                "main"
            } else {
                "feat/y"
            };
            assert_eq!(1, session.branches.len());
            assert_eq!(expected, session.branches[0].branch);
        }
    }

    #[test]
    fn the_store_supplies_the_branch_and_pull_requests_without_reading_commit_refs() {
        let root = tempdir().unwrap();
        let database = root.path().join("session-store.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT,
                    host_type TEXT, branch TEXT, summary TEXT);
                CREATE TABLE session_refs (id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL, ref_type TEXT NOT NULL, ref_value TEXT NOT NULL,
                    turn_index INTEGER, UNIQUE(session_id, ref_type, ref_value));
                INSERT INTO sessions(id, cwd, repository, host_type, branch, summary)
                    VALUES ('s1', '/work', 'acme/api', 'github', 'feat/store', 'SUMMARY_SECRET');
                INSERT INTO session_refs(session_id, ref_type, ref_value) VALUES
                    ('s1', 'pr', '42'),
                    ('s1', 'pr', 'other/repo#7'),
                    ('s1', 'pr', 'https://example.invalid/own/proj/pull/9?x=URL_SECRET'),
                    ('s1', 'pr', 'not a reference'),
                    ('s1', 'commit', 'COMMIT_SECRET');",
            )
            .unwrap();
        drop(connection);
        let mut diagnostics = Diagnostics::default();
        let store = read_copilot_session_store(&database, &mut diagnostics);
        // The one value that is not a reference is said so, once.
        assert_eq!(1, diagnostics.note_count);
        assert_eq!(0, diagnostics.warning_count);

        let path = root.path().join("session-state/s1/events.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "session.start", "timestamp": "2026-01-01T00:00:00Z",
                    "data": {"sessionId": "s1", "context": {"cwd": "/work"}}}),
                serde_json::json!({"type": "user.message", "timestamp": "2026-01-01T00:00:10Z", "data": {}}),
            ],
        );
        let parsed = parse_copilot_file(&path, &store, MAX_JSONL_LINE_BYTES);
        let session = &parsed.sessions[0];
        // The event log recorded no branch, so the store's fills the absence.
        assert_eq!(1, session.branches.len());
        assert_eq!("feat/store", session.branches[0].branch);
        let mut links: Vec<_> = session
            .pull_requests
            .iter()
            .map(|link| (link.number, link.repository.as_str()))
            .collect();
        links.sort();
        assert_eq!(
            vec![(7, "other/repo"), (9, "own/proj"), (42, "acme/api")],
            links
        );
        let stored = serde_json::to_string(&parsed).unwrap();
        for secret in [
            "SUMMARY_SECRET",
            "COMMIT_SECRET",
            "URL_SECRET",
            "example.invalid",
        ] {
            assert!(!stored.contains(secret), "{secret} reached the parse");
        }
        // A change to the rows must invalidate the cached parse.
        assert_ne!(
            copilot_context_fingerprint(&store, &path),
            copilot_context_fingerprint(&CopilotSessionStore::default(), &path)
        );
    }

    #[test]
    fn the_event_log_branch_wins_over_the_store_branch() {
        let root = tempdir().unwrap();
        let path = root.path().join("s1/events.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "session.start", "timestamp": "2026-01-01T00:00:00Z",
                    "data": {"sessionId": "s1", "context": {"cwd": "/work", "branch": "from-log"}}}),
                serde_json::json!({"type": "user.message", "timestamp": "2026-01-01T00:00:10Z", "data": {}}),
            ],
        );
        let mut store = CopilotSessionStore::default();
        store.by_id.insert(
            "s1".to_string(),
            CopilotStoreSession {
                branch: Some("from-store".to_string()),
                ..CopilotStoreSession::default()
            },
        );
        let parsed = parse_copilot_file(&path, &store, MAX_JSONL_LINE_BYTES);
        assert_eq!("from-log", parsed.sessions[0].branches[0].branch);
    }

    #[test]
    fn pull_request_references_keep_only_a_number_and_a_repository() {
        assert_eq!(Some((12, None)), parse_pull_request_ref(" 12 "));
        assert_eq!(
            Some((3, Some("a/b".to_string()))),
            parse_pull_request_ref("a/b#3")
        );
        assert_eq!(
            Some((5, Some("own/proj".to_string()))),
            parse_pull_request_ref("https://host.invalid/own/proj/pull/5/files")
        );
        for rejected in ["", "0", "abc", "#4", "a/b#", "x/pull/", &"9".repeat(600)] {
            assert_eq!(None, parse_pull_request_ref(rejected), "{rejected:?}");
        }
    }
}
