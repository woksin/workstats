//! GitHub Copilot Chat in VS Code: one JSON document per chat session under each
//! workspace's `chatSessions` directory.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::gemini::MAX_GEMINI_JSON_BYTES;
use super::{ParsedFile, deserialize_maybe_number, file_stem, load_files, safe_model};
use crate::cache::TranscriptCache;
use crate::model::{ActivityPoint, Diagnostics, ExactInterval, RawSession, Session};
use crate::paths::PathResolver;
use crate::timeutil::parse_epoch_milliseconds;

/// The largest real VS Code chat session measured on the machine this was designed
/// against, out of 26. The next three below it are 15.1 MB, 12.3 MB and 10.2 MB, all in
/// the one workspace that is used daily — an ordinary long-running session, not an
/// outlier. Recorded as a constant so the ceiling below is checked against evidence
/// rather than against itself.
pub const LARGEST_OBSERVED_VSCODE_CHAT_BYTES: u64 = 17_853_291;

/// A VS Code chat session is one JSON document too, so the line-bounded discipline the
/// JSONL adapters rely on does not apply and the file needs its own ceiling.
///
/// The previous ceiling was 16 MiB, set at 2.5x the then-largest observed session of
/// 6.4 MB. Sessions grow with use: the largest is now `LARGEST_OBSERVED_VSCODE_CHAT_BYTES`,
/// which the old ceiling rejected outright and lost a whole session's activity to, with
/// the next one down only 1.6 MB clear of it. So the ceiling has to clear real growth by
/// more than the last one did, not by a hair.
///
/// It can afford to. The parser deserializes into the narrow structs below rather than
/// into a `serde_json::Value`, so serde skips every unread field instead of building it:
/// measured peak resident cost is ~0.16 MB for the 17.9 MB session (a `Value` tree of
/// the same file costs 50 MB, 2.8x the file), ~0.15 MB for a synthetic 64 MB document
/// whose bulk is one unread string, and ~1.8 MB for a 42 MB document holding 20,000
/// requests. Cost tracks the request count, not the file size. Wall time at the ceiling
/// is ~0.12 s. So this bounds the pathological case — a corrupt or runaway file — at a
/// fraction of a second and a couple of megabytes, while sitting ~3.8x above the largest
/// session a real editor has produced here and an order of magnitude under the whole-file
/// bound `MAX_GEMINI_JSON_BYTES` already accepts.
pub const MAX_VSCODE_CHAT_JSON_BYTES: u64 = 64 * 1024 * 1024;

/// The relation that matters, held at compile time rather than left to a reviewer:
/// whatever the ceiling is set to, it clears the largest session real use has produced
/// by enough to absorb the growth that broke the last one, and it stays under the
/// whole-file bound the crate already accepts so that it is still a ceiling.
const _: () = assert!(MAX_VSCODE_CHAT_JSON_BYTES >= 3 * LARGEST_OBSERVED_VSCODE_CHAT_BYTES);
const _: () = assert!(MAX_VSCODE_CHAT_JSON_BYTES <= MAX_GEMINI_JSON_BYTES);

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
        repository_hint_cwd: None,
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
///
/// Bumped to v2 because a v1 entry may have been written under the old size ceiling,
/// which recorded a real session as an unreadable file. A finished chat session is never
/// written to again, so its size and mtime never change and the cache would go on
/// serving that refusal — and go on losing the session — indefinitely.
fn copilot_vscode_context_fingerprint(path: &Path) -> String {
    match copilot_vscode_workspace_file(path) {
        Some(file) => format!("copilot-vscode-v2:{}", crate::cache::file_context(&file)),
        None => "copilot-vscode-v2:none".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::canonical_string;
    use std::fs;
    use tempfile::tempdir;

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
        // A file that is genuinely pathological is still declined rather than read.
        let parsed = parse_copilot_vscode_file(&path, 16);
        assert!(parsed.sessions.is_empty());
        assert_eq!(1, parsed.diagnostics.unreadable_files);
    }

    /// Pins what the ceiling is *for* rather than what it says, because what it
    /// says has already been wrong once: it was set at 2.5x the largest session
    /// anyone had seen, sessions kept growing, and a real 17.9 MB session was
    /// dropped whole for being 1 MB over. An assertion on the literal value
    /// would have passed happily through all of that, so this one asserts the
    /// outcome instead — a session the size of the largest one real use has
    /// produced parses, and its activity is counted.
    #[test]
    fn a_chat_session_the_size_of_the_largest_real_one_is_read_not_refused() {
        let root = tempdir().unwrap();
        let sessions = root.path().join("1a2b/chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("large.json");

        // The bulk is one field the parser never reads, which is where a real
        // session's bulk is too: the prompt and response bodies this tool
        // deliberately does not look at.
        let head = r#"{"version":3,"sessionId":"chat-large","filler":""#;
        let tail = r#"","requests":[{"timestamp":1767225600000,"modelId":"copilot/gpt-test","result":{"timings":{"totalElapsed":30000}}}]}"#;
        let padding = LARGEST_OBSERVED_VSCODE_CHAT_BYTES as usize - head.len() - tail.len();
        fs::write(&path, format!("{head}{}{tail}", "p".repeat(padding))).unwrap();
        assert_eq!(
            LARGEST_OBSERVED_VSCODE_CHAT_BYTES,
            fs::metadata(&path).unwrap().len()
        );

        let parsed = parse_copilot_vscode_file(&path, MAX_VSCODE_CHAT_JSON_BYTES);
        assert_eq!(
            1,
            parsed.sessions.len(),
            "a session this size is ordinary, and losing it loses a whole session's activity: {:?}",
            parsed.diagnostics.messages
        );
        assert_eq!(0, parsed.diagnostics.unreadable_files);
        assert_eq!(1, parsed.sessions[0].exact_intervals.len());

        // The regression itself: the ceiling this replaced refused exactly this
        // file, and reported it as data the user could do nothing about.
        let refused = parse_copilot_vscode_file(&path, 16 * 1024 * 1024);
        assert!(refused.sessions.is_empty());
        assert_eq!(1, refused.diagnostics.unreadable_files);
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
}
