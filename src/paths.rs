use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::model::{Diagnostics, RawSession, Session};

#[derive(Debug)]
pub struct SourceRule {
    replacement: String,
    compiled: Regex,
}

impl SourceRule {
    pub fn new(pattern: impl Into<String>, replacement: impl Into<String>) -> Result<Self> {
        let pattern = pattern.into();
        let replacement = replacement.into();
        if pattern.len() > 512 || replacement.len() > 256 {
            bail!("source rule is too long");
        }
        let unsafe_nested = Regex::new(r"\)[*+{?]|[*+}][*+{?]|\\[1-9]").expect("static regex");
        if pattern.contains('|')
            || pattern.contains("(?")
            || unsafe_nested.is_match(&pattern)
            || (pattern.contains(".*") && !Regex::new(r"\.\*(?:\$)?$").unwrap().is_match(&pattern))
        {
            bail!("source rule is outside the safe path-regex subset");
        }
        let compiled = Regex::new(&pattern).context("invalid source-rule regex")?;
        let replacement = normalize_backreferences(&replacement);
        Ok(Self {
            replacement,
            compiled,
        })
    }

    pub fn apply(&self, path: &str) -> Option<String> {
        let candidate: String = path.chars().take(4096).collect();
        self.compiled.is_match(&candidate).then(|| {
            self.compiled
                .replace(&candidate, self.replacement.as_str())
                .into_owned()
        })
    }
}

fn normalize_backreferences(value: &str) -> String {
    let backref = Regex::new(r"\\([1-9])").expect("static regex");
    backref.replace_all(value, "$$${1}").into_owned()
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub source_roots: Vec<ConfigRule>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigRule {
    pub pattern: String,
    pub replacement: String,
}

pub fn load_config(path: Option<&Path>, diagnostics: &mut Diagnostics) -> Config {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    if !path.exists() {
        return Config::default();
    }
    match fs::read(&path)
        .with_context(|| format!("cannot read {}", path.display()))
        .and_then(|bytes| serde_json::from_slice(&bytes).context("invalid JSON"))
    {
        Ok(config) => config,
        Err(error) => {
            diagnostics.warn(format!("config ignored ({}): {error}", path.display()));
            Config::default()
        }
    }
}

pub fn configured_rules(config: Config, command_line: &[String]) -> Result<Vec<SourceRule>> {
    if command_line.len() + config.source_roots.len() > 32 {
        bail!("at most 32 source rules are supported");
    }
    let mut rules = Vec::new();
    for value in command_line {
        let Some((pattern, replacement)) = value.split_once('=') else {
            bail!("source rule must be REGEX=REPLACEMENT");
        };
        rules.push(SourceRule::new(pattern, replacement)?);
    }
    for rule in config.source_roots {
        rules.push(SourceRule::new(rule.pattern, rule.replacement)?);
    }
    Ok(rules)
}

pub struct PathResolver {
    rules: Vec<SourceRule>,
    home: PathBuf,
    repo_cache: HashMap<String, String>,
}

impl PathResolver {
    pub fn new(rules: Vec<SourceRule>) -> Self {
        Self::with_home(rules, home_dir())
    }

    pub fn with_home(rules: Vec<SourceRule>, home: PathBuf) -> Self {
        Self {
            rules,
            home: canonicalize_path(&home),
            repo_cache: HashMap::new(),
        }
    }

    pub fn canonicalize(&self, cwd: &str) -> String {
        let expanded = expand_path(cwd, &self.home);
        canonicalize_path(&expanded).to_string_lossy().into_owned()
    }

    pub fn nearest_repo(&mut self, cwd: &str) -> String {
        let canonical = self.canonicalize(cwd);
        if let Some(value) = self.repo_cache.get(&canonical) {
            return value.clone();
        }
        let path = PathBuf::from(&canonical);
        let mut current = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(&path).to_path_buf()
        };
        let answer = loop {
            if current.join(".git").exists() {
                break current.to_string_lossy().into_owned();
            }
            let Some(parent) = current.parent() else {
                break canonical.clone();
            };
            if parent == current {
                break canonical.clone();
            }
            current = parent.to_path_buf();
        };
        self.repo_cache.insert(canonical, answer.clone());
        answer
    }

    pub fn source_root(&self, path: &str) -> String {
        let canonical = self.canonicalize(path);
        for rule in &self.rules {
            if let Some(result) = rule.apply(&canonical) {
                return result;
            }
        }
        let system_temporary = env::temp_dir();
        let temporary = canonicalize_path(&system_temporary);
        if Path::new(&canonical).starts_with(&system_temporary)
            || Path::new(&canonical).starts_with(&temporary)
            || canonical == "/tmp"
            || canonical.starts_with("/tmp/")
            || canonical.starts_with("/private/tmp/")
        {
            return "tmp/scratch".to_string();
        }
        let path = Path::new(&canonical);
        if let Ok(relative) = path.strip_prefix(&self.home) {
            return relative
                .components()
                .find_map(|part| match part {
                    Component::Normal(value) => Some(format!("~/{}", value.to_string_lossy())),
                    _ => None,
                })
                .unwrap_or_else(|| "~".to_string());
        }
        path.parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "filesystem".to_string())
    }

    pub fn repo_label(&self, repo: &str) -> String {
        let path = Path::new(repo);
        if let Ok(relative) = path.strip_prefix(&self.home)
            && relative.as_os_str().is_empty()
        {
            return "~".to_string();
        }
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| repo.to_string())
    }

    pub fn describe(&mut self, cwd: &str) -> (String, String, String) {
        let canonical_cwd = self.canonicalize(cwd);
        let repo_path = self.nearest_repo(&canonical_cwd);
        let repo = self.repo_label(&repo_path);
        let root = self.source_root(&repo_path);
        (canonical_cwd, repo, root)
    }

    pub fn resolve_session(&mut self, raw: RawSession) -> Session {
        let (cwd, repo, root) = self.describe(&raw.cwd);
        Session {
            provider: raw.provider,
            session_id: raw.session_id,
            cwd,
            repo,
            root,
            points: raw.points,
            exact_intervals: raw.exact_intervals,
            human_points: raw.human_points,
            is_subagent: raw.is_subagent,
        }
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOMEDRIVE")
                .zip(env::var_os("HOMEPATH"))
                .map(|(drive, path)| {
                    let mut home = PathBuf::from(drive);
                    home.push(path);
                    home
                })
        })
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_CONFIG") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("workstats/config.json");
    }
    #[cfg(windows)]
    if let Some(path) = env::var_os("APPDATA") {
        return PathBuf::from(path).join("workstats/config.json");
    }
    home_dir().join(".config/workstats/config.json")
}

pub fn default_cache_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_CACHE") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("workstats/index.sqlite3");
    }
    #[cfg(windows)]
    if let Some(path) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("workstats/cache/index.sqlite3");
    }
    home_dir().join(".cache/workstats/index.sqlite3")
}

pub fn lossy_claude_cwd(project_dir: &Path) -> String {
    let encoded = project_dir
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    if encoded.len() >= 3
        && encoded.as_bytes()[0].is_ascii_alphabetic()
        && &encoded.as_bytes()[1..3] == b"--"
    {
        let drive = encoded.chars().next().unwrap().to_ascii_uppercase();
        return format!("{drive}:/{}", encoded[3..].replace('-', "/"));
    }
    let decoded = encoded.replace('-', "/");
    if decoded.starts_with('/') {
        decoded
    } else {
        format!("/{decoded}")
    }
}

fn expand_path(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = value.strip_prefix("~\\") {
        return home.join(rest);
    }
    PathBuf::from(value)
}

fn canonicalize_path(path: &Path) -> PathBuf {
    if let Ok(result) = path.canonicalize() {
        return result;
    }
    if path.is_absolute() {
        normalize_path(path)
    } else {
        env::current_dir()
            .map(|cwd| normalize_path(&cwd.join(path)))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_root_defaults_and_custom_rule_match_reference() {
        let project = PathBuf::from("/work/sourcecode/repos/studio/widget");
        let resolver = PathResolver::with_home(Vec::new(), PathBuf::from("/home/test"));
        assert_eq!("studio", resolver.source_root(&project.to_string_lossy()));
        let rule = SourceRule::new(r"^/work/clients/([^/]+)/.*", r"client/\1").unwrap();
        assert_eq!(
            Some("client/acme".into()),
            rule.apply("/work/clients/acme/repo")
        );
        assert!(SourceRule::new(r"^(a|aa)+$", "bad").is_err());
        assert_eq!(
            "tmp/scratch",
            resolver.source_root(&env::temp_dir().join("workstats-scratch").to_string_lossy())
        );
    }

    #[test]
    fn claude_project_path_fallback_is_lossy_but_absolute() {
        assert_eq!(
            "/tmp/real/project",
            lossy_claude_cwd(Path::new("-tmp-real-project"))
        );
        assert_eq!(
            "C:/Users/test/project",
            lossy_claude_cwd(Path::new("C--Users-test-project"))
        );
    }
}
