//! The diff viewer.
//!
//! **This is the only place in workstats that ever reads the contents of a
//! tracked file, and what it reads is display-only.** A `DiffView` is built on
//! demand when the reader opens one, held while it is on screen, and dropped by
//! `App` the moment they navigate away. It is never cached, never written into
//! a report, never stored in a saved view, and never sent anywhere.
//!
//! That rule is enforced by shape as well as by discipline: `DiffView` is
//! deliberately not `Clone`, not `Debug` and not `Serialize`, so it cannot be
//! copied into another store, printed into a log, or serialised into the JSON
//! report or the saved-views file even by accident. Adding any of those three
//! derives would quietly undo the tool's central privacy property.
//!
//! It is also the only place where bytes from a file reach a terminal, so every
//! line is clipped and stripped of control and direction-override characters
//! before it can become a cell: a diff must not be able to repaint the screen.

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tempfile::tempfile;

use crate::git::git_executable;

/// The whole diff. A single commit that regenerates a lock file or a bundle is
/// tens of megabytes, and there is no reading that anyway.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
/// Lines in the whole diff, which is the bound that actually protects the
/// renderer: it walks the vector to lay the pane out.
const MAX_DIFF_LINES: usize = 20_000;
/// Characters in one line. A minified bundle arrives as a single multi-megabyte
/// line; nothing past this is readable and the rest is only memory.
const MAX_LINE_CHARS: usize = 2_000;
/// Read in chunks while throwing away the tail of an over-long line, so the
/// discard itself cannot be what exhausts memory.
const DISCARD_CHUNK_BYTES: u64 = 64 * 1024;
/// Git's own error output kept for the message. The same bound `read_git_commits`
/// uses in `src/git.rs`.
const MAX_ERROR_BYTES: u64 = 1000;
/// `%H`, `%an`, `%aI`, `%s` — the four lines the pretty format emits before the
/// patch.
const HEADER_FIELDS: usize = 4;
/// Matches `CommitRecord::short_sha`, so the diff names the commit exactly as
/// the row the reader opened it from did.
const SHORT_SHA_CHARS: usize = 9;

pub struct DiffRequest {
    /// The repository working directory (`GitCommit::cwd`), for `git -C`.
    pub cwd: PathBuf,
    pub sha: String,
    /// Repository-relative. Empty asks for the whole commit.
    pub path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffKind {
    Meta,
    Hunk,
    Added,
    Removed,
    Context,
}

pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// See the module documentation before adding a derive here.
pub struct DiffView {
    pub title: String,
    pub lines: Vec<DiffLine>,
    /// True when the diff was cut short by one of the caps above.
    pub truncated: bool,
}

pub fn load(request: &DiffRequest) -> Result<DiffView> {
    if !is_object_name(&request.sha) {
        bail!(
            "{:?} is not a commit id the diff viewer will open",
            request.sha
        );
    }
    let git = git_executable()
        .context("Git not found; install it, add it to PATH, or set WORKSTATS_GIT")?;
    let path = (!request.path.is_empty()).then_some(request.path.as_str());
    if let Some(view) = show(&git, request, path)? {
        return Ok(view);
    }
    let Some(path) = path else {
        bail!("Git found no diff for {}", describe(request));
    };
    // A path can be missing from a commit when a saved position outlived the
    // report it was taken from. One commit's worth of reading is a better answer
    // than a blank pane, and it is the answer that shows a rename.
    let mut view = show(&git, request, None)?
        .with_context(|| format!("Git found no diff for {}", describe(request)))?;
    view.lines.insert(
        0,
        DiffLine {
            kind: DiffKind::Meta,
            text: format!("{path} is not changed in this commit; showing the whole commit"),
        },
    );
    Ok(view)
}

/// `Ok(None)` when Git found no change for the requested path in this commit:
/// it then prints nothing at all, not even the commit header, which is the only
/// signal that separates an absent path from a binary file whose entire diff is
/// metadata.
fn show(git: &Path, request: &DiffRequest, path: Option<&str>) -> Result<Option<DiffView>> {
    let mut errors = tempfile().context("temporary Git diagnostics unavailable")?;
    let stderr = errors
        .try_clone()
        .context("temporary Git diagnostics unavailable")?;
    let mut command = build(git, request, path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot run Git for {}", describe(request)))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("Git produced no output for {}", describe(request)))?;
    // The pipe closes when `parse` returns, so a read cut short by a cap ends
    // Git with a broken pipe instead of leaving it blocked on a full one.
    let view = parse(BufReader::new(stdout), request);
    let status = child
        .wait()
        .with_context(|| format!("cannot run Git for {}", describe(request)))?;
    if !status.success() && !view.as_ref().is_some_and(|view| view.truncated) {
        let mut message = String::new();
        let _ = errors.seek(SeekFrom::Start(0));
        let _ = errors
            .by_ref()
            .take(MAX_ERROR_BYTES)
            .read_to_string(&mut message);
        let message = message.trim();
        if message.is_empty() {
            bail!("Git could not read {}", describe(request));
        }
        bail!("Git could not read {}: {message}", describe(request));
    }
    Ok(view)
}

fn build(git: &Path, request: &DiffRequest, path: Option<&str>) -> Command {
    let mut command = Command::new(git);
    command
        .arg("--no-pager")
        .arg("-C")
        .arg(&request.cwd)
        // Without this Git octal-escapes and quotes every non-ASCII path, and
        // the pathspec below would stop matching the report's own paths.
        .arg("-c")
        .arg("core.quotePath=false");
    match path {
        // A pathspec is applied before rename detection, so `git show <sha> --
        // <path>` reports a moved file as an unrelated new one. `--follow` is
        // the only thing that resolves the rename, and it belongs to `git log`.
        // `--not <sha>^@` pins the walk to this single commit: `--follow`
        // otherwise keeps walking and would answer with an EARLIER commit when
        // this one does not touch the path, which is a wrong answer that looks
        // like a right one.
        Some(path) => {
            command
                .arg("log")
                .arg("--max-count=1")
                .arg("--follow")
                .arg("--no-color")
                .arg("--find-renames")
                .arg("--format=%H%n%an%n%aI%n%s")
                .arg("--patch")
                .arg(&request.sha)
                .arg("--not")
                .arg(format!("{}^@", request.sha))
                // After `--`, so a path beginning with a dash cannot be read as
                // an option.
                .arg("--")
                .arg(path);
        }
        None => {
            command
                .arg("show")
                .arg("--no-color")
                .arg("--find-renames")
                .arg("--format=%H%n%an%n%aI%n%s")
                .arg("--patch")
                .arg(&request.sha);
        }
    }
    command
}

fn parse<R: BufRead>(mut reader: R, request: &DiffRequest) -> Option<DiffView> {
    let mut buffer = Vec::new();
    let mut header: Vec<String> = Vec::new();
    while header.len() < HEADER_FIELDS {
        match read_line(&mut reader, &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => header.push(clip(&buffer)),
        }
    }
    let sha = header.first().filter(|value| !value.is_empty())?;
    let mut title: String = sha.chars().take(SHORT_SHA_CHARS).collect();
    let author = header.get(1).map_or("", String::as_str);
    let date = header.get(2).map_or("", String::as_str);
    let subject = header.get(3).map_or("", String::as_str);
    if !subject.is_empty() {
        title.push(' ');
        title.push_str(subject);
    }
    if !request.path.is_empty() {
        title.push_str(" — ");
        title.push_str(&request.path);
    }

    let mut lines = Vec::new();
    if !author.is_empty() || !date.is_empty() {
        lines.push(DiffLine {
            kind: DiffKind::Meta,
            text: format!("{author} · {date}"),
        });
    }
    let mut bytes = 0;
    let mut truncated = false;
    let mut started = false;
    loop {
        if lines.len() >= MAX_DIFF_LINES || bytes >= MAX_DIFF_BYTES {
            truncated = true;
            break;
        }
        let read = match read_line(&mut reader, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                truncated = true;
                break;
            }
        };
        bytes += read;
        let text = clip(&buffer);
        // Git separates the commit header from the patch with a blank line.
        if !started {
            if text.is_empty() {
                continue;
            }
            started = true;
        }
        lines.push(DiffLine {
            kind: kind_of(&text),
            text,
        });
    }
    Some(DiffView {
        title,
        lines,
        truncated,
    })
}

/// Which part of the patch a line belongs to. `---` and `+++` are tested before
/// the bare `-` and `+` they begin with, or every file header would read as a
/// removed and an added line.
fn kind_of(text: &str) -> DiffKind {
    const META_PREFIXES: &[&str] = &[
        "diff --git",
        "index ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "GIT binary patch",
        r"\ No newline at end of file",
    ];
    if text.starts_with("@@") {
        DiffKind::Hunk
    } else if text.starts_with("+++")
        || text.starts_with("---")
        || META_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
    {
        DiffKind::Meta
    } else if text.starts_with('+') {
        DiffKind::Added
    } else if text.starts_with('-') {
        DiffKind::Removed
    } else {
        DiffKind::Context
    }
}

/// One line of file content, made safe to draw.
///
/// A tracked file can hold anything, and this module is the only one that puts
/// its bytes on a terminal: an escape sequence rendered verbatim would let a
/// diff repaint the screen, and a direction override would let it reorder what
/// a reader sees. Neither survives to the buffer.
fn clip(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end_matches(['\r', '\n']);
    let mut clipped = String::with_capacity(trimmed.len());
    let mut width = 0;
    for character in trimmed.chars() {
        if width >= MAX_LINE_CHARS {
            clipped.push('…');
            break;
        }
        match character {
            // A terminal cell cannot hold a tab, and diffs are full of them.
            '\t' => {
                clipped.push_str("    ");
                width += 4;
            }
            _ if character.is_control() || is_direction_override(character) => {
                clipped.push('·');
                width += 1;
            }
            _ => {
                clipped.push(character);
                width += 1;
            }
        }
    }
    clipped
}

/// The bidirectional format characters. Unicode does not classify them as
/// control characters, but they reorder what is drawn, which in a diff is the
/// difference between reading a change and being shown a different one.
fn is_direction_override(character: char) -> bool {
    matches!(
        character,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// The revision is a positional argument to Git and is also spliced into
/// `<sha>^@`, so anything that is not a plain object name could be read as an
/// option or as a revision range. Report identifiers are always hexadecimal.
fn is_object_name(sha: &str) -> bool {
    (4..=64).contains(&sha.len()) && sha.chars().all(|character| character.is_ascii_hexdigit())
}

fn describe(request: &DiffRequest) -> String {
    if request.path.is_empty() {
        format!("commit {} in {}", request.sha, request.cwd.display())
    } else {
        format!(
            "{} at commit {} in {}",
            request.path,
            request.sha,
            request.cwd.display()
        )
    }
}

/// One line, with anything past the character cap discarded rather than held.
fn read_line<R: BufRead>(reader: &mut R, buffer: &mut Vec<u8>) -> io::Result<usize> {
    buffer.clear();
    // Four bytes per character is UTF-8's worst case, so this budget can never
    // cut a line short of what `clip` would have kept.
    let read = {
        let mut limited = reader.by_ref().take(MAX_LINE_CHARS as u64 * 4 + 1);
        limited.read_until(b'\n', buffer)?
    };
    if read > 0 && !buffer.ends_with(b"\n") {
        discard_to_newline(reader)?;
    }
    Ok(read)
}

fn discard_to_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    let mut sink = Vec::new();
    loop {
        sink.clear();
        let read = {
            let mut limited = reader.by_ref().take(DISCARD_CHUNK_BYTES);
            limited.read_until(b'\n', &mut sink)?
        };
        if read == 0 || sink.ends_with(b"\n") {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use tempfile::tempdir;

    const RENAME: &str = "\
59ae06f5069aaa88f68d93dbf70cf25d315700b0
Test Author
2026-08-18T16:54:59+02:00
rename and edit

diff --git a/a.txt b/c.txt
similarity index 57%
rename from a.txt
rename to c.txt
index 814f4a4..4cb29ea 100644
--- a/a.txt
+++ b/c.txt
@@ -1,2 +1,3 @@
 one
 two
+three
";

    const BINARY: &str = "\
b8387d48fbd87dd477b031eaf26dab3efe976d0b
Test Author
2026-08-18T16:54:59+02:00
first

diff --git a/b.bin b/b.bin
new file mode 100644
index 0000000..c94be36
Binary files /dev/null and b/b.bin differ
";

    fn request(path: &str) -> DiffRequest {
        DiffRequest {
            cwd: PathBuf::from("/repos/widget"),
            sha: "59ae06f5069aaa88f68d93dbf70cf25d315700b0".to_string(),
            path: path.to_string(),
        }
    }

    fn parsed(patch: &str, path: &str) -> DiffView {
        parse(Cursor::new(patch.as_bytes()), &request(path)).expect("a commit header")
    }

    fn kinds(view: &DiffView) -> Vec<DiffKind> {
        view.lines.iter().map(|line| line.kind).collect()
    }

    fn git(arguments: &[&str]) -> String {
        let output = Command::new(git_executable().expect("Git is required for this test"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "git command failed: {arguments:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn a_patch_becomes_styled_lines_with_the_header_lifted_into_the_title() {
        let view = parsed(RENAME, "c.txt");
        assert_eq!("59ae06f50 rename and edit — c.txt", view.title);
        assert!(!view.truncated);
        assert_eq!(
            "Test Author · 2026-08-18T16:54:59+02:00",
            view.lines[0].text
        );
        // The blank line separating the commit header from the patch is not a
        // line of the diff.
        assert_eq!("diff --git a/a.txt b/c.txt", view.lines[1].text);
        assert_eq!(
            vec![
                DiffKind::Meta,    // author and date
                DiffKind::Meta,    // diff --git
                DiffKind::Meta,    // similarity index
                DiffKind::Meta,    // rename from
                DiffKind::Meta,    // rename to
                DiffKind::Meta,    // index
                DiffKind::Meta,    // --- a/a.txt
                DiffKind::Meta,    // +++ b/c.txt
                DiffKind::Hunk,    // @@
                DiffKind::Context, //  one
                DiffKind::Context, //  two
                DiffKind::Added,   // +three
            ],
            kinds(&view)
        );
    }

    #[test]
    fn a_rename_reads_as_metadata_not_as_a_file_removed_and_added() {
        let view = parsed(RENAME, "c.txt");
        assert!(
            view.lines
                .iter()
                .any(|line| line.kind == DiffKind::Meta && line.text == "rename from a.txt")
        );
        // `--- a/a.txt` must not be mistaken for a removed line.
        assert_eq!(
            1,
            kinds(&view)
                .iter()
                .filter(|kind| **kind == DiffKind::Added)
                .count()
        );
        assert!(!kinds(&view).contains(&DiffKind::Removed));
    }

    #[test]
    fn a_binary_file_comes_back_as_metadata_rather_than_bytes() {
        let view = parsed(BINARY, "b.bin");
        assert!(kinds(&view).iter().all(|kind| *kind == DiffKind::Meta));
        assert!(
            view.lines
                .iter()
                .any(|line| line.text == "Binary files /dev/null and b/b.bin differ")
        );
    }

    #[test]
    fn no_output_at_all_means_the_path_is_not_in_the_commit() {
        // Git prints exactly this when a pathspec matches no change: not even
        // the commit header.
        assert!(parse(Cursor::new(b"\n".as_slice()), &request("src/main.rs")).is_none());
        assert!(parse(Cursor::new(b"".as_slice()), &request("src/main.rs")).is_none());
    }

    #[test]
    fn a_generated_files_diff_is_cut_short_rather_than_held() {
        let mut patch = String::from("aaaaaaaaaaaa\nT\n2026-01-01T00:00:00Z\ngenerate\n\n");
        for index in 0..MAX_DIFF_LINES + 100 {
            patch.push_str(&format!("+line {index}\n"));
        }
        let view = parsed(&patch, "dist/bundle.js");
        assert!(view.truncated);
        assert_eq!(MAX_DIFF_LINES, view.lines.len());
    }

    #[test]
    fn enormous_lines_trip_the_byte_budget_before_the_line_budget() {
        let long = "x".repeat(MAX_LINE_CHARS * 4);
        let mut patch = String::from("aaaaaaaaaaaa\nT\n2026-01-01T00:00:00Z\ngenerate\n\n");
        for _ in 0..400 {
            patch.push('+');
            patch.push_str(&long);
            patch.push('\n');
        }
        let view = parsed(&patch, "dist/bundle.js");
        assert!(view.truncated);
        assert!(view.lines.len() < MAX_DIFF_LINES);
    }

    #[test]
    fn a_line_past_the_read_budget_does_not_swallow_the_next_one() {
        let long = "x".repeat(MAX_LINE_CHARS * 5);
        let patch = format!("aaaaaaaaaaaa\nT\n2026-01-01T00:00:00Z\nminify\n\n+{long}\n-gone\n");
        let view = parsed(&patch, "dist/bundle.js");
        assert_eq!(3, view.lines.len());
        assert_eq!(MAX_LINE_CHARS + 1, view.lines[1].text.chars().count());
        assert!(view.lines[1].text.ends_with('…'));
        assert_eq!(DiffKind::Removed, view.lines[2].kind);
        assert_eq!("-gone", view.lines[2].text);
    }

    #[test]
    fn file_contents_cannot_repaint_or_reorder_the_terminal() {
        let patch =
            "aaaaaaaaaaaa\nT\n2026-01-01T00:00:00Z\nsneaky\n\n+\u{1b}[31mred\u{202e}txet\t.\n";
        let view = parsed(patch, "src/main.rs");
        assert_eq!("+·[31mred·txet    .", view.lines[1].text);
    }

    #[test]
    fn only_a_plain_object_name_is_handed_to_git() {
        assert!(is_object_name("59ae06f5069a"));
        assert!(is_object_name("59AE"));
        assert!(!is_object_name(""));
        assert!(!is_object_name("abc"));
        assert!(!is_object_name("HEAD"));
        assert!(!is_object_name("--upstream"));
        assert!(!is_object_name("v1.0.0"));
        assert!(!is_object_name(&"a".repeat(65)));
    }

    #[test]
    fn a_revision_that_is_not_an_object_name_is_refused_before_git_runs() {
        let Err(error) = load(&DiffRequest {
            cwd: PathBuf::from("/nonexistent"),
            sha: "--output=/tmp/owned".to_string(),
            path: "src/main.rs".to_string(),
        }) else {
            panic!("a revision that is not an object name must be refused");
        };
        assert!(format!("{error:#}").contains("commit id"));
    }

    #[test]
    fn a_real_rename_follows_and_a_missing_path_falls_back_to_the_commit() {
        let base = tempdir().unwrap();
        let repo = base.path().join("widget");
        fs::create_dir_all(&repo).unwrap();
        let path = repo.to_str().unwrap().to_string();
        git(&["init", "-q", &path]);
        git(&["-C", &path, "config", "user.name", "Test Author"]);
        git(&["-C", &path, "config", "user.email", "test@example.com"]);
        fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["-C", &path, "add", "a.txt"]);
        git(&["-C", &path, "commit", "-qm", "first"]);
        fs::remove_file(repo.join("a.txt")).unwrap();
        fs::write(repo.join("c.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["-C", &path, "add", "-A"]);
        git(&["-C", &path, "commit", "-qm", "rename and edit"]);
        let sha = git(&["-C", &path, "rev-parse", "HEAD"]);

        let renamed = load(&DiffRequest {
            cwd: repo.clone(),
            sha: sha.clone(),
            path: "c.txt".to_string(),
        })
        .expect("the renamed file");
        assert!(renamed.title.contains("rename and edit"));
        // A pathspec alone hides the source side from rename detection, so this
        // is what proves `--follow` is doing its job.
        assert!(
            renamed
                .lines
                .iter()
                .any(|line| line.text == "rename from a.txt")
        );
        assert!(
            renamed
                .lines
                .iter()
                .any(|line| line.kind == DiffKind::Added && line.text == "+three")
        );
        assert!(!renamed.truncated);

        let missing = load(&DiffRequest {
            cwd: repo.clone(),
            sha,
            path: "nowhere.txt".to_string(),
        })
        .expect("the whole commit instead");
        assert_eq!(DiffKind::Meta, missing.lines[0].kind);
        assert!(missing.lines[0].text.contains("nowhere.txt"));
        assert!(
            missing
                .lines
                .iter()
                .any(|line| line.text == "rename to c.txt")
        );

        let unknown = load(&DiffRequest {
            cwd: repo,
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            path: String::new(),
        });
        assert!(unknown.is_err());
    }
}
