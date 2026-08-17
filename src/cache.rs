use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::ai::{ParsedFile, file_time_range};

const PARSER_VERSION: i64 = 2;

type CacheRow = (
    i64,
    i64,
    i64,
    String,
    Option<i64>,
    Option<i64>,
    Vec<u8>,
    Option<Vec<u8>>,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
    size: i64,
    modified_ns: i64,
}

pub enum CacheLookup {
    Hit(ParsedFile),
    Pruned(ParsedFile),
    Miss,
}

pub struct TranscriptCache {
    connection: Connection,
    path: PathBuf,
}

impl TranscriptCache {
    pub fn open(path: &Path, rebuild: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create cache directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("cannot open transcript cache {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS transcript_cache (
                path TEXT NOT NULL,
                provider TEXT NOT NULL,
                source_size INTEGER NOT NULL,
                modified_ns INTEGER NOT NULL,
                parser_version INTEGER NOT NULL,
                context_fingerprint TEXT NOT NULL,
                min_micros INTEGER,
                max_micros INTEGER,
                payload BLOB NOT NULL,
                role_payload BLOB,
                PRIMARY KEY(path, provider)
            );
            CREATE INDEX IF NOT EXISTS transcript_cache_range
                ON transcript_cache(provider, min_micros, max_micros);",
        )?;
        let has_role_payload = {
            let mut statement = connection.prepare("PRAGMA table_info(transcript_cache)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .any(|name| name == "role_payload")
        };
        if !has_role_payload {
            connection.execute(
                "ALTER TABLE transcript_cache ADD COLUMN role_payload BLOB",
                [],
            )?;
        }
        if rebuild {
            connection.execute("DELETE FROM transcript_cache", [])?;
        }
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lookup(
        &self,
        path: &Path,
        provider: &str,
        context_fingerprint: &str,
        stamp: FileStamp,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<CacheLookup> {
        let canonical = canonical(path);
        let row: Option<CacheRow> = self
            .connection
            .query_row(
                "SELECT source_size, modified_ns, parser_version, context_fingerprint,
                        min_micros, max_micros, payload, role_payload
                   FROM transcript_cache
                  WHERE path = ?1 AND provider = ?2",
                params![canonical, provider],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            size,
            modified_ns,
            parser_version,
            context,
            minimum,
            maximum,
            payload,
            role_payload,
        )) = row
        else {
            return Ok(CacheLookup::Miss);
        };
        if size != stamp.size
            || modified_ns != stamp.modified_ns
            || parser_version != PARSER_VERSION
            || context != context_fingerprint
        {
            return Ok(CacheLookup::Miss);
        }
        let outside_lower = since
            .zip(maximum)
            .is_some_and(|(bound, maximum)| maximum < bound.timestamp_micros());
        let outside_upper = until
            .zip(minimum)
            .is_some_and(|(bound, minimum)| minimum >= bound.timestamp_micros());
        if outside_lower || outside_upper {
            let mut parsed: ParsedFile =
                serde_json::from_slice(role_payload.as_deref().unwrap_or(&payload))
                    .context("cached transcript role payload is invalid")?;
            for session in &mut parsed.sessions {
                session.points.clear();
                session.exact_intervals.clear();
                session.human_points.clear();
                session.token_events.clear();
            }
            return Ok(CacheLookup::Pruned(parsed));
        }
        let parsed =
            serde_json::from_slice(&payload).context("cached transcript payload is invalid")?;
        Ok(CacheLookup::Hit(parsed))
    }

    pub fn put(
        &mut self,
        path: &Path,
        provider: &str,
        context_fingerprint: &str,
        stamp: FileStamp,
        parsed: &ParsedFile,
    ) -> Result<()> {
        let payload = serde_json::to_vec(parsed)?;
        let mut roles = ParsedFile {
            sessions: parsed.sessions.clone(),
            diagnostics: parsed.diagnostics.clone(),
        };
        for session in &mut roles.sessions {
            session.points.clear();
            session.exact_intervals.clear();
            session.human_points.clear();
            session.token_events.clear();
        }
        let role_payload = serde_json::to_vec(&roles)?;
        let (minimum, maximum) = file_time_range(&parsed.sessions);
        self.connection.execute(
            "INSERT INTO transcript_cache(
                path, provider, source_size, modified_ns, parser_version,
                context_fingerprint, min_micros, max_micros, payload, role_payload
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(path, provider) DO UPDATE SET
                source_size = excluded.source_size,
                modified_ns = excluded.modified_ns,
                parser_version = excluded.parser_version,
                context_fingerprint = excluded.context_fingerprint,
                min_micros = excluded.min_micros,
                max_micros = excluded.max_micros,
                payload = excluded.payload,
                role_payload = excluded.role_payload",
            params![
                canonical(path),
                provider,
                stamp.size,
                stamp.modified_ns,
                PARSER_VERSION,
                context_fingerprint,
                minimum.map(|value| value.timestamp_micros()),
                maximum.map(|value| value.timestamp_micros()),
                payload,
                role_payload,
            ],
        )?;
        Ok(())
    }
}

pub fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = path.metadata().ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let nanoseconds = modified
        .as_nanos()
        .min(i64::MAX as u128)
        .try_into()
        .unwrap_or(i64::MAX);
    Some(FileStamp {
        size: metadata.len().min(i64::MAX as u64) as i64,
        modified_ns: nanoseconds,
    })
}

pub fn file_context(path: &Path) -> String {
    file_stamp(path).map_or_else(
        || "missing".to_string(),
        |stamp| format!("{}:{}", stamp.size, stamp.modified_ns),
    )
}

fn canonical(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::model::{ActivityPoint, Diagnostics, RawSession};
    use crate::timeutil::parse_timestamp;
    use tempfile::tempdir;

    fn parsed(source: &Path) -> ParsedFile {
        ParsedFile {
            sessions: vec![RawSession {
                provider: "codex".into(),
                session_id: "session".into(),
                source_file: source.to_path_buf(),
                cwd: "/tmp/repo".into(),
                points: vec![ActivityPoint {
                    timestamp: parse_timestamp("2026-01-01T00:00:00Z").unwrap(),
                    model: "gpt".into(),
                }],
                exact_intervals: vec![],
                human_points: vec![],
                token_events: vec![],
                is_subagent: true,
                approximate_cwd: false,
                version: None,
            }],
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn cache_hits_prunes_and_invalidates_changed_files() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("rollout-test.jsonl");
        fs::write(&source, "{}\n").unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let mut cache = TranscriptCache::open(&cache_path, false).unwrap();
        let original_stamp = file_stamp(&source).unwrap();
        cache
            .put(
                &source,
                "codex",
                "context",
                original_stamp,
                &parsed(&source),
            )
            .unwrap();

        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", original_stamp, None, None)
                .unwrap(),
            CacheLookup::Hit(_)
        ));
        let after = parse_timestamp("2026-02-01T00:00:00Z").unwrap();
        let CacheLookup::Pruned(pruned) = cache
            .lookup(
                &source,
                "codex",
                "context",
                original_stamp,
                Some(after),
                None,
            )
            .unwrap()
        else {
            panic!("expected range-pruned cache hit");
        };
        assert_eq!(1, pruned.sessions.len());
        assert!(pruned.sessions[0].points.is_empty());
        assert!(pruned.sessions[0].is_subagent);

        fs::write(&source, "{}\n{}\n").unwrap();
        let changed_stamp = file_stamp(&source).unwrap();
        assert_ne!(original_stamp, changed_stamp);
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", changed_stamp, None, None)
                .unwrap(),
            CacheLookup::Miss
        ));
    }

    #[test]
    fn rebuild_clears_existing_entries() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("session.jsonl");
        fs::write(&source, "{}\n").unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let stamp = file_stamp(&source).unwrap();
        TranscriptCache::open(&cache_path, false)
            .unwrap()
            .put(&source, "codex", "context", stamp, &parsed(&source))
            .unwrap();
        let cache = TranscriptCache::open(&cache_path, true).unwrap();
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", stamp, None, None)
                .unwrap(),
            CacheLookup::Miss
        ));
    }
}
