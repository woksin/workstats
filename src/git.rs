use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use tempfile::tempfile;

use crate::classify::{CategoryTally, classify};
use crate::model::{Authorship, Diagnostics, GitCommit};
use crate::paths::PathResolver;

pub const DEFAULT_IGNORES: &[&str] = &[
    "*/node_modules/*",
    "*/dist/*",
    "*/build/*",
    "*/out/*",
    "*/obj/*",
    "*/bin/*",
    "*/vendor/*",
    "*/coverage/*",
    "*/.next/*",
    "*/.nuxt/*",
    "*/.svelte-kit/*",
    "*/__snapshots__/*",
    "*/Pods/*",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.snap",
    "*.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "composer.lock",
    "Cargo.lock",
    "poetry.lock",
    "Gemfile.lock",
    "go.sum",
];

/// The coding agents whose commits arrive in an ordinary `git fetch`, as Git
/// author patterns.
///
/// Each one matches the tail of an address, never the number in front of it:
/// `198982749+Copilot@…` and `223556219+Copilot@…` are both GitHub's Copilot in
/// this machine's history, so an identity keyed on the id matches some of an
/// agent's work and silently misses the rest. Matching the address also covers
/// every display name the same account commits under — `Copilot` and
/// `copilot-swe-agent[bot]` share one address — which is what keeps this from
/// becoming a list of names to chase.
///
/// Automation that is not an AI agent is deliberately absent. `github-actions`
/// and `dependabot` push far more commits than Copilot does, and counting a
/// version bump as agent output would say something false about both.
///
/// These are *basic* regular expressions, because that is what `git log`
/// defaults to: `+`, `?`, `(`, `)` and `|` are literals here, and a backslash
/// is what would turn them into operators. See `git_regex_literal`.
pub const DEFAULT_AGENT_AUTHORS: &[&str] = &[
    r"+Copilot@users\.noreply\.github\.com>",
    r"<copilot@github\.com>",
    r"+claude\[bot\]@users\.noreply\.github\.com>",
    r"<noreply@anthropic\.com>",
];

/// `W` marks a commit header: no `--numstat` field can begin with it followed
/// by a tab except a path that literally starts `W<TAB>`, and `parse_git_log`
/// only reads the sentinel where a path cannot be.
const COMMIT_HEADER: &str = "--pretty=format:W%x09%H%x09%aI";

/// The same header plus every `Co-authored-by:` value, `\x02`-separated.
///
/// `unfold` is load-bearing rather than cosmetic. A trailer continued on an
/// indented line otherwise arrives with a newline inside it, and the newline
/// that closes the header would then be found in the middle of the trailer —
/// which silently costs that commit its entire diff. Verified against Git
/// 2.50.1 with a folded trailer, both ways.
///
/// Only the values are asked for, so no part of a commit message but the
/// identities on its trailers is ever read into memory.
const COMMIT_HEADER_WITH_CO_AUTHORS: &str = "--pretty=format:W%x09%H%x09%aI%x09%(trailers:key=Co-authored-by,valueonly,separator=%x02,unfold)";

pub fn git_executable() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("WORKSTATS_GIT") {
        let configured = PathBuf::from(configured);
        if configured.is_file() {
            return Some(configured);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(not(windows))]
    candidates.extend(
        [
            "/usr/bin/git",
            "/opt/homebrew/bin/git",
            "/usr/local/bin/git",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    {
        for root in [
            env::var_os("ProgramFiles"),
            env::var_os("ProgramFiles(x86)"),
            env::var_os("LOCALAPPDATA"),
        ]
        .into_iter()
        .flatten()
        {
            let root = PathBuf::from(root);
            candidates.push(root.join("Git/cmd/git.exe"));
            candidates.push(root.join("Programs/Git/cmd/git.exe"));
        }
    }
    if let Some(path) = env::var_os("PATH") {
        let executable = if cfg!(windows) { "git.exe" } else { "git" };
        candidates.extend(env::split_paths(&path).map(|directory| directory.join(executable)));
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub fn default_git_author() -> Option<String> {
    let git = git_executable()?;
    for key in ["user.email", "user.name"] {
        let output = Command::new(&git)
            .args(["config", "--global", "--get", key])
            .output()
            .ok()?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !value.is_empty() {
                return Some(git_regex_literal(&value));
            }
        }
    }
    None
}

/// Escapes `value` so that `git log --author` matches it as plain text.
///
/// `--author` takes a *basic* regular expression, and in one `+`, `?`, `(`,
/// `)`, `{`, `}` and `|` are already literal — a backslash is what promotes
/// them to operators. Escaping them is therefore the opposite of escaping them:
/// `person+work@example.com` written as `person\+work@…` asks GNU BRE for "one
/// or more n" and matches nothing that address ever committed, so every
/// plus-addressed author quietly reported an empty history.
fn git_regex_literal(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '.' | '^' | '$' | '*' | '[' | '\\') {
            result.push('\\');
        }
        result.push(character);
    }
    result
}

pub fn discover_repositories(
    base: &Path,
    depth: usize,
    diagnostics: &mut Diagnostics,
) -> Vec<PathBuf> {
    if !base.is_dir() {
        diagnostics.warn(format!("Git scan root not found: {}", base.display()));
        return Vec::new();
    }
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let mut found = Vec::new();
    discover_at(&base, 0, depth, &mut found);
    found.sort();
    found.dedup_by(|left, right| {
        left.canonicalize().unwrap_or_else(|_| left.clone())
            == right.canonicalize().unwrap_or_else(|_| right.clone())
    });
    found
}

fn discover_at(path: &Path, relative_depth: usize, maximum: usize, found: &mut Vec<PathBuf>) {
    if path.join(".git").exists() {
        found.push(path.to_path_buf());
        return;
    }
    if relative_depth >= maximum {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut directories: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        })
        .filter(|entry| {
            !matches!(
                entry.file_name().to_string_lossy().as_ref(),
                ".git" | "node_modules" | "bin" | "obj" | "dist" | "build" | "vendor" | ".cache"
            )
        })
        .map(|entry| entry.path())
        .collect();
    directories.sort();
    for directory in directories {
        discover_at(&directory, relative_depth + 1, maximum, found);
    }
}

/// One `git log` pass: whose commits it asks Git for, and what may be concluded
/// from them.
///
/// There are two passes rather than one widened `--author` because the two
/// answers must never be summed. Widening the filter is the obvious shortcut
/// and it is precisely the failure this tool exists to prevent: the agent's
/// commits would land in the collection the human estimate is built from, and
/// each one would cluster into a work block carrying setup and review credit
/// for work no person did. The repositories on one developer's machine can hold
/// thousands of them.
struct Pass<'a> {
    /// `--author` patterns, OR-ed by Git, which accepts the flag repeatedly.
    /// A list rather than one alternation because `--author` is a *basic*
    /// regular expression: alternation there is spelled `\|`, a built-in
    /// default relying on that reads as a typo, and the first person to
    /// "correct" it to `|` would break it silently.
    authors: &'a [String],
    /// Stamped on every commit the pass produces.
    authorship: Authorship,
    /// Whether to ask Git for `Co-authored-by:` values.
    co_authors: bool,
}

/// The commits the configured author wrote. These are human evidence; nothing
/// else in this file is.
#[allow(clippy::too_many_arguments)]
pub fn read_git_commits(
    base: &Path,
    author: &str,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    depth: usize,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    repo_filter: Option<&str>,
    path_includes: &[String],
    path_excludes: &[String],
    no_ignore: bool,
    co_authors: bool,
) -> Vec<GitCommit> {
    let authors = [author.to_string()];
    collect_commits(
        base,
        &Pass {
            authors: &authors,
            authorship: Authorship::default(),
            co_authors,
        },
        resolver,
        diagnostics,
        depth,
        since,
        until,
        repo_filter,
        path_includes,
        path_excludes,
        no_ignore,
    )
}

/// The commits a coding agent wrote, from the same local history and with no
/// network access: once a branch has been fetched, the agent's work is ordinary
/// Git history. The result is output, and it is zero evidence that anyone was
/// present — see `GitCommit::human_signal`.
#[allow(clippy::too_many_arguments)]
pub fn read_agent_commits(
    base: &Path,
    authors: &[String],
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    depth: usize,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    repo_filter: Option<&str>,
    path_includes: &[String],
    path_excludes: &[String],
    no_ignore: bool,
) -> Vec<GitCommit> {
    if authors.is_empty() {
        return Vec::new();
    }
    collect_commits(
        base,
        &Pass {
            authors,
            authorship: Authorship::agent(),
            // An agent's own commit is already agent-authored; who it credits
            // beside itself changes nothing and would only cost a wider read of
            // the message.
            co_authors: false,
        },
        resolver,
        diagnostics,
        depth,
        since,
        until,
        repo_filter,
        path_includes,
        path_excludes,
        no_ignore,
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_commits(
    base: &Path,
    pass: &Pass<'_>,
    resolver: &mut PathResolver,
    diagnostics: &mut Diagnostics,
    depth: usize,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    repo_filter: Option<&str>,
    path_includes: &[String],
    path_excludes: &[String],
    no_ignore: bool,
) -> Vec<GitCommit> {
    let Some(git) = git_executable() else {
        diagnostics.git_errors += 1;
        diagnostics.warn("Git not found; install it, add it to PATH, or set WORKSTATS_GIT");
        return Vec::new();
    };
    let includes = match compile_globs(path_includes) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.git_errors += 1;
            diagnostics.warn(format!("invalid Git include glob: {error}"));
            return Vec::new();
        }
    };
    let ignores: Vec<String> = if no_ignore {
        path_excludes.to_vec()
    } else {
        DEFAULT_IGNORES
            .iter()
            .map(|value| (*value).to_string())
            .chain(path_excludes.iter().cloned())
            .collect()
    };
    let ignores = match compile_globs(&ignores) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.git_errors += 1;
            diagnostics.warn(format!("invalid Git ignore glob: {error}"));
            return Vec::new();
        }
    };
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    for repo_path in discover_repositories(base, depth, diagnostics) {
        let (cwd, repo, root) = resolver.describe(&repo_path.to_string_lossy());
        // The same three fields the session filter in `main.rs` matches, so a
        // filter naming a source root selects commits as well as sessions.
        if repo_filter.is_some_and(|filter| {
            let filter = filter.to_lowercase();
            !repo.to_lowercase().contains(&filter)
                && !cwd.to_lowercase().contains(&filter)
                && !root.to_lowercase().contains(&filter)
        }) {
            continue;
        }
        let mut command = Command::new(&git);
        command
            .arg("--no-pager")
            .arg("-C")
            .arg(&repo_path)
            // Inert as long as `-z` is on, because Git only escapes a name
            // when the record terminator is a newline. It stays because a
            // reader added later that reads newline-terminated output would
            // otherwise silently start octal-escaping every non-ASCII path,
            // which breaks extension detection and the ignore globs.
            .arg("-c")
            .arg("core.quotePath=false")
            .arg("log")
            .arg("--regexp-ignore-case")
            .arg("--no-merges")
            .arg("--date=iso-strict")
            .arg(if pass.co_authors {
                COMMIT_HEADER_WITH_CO_AUTHORS
            } else {
                COMMIT_HEADER
            })
            .arg("--numstat")
            // NUL-separated fields let a path hold any byte and make a rename
            // a stated fact rather than a guess at text; see `parse_git_log`.
            // Rename detection itself stays on — `--no-renames` would turn
            // every large move into thousands of phantom added and deleted
            // lines.
            .arg("-z");
        // Git ORs repeated `--author` arguments, which is how one pass asks for
        // several identities without any alternation syntax to get wrong.
        for pattern in pass.authors {
            command.arg(format!("--author={pattern}"));
        }
        if let Some(since) = since {
            command.arg(format!("--since={}", iso(since)));
        }
        if let Some(until) = until {
            command.arg(format!("--until={}", iso(until)));
        }
        let mut errors = match tempfile() {
            Ok(file) => file,
            Err(error) => {
                diagnostics.git_errors += 1;
                diagnostics.warn(format!("temporary Git diagnostics unavailable: {error}"));
                continue;
            }
        };
        let stderr = match errors.try_clone() {
            Ok(file) => file,
            Err(error) => {
                diagnostics.git_errors += 1;
                diagnostics.warn(format!("temporary Git diagnostics unavailable: {error}"));
                continue;
            }
        };
        let mut child = match command
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                diagnostics.git_errors += 1;
                diagnostics.warn(format!(
                    "Git unavailable for {}: {error}",
                    repo_path.display()
                ));
                continue;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            diagnostics.git_errors += 1;
            diagnostics.warn(format!(
                "Git stdout unavailable for {}",
                repo_path.display()
            ));
            continue;
        };
        let repo_commits = parse_git_log(
            BufReader::new(stdout),
            pass,
            &repo,
            &cwd,
            &root,
            includes.as_ref(),
            ignores.as_ref(),
            &seen,
        );
        let status = child.wait();
        let _ = errors.seek(SeekFrom::Start(0));
        let mut error_text = String::new();
        let _ = errors.take(1000).read_to_string(&mut error_text);
        match status {
            Ok(status) if status.success() => {
                for commit in repo_commits {
                    seen.insert(commit.sha.clone());
                    commits.push(commit);
                }
            }
            Ok(_) if error_text.contains("does not have any commits yet") => {}
            Ok(_) => {
                diagnostics.git_errors += 1;
                diagnostics.warn(format!(
                    "Git log failed for {}: {}",
                    repo_path.display(),
                    error_text.trim().chars().take(200).collect::<String>()
                ));
            }
            Err(error) => {
                diagnostics.git_errors += 1;
                diagnostics.warn(format!("Git failed for {}: {error}", repo_path.display()));
            }
        }
    }
    commits
}

/// One commit being accumulated across its `--numstat` fields.
#[derive(Default)]
struct PendingCommit {
    sha: String,
    timestamp: Option<DateTime<Utc>>,
    additions: u64,
    deletions: u64,
    ignored_additions: u64,
    ignored_deletions: u64,
    files: Vec<String>,
    categories: CategoryTally,
    matched_file: bool,
    authorship: Authorship,
}

/// Which field of the `-z` stream comes next. A rename spends three fields on
/// one change, and either of its path fields may itself read like a header or
/// like a change — a file really can be named `W\tsomething` — so the two are
/// claimed by position rather than recognised by shape.
enum Expected {
    Change,
    RenameSource(u64, u64),
    RenameTarget(u64, u64),
}

/// `git log -z --numstat` writes NUL-separated fields:
///
/// * change: `<added>\t<removed>\t<path>`
/// * rename: `<added>\t<removed>\t` with an empty path, then two more fields —
///   the path moved from and the path moved to
/// * header: `W\t<sha>\t<date>`, glued to the front of the commit's first
///   change field and closed by a newline. A commit with no diff has no field
///   to be glued to, so Git closes its header with the field's own NUL. When
///   the pass asks for them, a fourth header field carries the
///   `Co-authored-by:` values, `\x02`-separated and empty when there are none.
///
/// Commits are separated by an empty field. Paths are the point of all this:
/// under `-z` Git never quotes or escapes one, so a path may hold every byte
/// but NUL — tabs, newlines, quotes, backslashes, braces and ` => ` included —
/// and no field ever has to be un-mangled or guessed at.
#[allow(clippy::too_many_arguments)]
fn parse_git_log(
    mut reader: impl BufRead,
    pass: &Pass<'_>,
    repo: &str,
    cwd: &str,
    root: &str,
    includes: Option<&GlobSet>,
    ignores: Option<&GlobSet>,
    globally_seen: &HashSet<String>,
) -> Vec<GitCommit> {
    let mut commits = Vec::new();
    let mut repo_seen = HashSet::new();
    let mut pending = PendingCommit::default();

    let emit =
        |commits: &mut Vec<GitCommit>, repo_seen: &mut HashSet<String>, pending: PendingCommit| {
            if let Some(timestamp) = pending.timestamp
                && !pending.sha.is_empty()
                && pending.matched_file
                && !globally_seen.contains(&pending.sha)
                && !repo_seen.contains(&pending.sha)
            {
                repo_seen.insert(pending.sha.clone());
                commits.push(GitCommit {
                    sha: pending.sha,
                    timestamp,
                    repo: repo.to_string(),
                    cwd: cwd.to_string(),
                    root: root.to_string(),
                    additions: pending.additions,
                    deletions: pending.deletions,
                    files: pending.files,
                    ignored_additions: pending.ignored_additions,
                    ignored_deletions: pending.ignored_deletions,
                    categories: pending.categories,
                    authorship: pending.authorship,
                });
            }
        };

    // One field at a time, never the whole history: some of these repositories
    // have hundreds of megabytes of log.
    let mut buffer = Vec::new();
    let mut expected = Expected::Change;
    loop {
        buffer.clear();
        match reader.read_until(b'\0', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buffer.last() == Some(&b'\0') {
            buffer.pop();
        }
        // A path is bytes and need not be UTF-8. Nothing downstream — the
        // globs, the categories, the report — can carry the original bytes, so
        // the lossy spelling is as close as such a path gets. Failing to
        // decode must not end the stream: one odd path would otherwise cost
        // the repository every commit behind it.
        let field = String::from_utf8_lossy(&buffer);
        let mut rest: &str = &field;

        match std::mem::replace(&mut expected, Expected::Change) {
            Expected::RenameSource(added, removed) => {
                expected = Expected::RenameTarget(added, removed);
                continue;
            }
            // A rename is credited to where the file landed, because the
            // categories, the ignore globs, `--path` and the reported file
            // list all describe the tree as it stands now.
            Expected::RenameTarget(added, removed) => {
                record_change(&mut pending, added, removed, rest, includes, ignores);
                continue;
            }
            Expected::Change => {}
        }

        if let Some(header) = rest.strip_prefix("W\t") {
            let (header, remainder) = header.split_once('\n').unwrap_or((header, ""));
            emit(&mut commits, &mut repo_seen, std::mem::take(&mut pending));
            // Three-way for the same reason the change fields are: whatever
            // follows the date is one piece, tabs and all.
            let mut header = header.splitn(3, '\t');
            pending.sha = header.next().unwrap_or_default().to_string();
            pending.timestamp = header
                .next()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            pending.authorship = pass.authorship;
            // Absent unless the pass asked for trailers, and present but empty
            // for a commit that carries none.
            for co_author in header
                .next()
                .unwrap_or_default()
                .split('\u{2}')
                .filter(|value| !value.is_empty())
            {
                // A flag on the commit already being counted. An agent named
                // here did not write a second commit, and inventing a signal
                // for it would count one piece of work twice.
                pending.authorship.note_co_author(co_author);
            }
            rest = remainder;
        }

        // Three-way, so that a tab inside the path stays part of the path.
        let mut fields = rest.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Some(added), Some(removed)) = (parse_numstat(added), parse_numstat(removed)) else {
            continue;
        };
        if path.is_empty() {
            expected = Expected::RenameSource(added, removed);
            continue;
        }
        record_change(&mut pending, added, removed, path, includes, ignores);
    }
    emit(&mut commits, &mut repo_seen, pending);
    commits
}

fn record_change(
    pending: &mut PendingCommit,
    added: u64,
    removed: u64,
    path: &str,
    includes: Option<&GlobSet>,
    ignores: Option<&GlobSet>,
) {
    if includes.is_some_and(|patterns| !patterns.is_match(path)) {
        return;
    }
    pending.matched_file = true;
    // Matching the real path is enough now. The old parser also matched the
    // raw field, because a rename shared its spelling with a filename that
    // genuinely contains " => " — resolving one of those stripped the
    // directory the vendor rule matches on, so `node_modules/a => b.js`
    // arrived as `b.js` and counted as authored source. `-z` states the rename
    // instead of spelling it into the path, so there is no second spelling
    // left to defend against, and a file moved *out* of an ignored directory
    // is now counted from the commit that moved it.
    if ignores.is_some_and(|patterns| patterns.is_match(path)) {
        pending.ignored_additions += added;
        pending.ignored_deletions += removed;
    } else {
        pending.additions += added;
        pending.deletions += removed;
        pending.categories.add(classify(path), added, removed);
        pending.files.push(path.to_string());
    }
}

fn parse_numstat(value: &str) -> Option<u64> {
    if value == "-" {
        Some(0)
    } else {
        value.parse().ok()
    }
}

fn compile_globs(patterns: &[String]) -> Result<Option<GlobSet>, globset::Error> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let mut variants = vec![pattern.clone()];
        if !pattern.starts_with('/') {
            variants.push(format!("/{pattern}"));
        }
        // `*/node_modules/*` needs something before the slash, but Git reports
        // repository-relative paths — so a top-level `node_modules/` has
        // nothing there and would escape the rule. Anchor it at the root too.
        if let Some(rooted) = pattern.strip_prefix("*/") {
            variants.push(rooted.to_string());
        }
        for variant in variants {
            builder.add(
                GlobBuilder::new(&variant)
                    .literal_separator(false)
                    .backslash_escape(true)
                    .build()?,
            );
        }
    }
    builder.build().map(Some)
}

fn iso(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::active_registry;
    use crate::paths::SourceRule;
    use tempfile::tempdir;

    fn git(arguments: &[&str]) {
        let status = Command::new(git_executable().expect("Git is required for this test"))
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {arguments:?}");
    }

    /// A repository with an author configured, ready to commit into.
    fn repository(base: &Path, relative: &str) -> PathBuf {
        let repo = base.join(relative);
        fs::create_dir_all(&repo).unwrap();
        let path = repo.to_str().unwrap().to_string();
        git(&["init", "-q", &path]);
        git(&["-C", &path, "config", "user.name", "Test Author"]);
        git(&["-C", &path, "config", "user.email", "test@example.com"]);
        repo
    }

    fn lines(commit: &GitCommit, category: &str) -> (u64, u64) {
        let registry = active_registry();
        let index = registry.index_of(category).expect("known category");
        let lines = commit.categories.get(index);
        (lines.additions, lines.deletions)
    }

    /// The pass the ordinary author scan runs, for tests that drive the parser
    /// over a literal stream. The author list is unused there — that filtering
    /// happened inside Git.
    fn human_pass() -> Pass<'static> {
        Pass {
            authors: &[],
            authorship: Authorship::default(),
            co_authors: false,
        }
    }

    fn agent_authors() -> Vec<String> {
        DEFAULT_AGENT_AUTHORS
            .iter()
            .map(|value| (*value).to_string())
            .collect()
    }

    /// One more line in one file, committed as `author`, so that every commit
    /// in a fixture has a diff and the identities are the only thing that
    /// differs between them.
    fn commit_as(repo: &Path, body: &mut String, author: &str, message: &[&str]) {
        let path = repo.to_str().unwrap().to_string();
        let author = format!("--author={author}");
        body.push_str("line\n");
        fs::write(repo.join("code.rs"), body.as_str()).unwrap();
        git(&["-C", path.as_str(), "add", "."]);
        let mut arguments = vec!["-C", path.as_str(), "commit", "-q", author.as_str()];
        for part in message {
            arguments.push("-m");
            arguments.push(part);
        }
        git(&arguments);
    }

    fn shas(commits: &[GitCommit]) -> HashSet<String> {
        commits.iter().map(|commit| commit.sha.clone()).collect()
    }

    /// The identities are the ones this machine's repositories actually carry,
    /// including *both* numeric ids GitHub has issued for the one Copilot
    /// account. A matcher keyed on the id passes the first and silently drops
    /// the second, which is the whole reason the address suffix is what is
    /// matched.
    ///
    /// The other half of the test is the more important one: running the agent
    /// pass must leave the author scan reporting exactly what it reported
    /// before. Widening `--author` instead of adding a pass is the mistake this
    /// guards against, and it would show up here as the author scan suddenly
    /// finding an agent's commits.
    #[test]
    fn the_agent_pass_finds_the_bots_and_leaves_the_author_filter_alone() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/agents");
        let mut body = String::new();
        commit_as(
            &repo,
            &mut body,
            "Test Author <test@example.com>",
            &["mine"],
        );
        for author in [
            "copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>",
            "Copilot <223556219+Copilot@users.noreply.github.com>",
            "GitHub Copilot <copilot@github.com>",
        ] {
            commit_as(&repo, &mut body, author, &["Initial plan"]);
        }
        // Automation, but not an agent: counting a version bump as agent output
        // would misdescribe both.
        commit_as(
            &repo,
            &mut body,
            "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>",
            &["bump"],
        );

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let agents = read_agent_commits(
            base.path(),
            &agent_authors(),
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
        );
        assert_eq!(3, agents.len(), "both Copilot ids and the web identity");
        assert!(
            agents
                .iter()
                .all(|commit| commit.authorship.is_agent_authored())
        );

        let mine = read_git_commits(
            base.path(),
            "test@example.com",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, mine.len(), "the author filter must not have widened");
        assert!(!mine[0].authorship.is_agent_authored());
        assert!(shas(&mine).is_disjoint(&shas(&agents)));

        // Asking for no identities at all runs no second pass rather than
        // matching every author.
        assert!(
            read_agent_commits(
                base.path(),
                &[],
                &mut resolver,
                &mut diagnostics,
                3,
                None,
                None,
                None,
                &[],
                &[],
                false,
            )
            .is_empty()
        );
        assert_eq!(0, diagnostics.git_errors);
    }

    /// A commit the developer wrote with an agent is one commit. The trailer
    /// says how it was written, so it sets a flag on the commit already being
    /// counted — a second commit, or a second signal, would be the same work
    /// counted twice.
    #[test]
    fn a_co_authored_commit_is_one_commit_carrying_a_flag() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/pairs");
        let mut body = String::new();
        let me = "Test Author <test@example.com>";
        commit_as(&repo, &mut body, me, &["alone"]);
        commit_as(
            &repo,
            &mut body,
            me,
            &[
                "with copilot",
                "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>",
            ],
        );
        commit_as(
            &repo,
            &mut body,
            me,
            &[
                "with autofix",
                "Co-authored-by: Copilot Autofix powered by AI <223894421+github-code-quality[bot]@users.noreply.github.com>",
            ],
        );

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "test@example.com",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            true,
        );
        // Newest first: autofix, copilot, alone.
        assert_eq!(3, commits.len(), "a trailer must not add a commit");
        assert!(
            commits
                .iter()
                .all(|commit| !commit.authorship.is_agent_authored())
        );
        assert!(commits[0].authorship.is_autofix_assisted());
        assert!(
            !commits[0].authorship.is_agent_assisted(),
            "code scanning is not interactive Copilot"
        );
        assert!(commits[1].authorship.is_agent_assisted());
        assert!(!commits[1].authorship.is_autofix_assisted());
        assert_eq!(Authorship::default(), commits[2].authorship);

        // Off by default, and off means the trailers are never asked for —
        // same commits, same lines, no flags.
        let plain = read_git_commits(
            base.path(),
            "test@example.com",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(shas(&commits), shas(&plain));
        assert!(
            plain
                .iter()
                .all(|commit| commit.authorship == Authorship::default())
        );
        assert_eq!(
            commits.iter().map(|commit| commit.additions).sum::<u64>(),
            plain.iter().map(|commit| commit.additions).sum::<u64>()
        );
    }

    /// Git continues a long trailer on an indented line, and without `unfold`
    /// that continuation arrives with a newline inside the header field. The
    /// newline that closes the header would then be found in the middle of the
    /// trailer, and everything after it — the commit's entire diff — would be
    /// read as neither.
    #[test]
    fn a_folded_co_author_trailer_does_not_swallow_the_diff() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/folded");
        let mut body = String::new();
        commit_as(
            &repo,
            &mut body,
            "Test Author <test@example.com>",
            &[
                "folded",
                "Co-authored-by: A Very Long Name\n  Continued <copilot@github.com>",
            ],
        );

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "test@example.com",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            true,
        );
        assert_eq!(1, commits.len());
        assert_eq!(
            1, commits[0].additions,
            "the diff was read past the trailer"
        );
        assert_eq!(vec!["code.rs".to_string()], commits[0].files);
        assert!(commits[0].authorship.is_agent_assisted());
    }

    /// `--author` is a *basic* regular expression, so escaping `+`, `(` or `)`
    /// is what makes them operators rather than what makes them literal. The
    /// end-to-end half matters more than the string comparison: a
    /// plus-addressed author used to be handed `person\+work@…`, which asks for
    /// "one or more n" and matched none of that developer's commits.
    #[test]
    fn configured_author_is_safe_as_a_git_regex() {
        assert_eq!(
            r"person+work@example\.com",
            git_regex_literal("person+work@example.com")
        );
        assert_eq!("A (Team)", git_regex_literal("A (Team)"));
        // `]` needs no escape once `[` carries one: nothing opened a bracket
        // expression for it to close.
        assert_eq!(r"a\.b\[c]\*d", git_regex_literal("a.b[c]*d"));

        let base = tempdir().unwrap();
        let repo = base.path().join("org/plus");
        fs::create_dir_all(&repo).unwrap();
        let path = repo.to_str().unwrap().to_string();
        git(&["init", "-q", &path]);
        git(&["-C", &path, "config", "user.name", "Plus Author"]);
        git(&[
            "-C",
            &path,
            "config",
            "user.email",
            "person+work@example.com",
        ]);
        fs::write(repo.join("main.rs"), "one\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "plus"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            &git_regex_literal("person+work@example.com"),
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len(), "a plus-addressed author found nothing");
    }

    #[test]
    fn reads_commits_and_separates_ignored_lines() {
        let base = tempdir().unwrap();
        let repo = base.path().join("org/repo");
        fs::create_dir_all(&repo).unwrap();
        git(&["init", "-q", repo.to_str().unwrap()]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.name",
            "Test Author",
        ]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        fs::write(repo.join("code.txt"), "one\ntwo\n").unwrap();
        fs::write(repo.join("yarn.lock"), "generated\n").unwrap();
        git(&["-C", repo.to_str().unwrap(), "add", "code.txt", "yarn.lock"]);
        git(&["-C", repo.to_str().unwrap(), "commit", "-qm", "test"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        assert_eq!(2, commits[0].additions);
        assert_eq!(1, commits[0].ignored_additions);
        assert_eq!(0, diagnostics.git_errors);

        let filtered = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &["*.md".into()],
            &[],
            false,
            false,
        );
        assert!(filtered.is_empty());
    }

    /// A filename may legitimately contain " => ", `{` and `}`. Without `-z`
    /// Git spelled a rename the same way, and resolving one of those stripped
    /// the directory the vendor rule matches on — `node_modules/a => b.js`
    /// arrived as `b.js` and was counted as authored source. Under `-z` such a
    /// name is just a name.
    // Windows forbids `>` in a filename, so the ambiguity this guards against
    // cannot arise there and the fixture cannot even be written.
    #[cfg(not(windows))]
    #[test]
    fn a_filename_that_looks_like_a_rename_is_an_ordinary_path() {
        let base = tempdir().unwrap();
        let repo = base.path().join("org/arrows");
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&["init", "-q", repo.to_str().unwrap()]);
        let path = repo.to_str().unwrap();
        git(&["-C", path, "config", "user.name", "Arrow Author"]);
        git(&["-C", path, "config", "user.email", "arrow@example.com"]);
        fs::write(repo.join("node_modules/a => b.js"), "one\ntwo\nthree\n").unwrap();
        fs::write(repo.join("node_modules/plain.js"), "x\ny\n").unwrap();
        fs::write(repo.join("src/x => y.rs"), "real\n").unwrap();
        fs::write(repo.join("src/{braced}.rs"), "real\n").unwrap();
        fs::write(repo.join("src/{a => b}.rs"), "real\n").unwrap();
        git(&["-C", path, "add", "."]);
        git(&["-C", path, "commit", "-qm", "arrows"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Arrow Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        assert_eq!(3, commits[0].additions, "the three files under src");
        assert_eq!(5, commits[0].ignored_additions, "both vendored files");
        let mut files = commits[0].files.clone();
        files.sort();
        // Every name is reported whole: nothing was resolved away.
        assert_eq!(
            vec![
                "src/x => y.rs".to_string(),
                "src/{a => b}.rs".to_string(),
                "src/{braced}.rs".to_string(),
            ],
            files
        );
    }

    /// The exact stream Git wrote for the fixtures that used to be ambiguous:
    /// a rename out of a vendored directory, a commit with no diff at all, a
    /// binary file, a filename holding " => ", and a filename holding a tab.
    #[test]
    fn the_zero_terminated_stream_is_read_field_by_field() {
        let stream: &[u8] =
            b"W\t1111111111111111111111111111111111111111\t2024-05-01T10:00:00+02:00\n\
             4\t2\t\x00node_modules/a => b.js\x00src/moved.rs\x00\
             \x00W\t2222222222222222222222222222222222222222\t2024-05-02T10:00:00+02:00\
             \x00W\t3333333333333333333333333333333333333333\t2024-05-03T10:00:00+02:00\n\
             3\t1\tnode_modules/a => b.js\x00\
             -\t-\tassets/logo.png\x00\
             2\t0\tsrc/tab\there.rs\x00";
        let patterns: Vec<String> = DEFAULT_IGNORES
            .iter()
            .map(|value| (*value).to_string())
            .collect();
        let ignores = compile_globs(&patterns).unwrap();
        let commits = parse_git_log(
            stream,
            &human_pass(),
            "repo",
            "cwd",
            "root",
            None,
            ignores.as_ref(),
            &HashSet::new(),
        );

        // The commit in the middle has no diff, so it contributes no file and
        // is not reported — but its NUL-closed header must not swallow the
        // commit behind it.
        assert_eq!(2, commits.len());
        assert_eq!("1111111111111111111111111111111111111111", commits[0].sha);
        assert_eq!("3333333333333333333333333333333333333333", commits[1].sha);

        // A rename is credited to where it landed, so leaving node_modules
        // makes it authored work.
        assert_eq!(vec!["src/moved.rs".to_string()], commits[0].files);
        assert_eq!((4, 2), (commits[0].additions, commits[0].deletions));
        assert_eq!(0, commits[0].ignored_additions);

        // Whereas the file merely *named* `a => b.js` keeps its directory and
        // stays ignored. The binary counts as zero, and the tab is part of the
        // name rather than a field separator.
        assert_eq!(
            vec![
                "assets/logo.png".to_string(),
                "src/tab\there.rs".to_string()
            ],
            commits[1].files
        );
        assert_eq!((2, 0), (commits[1].additions, commits[1].deletions));
        assert_eq!(
            (3, 1),
            (commits[1].ignored_additions, commits[1].ignored_deletions)
        );
    }

    /// Git quotes and escapes a path holding a quote, a backslash or a control
    /// character whatever `core.quotePath` says — but only when the record
    /// terminator is a newline. The wrapping quote used to become part of the
    /// extension and to hide a vendored path from the root-anchored globs.
    // Windows forbids `"`, `\` and tabs in a filename.
    #[cfg(not(windows))]
    #[test]
    fn paths_git_would_have_quoted_arrive_verbatim() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/hostile");
        let path = repo.to_str().unwrap().to_string();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        fs::write(repo.join("src/say \"hi\".rs"), "one\n").unwrap();
        fs::write(repo.join("src/back\\slash.rs"), "one\n").unwrap();
        fs::write(repo.join("src/tab\there.rs"), "one\n").unwrap();
        fs::write(repo.join("node_modules/a\"b.js"), "generated\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "hostile"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        assert_eq!((3, 0), lines(&commits[0], "source"));
        assert_eq!(1, commits[0].ignored_additions, "the vendored file");
        let mut files = commits[0].files.clone();
        files.sort();
        assert_eq!(
            vec![
                "src/back\\slash.rs".to_string(),
                "src/say \"hi\".rs".to_string(),
                "src/tab\there.rs".to_string(),
            ],
            files
        );
    }

    #[test]
    fn generated_directories_are_ignored_at_the_repository_root_too() {
        let patterns: Vec<String> = DEFAULT_IGNORES
            .iter()
            .map(|value| (*value).to_string())
            .collect();
        let globs = compile_globs(&patterns).unwrap().unwrap();

        // Git reports repository-relative paths, so these have nothing before
        // the first slash and used to escape every `*/directory/*` rule.
        assert!(globs.is_match("node_modules/react/index.js"));
        assert!(globs.is_match("dist/bundle.js"));
        assert!(globs.is_match("bin/workstats"));
        assert!(globs.is_match("vendor/library.go"));

        // Nested matches keep working.
        assert!(globs.is_match("packages/web/node_modules/react/index.js"));
        assert!(globs.is_match("services/api/dist/bundle.js"));

        // Real source is still counted, including lookalike directory names.
        assert!(!globs.is_match("src/app.js"));
        assert!(!globs.is_match("src/bindings/mod.rs"));
        assert!(!globs.is_match("distribution/notes.md"));
    }

    #[test]
    fn commit_lines_are_attributed_to_file_areas() {
        let base = tempdir().unwrap();
        let repo = base.path().join("org/areas");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("tests")).unwrap();
        git(&["init", "-q", repo.to_str().unwrap()]);
        let path = repo.to_str().unwrap();
        git(&["-C", path, "config", "user.name", "Area Author"]);
        git(&["-C", path, "config", "user.email", "area@example.com"]);
        fs::write(repo.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
        fs::write(repo.join("tests/lib_test.rs"), "check\n").unwrap();
        fs::write(repo.join("README.md"), "docs\ndocs\n").unwrap();
        git(&["-C", path, "add", "."]);
        git(&["-C", path, "commit", "-qm", "areas"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Area Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        assert_eq!((3, 0), lines(&commits[0], "source"));
        assert_eq!((1, 0), lines(&commits[0], "test"));
        assert_eq!((2, 0), lines(&commits[0], "docs"));
        assert_eq!((0, 0), lines(&commits[0], "config"));
        assert_eq!(6, commits[0].additions);
    }

    /// The three shapes the old parser had to spell out — a move across
    /// directories, a rename inside one, and a rename with no edit at all —
    /// now arrive as a pair of path fields, and each is credited to the path
    /// the file moved to.
    #[test]
    fn renames_of_every_shape_report_the_path_moved_to() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/renames");
        let path = repo.to_str().unwrap().to_string();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("tests")).unwrap();
        // Rename detection is Git's default, but the test must not depend on
        // whoever runs it having left it that way.
        git(&["-C", &path, "config", "diff.renames", "true"]);
        let body: String = (0..30).map(|line| format!("line {line}\n")).collect();
        for name in ["across", "inside", "pure"] {
            fs::write(repo.join(format!("src/{name}.rs")), &body).unwrap();
        }
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "before"]);

        git(&["-C", &path, "mv", "src/across.rs", "tests/across.rs"]);
        fs::write(repo.join("tests/across.rs"), format!("{body}extra\n")).unwrap();
        git(&["-C", &path, "add", "-A"]);
        git(&["-C", &path, "commit", "-qm", "across directories"]);

        git(&["-C", &path, "mv", "src/inside.rs", "src/inside_new.rs"]);
        fs::write(repo.join("src/inside_new.rs"), format!("{body}extra\n")).unwrap();
        git(&["-C", &path, "add", "-A"]);
        git(&["-C", &path, "commit", "-qm", "inside one directory"]);

        git(&["-C", &path, "mv", "src/pure.rs", "src/pure_new.rs"]);
        git(&["-C", &path, "commit", "-qm", "no edit at all"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        // Newest first, and the initial commit last.
        assert_eq!(4, commits.len());
        assert_eq!(vec!["src/pure_new.rs".to_string()], commits[0].files);
        assert_eq!(0, commits[0].additions, "a pure rename edits nothing");
        assert_eq!(vec!["src/inside_new.rs".to_string()], commits[1].files);
        assert_eq!(1, commits[1].additions);
        assert_eq!(vec!["tests/across.rs".to_string()], commits[2].files);
        assert_eq!(1, commits[2].additions);
    }

    /// A commit with no diff has no change field for its header to be glued
    /// to, so Git closes the header with the field's own NUL instead of a
    /// newline. Misreading that field loses every commit behind it.
    #[test]
    fn an_empty_commit_does_not_hide_the_commits_behind_it() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/empty");
        let path = repo.to_str().unwrap().to_string();
        fs::write(repo.join("first.rs"), "one\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "first"]);
        git(&["-C", &path, "commit", "-q", "--allow-empty", "-m", "empty"]);
        fs::write(repo.join("second.rs"), "one\ntwo\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "second"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(2, commits.len(), "the empty commit touches no file");
        assert_eq!(vec!["second.rs".to_string()], commits[0].files);
        assert_eq!(vec!["first.rs".to_string()], commits[1].files);
    }

    #[test]
    fn a_moved_file_is_counted_where_it_landed() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/moved");
        let path = repo.to_str().unwrap().to_string();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("tests")).unwrap();
        // Rename detection is Git's default, but the test must not depend on
        // whoever runs it having left it that way.
        git(&["-C", &path, "config", "diff.renames", "true"]);
        let body: String = (0..20).map(|line| format!("line {line}\n")).collect();
        fs::write(repo.join("src/helper.rs"), &body).unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "before"]);
        fs::remove_file(repo.join("src/helper.rs")).unwrap();
        fs::write(repo.join("tests/helper.rs"), format!("{body}extra\n")).unwrap();
        git(&["-C", &path, "add", "-A"]);
        git(&["-C", &path, "commit", "-qm", "move"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        let moved = commits
            .iter()
            .find(|commit| {
                commit.additions == 1 && commit.files.iter().any(|file| file.ends_with("helper.rs"))
            })
            .expect("the move commit");
        assert_eq!("tests/helper.rs", moved.files[0]);
        assert_eq!((1, 0), lines(moved, "test"));
        assert_eq!((0, 0), lines(moved, "source"));
    }

    #[test]
    fn non_ascii_paths_arrive_as_real_paths() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "org/unicode");
        let path = repo.to_str().unwrap().to_string();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        fs::write(repo.join("src/spørsmål.rs"), "one\n").unwrap();
        fs::write(repo.join("node_modules/pakke_æøå.js"), "generated\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "unicode"]);

        let mut diagnostics = Diagnostics::default();
        let mut resolver = PathResolver::with_home(Vec::new(), base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            None,
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        // Quoted, octal-escaped paths lose their extension and escape the
        // ignore globs; neither may happen here.
        assert_eq!((1, 0), lines(&commits[0], "source"));
        assert_eq!(1, commits[0].ignored_additions);
        assert!(!commits[0].files.iter().any(|file| file.contains('"')));
    }

    #[test]
    fn the_repository_filter_also_matches_the_source_root() {
        let base = tempdir().unwrap();
        let repo = repository(base.path(), "studio/widget");
        let path = repo.to_str().unwrap().to_string();
        fs::write(repo.join("main.rs"), "one\n").unwrap();
        git(&["-C", &path, "add", "."]);
        git(&["-C", &path, "commit", "-qm", "widget"]);

        let mut diagnostics = Diagnostics::default();
        // A source-root label that appears in neither the repo name nor the
        // path, so only the root can satisfy the filter.
        // No separator in the pattern: the same rule has to match a POSIX path
        // and a Windows one, where the components are joined with backslashes.
        let rule = SourceRule::new(r"^.+widget$", "acme-portfolio").unwrap();
        let mut resolver = PathResolver::with_home(vec![rule], base.path().to_path_buf());
        let commits = read_git_commits(
            base.path(),
            "Test Author",
            &mut resolver,
            &mut diagnostics,
            3,
            None,
            None,
            Some("acme"),
            &[],
            &[],
            false,
            false,
        );
        assert_eq!(1, commits.len());
        assert_eq!("acme-portfolio", commits[0].root);
    }
}
