//! OpenCode: a SQLite database read structurally and read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use super::{
    ParsedFile, load_files, safe_model, sqlite_columns, sqlite_number, sqlite_table_exists,
    sqlite_text, unknown_model,
};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, RawSession, Session};
use crate::paths::PathResolver;
use crate::timeutil::parse_epoch_milliseconds;

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
    let parsed = (|| -> rusqlite::Result<(Vec<RawSession>, u64, u64)> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if !sqlite_table_exists(&connection, "session")? {
            return Ok((Vec::new(), 0, 0));
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
        // Message rows seen, used or not. Sessions alone prove nothing about drift: an
        // opened session with no messages is ordinary, whereas messages that yield no
        // timestamp mean the columns they are read from no longer mean what they did.
        let mut messages_read = 0_u64;
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
                    messages_read += 1;
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
                    messages_read += 1;
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
                    repository_hint_cwd: None,
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
            messages_read,
        ))
    })();
    match parsed {
        Ok((sessions, skipped_rows, messages_read)) => {
            result.sessions = sessions;
            result.records_read = messages_read;
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ai::{fixture, utc};
    use std::fs;
    use tempfile::tempdir;

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
    fn the_opencode_fixture_schema_parses_to_its_documented_timestamps() {
        let root = tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let sql = fs::read_to_string(fixture("opencode/session.sql")).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch(&sql)
            .unwrap();

        let parsed = parse_opencode_database(&path);
        assert_eq!(0, parsed.diagnostics.malformed_lines);
        // Four current messages and two legacy ones.
        assert_eq!(6, parsed.records_read);
        assert_eq!(3, parsed.sessions.len());
        let session = |id: &str| {
            parsed
                .sessions
                .iter()
                .find(|item| item.session_id == id)
                .unwrap()
        };

        let current = session("current");
        assert_eq!("/home/example/project", current.cwd);
        assert!(!current.is_subagent);
        assert_eq!(
            vec![
                utc("2026-01-01T15:00:00Z"),
                utc("2026-01-01T15:00:20Z"),
                utc("2026-01-01T15:05:00Z"),
            ],
            current
                .points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(2, current.human_points.len());
        assert_eq!("anthropic/model-b", current.points[1].model);

        // A delegated session is activity but never a prompt.
        let delegated = session("delegated");
        assert!(delegated.is_subagent);
        assert_eq!(1, delegated.points.len());
        assert!(delegated.human_points.is_empty());

        // A session `session_message` does not cover is read from the legacy table.
        let legacy = session("legacy");
        assert_eq!(
            vec![utc("2026-01-01T16:00:00Z"), utc("2026-01-01T16:00:30Z")],
            legacy
                .points
                .iter()
                .map(|point| point.timestamp)
                .collect::<Vec<_>>()
        );
        assert_eq!(1, legacy.human_points.len());
        assert_eq!("openai/model-c", legacy.points[1].model);
    }
}
