//! The uncached reader behind `--describe sessions`: tool-generated session
//! titles from the named fields only, never prompt text.
//!
//! This is deliberately not a parser in the sense of the sibling modules. It
//! runs after the pipeline, over the transcripts of sessions that are already
//! in the window, only when the user asked, and returns nothing that is stored:
//! the titles go to the output and are gone. `last-prompt`, `queue-operation`,
//! Codex `title` / `preview` / `first_user_message` and every message body are
//! never selected, and a line is only decoded after a substring prefilter says
//! it could be a title record, into a struct that has no field for anything
//! else. See `docs/privacy.md`.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use super::{
    MAX_JSONL_LINE_BYTES, read_bounded_line, sqlite_columns, sqlite_table_exists, sqlite_text,
};
use crate::model::Session;
use crate::output::safe_text;

/// The providers that have a title source at all.
pub(crate) const TITLE_PROVIDERS: &[&str] = &["claude", "codex", "copilot", "opencode", "pi"];

/// What `--describe sessions` reads without being told. Codex is left out
/// because its `title` and `preview` columns are the first prompt verbatim and
/// its `name`s often start like the prompt too; it is read only when named.
pub(crate) const DEFAULT_PROVIDERS: &[&str] = &["claude", "copilot", "opencode", "pi"];

/// A title is a label, not a document.
pub(crate) const MAX_TITLE_CHARS: usize = 200;

/// `(provider, session_id)`, as `Session` carries them.
pub(crate) type SessionKey = (String, String);

/// The title of every session in `sessions` that has one, among the named
/// `providers`. A source that cannot be read costs its own titles and adds a
/// line to `warnings`; it never stops the run.
pub(crate) fn read_titles(
    sessions: &[&Session],
    providers: &BTreeSet<String>,
    codex_db: Option<&Path>,
    warnings: &mut Vec<String>,
) -> HashMap<SessionKey, String> {
    let mut titles = HashMap::new();
    let mut by_file: HashMap<PathBuf, Option<String>> = HashMap::new();
    // Database-backed providers: the wanted ids per database, read in one
    // pass each.
    let mut opencode: HashMap<PathBuf, HashMap<String, Vec<SessionKey>>> = HashMap::new();
    let mut copilot: HashMap<PathBuf, HashMap<String, Vec<SessionKey>>> = HashMap::new();
    let mut codex: HashMap<String, Vec<SessionKey>> = HashMap::new();
    let mut codex_homes: BTreeSet<PathBuf> = BTreeSet::new();
    for session in sessions {
        if !providers.contains(&session.provider) {
            continue;
        }
        // Sessions that came from a bundle have no transcript on this machine
        // (an empty source file), so they have no title to read. That is not
        // a failure, and one warning per session would bury the real ones.
        if session.source_file.as_os_str().is_empty() {
            continue;
        }
        let key = (session.provider.clone(), session.session_id.clone());
        match session.provider.as_str() {
            "claude" | "pi" => {
                let title = by_file
                    .entry(session.source_file.clone())
                    .or_insert_with(|| {
                        let reader = if session.provider == "claude" {
                            claude_title
                        } else {
                            pi_title
                        };
                        match reader(&session.source_file) {
                            Ok(title) => title,
                            Err(error) => {
                                warnings.push(format!(
                                    "session titles unavailable for {}: {error}",
                                    session.source_file.display()
                                ));
                                None
                            }
                        }
                    })
                    .clone();
                if let Some(title) = title {
                    titles.insert(key, title);
                }
            }
            "opencode" => {
                opencode
                    .entry(session.source_file.clone())
                    .or_default()
                    .entry(session.session_id.clone())
                    .or_default()
                    .push(key);
            }
            "copilot" => {
                // The session's directory name is the store's `id`; the
                // session id itself carries a working-directory suffix when a
                // session moved between checkouts.
                let directory = session
                    .source_file
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned());
                match (copilot_store(&session.source_file), directory) {
                    (Some(store), Some(directory)) => {
                        copilot
                            .entry(store)
                            .or_default()
                            .entry(directory)
                            .or_default()
                            .push(key);
                    }
                    _ => warnings.push(format!(
                        "no Copilot session store beside {}; no title for that session",
                        session.source_file.display()
                    )),
                }
            }
            "codex" => {
                codex
                    .entry(session.session_id.clone())
                    .or_default()
                    .push(key);
                for ancestor in session.source_file.ancestors() {
                    if ancestor.file_name().is_some_and(|name| name == "sessions")
                        && let Some(home) = ancestor.parent()
                    {
                        codex_homes.insert(home.to_path_buf());
                    }
                }
            }
            _ => {}
        }
    }
    for (database, wanted) in opencode {
        match opencode_titles(&database, &wanted) {
            Ok(found) => titles.extend(found),
            Err(error) => warnings.push(format!(
                "OpenCode session titles unavailable: {}: {error}",
                database.display()
            )),
        }
    }
    for (store, wanted) in copilot {
        match copilot_titles(&store, &wanted) {
            Ok(found) => titles.extend(found),
            Err(error) => warnings.push(format!(
                "Copilot session titles unavailable: {}: {error}",
                store.display()
            )),
        }
    }
    if !codex.is_empty() {
        if let Some(parent) = codex_db.and_then(Path::parent) {
            codex_homes.insert(parent.to_path_buf());
        }
        // The database first, then the index: both hold names the user or the
        // tool gave the thread, and the index is appended to on a rename.
        if let Some(database) = codex_db.filter(|path| path.is_file()) {
            match codex_thread_names(database, &codex) {
                Ok(found) => titles.extend(found),
                Err(error) => warnings.push(format!(
                    "Codex thread names unavailable: {}: {error}",
                    database.display()
                )),
            }
        }
        for home in &codex_homes {
            let index = home.join("session_index.jsonl");
            if !index.is_file() {
                continue;
            }
            match codex_index_names(&index, &codex) {
                Ok(found) => titles.extend(found),
                Err(error) => warnings.push(format!(
                    "Codex session index unavailable: {}: {error}",
                    index.display()
                )),
            }
        }
    }
    titles
}

/// One title, made safe to print: control and direction characters replaced,
/// whitespace collapsed, bounded. `None` when nothing is left.
pub(crate) fn clean_title(raw: &str) -> Option<String> {
    let single_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = safe_text(&single_line)
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    (!bounded.is_empty()).then_some(bounded)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Streams the lines of `path`, handing those that contain any of `prefilter`
/// to `decode`. The line is never decoded otherwise; an oversized or
/// undecodable line is skipped, since it can only cost a title.
fn for_prefiltered_lines(
    path: &Path,
    prefilter: &[&[u8]],
    mut decode: impl FnMut(&[u8]),
) -> std::io::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    while let Some((line, oversized)) = read_bounded_line(&mut reader, MAX_JSONL_LINE_BYTES)? {
        if oversized || !prefilter.iter().any(|needle| contains(&line, needle)) {
            continue;
        }
        decode(&line);
    }
    Ok(())
}

/// The only fields of a Claude record this reader can see.
#[derive(Deserialize)]
struct ClaudeTitleRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    summary: Option<String>,
}

/// `ai-title.aiTitle`, `custom-title.customTitle` and the legacy
/// `summary.summary`; the last one in the file wins.
fn claude_title(path: &Path) -> std::io::Result<Option<String>> {
    let mut title = None;
    for_prefiltered_lines(
        path,
        &[b"ai-title", b"custom-title", b"\"summary\""],
        |line| {
            let Ok(record) = serde_json::from_slice::<ClaudeTitleRecord>(line) else {
                return;
            };
            let value = match record.kind.as_deref() {
                Some("ai-title") => record.ai_title,
                Some("custom-title") => record.custom_title,
                Some("summary") => record.summary,
                _ => None,
            };
            if let Some(cleaned) = value.as_deref().and_then(clean_title) {
                title = Some(cleaned);
            }
        },
    )?;
    Ok(title)
}

#[derive(Deserialize)]
struct PiInfoRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
}

/// `session_info.name`; the last one wins.
fn pi_title(path: &Path) -> std::io::Result<Option<String>> {
    let mut title = None;
    for_prefiltered_lines(path, &[b"session_info"], |line| {
        let Ok(record) = serde_json::from_slice::<PiInfoRecord>(line) else {
            return;
        };
        if record.kind.as_deref() == Some("session_info")
            && let Some(cleaned) = record.name.as_deref().and_then(clean_title)
        {
            title = Some(cleaned);
        }
    })?;
    Ok(title)
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// Reads `(id, <column>)` pairs of `table`, but only when the table and both
/// columns exist: a newer or older schema costs the titles, not the run. The
/// column name is one of this module's constants, never user input.
fn id_and_text(
    connection: &Connection,
    table: &str,
    id_column: &str,
    text_column: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    if !sqlite_table_exists(connection, table)? {
        return Ok(Vec::new());
    }
    let columns = sqlite_columns(connection, table)?;
    if !columns.contains(id_column) || !columns.contains(text_column) {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(&format!(
        "SELECT \"{id_column}\", \"{text_column}\" FROM \"{table}\""
    ))?;
    let mut rows = statement.query([])?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()? {
        if let (Some(id), Some(text)) = (sqlite_text(row, 0), sqlite_text(row, 1)) {
            found.push((id, text));
        }
    }
    Ok(found)
}

fn resolve(
    rows: Vec<(String, String)>,
    wanted: &HashMap<String, Vec<SessionKey>>,
) -> HashMap<SessionKey, String> {
    let mut found = HashMap::new();
    for (id, text) in rows {
        let (Some(keys), Some(title)) = (wanted.get(&id), clean_title(&text)) else {
            continue;
        };
        for key in keys {
            found.insert(key.clone(), title.clone());
        }
    }
    found
}

/// `session.title`, only when the column exists.
fn opencode_titles(
    database: &Path,
    wanted: &HashMap<String, Vec<SessionKey>>,
) -> rusqlite::Result<HashMap<SessionKey, String>> {
    let connection = open_read_only(database)?;
    Ok(resolve(
        id_and_text(&connection, "session", "id", "title")?,
        wanted,
    ))
}

/// The store's `sessions.summary`, best effort: its origin is not documented,
/// and none of the summaries seen was a prefix of the first prompt.
fn copilot_titles(
    store: &Path,
    wanted: &HashMap<String, Vec<SessionKey>>,
) -> rusqlite::Result<HashMap<SessionKey, String>> {
    let connection = open_read_only(store)?;
    Ok(resolve(
        id_and_text(&connection, "sessions", "id", "summary")?,
        wanted,
    ))
}

/// `threads.name` only. `title`, `preview` and `first_user_message` are the
/// first prompt, so the query names `id` and `name` and nothing else.
fn codex_thread_names(
    database: &Path,
    wanted: &HashMap<String, Vec<SessionKey>>,
) -> rusqlite::Result<HashMap<SessionKey, String>> {
    let connection = open_read_only(database)?;
    Ok(resolve(
        id_and_text(&connection, "threads", "id", "name")?,
        wanted,
    ))
}

#[derive(Deserialize)]
struct CodexIndexRecord {
    id: Option<String>,
    thread_name: Option<String>,
}

/// `session_index.jsonl {id, thread_name}`; the last line for an id wins.
fn codex_index_names(
    index: &Path,
    wanted: &HashMap<String, Vec<SessionKey>>,
) -> std::io::Result<HashMap<SessionKey, String>> {
    let mut rows = Vec::new();
    for_prefiltered_lines(index, &[b"thread_name"], |line| {
        if let Ok(record) = serde_json::from_slice::<CodexIndexRecord>(line)
            && let (Some(id), Some(name)) = (record.id, record.thread_name)
        {
            rows.push((id, name));
        }
    })?;
    Ok(resolve(rows, wanted))
}

/// The Copilot CLI's `session-store.db` sits beside `session-state/`, so it is
/// found from the transcript path alone: the nearest ancestor from the
/// `session-state` level up whose parent holds one.
fn copilot_store(source_file: &Path) -> Option<PathBuf> {
    source_file
        .ancestors()
        .skip(2)
        .take(4)
        .filter_map(|ancestor| ancestor.parent())
        .map(|home| home.join("session-store.db"))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn session(provider: &str, id: &str, source_file: PathBuf) -> Session {
        Session {
            provider: provider.into(),
            session_id: id.into(),
            cwd: "/work".into(),
            repo: "work".into(),
            repo_id: "work".into(),
            root: "root".into(),
            points: Vec::new(),
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: false,
            source_file,
            branches: Vec::new(),
            branch_source: Default::default(),
            pull_requests: Vec::new(),
        }
    }

    #[test]
    fn imported_sessions_have_no_source_and_no_warning() {
        let sessions = [
            session("claude", "a", PathBuf::new()),
            session("pi", "b", PathBuf::new()),
            session("copilot", "c", PathBuf::new()),
            session("codex", "d", PathBuf::new()),
            session("opencode", "e", PathBuf::new()),
        ];
        let refs: Vec<&Session> = sessions.iter().collect();
        let all: BTreeSet<String> = TITLE_PROVIDERS
            .iter()
            .map(|name| name.to_string())
            .collect();
        let mut warnings = Vec::new();
        let titles = read_titles(&refs, &all, None, &mut warnings);
        assert!(titles.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    fn only(provider: &str) -> BTreeSet<String> {
        BTreeSet::from([provider.to_string()])
    }

    fn write_lines(path: &Path, lines: &[serde_json::Value]) {
        fs::write(
            path,
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    #[test]
    fn claude_reads_the_last_of_the_three_title_records_and_nothing_else() {
        let root = tempdir().unwrap();
        let path = root.path().join("s.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "user", "message": {"content": "PROMPT_SECRET summary"}}),
                serde_json::json!({"type": "summary", "summary": "Legacy summary"}),
                serde_json::json!({"type": "ai-title", "aiTitle": "Generated title"}),
                serde_json::json!({"type": "last-prompt", "lastPrompt": "LAST_SECRET"}),
                serde_json::json!({"type": "queue-operation", "content": "QUEUE_SECRET"}),
                serde_json::json!({"type": "custom-title", "customTitle": "Renamed by me"}),
                serde_json::json!({"type": "assistant", "summary": "NOT_A_TITLE"}),
            ],
        );
        let sessions = [session("claude", "s", path)];
        let refs: Vec<&Session> = sessions.iter().collect();
        let mut warnings = Vec::new();
        let titles = read_titles(&refs, &only("claude"), None, &mut warnings);
        assert_eq!(
            Some(&"Renamed by me".to_string()),
            titles.get(&("claude".to_string(), "s".to_string()))
        );
        assert!(warnings.is_empty());
        let everything = format!("{titles:?}");
        for secret in [
            "PROMPT_SECRET",
            "LAST_SECRET",
            "QUEUE_SECRET",
            "NOT_A_TITLE",
        ] {
            assert!(!everything.contains(secret));
        }
    }

    #[test]
    fn a_provider_that_was_not_asked_for_is_not_read() {
        let root = tempdir().unwrap();
        let path = root.path().join("s.jsonl");
        write_lines(
            &path,
            &[serde_json::json!({"type": "ai-title", "aiTitle": "T"})],
        );
        let sessions = [session("claude", "s", path)];
        let refs: Vec<&Session> = sessions.iter().collect();
        let titles = read_titles(&refs, &only("pi"), None, &mut Vec::new());
        assert!(titles.is_empty());
    }

    #[test]
    fn pi_reads_session_info_name() {
        let root = tempdir().unwrap();
        let path = root.path().join("s.jsonl");
        write_lines(
            &path,
            &[
                serde_json::json!({"type": "session_info", "name": "First"}),
                serde_json::json!({"type": "message", "message": {"content": "session_info BODY"}}),
                serde_json::json!({"type": "session_info", "name": "Second"}),
            ],
        );
        let sessions = [session("pi", "p", path)];
        let refs: Vec<&Session> = sessions.iter().collect();
        let titles = read_titles(&refs, &only("pi"), None, &mut Vec::new());
        assert_eq!(Some("Second"), titles.values().next().map(String::as_str));
    }

    #[test]
    fn opencode_title_is_read_only_when_the_column_exists() {
        let root = tempdir().unwrap();
        let with = root.path().join("with.db");
        let connection = Connection::open(&with).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT, directory TEXT, title TEXT);
                 INSERT INTO session VALUES ('a', '/x', 'Fix the parser');
                 INSERT INTO session VALUES ('b', '/x', 'Other session');",
            )
            .unwrap();
        drop(connection);
        let without = root.path().join("without.db");
        let connection = Connection::open(&without).unwrap();
        connection
            .execute_batch("CREATE TABLE session (id TEXT, directory TEXT);")
            .unwrap();
        drop(connection);
        let sessions = [
            session("opencode", "a", with),
            session("opencode", "z", without),
        ];
        let refs: Vec<&Session> = sessions.iter().collect();
        let mut warnings = Vec::new();
        let titles = read_titles(&refs, &only("opencode"), None, &mut warnings);
        assert_eq!(1, titles.len());
        assert_eq!(
            "Fix the parser",
            titles[&("opencode".to_string(), "a".to_string())]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn copilot_reads_the_store_summary_for_the_session_directory() {
        let root = tempdir().unwrap();
        let events = root.path().join("session-state/abc/events.jsonl");
        fs::create_dir_all(events.parent().unwrap()).unwrap();
        fs::write(&events, "").unwrap();
        let connection = Connection::open(root.path().join("session-store.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (id TEXT, summary TEXT, first_message TEXT);
                 INSERT INTO sessions VALUES ('abc', 'Wire the exporter', 'FIRST_PROMPT_SECRET');",
            )
            .unwrap();
        drop(connection);
        let sessions = [session("copilot", "abc:/some/cwd", events)];
        let refs: Vec<&Session> = sessions.iter().collect();
        let titles = read_titles(&refs, &only("copilot"), None, &mut Vec::new());
        assert_eq!(
            Some("Wire the exporter"),
            titles.values().next().map(String::as_str)
        );
    }

    #[test]
    fn codex_reads_the_name_and_the_index_but_never_the_prompt_columns() {
        let root = tempdir().unwrap();
        let database = root.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT, name TEXT, title TEXT, preview TEXT, first_user_message TEXT);
                 INSERT INTO threads VALUES ('t1', 'Named thread', 'TITLE_SECRET', 'PREVIEW_SECRET', 'FIRST_SECRET');
                 INSERT INTO threads VALUES ('t2', NULL, 'TITLE2_SECRET', 'P', 'F');",
            )
            .unwrap();
        drop(connection);
        fs::write(
            root.path().join("session_index.jsonl"),
            "{\"id\":\"t2\",\"thread_name\":\"From the index\"}\n",
        )
        .unwrap();
        let rollout = root.path().join("sessions/2026/01/01/rollout.jsonl");
        let sessions = [
            session("codex", "t1", rollout.clone()),
            session("codex", "t2", rollout),
            session("codex", "t3", root.path().join("x.jsonl")),
        ];
        let refs: Vec<&Session> = sessions.iter().collect();
        let titles = read_titles(&refs, &only("codex"), Some(&database), &mut Vec::new());
        assert_eq!(2, titles.len());
        assert_eq!(
            "Named thread",
            titles[&("codex".to_string(), "t1".to_string())]
        );
        assert_eq!(
            "From the index",
            titles[&("codex".to_string(), "t2".to_string())]
        );
        assert!(!format!("{titles:?}").contains("SECRET"));
    }

    #[test]
    fn titles_are_one_bounded_line_without_control_characters() {
        let cleaned = clean_title("  a\u{1b}[31m\nb\u{202e}c  ").unwrap();
        assert!(!cleaned.chars().any(char::is_control));
        assert!(!cleaned.contains('\u{202e}'));
        assert!(!cleaned.contains('\n'));
        let long = "x".repeat(500);
        assert_eq!(MAX_TITLE_CHARS, clean_title(&long).unwrap().chars().count());
        assert_eq!(None, clean_title("   "));
    }

    #[test]
    fn an_unreadable_transcript_is_a_warning_not_a_failure() {
        let root = tempdir().unwrap();
        let sessions = [session("claude", "s", root.path().join("missing.jsonl"))];
        let refs: Vec<&Session> = sessions.iter().collect();
        let mut warnings = Vec::new();
        let titles = read_titles(&refs, &only("claude"), None, &mut warnings);
        assert!(titles.is_empty());
        assert_eq!(1, warnings.len());
    }
}
