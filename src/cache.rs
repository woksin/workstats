use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::ai::{ParsedFile, file_time_range};
use crate::paths::RepositoryHistoryEntry;

/// Bumped whenever a parser changes what it stores or how a range is derived, so that
/// entries written by an older build are recomputed instead of answered from. Version 4
/// retains Pi parent-CWD repository hints for deleted temporary worktrees.
///
/// This is the manual half of the validity key. Forgetting to bump it serves stale
/// parses, which is why `parser_stamp` also mixes in the crate version: a release
/// invalidates every older entry whether or not anyone remembered.
const PARSER_VERSION: i64 = 4;

/// What a cache entry must carry to be answered from: `PARSER_VERSION` joined to the
/// crate version that wrote it.
///
/// Mixing in `CARGO_PKG_VERSION` makes invalidation automatic at every release. The cost
/// is one re-parse after each upgrade, which is cheap next to serving a parse that an
/// older build got wrong. The value always contains a dot-separated version, so SQLite
/// never mistakes it for a number and stores it as text even in a column an older build
/// declared `INTEGER`.
fn parser_stamp() -> &'static str {
    static STAMP: LazyLock<String> =
        LazyLock::new(|| format!("{PARSER_VERSION}+{}", env!("CARGO_PKG_VERSION")));
    &STAMP
}

type CacheRow = (
    i64,
    i64,
    String,
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
                parser_version TEXT NOT NULL,
                context_fingerprint TEXT NOT NULL,
                min_micros INTEGER,
                max_micros INTEGER,
                payload BLOB NOT NULL,
                role_payload BLOB,
                PRIMARY KEY(path, provider)
            );
            CREATE INDEX IF NOT EXISTS transcript_cache_range
                ON transcript_cache(provider, min_micros, max_micros);
            CREATE TABLE IF NOT EXISTS repository_identity_history (
                cwd_key TEXT NOT NULL,
                natural_id TEXT NOT NULL,
                label TEXT NOT NULL,
                repo_path TEXT NOT NULL,
                PRIMARY KEY(cwd_key, natural_id)
            );
            CREATE INDEX IF NOT EXISTS repository_identity_history_cwd
                ON repository_identity_history(cwd_key);",
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

    pub fn load_repository_history(&self) -> Result<Vec<RepositoryHistoryEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT cwd_key, natural_id, label, repo_path
               FROM repository_identity_history
              ORDER BY cwd_key, natural_id",
        )?;
        let entries = statement
            .query_map([], |row| {
                Ok(RepositoryHistoryEntry {
                    cwd_key: row.get(0)?,
                    natural_id: row.get(1)?,
                    label: row.get(2)?,
                    repo_path: PathBuf::from(row.get::<_, String>(3)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    }

    pub fn remember_repository_identities(
        &mut self,
        entries: &[RepositoryHistoryEntry],
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let transaction = self.connection.transaction()?;
        let mut written = 0;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO repository_identity_history(cwd_key, natural_id, label, repo_path)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(cwd_key, natural_id) DO UPDATE SET
                    label = excluded.label,
                    repo_path = excluded.repo_path",
            )?;
            for entry in entries {
                written += statement.execute(params![
                    entry.cwd_key,
                    entry.natural_id,
                    entry.label,
                    entry.repo_path.to_string_lossy(),
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
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
                // Cast because a cache written before the crate version joined the key
                // holds an integer here, and reading that as text would be an error
                // rather than the plain miss it should be.
                "SELECT source_size, modified_ns, CAST(parser_version AS TEXT), context_fingerprint,
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
            || parser_version != parser_stamp()
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
            // Both survive pruning: the drift check reads them from a pruned hit, whose
            // timestamps are gone by design.
            records_read: parsed.records_read,
            unrecognized: parsed.unrecognized,
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
                parser_stamp(),
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

    use crate::model::{ActivityPoint, Diagnostics, RawSession, TokenEvent, TokenUsage};
    use crate::timeutil::parse_timestamp;
    use tempfile::tempdir;

    fn parsed(source: &Path) -> ParsedFile {
        ParsedFile {
            sessions: vec![RawSession {
                provider: "codex".into(),
                session_id: "session".into(),
                source_file: source.to_path_buf(),
                cwd: "/tmp/repo".into(),
                repository_hint_cwd: Some("/tmp/parent".into()),
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
            ..ParsedFile::default()
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
        assert_eq!(
            Some("/tmp/parent"),
            pruned.sessions[0].repository_hint_cwd.as_deref()
        );

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
    fn a_token_event_after_the_last_point_keeps_the_entry_in_range() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("events.jsonl");
        fs::write(&source, "{}\n").unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let mut cache = TranscriptCache::open(&cache_path, false).unwrap();
        let stamp = file_stamp(&source).unwrap();
        let mut entry = parsed(&source);
        // Copilot reports a session's usage at shutdown, after the last activity point
        // and often on the other side of a day boundary.
        entry.sessions[0].token_events.push(TokenEvent {
            timestamp: parse_timestamp("2026-01-02T00:30:00Z").unwrap(),
            model: "gpt".into(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        });
        cache
            .put(&source, "copilot", "context", stamp, &entry)
            .unwrap();

        let since = parse_timestamp("2026-01-02T00:00:00Z").unwrap();
        let CacheLookup::Hit(hit) = cache
            .lookup(&source, "copilot", "context", stamp, Some(since), None)
            .unwrap()
        else {
            panic!("expected a cache hit covering the token event");
        };
        assert_eq!(1, hit.sessions[0].token_events.len());
    }

    #[test]
    fn repository_identity_history_survives_reopen_and_transcript_rebuild() {
        let directory = tempdir().unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let cwd = directory.path().join("deleted-worktree");
        let mut cache = TranscriptCache::open(&cache_path, false).unwrap();
        cache
            .remember_repository_identities(&[
                RepositoryHistoryEntry {
                    cwd_key: cwd.to_string_lossy().into_owned(),
                    natural_id: "remote:host/one".into(),
                    label: "one".into(),
                    repo_path: directory.path().join("one"),
                },
                RepositoryHistoryEntry {
                    cwd_key: cwd.to_string_lossy().into_owned(),
                    natural_id: "remote:host/two".into(),
                    label: "two".into(),
                    repo_path: directory.path().join("two"),
                },
            ])
            .unwrap();
        drop(cache);

        let cache = TranscriptCache::open(&cache_path, true).unwrap();
        let history = cache.load_repository_history().unwrap();

        assert_eq!(2, history.len());
        assert_eq!("remote:host/one", history[0].natural_id);
        assert_eq!("remote:host/two", history[1].natural_id);
    }

    #[test]
    fn an_entry_from_another_crate_version_is_recomputed() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("session.jsonl");
        fs::write(&source, "{}\n").unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let stamp = file_stamp(&source).unwrap();
        let mut cache = TranscriptCache::open(&cache_path, false).unwrap();
        cache
            .put(&source, "codex", "context", stamp, &parsed(&source))
            .unwrap();
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", stamp, None, None)
                .unwrap(),
            CacheLookup::Hit(_)
        ));

        // The same parser version, written by a build that was released earlier.
        let other = format!("{PARSER_VERSION}+0.0.0-older");
        assert_ne!(other, parser_stamp());
        cache
            .connection
            .execute("UPDATE transcript_cache SET parser_version = ?1", [other])
            .unwrap();
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", stamp, None, None)
                .unwrap(),
            CacheLookup::Miss
        ));
    }

    #[test]
    fn a_cache_written_before_the_crate_version_joined_the_key_is_recomputed() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("session.jsonl");
        fs::write(&source, "{}\n").unwrap();
        let cache_path = directory.path().join("index.sqlite3");
        let stamp = file_stamp(&source).unwrap();
        // The schema an older build created: an integer `parser_version`.
        let old = Connection::open(&cache_path).unwrap();
        old.execute_batch(
            "CREATE TABLE transcript_cache (
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
            );",
        )
        .unwrap();
        old.execute(
            "INSERT INTO transcript_cache VALUES (?1, 'codex', ?2, ?3, ?4, 'context', NULL, NULL, x'7b7d', NULL)",
            params![
                canonical(&source),
                stamp.size,
                stamp.modified_ns,
                PARSER_VERSION
            ],
        )
        .unwrap();
        drop(old);

        let mut cache = TranscriptCache::open(&cache_path, false).unwrap();
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", stamp, None, None)
                .unwrap(),
            CacheLookup::Miss
        ));
        // Recomputing replaces the old row in place rather than failing on it.
        cache
            .put(&source, "codex", "context", stamp, &parsed(&source))
            .unwrap();
        assert!(matches!(
            cache
                .lookup(&source, "codex", "context", stamp, None, None)
                .unwrap(),
            CacheLookup::Hit(_)
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
