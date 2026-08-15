use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use tempfile::tempfile;

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
        if repo_filter.is_some_and(|filter| {
            let filter = filter.to_lowercase();
            !repo.to_lowercase().contains(&filter) && !cwd.to_lowercase().contains(&filter)
        }) {
            continue;
        }
        let mut command = Command::new(&git);
        command
            .arg("--no-pager")
            .arg("-C")
            .arg(&repo_path)
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
    let mut current_sha = String::new();
    let mut current_time = None;
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    let mut ignored_additions = 0_u64;
    let mut ignored_deletions = 0_u64;
    let mut files = Vec::new();
    let mut matched_file = false;

    let emit = |commits: &mut Vec<GitCommit>,
                repo_seen: &mut HashSet<String>,
                sha: &str,
                timestamp: Option<DateTime<Utc>>,
                additions: u64,
                deletions: u64,
                ignored_additions: u64,
                ignored_deletions: u64,
                files: &mut Vec<String>,
                matched_file: bool| {
        if let Some(timestamp) = timestamp
            && !sha.is_empty()
            && matched_file
            && !globally_seen.contains(sha)
            && !repo_seen.contains(sha)
        {
            repo_seen.insert(sha.to_string());
            commits.push(GitCommit {
                sha: sha.to_string(),
                timestamp,
                repo: repo.to_string(),
                cwd: cwd.to_string(),
                root: root.to_string(),
                additions,
                deletions,
                files: std::mem::take(files),
                ignored_additions,
                ignored_deletions,
            });
        } else {
            files.clear();
        }
    };

    for line in reader.lines().map_while(Result::ok) {
        if let Some(header) = line.strip_prefix("W\t") {
            emit(
                &mut commits,
                &mut repo_seen,
                &current_sha,
                current_time,
                additions,
                deletions,
                ignored_additions,
                ignored_deletions,
                &mut files,
                matched_file,
            );
            let mut fields = header.splitn(2, '\t');
            current_sha = fields.next().unwrap_or_default().to_string();
            current_time = fields
                .next()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc));
            additions = 0;
            deletions = 0;
            ignored_additions = 0;
            ignored_deletions = 0;
            matched_file = false;
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let added = parse_numstat(fields[0]);
        let removed = parse_numstat(fields[1]);
        if added.is_none() || removed.is_none() {
            continue;
        }
        let file_path = fields.last().copied().unwrap_or_default();
        if includes.is_some_and(|patterns| !patterns.is_match(file_path)) {
            continue;
        }
        matched_file = true;
        if ignores.is_some_and(|patterns| patterns.is_match(file_path)) {
            ignored_additions += added.unwrap();
            ignored_deletions += removed.unwrap();
        } else {
            additions += added.unwrap();
            deletions += removed.unwrap();
            files.push(file_path.to_string());
        }
    }
    emit(
        &mut commits,
        &mut repo_seen,
        &current_sha,
        current_time,
        additions,
        deletions,
        ignored_additions,
        ignored_deletions,
        &mut files,
        matched_file,
    );
    commits
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
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(false)
                .backslash_escape(true)
                .build()?,
        );
        if !pattern.starts_with('/') {
            builder.add(
                GlobBuilder::new(&format!("/{pattern}"))
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
    use tempfile::tempdir;

    fn git(arguments: &[&str]) {
        let status = Command::new(git_executable().expect("Git is required for this test"))
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {arguments:?}");
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
}
