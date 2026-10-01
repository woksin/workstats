use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::classify::{CategoryMode, CategoryRegistry, CategoryRules};
use crate::model::{BranchSource, Diagnostics, RawSession, Session};

#[derive(Clone, Debug)]
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
    #[serde(default)]
    pub check_updates: Option<bool>,
    /// Git author patterns for the developer's identities, used when neither
    /// `--author` nor `WORKSTATS_AUTHOR` is given. Each is a `git log
    /// --author` basic regular expression, exactly as on the command line, and
    /// they are OR-ed. A single string is accepted for a single identity.
    /// Kept as raw JSON so a value of any other type is a hard error naming
    /// `authors` rather than the whole config being ignored with a warning.
    #[serde(default)]
    pub authors: Option<serde_json::Value>,
    /// File-area rules, keyed by category name. A name the built-ins do not
    /// know creates a new category.
    #[serde(default)]
    pub categories: BTreeMap<String, CategoryRules>,
    /// `"extend"` (default) or `"replace"`.
    #[serde(default)]
    pub category_mode: Option<String>,
    /// Explicitly combines several Git repositories into one product/project.
    /// The map key is the stable grouping id; `label` is display-only.
    #[serde(default)]
    pub project_aliases: BTreeMap<String, ProjectAliasConfig>,
    /// Per-model list rates that override the built-in table used by
    /// `allocate`. The key is a model-name prefix; see `pricing::RateOverrides`.
    /// Kept as raw JSON and checked entry by entry when compiled: typed here, a
    /// misspelt field or a wrong type would make serde reject the whole file,
    /// and everything else in it (authors, defaults, aliases) would be lost
    /// with only a warning.
    #[serde(default)]
    pub model_rates: Option<serde_json::Value>,
    /// Everyday flags (`dir`, `depth`, `format`, `providers`, `group_by`,
    /// `gap_cap`, `human_idle`, `review_credit`) that apply when the flag and
    /// its environment variable are absent. Kept as raw JSON so a bad key or
    /// value is a hard error naming it, like `project_aliases`, rather than
    /// the whole config being ignored with a warning.
    #[serde(default)]
    pub defaults: Option<serde_json::Value>,
}

impl Config {
    pub fn category_registry(&self) -> Result<CategoryRegistry> {
        let mode = CategoryMode::parse(self.category_mode.as_deref())?;
        CategoryRegistry::from_config(&self.categories, mode)
            .context("invalid \"categories\" configuration")
    }

    pub fn compiled_project_aliases(&self, home: &Path) -> Result<ProjectAliases> {
        ProjectAliases::compile(&self.project_aliases, home)
            .context("invalid \"project_aliases\" configuration")
    }

    pub fn configured_authors(&self) -> Result<Vec<String>> {
        let Some(value) = &self.authors else {
            return Ok(Vec::new());
        };
        let invalid = || {
            anyhow::anyhow!(
                "invalid \"authors\" configuration: expected a string or a list of strings, got {value}"
            )
        };
        match value {
            serde_json::Value::Null => Ok(Vec::new()),
            serde_json::Value::String(author) => Ok(vec![author.clone()]),
            serde_json::Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_string).ok_or_else(invalid))
                .collect(),
            _ => Err(invalid()),
        }
    }

    pub fn config_defaults(&self, home: &Path) -> Result<crate::cli::ConfigDefaults> {
        match &self.defaults {
            Some(value) => crate::cli::ConfigDefaults::parse(value, home),
            None => Ok(crate::cli::ConfigDefaults::default()),
        }
    }

    pub fn compiled_model_rates(&self) -> Result<crate::pricing::RateOverrides> {
        match &self.model_rates {
            Some(value) => crate::pricing::RateOverrides::from_value(value)
                .context("invalid \"model_rates\" configuration"),
            None => Ok(crate::pricing::RateOverrides::default()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfigRule {
    pub pattern: String,
    pub replacement: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAliasConfig {
    pub label: String,
    #[serde(default)]
    pub remotes: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RepositoryHistoryEntry {
    pub cwd_key: String,
    pub natural_id: String,
    pub label: String,
    pub repo_path: PathBuf,
}

#[derive(Clone, Debug)]
struct ProjectAlias {
    key: String,
    label: String,
    remotes: HashSet<String>,
    paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectAliases {
    aliases: Vec<ProjectAlias>,
}

impl ProjectAliases {
    fn compile(config: &BTreeMap<String, ProjectAliasConfig>, home: &Path) -> Result<Self> {
        if config.len() > 64 {
            bail!("at most 64 project aliases are supported");
        }
        let valid_key = Regex::new(r"^[a-z][a-z0-9_-]{0,63}$").expect("static regex");
        let mut aliases = Vec::new();
        let mut claimed_labels: HashMap<String, String> = HashMap::new();
        let mut claimed_remotes: HashMap<String, String> = HashMap::new();
        let mut claimed_paths: Vec<(PathBuf, String)> = Vec::new();
        let mut total_members = 0;
        for (key, value) in config {
            if !valid_key.is_match(key) {
                bail!("project alias key {key:?} must use lowercase letters, numbers, '_' or '-'");
            }
            let label = value.label.trim();
            if label.is_empty()
                || label.len() > 128
                || label.chars().any(|character| character.is_control())
            {
                bail!("project alias {key:?} has an invalid label");
            }
            let normalized_label = label.to_lowercase();
            if let Some(other) = claimed_labels.insert(normalized_label, key.clone()) {
                bail!("project aliases {other:?} and {key:?} use the same label");
            }
            if value.remotes.is_empty() && value.paths.is_empty() {
                bail!("project alias {key:?} must name at least one remote or path");
            }
            total_members += value.remotes.len() + value.paths.len();
            if total_members > 128 {
                bail!("at most 128 project alias members are supported");
            }
            let mut remotes = HashSet::new();
            for remote in &value.remotes {
                let Some((_, identity)) = remote_repository(remote, Path::new("/")) else {
                    bail!("project alias {key:?} has an invalid remote");
                };
                if let Some(other) = claimed_remotes.insert(identity.clone(), key.clone())
                    && other != *key
                {
                    bail!("project aliases {other:?} and {key:?} claim the same remote");
                }
                remotes.insert(format!("remote:{identity}"));
            }
            let mut paths = Vec::new();
            for path in &value.paths {
                let expanded = expand_path(path, home);
                let path = canonicalize_path(&expanded);
                if let Some((_, other)) = claimed_paths
                    .iter()
                    .find(|(claimed, _)| path.starts_with(claimed) || claimed.starts_with(&path))
                {
                    bail!("project aliases {other:?} and {key:?} have overlapping paths");
                }
                claimed_paths.push((path.clone(), key.clone()));
                paths.push(path);
            }
            aliases.push(ProjectAlias {
                key: key.clone(),
                label: label.to_string(),
                remotes,
                paths,
            });
        }
        Ok(Self { aliases })
    }

    fn resolve<'a>(
        &'a self,
        natural_id: &str,
        repo_path: &Path,
    ) -> std::result::Result<Option<(&'a str, &'a str)>, Vec<&'a str>> {
        let matches: Vec<_> = self
            .aliases
            .iter()
            .filter(|alias| {
                alias.remotes.contains(natural_id)
                    || alias.paths.iter().any(|path| repo_path.starts_with(path))
            })
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [alias] => Ok(Some((alias.key.as_str(), alias.label.as_str()))),
            aliases => Err(aliases.iter().map(|alias| alias.key.as_str()).collect()),
        }
    }
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

pub fn configured_rules(config: &Config, command_line: &[String]) -> Result<Vec<SourceRule>> {
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
    for rule in &config.source_roots {
        rules.push(SourceRule::new(&rule.pattern, &rule.replacement)?);
    }
    Ok(rules)
}

#[derive(Clone, Debug)]
struct RepositoryResolution {
    label: String,
    natural_label: String,
    repo_id: String,
    natural_id: Option<String>,
    repo_path: PathBuf,
    method: &'static str,
    aliased: bool,
}

#[derive(Clone, Debug, Default)]
struct AttributionCheckout {
    label: String,
    methods: HashSet<String>,
    natural_ids: HashSet<String>,
    aliased: bool,
}

#[derive(Clone, Debug, Default)]
struct AttributionProject {
    label: String,
    methods: HashSet<String>,
    checkouts: HashSet<String>,
    history_checkouts: HashSet<String>,
    natural_ids: HashSet<String>,
    aliased: bool,
}

pub struct PathResolver {
    rules: Vec<SourceRule>,
    home: PathBuf,
    aliases: ProjectAliases,
    repo_cache: HashMap<String, String>,
    repository_cache: HashMap<String, RepositoryResolution>,
    history: HashMap<String, Vec<RepositoryHistoryEntry>>,
    observations: HashMap<(String, String), RepositoryHistoryEntry>,
    attribution: HashMap<(String, String), AttributionCheckout>,
    history_ambiguities: HashSet<String>,
    alias_conflicts: HashSet<String>,
}

impl PathResolver {
    #[cfg(test)]
    pub fn with_home(rules: Vec<SourceRule>, home: PathBuf) -> Self {
        Self::with_context(rules, ProjectAliases::default(), Vec::new(), home)
    }

    pub fn with_context(
        rules: Vec<SourceRule>,
        aliases: ProjectAliases,
        history: Vec<RepositoryHistoryEntry>,
        home: PathBuf,
    ) -> Self {
        let mut by_cwd: HashMap<String, Vec<RepositoryHistoryEntry>> = HashMap::new();
        for entry in history {
            by_cwd.entry(entry.cwd_key.clone()).or_default().push(entry);
        }
        Self {
            rules,
            home: canonicalize_path(&home),
            aliases,
            repo_cache: HashMap::new(),
            repository_cache: HashMap::new(),
            history: by_cwd,
            observations: HashMap::new(),
            attribution: HashMap::new(),
            history_ambiguities: HashSet::new(),
            alias_conflicts: HashSet::new(),
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

    /// Returns the stable display label and grouping identity for a checkout.
    ///
    /// A linked worktree has its branch or task name on disk, not the name of
    /// the repository it belongs to. Clones can likewise have arbitrary local
    /// folder names. Git's common directory and configured fetch remote are
    /// the local facts that connect those checkouts, so use them without ever
    /// contacting the remote. The identity stays internal; the label remains a
    /// short repository name in reports.
    fn apply_alias(&mut self, mut resolution: RepositoryResolution) -> RepositoryResolution {
        let natural_id = resolution.natural_id.as_deref().unwrap_or_default();
        match self.aliases.resolve(natural_id, &resolution.repo_path) {
            Ok(Some((key, label))) => {
                resolution.repo_id = format!("project:{key}");
                resolution.label = label.to_string();
                resolution.aliased = true;
            }
            Ok(None) => {}
            Err(mut aliases) => {
                aliases.sort_unstable();
                self.alias_conflicts.insert(aliases.join(", "));
                resolution.method = "ambiguous_project_alias";
            }
        }
        resolution
    }

    fn repository(&mut self, repo: &str) -> RepositoryResolution {
        if let Some(value) = self.repository_cache.get(repo) {
            return value.clone();
        }
        let value = self.apply_alias(repository(repo, &self.home));
        self.repository_cache
            .insert(repo.to_string(), value.clone());
        value
    }

    fn history_key(&self, cwd: &str) -> String {
        normalize_path(&expand_path(cwd, &self.home))
            .to_string_lossy()
            .into_owned()
    }

    fn history_resolution(&mut self, cwd: &str) -> Option<RepositoryResolution> {
        if Path::new(cwd).exists() {
            return None;
        }
        let mut candidates = Vec::new();
        for key in [cwd.to_string(), self.history_key(cwd)] {
            if let Some(entries) = self.history.get(&key) {
                for entry in entries {
                    if !candidates.iter().any(|candidate: &RepositoryHistoryEntry| {
                        candidate.natural_id == entry.natural_id
                    }) {
                        candidates.push(entry.clone());
                    }
                }
            }
        }
        match candidates.as_slice() {
            [entry] => Some(self.apply_alias(RepositoryResolution {
                label: entry.label.clone(),
                natural_label: entry.label.clone(),
                repo_id: entry.natural_id.clone(),
                natural_id: Some(entry.natural_id.clone()),
                repo_path: entry.repo_path.clone(),
                method: "repository_history",
                aliased: false,
            })),
            [] => None,
            _ => {
                self.history_ambiguities.insert(self.history_key(cwd));
                None
            }
        }
    }

    fn observe(&mut self, cwd: &str, resolution: &RepositoryResolution) {
        let Some(natural_id) = resolution.natural_id.as_ref() else {
            return;
        };
        let mut keys = HashSet::from([
            cwd.to_string(),
            self.history_key(cwd),
            self.canonicalize(cwd),
        ]);
        for cwd_key in keys.drain() {
            let entry = RepositoryHistoryEntry {
                cwd_key: cwd_key.clone(),
                natural_id: natural_id.clone(),
                label: resolution.natural_label.clone(),
                repo_path: resolution.repo_path.clone(),
            };
            self.observations
                .insert((cwd_key, natural_id.clone()), entry);
        }
    }

    fn note_attribution(&mut self, cwd: &str, resolution: &RepositoryResolution) {
        let checkout = self.history_key(cwd);
        let evidence = self
            .attribution
            .entry((resolution.repo_id.clone(), checkout))
            .or_default();
        evidence.label = resolution.label.clone();
        evidence.methods.insert(resolution.method.to_string());
        if let Some(natural_id) = &resolution.natural_id {
            evidence.natural_ids.insert(natural_id.clone());
        }
        evidence.aliased |= resolution.aliased;
    }

    fn describe_resolution(&mut self, cwd: &str) -> (String, String, RepositoryResolution) {
        let canonical_cwd = self.canonicalize(cwd);
        let repo_path = self.nearest_repo(&canonical_cwd);
        let resolution = self.repository(&repo_path);
        let root = self.source_root(&repo_path);
        (canonical_cwd, root, resolution)
    }

    pub fn describe(&mut self, cwd: &str) -> (String, String, String, String, String) {
        let (canonical_cwd, root, resolution) = self.describe_resolution(cwd);
        self.observe(&canonical_cwd, &resolution);
        self.note_attribution(&canonical_cwd, &resolution);
        let member_id = resolution
            .natural_id
            .clone()
            .unwrap_or_else(|| resolution.repo_id.clone());
        (
            canonical_cwd,
            resolution.label,
            root,
            resolution.repo_id,
            member_id,
        )
    }

    pub fn resolve_session(&mut self, raw: RawSession) -> Session {
        let raw_cwd = raw.cwd.clone();
        let (cwd, mut root, mut resolution) = self.describe_resolution(&raw_cwd);
        let own_resolution = resolution.natural_id.is_some();
        // Temporary Pi worktrees commonly disappear before a report runs. Pi
        // records the parent transcript that delegated the child, and the
        // parser retains that parent's CWD as a repository-only hint. Use it
        // only when the child's own path has no Git identity; a real checkout
        // always wins, including when a subagent deliberately works elsewhere.
        if !own_resolution && let Some(hint) = raw.repository_hint_cwd.as_deref() {
            let (_, hinted_root, mut hinted) = self.describe_resolution(hint);
            if hinted.natural_id.is_some() {
                hinted.method = "parent_session_hint";
                root = hinted_root;
                resolution = hinted;
            }
        }
        if resolution.natural_id.is_none()
            && let Some(historical) = self.history_resolution(&raw_cwd)
        {
            root = self.source_root(&historical.repo_path.to_string_lossy());
            resolution = historical;
        }
        if own_resolution {
            self.observe(&raw_cwd, &resolution);
        }
        self.note_attribution(&cwd, &resolution);
        let branch_source = if raw.branches.is_empty() {
            BranchSource::None
        } else {
            BranchSource::Recorded
        };
        Session {
            provider: raw.provider,
            session_id: raw.session_id,
            cwd,
            repo: resolution.label,
            repo_id: resolution.repo_id,
            root,
            points: raw.points,
            exact_intervals: raw.exact_intervals,
            human_points: raw.human_points,
            token_events: raw.token_events,
            is_subagent: raw.is_subagent,
            source_file: raw.source_file,
            branches: raw.branches,
            branch_source,
            pull_requests: raw.pull_requests,
        }
    }

    pub fn validate_project_aliases(&self) -> Result<()> {
        if self.alias_conflicts.is_empty() {
            return Ok(());
        }
        let mut conflicts: Vec<_> = self.alias_conflicts.iter().cloned().collect();
        conflicts.sort();
        bail!(
            "project aliases overlap through remote and path membership: {}",
            conflicts.join("; ")
        )
    }

    pub fn take_repository_observations(&mut self) -> Vec<RepositoryHistoryEntry> {
        self.observations.drain().map(|(_, entry)| entry).collect()
    }

    pub fn apply_display_labels(&mut self, labels: &HashMap<String, String>) {
        for ((repo_id, _), evidence) in &mut self.attribution {
            if let Some(label) = labels.get(repo_id) {
                evidence.label.clone_from(label);
            }
        }
    }

    pub fn repository_attribution(
        &self,
        included_checkouts: &HashSet<(String, String)>,
    ) -> crate::model::RepositoryAttribution {
        let mut by_project: HashMap<String, AttributionProject> = HashMap::new();
        for ((repo_id, checkout), evidence) in &self.attribution {
            if !included_checkouts.contains(&(repo_id.clone(), checkout.clone())) {
                continue;
            }
            let project = by_project.entry(repo_id.clone()).or_default();
            project.label.clone_from(&evidence.label);
            project.methods.extend(evidence.methods.iter().cloned());
            project.checkouts.insert(checkout.clone());
            if evidence.methods.contains("repository_history") {
                project.history_checkouts.insert(checkout.clone());
            }
            project
                .natural_ids
                .extend(evidence.natural_ids.iter().cloned());
            project.aliased |= evidence.aliased;
        }
        let active_checkout_paths: HashSet<_> = by_project
            .values()
            .flat_map(|project| project.checkouts.iter().cloned())
            .collect();
        let history_hits = by_project
            .values()
            .map(|project| project.history_checkouts.len() as u64)
            .sum();
        let history_ambiguities = self
            .history_ambiguities
            .intersection(&active_checkout_paths)
            .count() as u64;
        let mut projects: Vec<_> = by_project
            .into_values()
            .map(|project| crate::model::RepositoryAttributionProject {
                label: project.label,
                methods: {
                    let mut values: Vec<_> = project.methods.into_iter().collect();
                    values.sort();
                    values
                },
                checkout_count: project.checkouts.len(),
                natural_repository_count: project.natural_ids.len(),
                configured_alias: project.aliased,
                resolved: project.aliased || !project.natural_ids.is_empty(),
            })
            .collect();
        projects.sort_by_key(|left| left.label.to_lowercase());
        let unresolved_checkouts = projects
            .iter()
            .filter(|project| !project.resolved)
            .map(|project| project.checkout_count)
            .sum();
        crate::model::RepositoryAttribution {
            version: "repository-attribution-v1",
            history_hits,
            history_ambiguities,
            unresolved_checkouts,
            projects,
        }
    }
}

/// Git files are tiny in ordinary repositories. Bounding these reads keeps a
/// hostile checkout from turning repository labelling into an unbounded file
/// read while still leaving ample room for a real config.
const MAX_GIT_METADATA_BYTES: u64 = 1024 * 1024;

fn repository(repo: &str, home: &Path) -> RepositoryResolution {
    let path = Path::new(repo);
    let fallback = checkout_label(path, home, repo);
    let Some(common_dir) = common_git_dir(path) else {
        return RepositoryResolution {
            label: fallback.clone(),
            natural_label: fallback,
            repo_id: format!("path:{repo}"),
            natural_id: None,
            repo_path: path.to_path_buf(),
            method: if path.exists() {
                "existing_non_git"
            } else {
                "missing"
            },
            aliased: false,
        };
    };

    if let Some(remote) = remote_url(&common_dir)
        && let Some((label, identity)) = remote_repository(&remote, path)
    {
        let natural_id = format!("remote:{identity}");
        return RepositoryResolution {
            natural_label: label.clone(),
            label,
            repo_id: natural_id.clone(),
            natural_id: Some(natural_id),
            repo_path: path.to_path_buf(),
            method: "remote",
            aliased: false,
        };
    }

    // A linked worktree points at `<primary>/.git/worktrees/<name>` and its
    // `commondir` resolves to `<primary>/.git`. Without a remote, that common
    // directory is still an exact, local identity shared by every worktree.
    let label = common_dir
        .file_name()
        .is_some_and(|name| name == ".git")
        .then(|| common_dir.parent())
        .flatten()
        .map(|primary| checkout_label(primary, home, repo))
        .unwrap_or(fallback);
    let natural_id = format!("git:{}", common_dir.to_string_lossy());
    RepositoryResolution {
        natural_label: label.clone(),
        label,
        repo_id: natural_id.clone(),
        natural_id: Some(natural_id),
        repo_path: path.to_path_buf(),
        method: "git_common_dir",
        aliased: false,
    }
}

fn checkout_label(path: &Path, home: &Path, fallback: &str) -> String {
    if let Ok(relative) = path.strip_prefix(home)
        && relative.as_os_str().is_empty()
    {
        return "~".to_string();
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn common_git_dir(repo: &Path) -> Option<PathBuf> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        canonicalize_path(&dot_git)
    } else {
        let contents = read_git_metadata(&dot_git)?;
        let location = contents
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))?
            .trim();
        canonicalize_path(&if Path::new(location).is_absolute() {
            PathBuf::from(location)
        } else {
            repo.join(location)
        })
    };
    let common = git_dir.join("commondir");
    let Some(location) = read_git_metadata(&common) else {
        // An interrupted/pruned worktree can leave its checkout and `.git`
        // pointer behind after `.git/worktrees/<name>/commondir` disappears.
        // The pointer still names the common directory structurally; recover
        // it without requiring the stale administration directory to exist.
        return orphaned_worktree_common_dir(&git_dir).or(Some(git_dir));
    };
    let location = location.trim();
    if location.is_empty() {
        return Some(git_dir);
    }
    Some(canonicalize_path(&if Path::new(location).is_absolute() {
        PathBuf::from(location)
    } else {
        git_dir.join(location)
    }))
}

fn orphaned_worktree_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let worktrees = git_dir.parent()?;
    if worktrees
        .file_name()
        .is_some_and(|name| name == "worktrees")
    {
        let common = worktrees.parent()?;
        if common.file_name().is_some_and(|name| name == ".git") {
            return Some(common.to_path_buf());
        }
    }
    None
}

fn read_git_metadata(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_GIT_METADATA_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn remote_url(common_dir: &Path) -> Option<String> {
    let config = read_git_metadata(&common_dir.join("config"))?;
    let mut current_remote = None;
    let mut remotes: HashMap<String, String> = HashMap::new();
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            current_remote = remote_section(&line[1..line.len() - 1]);
            continue;
        }
        let Some(remote) = current_remote.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let value = config_value(value);
            if !value.is_empty() {
                remotes.entry(remote.clone()).or_insert(value);
            }
        }
    }
    remotes.remove("origin").or_else(|| {
        (remotes.len() == 1)
            .then(|| remotes.into_values().next())
            .flatten()
    })
}

fn remote_section(section: &str) -> Option<String> {
    let section = section.trim();
    let lowercase = section.to_ascii_lowercase();
    if lowercase.starts_with("remote \"") && section.ends_with('"') {
        return Some(section[8..section.len() - 1].to_ascii_lowercase());
    }
    lowercase.strip_prefix("remote.").map(str::to_string)
}

fn config_value(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1]
            .replace(r#"\""#, "\"")
            .replace(r#"\\"#, "\\")
    } else {
        value.to_string()
    }
}

/// Converts common HTTPS, SSH and scp-like remote spellings into one identity.
/// Credentials and protocols are deliberately excluded, both for privacy and
/// so `git@host:org/repo.git` and `https://host/org/repo.git` combine.
/// A deterministic label candidate accepted by repository filters when a
/// displayed name needed disambiguation. It is safe to compute before all
/// colliding repositories have been discovered; non-colliding repositories
/// continue to display their plain label.
pub fn disambiguated_repository_label(label: &str, repo_id: &str) -> String {
    let digest = Sha256::digest(repo_id.as_bytes());
    let suffix = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{label} [{suffix}]")
}

fn remote_repository(remote: &str, repo: &Path) -> Option<(String, String)> {
    let remote = remote.trim().trim_end_matches('/');
    if remote.is_empty() {
        return None;
    }
    let identity = if let Some((scheme, rest)) = remote.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            canonicalize_path(Path::new(rest))
                .to_string_lossy()
                .into_owned()
        } else {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host)
                .to_ascii_lowercase();
            format!("{host}/{}", path.trim_start_matches('/'))
        }
    } else if let Some((authority, path)) = remote.split_once(':')
        && !authority.is_empty()
        && !path.is_empty()
        && !authority.contains('/')
        && !authority.contains('\\')
        // On Windows every `C:...` spelling is a drive path. When reading a
        // portable config elsewhere, an uppercase drive or a backslash still
        // identifies that spelling, while lowercase `x:/path` remains Git's
        // valid one-letter scp host syntax.
        && !(authority.len() == 1
            && authority.as_bytes()[0].is_ascii_alphabetic()
            && (cfg!(windows)
                || authority.as_bytes()[0].is_ascii_uppercase()
                || path.starts_with('\\')))
    {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host)
            .to_ascii_lowercase();
        format!("{host}/{}", path.trim_start_matches('/'))
    } else {
        let path = Path::new(remote);
        canonicalize_path(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo.join(path)
        })
        .to_string_lossy()
        .into_owned()
    };
    let identity = identity
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(identity.trim_end_matches('/'))
        .to_string();
    let label = identity
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())?
        .to_string();
    (!label.is_empty()).then_some((label, identity))
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

pub fn default_update_check_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_UPDATE_CACHE") {
        return PathBuf::from(path);
    }
    default_cache_path()
        .parent()
        .map(|parent| parent.join("update-check.json"))
        .unwrap_or_else(|| home_dir().join(".cache/workstats/update-check.json"))
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

/// Recovers a working directory from the name of a Pi session directory.
///
/// Pi encodes it as `--<path with the leading separator stripped and `/`, `\` and `:`
/// replaced by `-`>--`. That is only reached when a session header carries no `cwd`,
/// because the header records the resolved absolute path and is always preferred.
///
/// The decoding cannot be exact and is not meant to be: every `-` becomes a separator,
/// so a directory whose own name contains one is indistinguishable from two nested
/// directories. Sessions decoded this way are counted in `approximate_cwds` and reported
/// as approximate, which is why guessing here is safe and silence would not be.
pub fn lossy_pi_cwd(session_dir: &Path) -> String {
    let encoded = session_dir
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    // The wrapping `--` is Pi's marker, not part of the path. Stripping it is what keeps
    // the result from gaining the empty leading and trailing components that a plain
    // separator substitution would produce.
    let inner = encoded
        .strip_prefix("--")
        .and_then(|value| value.strip_suffix("--"))
        .unwrap_or(&encoded);
    // A Windows path arrives as `C--Users-test-project`: the drive's `:` and the
    // separator after it were both replaced, so the drive letter is followed by two
    // dashes. Not gated on the host platform, because a session directory copied from a
    // Windows machine has to decode the same way wherever it is read.
    if inner.len() >= 3
        && inner.as_bytes()[0].is_ascii_alphabetic()
        && &inner.as_bytes()[1..3] == b"--"
    {
        let drive = inner.chars().next().unwrap_or('C').to_ascii_uppercase();
        return format!("{drive}:/{}", inner[3..].replace('-', "/"));
    }
    format!("/{}", inner.replace('-', "/"))
}

pub(crate) fn expand_path(value: &str, home: &Path) -> PathBuf {
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

    fn raw_session(cwd: &Path) -> RawSession {
        RawSession {
            provider: "test".into(),
            session_id: "session".into(),
            source_file: PathBuf::from("session.jsonl"),
            cwd: cwd.to_string_lossy().into_owned(),
            repository_hint_cwd: None,
            points: Vec::new(),
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: false,
            approximate_cwd: false,
            version: None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
        }
    }

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
    fn deleted_temporary_worktree_uses_a_trusted_parent_repository_hint() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("product");
        let child = temporary.path().join("deleted-pi-agent");
        let other = temporary.path().join("other-product");
        for (checkout, remote) in [
            (&parent, "https://github.com/acme/product.git"),
            (&other, "https://github.com/acme/other-product.git"),
        ] {
            fs::create_dir_all(checkout.join(".git")).unwrap();
            fs::write(
                checkout.join(".git/config"),
                format!("[remote \"origin\"]\n\turl = {remote}\n"),
            )
            .unwrap();
        }
        let raw = |cwd: &Path, hint: Option<&Path>| RawSession {
            provider: "pi".into(),
            session_id: "session".into(),
            source_file: temporary.path().join("session.jsonl"),
            cwd: cwd.to_string_lossy().into_owned(),
            repository_hint_cwd: hint.map(|path| path.to_string_lossy().into_owned()),
            points: Vec::new(),
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: true,
            approximate_cwd: false,
            version: None,
            branches: Vec::new(),
            pull_requests: Vec::new(),
        };
        let mut resolver = PathResolver::with_home(Vec::new(), temporary.path().to_path_buf());

        let recovered = resolver.resolve_session(raw(&child, Some(&parent)));
        assert_eq!(child.to_string_lossy(), recovered.cwd);
        assert_eq!("product", recovered.repo);
        assert_eq!("remote:github.com/acme/product", recovered.repo_id);

        // A concrete child repository is authoritative even when its parent
        // session came from another project.
        let concrete = resolver.resolve_session(raw(&other, Some(&parent)));
        assert_eq!("other-product", concrete.repo);
        assert_eq!("remote:github.com/acme/other-product", concrete.repo_id);
    }

    #[test]
    fn linked_worktrees_share_one_logical_repository() {
        let temporary = tempfile::tempdir().unwrap();
        let primary = temporary.path().join("product-primary");
        let worktree = temporary.path().join("feature-checkout");
        let common = primary.join(".git");
        let administration = common.join("worktrees/feature-checkout");
        fs::create_dir_all(&administration).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(common.join("config"), "[core]\n\tbare = false\n").unwrap();
        fs::write(administration.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", administration.display()),
        )
        .unwrap();

        let mut resolver = PathResolver::with_home(Vec::new(), temporary.path().to_path_buf());
        let (_, primary_label, _, primary_id, _) = resolver.describe(&primary.to_string_lossy());
        let (_, worktree_label, _, worktree_id, _) = resolver.describe(&worktree.to_string_lossy());

        assert_eq!("product-primary", primary_label);
        assert_eq!(primary_label, worktree_label);
        assert_eq!(primary_id, worktree_id);
    }

    #[test]
    fn orphaned_worktree_pointer_still_reaches_the_common_repository() {
        let temporary = tempfile::tempdir().unwrap();
        let primary = temporary.path().join("product");
        let orphan = temporary.path().join("orphan-worktree");
        fs::create_dir_all(primary.join(".git")).unwrap();
        fs::create_dir_all(&orphan).unwrap();
        fs::write(
            primary.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/product.git\n",
        )
        .unwrap();
        // The target administration directory deliberately does not exist.
        fs::write(
            orphan.join(".git"),
            format!(
                "gitdir: {}\n",
                primary.join(".git/worktrees/orphan-worktree").display()
            ),
        )
        .unwrap();

        let mut resolver = PathResolver::with_home(Vec::new(), temporary.path().to_path_buf());
        let (_, label, _, repo_id, _) = resolver.describe(&orphan.to_string_lossy());

        assert_eq!("product", label);
        assert_eq!("remote:github.com/acme/product", repo_id);
    }

    #[test]
    fn clones_of_one_remote_share_an_identity_without_collapsing_other_origins() {
        let temporary = tempfile::tempdir().unwrap();
        let ssh = temporary.path().join("task-123");
        let https = temporary.path().join("release-copy");
        let unrelated = temporary.path().join("someone-elses-product");
        for (checkout, remote) in [
            (&ssh, "git@github.com:acme/product.git"),
            (&https, "https://github.com/acme/product.git"),
            (&unrelated, "https://gitlab.example/other/product.git"),
        ] {
            fs::create_dir_all(checkout.join(".git")).unwrap();
            fs::write(
                checkout.join(".git/config"),
                format!("[remote \"origin\"]\n\turl = {remote}\n"),
            )
            .unwrap();
        }

        let mut resolver = PathResolver::with_home(Vec::new(), temporary.path().to_path_buf());
        let (_, ssh_label, _, ssh_id, _) = resolver.describe(&ssh.to_string_lossy());
        let (_, https_label, _, https_id, _) = resolver.describe(&https.to_string_lossy());
        let (_, unrelated_label, _, unrelated_id, _) =
            resolver.describe(&unrelated.to_string_lossy());

        assert_eq!("product", ssh_label);
        assert_eq!(ssh_label, https_label);
        assert_eq!(ssh_id, https_id);
        assert_eq!("product", unrelated_label);
        assert_ne!(ssh_id, unrelated_id);
        assert_eq!(
            Some(("product".to_string(), "work-git/team/product".to_string())),
            remote_repository("work-git:team/product.git", temporary.path())
        );
        if !cfg!(windows) {
            assert_eq!(
                Some(("product".to_string(), "x/team/product".to_string())),
                remote_repository("x:team/product.git", temporary.path())
            );
            assert_eq!(
                Some(("path".to_string(), "x/absolute/path".to_string())),
                remote_repository("x:/absolute/path.git", temporary.path())
            );
        }
        assert_ne!(
            Some("c/repo"),
            remote_repository("C:repo.git", temporary.path())
                .map(|(_, identity)| identity)
                .as_deref()
        );
        assert_ne!(
            Some("remote:c/team/product"),
            remote_repository("C:\\team\\product.git", temporary.path())
                .map(|(_, identity)| format!("remote:{identity}"))
                .as_deref()
        );
    }

    #[test]
    fn configured_project_alias_combines_distinct_repositories_but_not_cwds() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("api");
        let second = temporary.path().join("web");
        for (checkout, remote) in [
            (&first, "https://github.com/acme/api.git"),
            (&second, "git@github.com:acme/web.git"),
        ] {
            fs::create_dir_all(checkout.join(".git")).unwrap();
            fs::write(
                checkout.join(".git/config"),
                format!("[remote \"origin\"]\n\turl = {remote}\n"),
            )
            .unwrap();
        }
        let config = BTreeMap::from([(
            "acme".to_string(),
            ProjectAliasConfig {
                label: "Acme Product".into(),
                remotes: vec![
                    "https://github.com/acme/api.git".into(),
                    "https://github.com/acme/web.git".into(),
                ],
                paths: Vec::new(),
            },
        )]);
        let aliases = ProjectAliases::compile(&config, temporary.path()).unwrap();
        let mut resolver =
            PathResolver::with_context(Vec::new(), aliases, Vec::new(), temporary.path().into());

        let api = resolver.resolve_session(raw_session(&first));
        let web = resolver.resolve_session(raw_session(&second));

        assert_eq!("Acme Product", api.repo);
        assert_eq!(api.repo, web.repo);
        assert_eq!("project:acme", api.repo_id);
        assert_eq!(api.repo_id, web.repo_id);
        assert_ne!(api.cwd, web.cwd);
        resolver.validate_project_aliases().unwrap();
    }

    #[test]
    fn remote_and_path_membership_cannot_silently_choose_different_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let checkout = temporary.path().join("product");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::write(
            checkout.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/product.git\n",
        )
        .unwrap();
        let config = BTreeMap::from([
            (
                "by-remote".to_string(),
                ProjectAliasConfig {
                    label: "Remote product".into(),
                    remotes: vec!["https://github.com/acme/product.git".into()],
                    paths: Vec::new(),
                },
            ),
            (
                "by-path".to_string(),
                ProjectAliasConfig {
                    label: "Path product".into(),
                    remotes: Vec::new(),
                    paths: vec![checkout.to_string_lossy().into_owned()],
                },
            ),
        ]);
        let aliases = ProjectAliases::compile(&config, temporary.path()).unwrap();
        let mut resolver =
            PathResolver::with_context(Vec::new(), aliases, Vec::new(), temporary.path().into());

        resolver.describe(&checkout.to_string_lossy());
        let error = resolver.validate_project_aliases().unwrap_err().to_string();
        assert!(error.contains("by-path, by-remote"), "{error}");
    }

    #[test]
    fn repository_history_recovers_one_deleted_checkout_and_refuses_ambiguity() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("deleted-worktree");
        let key = normalize_path(&missing).to_string_lossy().into_owned();
        let entry = |natural_id: &str, label: &str| RepositoryHistoryEntry {
            cwd_key: key.clone(),
            natural_id: natural_id.into(),
            label: label.into(),
            repo_path: temporary.path().join(label),
        };
        let mut resolver = PathResolver::with_context(
            Vec::new(),
            ProjectAliases::default(),
            vec![entry("remote:host/product", "product")],
            temporary.path().into(),
        );

        let recovered = resolver.resolve_session(raw_session(&missing));

        assert_eq!("product", recovered.repo);
        assert_eq!("remote:host/product", recovered.repo_id);
        let live = temporary.path().join("live-product");
        fs::create_dir_all(live.join(".git")).unwrap();
        fs::write(
            live.join(".git/config"),
            "[remote \"origin\"]\n\turl = host:product.git\n",
        )
        .unwrap();
        let (live_cwd, _, _, live_repo_id, _) = resolver.describe(&live.to_string_lossy());
        assert_eq!(recovered.repo_id, live_repo_id);
        let recovered_key = (recovered.repo_id.clone(), recovered.cwd.clone());
        let attribution = resolver.repository_attribution(&HashSet::from([recovered_key.clone()]));
        assert_eq!(1, attribution.projects[0].checkout_count);
        assert_eq!(1, attribution.history_hits);
        let attribution = resolver
            .repository_attribution(&HashSet::from([recovered_key, (live_repo_id, live_cwd)]));
        assert_eq!(2, attribution.projects[0].checkout_count);
        assert_eq!(1, attribution.history_hits);
        assert_eq!(0, attribution.history_ambiguities);
        assert_eq!(0, attribution.unresolved_checkouts);

        let mut ambiguous = PathResolver::with_context(
            Vec::new(),
            ProjectAliases::default(),
            vec![
                entry("remote:host/product", "product"),
                entry("remote:host/other", "other"),
            ],
            temporary.path().into(),
        );
        let unresolved = ambiguous.resolve_session(raw_session(&missing));
        ambiguous.resolve_session(raw_session(&missing));
        assert!(unresolved.repo_id.starts_with("path:"));
        let attribution = ambiguous
            .repository_attribution(&HashSet::from([(unresolved.repo_id, unresolved.cwd)]));
        assert_eq!(0, attribution.history_hits);
        assert_eq!(1, attribution.history_ambiguities);
        assert_eq!(1, attribution.unresolved_checkouts);
    }

    #[test]
    fn categories_and_source_roots_load_from_the_same_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        fs::write(
            &path,
            r#"{"source_roots": [{"pattern": "^/work/([^/]+)/.*", "replacement": "work/\\1"}],
                "categories": {"ai": {"directories": [".claude"]}},
                "category_mode": "extend"}"#,
        )
        .unwrap();
        let mut diagnostics = Diagnostics::default();
        let config = load_config(Some(&path), &mut diagnostics);
        assert!(
            diagnostics.messages.is_empty(),
            "{:?}",
            diagnostics.messages
        );
        let registry = config.category_registry().unwrap();
        let ai = registry.index_of("ai").expect("configured category");
        assert_eq!(ai, registry.classify(".claude/settings.json"));
        assert_eq!(1, configured_rules(&config, &[]).unwrap().len());
    }

    #[test]
    fn an_unusable_config_is_reported_rather_than_guessed_at() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        // A misspelled rule set must not silently do nothing.
        fs::write(
            &path,
            r#"{"categories": {"ai": {"directory": [".claude"]}}}"#,
        )
        .unwrap();
        let mut diagnostics = Diagnostics::default();
        let config = load_config(Some(&path), &mut diagnostics);
        assert_eq!(1, diagnostics.messages.len());
        assert!(config.categories.is_empty());

        // A bad mode is a hard error rather than a silently ignored setting.
        let config = Config {
            category_mode: Some("merge".to_string()),
            ..Config::default()
        };
        assert!(config.category_registry().is_err());
    }

    #[test]
    fn authors_accept_a_string_or_a_list_and_refuse_anything_else_by_name() {
        let authors = |json: &str| {
            let config: Config = serde_json::from_str(json).unwrap();
            config.configured_authors()
        };
        assert_eq!(
            vec!["me@example.com"],
            authors(r#"{"authors": "me@example.com"}"#).unwrap()
        );
        assert_eq!(
            vec!["a@example.com", "b@example.com"],
            authors(r#"{"authors": ["a@example.com", "b@example.com"]}"#).unwrap()
        );
        assert!(authors("{}").unwrap().is_empty());
        assert!(authors(r#"{"authors": null}"#).unwrap().is_empty());
        for bad in [
            r#"{"authors": 7}"#,
            r#"{"authors": {"me": true}}"#,
            r#"{"authors": ["a@example.com", 7]}"#,
        ] {
            let error = format!("{:#}", authors(bad).unwrap_err());
            assert!(error.contains("\"authors\""), "{bad}: {error}");
        }
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

    /// Pi wraps the encoded path in `--`, which Claude's decoder does not know about and
    /// would turn into empty leading and trailing path components.
    #[test]
    fn pi_session_path_fallback_strips_the_marker_it_is_wrapped_in() {
        assert_eq!(
            "/tmp/real/project",
            lossy_pi_cwd(Path::new("--tmp-real-project--"))
        );
        // A Windows drive loses its colon to the same substitution as the separators, so
        // the letter is followed by two dashes rather than one.
        assert_eq!(
            "C:/Users/test/project",
            lossy_pi_cwd(Path::new("--C--Users-test-project--"))
        );
        // Decoded on any host, because a copied session directory has to read the same
        // way everywhere.
        assert_eq!(
            "/tmp/real/project",
            lossy_pi_cwd(Path::new("tmp-real-project"))
        );
    }
}
