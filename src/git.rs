use std::borrow::Cow;
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
use crate::model::{Diagnostics, GitCommit};
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

fn git_regex_literal(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
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
            // Without this Git octal-escapes and quotes every non-ASCII path,
            // which breaks extension detection and the ignore globs.
            .arg("-c")
            .arg("core.quotePath=false")
            .arg("log")
            .arg("--regexp-ignore-case")
            .arg(format!("--author={author}"))
            .arg("--no-merges")
            .arg("--date=iso-strict")
            .arg("--pretty=format:W%x09%H%x09%aI")
            .arg("--numstat");
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

/// One commit being accumulated across its `--numstat` lines.
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
}

fn parse_git_log(
    reader: impl BufRead,
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
                });
            }
        };

    for line in reader.lines().map_while(Result::ok) {
        if let Some(header) = line.strip_prefix("W\t") {
            emit(&mut commits, &mut repo_seen, std::mem::take(&mut pending));
            let mut fields = header.splitn(2, '\t');
            pending.sha = fields.next().unwrap_or_default().to_string();
            pending.timestamp = fields
                .next()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let added = parse_numstat(fields[0]);
        let removed = parse_numstat(fields[1]);
        let (Some(added), Some(removed)) = (added, removed) else {
            continue;
        };
        let raw = fields.last().copied().unwrap_or_default();
        let resolved = renamed_target(raw);
        let file_path: &str = &resolved;
        if includes.is_some_and(|patterns| !patterns.is_match(file_path)) {
            continue;
        }
        pending.matched_file = true;
        // Both spellings are tested against the ignores. Without `-z`, git's
        // rename notation is ambiguous with a filename that genuinely contains
        // " => ", and resolving such a path strips the directory that the
        // vendor rule matches on — so `node_modules/a => b.js` would be read as
        // `b.js` and counted as authored source. Ignoring on either spelling
        // keeps generated files out; the residual is that a file moved *out* of
        // an ignored directory stays ignored for that one commit.
        if ignores.is_some_and(|patterns| patterns.is_match(file_path) || patterns.is_match(raw)) {
            pending.ignored_additions += added;
            pending.ignored_deletions += removed;
        } else {
            pending.additions += added;
            pending.deletions += removed;
            pending.categories.add(classify(file_path), added, removed);
            pending.files.push(file_path.to_string());
        }
    }
    emit(&mut commits, &mut repo_seen, pending);
    commits
}

/// `--numstat` reports a rename as one field in three shapes: `old => new`,
/// `dir/{old => new}`, and `{old => new}/file`. Categories, ignore globs,
/// `--path`, and the reported file list all expect a real path, so the entry is
/// resolved to where the file ended up before any of them sees it. Renames are
/// left on (`--no-renames` would turn every large move into thousands of
/// phantom added and deleted lines).
fn renamed_target(field: &str) -> Cow<'_, str> {
    let Some(arrow) = field.find(" => ") else {
        return Cow::Borrowed(field);
    };
    let (left, right) = (&field[..arrow], &field[arrow + 4..]);
    if let Some(open) = left.rfind('{')
        && let Some(close) = right.find('}')
    {
        return Cow::Owned(join_components(&format!(
            "{}{}{}",
            &left[..open],
            &right[..close],
            &right[close + 1..]
        )));
    }
    Cow::Owned(join_components(right))
}

/// `dir/{old => }/file` leaves an empty component behind.
fn join_components(path: &str) -> String {
    path.split('/')
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>()
        .join("/")
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

    #[test]
    fn configured_author_is_safe_as_a_git_regex() {
        assert_eq!(
            r"person\+work@example\.com",
            git_regex_literal("person+work@example.com")
        );
        assert_eq!(r"A \(Team\)", git_regex_literal("A (Team)"));
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
        );
        assert!(filtered.is_empty());
    }

    /// A filename may legitimately contain " => ", and without `-z` git writes
    /// it exactly like a rename. Resolving one of those strips the directory
    /// the vendor rule matches on, so the file would be counted as authored
    /// source; matching the ignores on the raw spelling too keeps it out.
    // Windows forbids `>` in a filename, so the ambiguity this guards against
    // cannot arise there and the fixture cannot even be written.
    #[cfg(not(windows))]
    #[test]
    fn a_filename_that_looks_like_a_rename_still_matches_the_ignores() {
        let base = tempdir().unwrap();
        let repo = base.path().join("org/arrows");
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        git(&["init", "-q", repo.to_str().unwrap()]);
        let path = repo.to_str().unwrap();
        git(&["-C", path, "config", "user.name", "Arrow Author"]);
        git(&["-C", path, "config", "user.email", "arrow@example.com"]);
        fs::write(repo.join("node_modules/a => b.js"), "one\ntwo\nthree\n").unwrap();
        fs::write(repo.join("node_modules/plain.js"), "x\ny\n").unwrap();
        fs::write(repo.join("src.rs"), "real\n").unwrap();
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
        );
        assert_eq!(1, commits.len());
        assert_eq!(1, commits[0].additions, "only src.rs is authored");
        assert_eq!(5, commits[0].ignored_additions, "both vendored files");
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
        );
        assert_eq!(1, commits.len());
        assert_eq!((3, 0), lines(&commits[0], "source"));
        assert_eq!((1, 0), lines(&commits[0], "test"));
        assert_eq!((2, 0), lines(&commits[0], "docs"));
        assert_eq!((0, 0), lines(&commits[0], "config"));
        assert_eq!(6, commits[0].additions);
    }

    #[test]
    fn rename_entries_resolve_to_the_path_the_file_landed_on() {
        assert_eq!("tests/b.rs", renamed_target("src/a.rs => tests/b.rs"));
        assert_eq!("tests/b.rs", renamed_target("tests/{a.rs => b.rs}"));
        assert_eq!(
            "tests/helper.rs",
            renamed_target("{src => tests}/helper.rs")
        );
        assert_eq!(
            "src/nested/file.rs",
            renamed_target("src/{ => nested}/file.rs")
        );
        assert_eq!("src/file.rs", renamed_target("src/{nested => }/file.rs"));
        // An ordinary path is untouched.
        assert_eq!("src/main.rs", renamed_target("src/main.rs"));
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
        );
        assert_eq!(1, commits.len());
        assert_eq!("acme-portfolio", commits[0].root);
    }
}
