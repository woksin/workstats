//! The explorer's vocabulary: the drill-down stack, the table schema, the
//! indexed dataset every level is derived from, and saved views.
//!
//! Nothing here reads history. `Dataset::build` is handed a report that has
//! already been computed plus the commits that produced it, so opening the
//! explorer costs a normal run exactly nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::classify::{CategoryTally, active_registry};
use crate::model::{GitCommit, Report};
use crate::paths::{default_config_path, home_dir};
use crate::timeutil::{local_date, local_month};

/// Saved views are user-written configuration, so they are bounded the same way
/// the config file is rather than trusted because they happen to be local.
const MAX_SAVED_VIEWS: usize = 64;
const MAX_VIEW_NAME_BYTES: usize = 64;
/// The five keys it takes to reach a file. A saved view never stores the diff
/// level: restoring one would read file contents nobody asked for.
const MAX_VIEW_DEPTH: usize = 5;
const VIEWS_VERSION: u32 = 1;

/// One level of the drill-down. The order of the variants IS the drill-down
/// order, and `child` is the only place that order is written down.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelKind {
    Overview,
    Repo,
    Period,
    Category,
    Commit,
    File,
    Diff,
}

impl LevelKind {
    pub const fn child(self) -> Option<Self> {
        match self {
            Self::Overview => Some(Self::Repo),
            Self::Repo => Some(Self::Period),
            Self::Period => Some(Self::Category),
            Self::Category => Some(Self::Commit),
            Self::Commit => Some(Self::File),
            Self::File => Some(Self::Diff),
            Self::Diff => None,
        }
    }

    /// What the rows at this level are, for the footer and the empty state.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "repositories",
            Self::Repo => "periods",
            Self::Period => "categories",
            Self::Category => "commits",
            Self::Commit => "files",
            Self::File => "history",
            Self::Diff => "diff",
        }
    }
}

/// One frame of the drill-down stack. `key` is the row identity chosen at the
/// parent level; keeping the selection here is what makes going back up land
/// where you left off.
#[derive(Clone, Debug)]
pub struct Level {
    pub kind: LevelKind,
    pub key: String,
    pub label: String,
    pub selected: usize,
    pub offset: usize,
}

impl Level {
    pub fn new(kind: LevelKind, key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
            label: label.into(),
            selected: 0,
            offset: 0,
        }
    }
}

/// Which calendar bucket the period level splits on.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Grain {
    #[default]
    Month,
    Day,
}

impl Grain {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Day => "day",
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Month => Self::Day,
            Self::Day => Self::Month,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Sort {
    pub column: usize,
    pub descending: bool,
}

/// A column of the current level's table.
pub struct Column {
    pub title: &'static str,
    /// Preferred width in cells; 0 means "take whatever is left".
    pub width: u16,
    /// Right-align when rendering.
    pub numeric: bool,
}

const fn column(title: &'static str, width: u16, numeric: bool) -> Column {
    Column {
        title,
        width,
        numeric,
    }
}

const OVERVIEW_COLUMNS: &[Column] = &[
    column("Repository", 0, false),
    column("Source root", 18, false),
    column("Commits", 9, true),
    column("Files", 8, true),
    column("Added", 10, true),
    column("Removed", 10, true),
    column("Net", 9, true),
    column("AI h", 8, true),
    column("Human h", 9, true),
];

const PERIOD_COLUMNS: &[Column] = &[
    column("Period", 12, false),
    column("Commits", 9, true),
    column("Files", 8, true),
    column("Added", 10, true),
    column("Removed", 10, true),
    column("Net", 9, true),
];

const CATEGORY_COLUMNS: &[Column] = &[
    column("Category", 16, false),
    column("Files", 8, true),
    column("Added", 10, true),
    column("Removed", 10, true),
    column("Net", 9, true),
    column("Share", 8, true),
];

const COMMIT_COLUMNS: &[Column] = &[
    column("When", 17, false),
    column("Commit", 10, false),
    column("Change", 0, false),
    column("Files", 7, true),
    column("Added", 9, true),
    column("Removed", 9, true),
];

const FILE_COLUMNS: &[Column] = &[column("File", 0, false), column("Category", 14, false)];

const HISTORY_COLUMNS: &[Column] = &[
    column("When", 17, false),
    column("Commit", 10, false),
    column("Change", 0, false),
    column("Period", 12, false),
];

pub fn columns(kind: LevelKind) -> &'static [Column] {
    match kind {
        LevelKind::Overview => OVERVIEW_COLUMNS,
        LevelKind::Repo => PERIOD_COLUMNS,
        LevelKind::Period => CATEGORY_COLUMNS,
        LevelKind::Category => COMMIT_COLUMNS,
        LevelKind::Commit => FILE_COLUMNS,
        LevelKind::File => HISTORY_COLUMNS,
        LevelKind::Diff => &[],
    }
}

/// What a level sorts by before the user says otherwise: the biggest first for
/// a measurement, the newest first for anything with a date in it, and plain
/// alphabetical for a list of paths.
pub fn default_sort(kind: LevelKind) -> Sort {
    let (column, descending) = match kind {
        LevelKind::Overview => (2, true),
        LevelKind::Repo => (0, true),
        LevelKind::Period => (2, true),
        LevelKind::Category => (0, true),
        LevelKind::Commit => (0, false),
        LevelKind::File => (0, true),
        LevelKind::Diff => (0, false),
    };
    Sort { column, descending }
}

/// One cell. `value` is the sort key when present, so a formatted number still
/// sorts numerically and a timestamp sorts by the instant, not by its text.
#[derive(Clone, Debug)]
pub struct Field {
    pub text: String,
    pub value: Option<f64>,
}

impl Field {
    pub fn text(value: &str) -> Self {
        Self {
            text: value.to_string(),
            value: None,
        }
    }

    pub fn count(value: u64) -> Self {
        Self {
            text: group_digits(value),
            value: Some(value as f64),
        }
    }

    pub fn lines(value: i64) -> Self {
        let sign = if value < 0 { '-' } else { '+' };
        Self {
            text: format!("{sign}{}", group_digits(value.unsigned_abs())),
            value: Some(value as f64),
        }
    }

    pub fn hours(seconds: f64) -> Self {
        Self {
            text: format!("{:.1}", seconds / 3600.0),
            value: Some(seconds),
        }
    }

    pub fn share(value: f64) -> Self {
        Self {
            text: format!("{:.1}%", value * 100.0),
            value: Some(value),
        }
    }

    pub fn moment(timestamp: DateTime<Utc>) -> Self {
        Self {
            text: timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            value: Some(timestamp.timestamp() as f64),
        }
    }
}

/// One row of the current level, already filtered and sorted.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Identity used to descend into this row, and what a saved view stores.
    pub id: String,
    pub fields: Vec<Field>,
}

impl Entry {
    /// Everything the live filter matches against.
    pub fn haystack(&self) -> String {
        self.fields
            .iter()
            .map(|field| field.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What the next keystroke means.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    Normal,
    Filter,
    Search,
    SaveView,
    Views,
}

/// The `?` overlay, and the only place the key map is described to a user.
pub const KEYBINDINGS: &[(&str, &str)] = &[
    ("↑ ↓ / k j", "move the selection"),
    ("PgUp PgDn", "move by a screen"),
    ("Home / End", "first / last row"),
    ("Enter / → / l", "descend into the selected row"),
    ("Esc", "close an overlay, then clear the filter, then go up"),
    ("Backspace / ← / h", "go up one level"),
    ("/", "filter the current level as you type"),
    ("s", "fuzzy search repositories, files and commits"),
    ("1 – 9", "sort by that column; press again to reverse"),
    ("[ / ]", "previous / next sort column"),
    ("o", "reverse the sort order"),
    ("p", "switch the period between month and day"),
    ("w", "save the current view"),
    ("v", "open the saved views"),
    ("d", "delete the highlighted saved view"),
    ("?", "show or hide this help"),
    ("q / Ctrl-C", "quit"),
];

/// A named position in the explorer. Deliberately holds no diff and no measured
/// data: it is a bookmark, not a copy of anything the tool computed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SavedView {
    pub name: String,
    /// The drill-down keys from the overview downwards, one per level entered.
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub grain: Grain,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub filter: String,
}

impl SavedView {
    fn is_valid(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= MAX_VIEW_NAME_BYTES
            && !self.name.chars().any(char::is_control)
            && self.path.len() <= MAX_VIEW_DEPTH
            && !self
                .path
                .iter()
                .any(|key| key.len() > 4096 || key.chars().any(char::is_control))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SavedViews {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub views: Vec<SavedView>,
}

impl SavedViews {
    /// A missing or unreadable file is an empty set of bookmarks, not an error:
    /// a broken views file must never stop the explorer from opening.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let mut views: Self = serde_json::from_slice(&bytes).unwrap_or_default();
        views.views.retain(SavedView::is_valid);
        views.views.truncate(MAX_SAVED_VIEWS);
        views
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let encoded = serde_json::to_vec_pretty(&Self {
            version: VIEWS_VERSION,
            views: self.views.clone(),
        })?;
        // Written through a temporary file in the same directory so an
        // interrupted save cannot leave a half-written bookmark file behind.
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot write to {}", parent.display()))?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.persist(path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    pub fn insert(&mut self, view: SavedView) -> Result<()> {
        if !view.is_valid() {
            bail!("a view name must be 1 to {MAX_VIEW_NAME_BYTES} printable characters");
        }
        if let Some(existing) = self
            .views
            .iter_mut()
            .find(|existing| existing.name == view.name)
        {
            *existing = view;
            return Ok(());
        }
        if self.views.len() >= MAX_SAVED_VIEWS {
            bail!("at most {MAX_SAVED_VIEWS} saved views are supported");
        }
        self.views.push(view);
        Ok(())
    }

    pub fn remove(&mut self, index: usize) -> Option<SavedView> {
        (index < self.views.len()).then(|| self.views.remove(index))
    }
}

pub fn default_views_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_VIEWS") {
        return PathBuf::from(path);
    }
    // Views are configuration the user wrote, not data the tool derived, so
    // they sit beside the config file and survive a cache wipe.
    default_config_path()
        .parent()
        .map(|parent| parent.join("views.json"))
        .unwrap_or_else(|| home_dir().join(".config/workstats/views.json"))
}

/// One changed path of a commit, with the category it classified into.
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: String,
    pub category: usize,
}

#[derive(Clone, Debug)]
pub struct CommitRecord {
    pub sha: String,
    pub short_sha: String,
    pub timestamp: DateTime<Utc>,
    pub day: String,
    pub month: String,
    pub repo_key: String,
    /// A derived description of the change. `git log` is asked for a sha and a
    /// date only, so there is no commit subject to show; see
    /// `HANDOFF-tui-core.md` for the request to carry one.
    pub summary: String,
    pub additions: u64,
    pub deletions: u64,
    pub categories: CategoryTally,
    pub files: Vec<FileRecord>,
}

impl CommitRecord {
    pub fn period(&self, grain: Grain) -> &str {
        match grain {
            Grain::Month => &self.month,
            Grain::Day => &self.day,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepoRecord {
    /// The repository working directory. Unique, and what the diff viewer runs
    /// `git -C` against.
    pub key: String,
    pub label: String,
    pub root: String,
    pub commits: Vec<usize>,
    pub additions: u64,
    pub deletions: u64,
    pub files: usize,
    /// Joined from the report rows, so it stays zero unless the run grouped by
    /// repo. Git output is always exact; AI time is only as attributable as the
    /// grouping the user asked for.
    pub active_seconds: f64,
    pub human_seconds: f64,
}

/// Everything the explorer can show, indexed once at startup.
pub struct Dataset {
    pub summary: Vec<(String, String)>,
    pub categories: Vec<String>,
    pub repos: Vec<RepoRecord>,
    commits: Vec<CommitRecord>,
    by_sha: BTreeMap<String, usize>,
    by_repo: BTreeMap<String, usize>,
}

impl Dataset {
    pub fn build(report: &Report, commits: Vec<GitCommit>) -> Self {
        let mut data = Self::from_commits(commits);
        data.summary = report_summary(report);
        join_report_seconds(report, &mut data.repos);
        data
    }

    /// Split out from `build` so the index can be exercised without standing up
    /// a whole `Report`.
    pub(super) fn from_commits(commits: Vec<GitCommit>) -> Self {
        let registry = active_registry();
        let categories: Vec<String> = registry.names().map(str::to_string).collect();
        let mut records: Vec<CommitRecord> = Vec::with_capacity(commits.len());
        let mut repos: Vec<RepoRecord> = Vec::new();
        let mut by_repo: BTreeMap<String, usize> = BTreeMap::new();
        let mut repo_files: Vec<BTreeSet<String>> = Vec::new();

        for commit in commits {
            let index = records.len();
            let slot = *by_repo.entry(commit.cwd.clone()).or_insert_with(|| {
                repos.push(RepoRecord {
                    key: commit.cwd.clone(),
                    label: commit.repo.clone(),
                    root: commit.root.clone(),
                    commits: Vec::new(),
                    additions: 0,
                    deletions: 0,
                    files: 0,
                    active_seconds: 0.0,
                    human_seconds: 0.0,
                });
                repo_files.push(BTreeSet::new());
                repos.len() - 1
            });
            repos[slot].commits.push(index);
            repos[slot].additions = repos[slot].additions.saturating_add(commit.additions);
            repos[slot].deletions = repos[slot].deletions.saturating_add(commit.deletions);

            let mut files = Vec::with_capacity(commit.files.len());
            for path in &commit.files {
                repo_files[slot].insert(path.clone());
                files.push(FileRecord {
                    path: path.clone(),
                    category: registry.classify(path),
                });
            }
            records.push(CommitRecord {
                short_sha: commit.sha.chars().take(9).collect(),
                summary: describe_change(&files, &categories),
                sha: commit.sha,
                timestamp: commit.timestamp,
                day: local_date(commit.timestamp),
                month: local_month(commit.timestamp),
                repo_key: commit.cwd,
                additions: commit.additions,
                deletions: commit.deletions,
                categories: commit.categories,
                files,
            });
        }

        for (repo, paths) in repos.iter_mut().zip(&repo_files) {
            repo.files = paths.len();
        }
        let by_sha = records
            .iter()
            .enumerate()
            .map(|(index, record)| (record.sha.clone(), index))
            .collect();
        Self {
            summary: Vec::new(),
            categories,
            repos,
            commits: records,
            by_sha,
            by_repo,
        }
    }

    pub fn commits(&self) -> &[CommitRecord] {
        &self.commits
    }

    pub fn commit(&self, sha: &str) -> Option<&CommitRecord> {
        self.by_sha
            .get(sha)
            .and_then(|index| self.commits.get(*index))
    }

    pub fn repo(&self, key: &str) -> Option<&RepoRecord> {
        self.by_repo
            .get(key)
            .and_then(|index| self.repos.get(*index))
    }

    pub fn category_name(&self, index: usize) -> &str {
        self.categories.get(index).map_or("other", String::as_str)
    }

    /// The commits in the current scope. An empty `repo` or `period` means "not
    /// narrowed by that", which is what the overview and the file history want.
    pub fn scope(&self, repo: &str, period: &str, grain: Grain) -> Vec<usize> {
        let indexes: Vec<usize> = match self.repo(repo) {
            Some(record) => record.commits.clone(),
            None if repo.is_empty() => (0..self.commits.len()).collect(),
            None => Vec::new(),
        };
        if period.is_empty() {
            return indexes;
        }
        indexes
            .into_iter()
            .filter(|index| self.commits[*index].period(grain) == period)
            .collect()
    }

    /// The rows of whatever level the stack currently ends on.
    pub fn rows(&self, stack: &[Level], grain: Grain) -> Vec<Entry> {
        let kind = stack.last().map_or(LevelKind::Overview, |level| level.kind);
        let repo = key_of(stack, LevelKind::Repo);
        let period = key_of(stack, LevelKind::Period);
        match kind {
            LevelKind::Overview => self.overview_rows(),
            LevelKind::Repo => self.period_rows(repo, grain),
            LevelKind::Period => self.category_rows(repo, period, grain),
            LevelKind::Category => {
                self.commit_rows(repo, period, key_of(stack, LevelKind::Category), grain)
            }
            LevelKind::Commit => self.file_rows(key_of(stack, LevelKind::Commit)),
            LevelKind::File => self.history_rows(repo, key_of(stack, LevelKind::File), grain),
            LevelKind::Diff => Vec::new(),
        }
    }

    fn overview_rows(&self) -> Vec<Entry> {
        self.repos
            .iter()
            .map(|repo| Entry {
                id: repo.key.clone(),
                fields: vec![
                    Field::text(&repo.label),
                    Field::text(&repo.root),
                    Field::count(repo.commits.len() as u64),
                    Field::count(repo.files as u64),
                    Field::count(repo.additions),
                    Field::count(repo.deletions),
                    Field::lines(net(repo.additions, repo.deletions)),
                    Field::hours(repo.active_seconds),
                    Field::hours(repo.human_seconds),
                ],
            })
            .collect()
    }

    fn period_rows(&self, repo: &str, grain: Grain) -> Vec<Entry> {
        let mut buckets: BTreeMap<&str, Bucket> = BTreeMap::new();
        for index in self.scope(repo, "", grain) {
            let commit = &self.commits[index];
            buckets.entry(commit.period(grain)).or_default().add(commit);
        }
        buckets
            .into_iter()
            .map(|(period, bucket)| Entry {
                id: period.to_string(),
                fields: vec![
                    Field::text(period),
                    Field::count(bucket.commits),
                    Field::count(bucket.files.len() as u64),
                    Field::count(bucket.additions),
                    Field::count(bucket.deletions),
                    Field::lines(net(bucket.additions, bucket.deletions)),
                ],
            })
            .collect()
    }

    fn category_rows(&self, repo: &str, period: &str, grain: Grain) -> Vec<Entry> {
        let mut tally = CategoryTally::default();
        let mut files: Vec<BTreeSet<&str>> = vec![BTreeSet::new(); self.categories.len()];
        for index in self.scope(repo, period, grain) {
            let commit = &self.commits[index];
            tally.merge(&commit.categories);
            for file in &commit.files {
                if let Some(slot) = files.get_mut(file.category) {
                    slot.insert(file.path.as_str());
                }
            }
        }
        // A share needs a denominator even when nothing changed; the rows are
        // filtered to non-empty categories anyway, so the guard never shows.
        let total = tally.touched().max(1) as f64;
        self.categories
            .iter()
            .enumerate()
            .filter(|(index, _)| tally.get(*index).touched() > 0)
            .map(|(index, name)| {
                let lines = tally.get(index);
                Entry {
                    id: name.clone(),
                    fields: vec![
                        Field::text(name),
                        Field::count(files[index].len() as u64),
                        Field::count(lines.additions),
                        Field::count(lines.deletions),
                        Field::lines(net(lines.additions, lines.deletions)),
                        Field::share(lines.touched() as f64 / total),
                    ],
                }
            })
            .collect()
    }

    fn commit_rows(&self, repo: &str, period: &str, category: &str, grain: Grain) -> Vec<Entry> {
        let wanted = self.categories.iter().position(|name| name == category);
        self.scope(repo, period, grain)
            .into_iter()
            .map(|index| &self.commits[index])
            .filter(|commit| wanted.is_none_or(|slot| commit.categories.get(slot).touched() > 0))
            .map(|commit| Entry {
                id: commit.sha.clone(),
                fields: vec![
                    Field::moment(commit.timestamp),
                    Field::text(&commit.short_sha),
                    Field::text(&commit.summary),
                    Field::count(commit.files.len() as u64),
                    Field::count(commit.additions),
                    Field::count(commit.deletions),
                ],
            })
            .collect()
    }

    /// Every path the commit touched, not only the category that was drilled
    /// through: a commit is one atomic change and hiding half of it misleads.
    fn file_rows(&self, sha: &str) -> Vec<Entry> {
        let Some(commit) = self.commit(sha) else {
            return Vec::new();
        };
        commit
            .files
            .iter()
            .map(|file| Entry {
                id: file.path.clone(),
                fields: vec![
                    Field::text(&file.path),
                    Field::text(self.category_name(file.category)),
                ],
            })
            .collect()
    }

    /// The file's whole history in this repository rather than only the period
    /// that was drilled through: "when else did this change?" is the question a
    /// reader has once they are looking at a single file.
    fn history_rows(&self, repo: &str, path: &str, grain: Grain) -> Vec<Entry> {
        self.scope(repo, "", grain)
            .into_iter()
            .map(|index| &self.commits[index])
            .filter(|commit| commit.files.iter().any(|file| file.path == path))
            .map(|commit| Entry {
                id: commit.sha.clone(),
                fields: vec![
                    Field::moment(commit.timestamp),
                    Field::text(&commit.short_sha),
                    Field::text(&commit.summary),
                    Field::text(commit.period(grain)),
                ],
            })
            .collect()
    }
}

/// A period's running totals while its row is being built.
#[derive(Default)]
struct Bucket {
    commits: u64,
    files: BTreeSet<String>,
    additions: u64,
    deletions: u64,
}

impl Bucket {
    fn add(&mut self, commit: &CommitRecord) {
        self.commits += 1;
        self.additions = self.additions.saturating_add(commit.additions);
        self.deletions = self.deletions.saturating_add(commit.deletions);
        for file in &commit.files {
            self.files.insert(file.path.clone());
        }
    }
}

/// The key chosen at the given level, or `""` when the stack has not reached it.
pub fn key_of(stack: &[Level], kind: LevelKind) -> &str {
    stack
        .iter()
        .find(|level| level.kind == kind)
        .map_or("", |level| level.key.as_str())
}

fn net(additions: u64, deletions: u64) -> i64 {
    additions as i64 - deletions as i64
}

/// The categories that carry the change. Without a commit subject this is the
/// closest honest description the numstat output can give.
fn describe_change(files: &[FileRecord], categories: &[String]) -> String {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for file in files {
        *counts.entry(file.category).or_default() += 1;
    }
    let mut ranked: Vec<(usize, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    let names: Vec<&str> = ranked
        .iter()
        .take(3)
        .map(|(index, _)| categories.get(*index).map_or("other", String::as_str))
        .collect();
    match files.len() {
        0 => "no counted files".to_string(),
        1 => format!("1 file · {}", names.join(", ")),
        count => format!("{count} files · {}", names.join(", ")),
    }
}

/// Report rows are keyed by whatever `--group-by` the run asked for, so AI and
/// human seconds can only be attributed to a repository when `repo` was one of
/// the dimensions. When it was not, those columns stay at zero rather than
/// inventing a split.
fn join_report_seconds(report: &Report, repos: &mut [RepoRecord]) {
    if !report.group_by.iter().any(|name| name == "repo") {
        return;
    }
    let mut seconds: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for row in &report.rows {
        let Some(repo) = row.key.get("repo") else {
            continue;
        };
        let entry = seconds.entry(repo.as_str()).or_default();
        entry.0 += row.active_seconds;
        entry.1 += row.human_estimated_seconds;
    }
    for repo in repos {
        if let Some((active, human)) = seconds.get(repo.label.as_str()) {
            repo.active_seconds = *active;
            repo.human_seconds = *human;
        }
    }
}

fn report_summary(report: &Report) -> Vec<(String, String)> {
    let summary = &report.summary;
    vec![
        ("Observed".to_string(), observed(report)),
        (
            "Commits".to_string(),
            group_digits(summary.commit_count as u64),
        ),
        (
            "Sessions".to_string(),
            group_digits(summary.session_count as u64),
        ),
        (
            "Lines".to_string(),
            format!(
                "+{} / -{}",
                group_digits(summary.additions),
                group_digits(summary.deletions)
            ),
        ),
        (
            "Human".to_string(),
            format!("{:.1} h", summary.human_estimated_seconds / 3600.0),
        ),
        (
            "AI".to_string(),
            format!("{:.1} h", summary.attributed_active_seconds / 3600.0),
        ),
        ("Tokens".to_string(), group_digits(summary.total_tokens)),
    ]
}

fn observed(report: &Report) -> String {
    match (&report.observed.first_seen, &report.observed.last_seen) {
        (Some(first), Some(last)) => format!("{} → {}", date_part(first), date_part(last)),
        _ => "nothing".to_string(),
    }
}

/// RFC 3339 is ASCII, so the calendar date is the first ten bytes.
fn date_part(value: &str) -> &str {
    &value[..value.len().min(10)]
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    grouped
}

/// Shared by the tests in this module and in `app`, so both exercise the same
/// two-commit repository rather than each inventing one.
#[cfg(test)]
pub(super) fn sample_commit(sha: &str, cwd: &str, files: &[(&str, u64, u64)]) -> GitCommit {
    use chrono::TimeZone;

    use crate::model::Authorship;

    let registry = active_registry();
    let mut categories = CategoryTally::default();
    let mut additions = 0;
    let mut deletions = 0;
    for (path, added, removed) in files {
        categories.add(registry.classify(path), *added, *removed);
        additions += *added;
        deletions += *removed;
    }
    GitCommit {
        sha: sha.to_string(),
        timestamp: Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap(),
        repo: cwd.rsplit('/').next().unwrap_or(cwd).to_string(),
        cwd: cwd.to_string(),
        root: "studio".to_string(),
        additions,
        deletions,
        files: files
            .iter()
            .map(|(path, _, _)| (*path).to_string())
            .collect(),
        ignored_additions: 0,
        ignored_deletions: 0,
        categories,
        // The explorer is handed the commits the *report* was built from, and
        // `main` never passes it the agent-authorship pass — so every commit
        // that reaches a `Dataset` is one the configured author wrote.
        authorship: Authorship::default(),
    }
}

#[cfg(test)]
pub(super) fn sample_dataset() -> Dataset {
    Dataset::from_commits(vec![
        sample_commit("aaaaaaaaaaaa", "/repos/widget", &[("src/lib.rs", 10, 2)]),
        sample_commit(
            "bbbbbbbbbbbb",
            "/repos/widget",
            &[("src/lib.rs", 1, 1), ("tests/lib.rs", 20, 0)],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_knows_only_what_comes_after_it() {
        assert_eq!(Some(LevelKind::Repo), LevelKind::Overview.child());
        assert_eq!(Some(LevelKind::Diff), LevelKind::File.child());
        assert_eq!(None, LevelKind::Diff.child());
    }

    #[test]
    fn every_row_carries_exactly_one_field_per_column() {
        let data = sample_dataset();
        let mut stack = vec![Level::new(LevelKind::Overview, "", "workstats")];
        for kind in [
            LevelKind::Overview,
            LevelKind::Repo,
            LevelKind::Period,
            LevelKind::Category,
            LevelKind::Commit,
            LevelKind::File,
        ] {
            let rows = data.rows(&stack, Grain::Month);
            assert!(!rows.is_empty(), "{kind:?} produced no rows");
            for row in &rows {
                assert_eq!(columns(kind).len(), row.fields.len(), "{kind:?}");
            }
            let child = kind.child().expect("every tested level has a child");
            stack.push(Level::new(child, rows[0].id.clone(), rows[0].id.clone()));
        }
    }

    #[test]
    fn drilling_narrows_to_the_chosen_repository_and_period() {
        let data = sample_dataset();
        let stack = vec![
            Level::new(LevelKind::Overview, "", "workstats"),
            Level::new(LevelKind::Repo, "/repos/widget", "widget"),
            Level::new(LevelKind::Period, "2026-06", "2026-06"),
        ];
        let categories: Vec<String> = data
            .rows(&stack, Grain::Month)
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(categories.contains(&"source".to_string()), "{categories:?}");
        assert!(categories.contains(&"test".to_string()), "{categories:?}");
        // The same key is not a day, so the day grain narrows it away entirely.
        assert!(data.rows(&stack, Grain::Day).is_empty());
    }

    #[test]
    fn a_file_level_shows_every_commit_that_touched_that_path() {
        let data = sample_dataset();
        let stack = vec![
            Level::new(LevelKind::Overview, "", "workstats"),
            Level::new(LevelKind::Repo, "/repos/widget", "widget"),
            Level::new(LevelKind::Period, "2026-06", "2026-06"),
            Level::new(LevelKind::Category, "source", "source"),
            Level::new(LevelKind::Commit, "aaaaaaaaaaaa", "aaaaaaaaa"),
            Level::new(LevelKind::File, "src/lib.rs", "src/lib.rs"),
        ];
        assert_eq!(2, data.rows(&stack, Grain::Month).len());
    }

    #[test]
    fn a_saved_view_is_bounded_and_survives_a_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("views.json");
        let mut views = SavedViews::default();
        views
            .insert(SavedView {
                name: "widget".to_string(),
                path: vec!["/repos/widget".to_string(), "2026-06".to_string()],
                grain: Grain::Day,
                sort: Sort {
                    column: 3,
                    descending: true,
                },
                filter: "src".to_string(),
            })
            .unwrap();
        // An unnamed bookmark is unreachable, so it is refused rather than saved.
        assert!(views.insert(SavedView::default()).is_err());
        views.save(&path).unwrap();

        let loaded = SavedViews::load(&path);
        assert_eq!(VIEWS_VERSION, loaded.version);
        assert_eq!(1, loaded.views.len());
        assert_eq!(Grain::Day, loaded.views[0].grain);
        assert_eq!("src", loaded.views[0].filter);
        assert_eq!(2, loaded.views[0].path.len());
    }

    #[test]
    fn an_unusable_views_file_is_an_empty_bookmark_list() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("views.json");
        fs::write(&path, b"{not json").unwrap();
        assert!(SavedViews::load(&path).views.is_empty());
        assert!(
            SavedViews::load(&directory.path().join("missing.json"))
                .views
                .is_empty()
        );
    }

    #[test]
    fn numbers_are_grouped_and_signed_where_it_helps() {
        assert_eq!("1 234 567", group_digits(1_234_567));
        assert_eq!("0", group_digits(0));
        assert_eq!("-42", Field::lines(-42).text);
        assert_eq!("+42", Field::lines(42).text);
        assert_eq!(Some(-42.0), Field::lines(-42).value);
        assert_eq!("2026-06-15", date_part("2026-06-15T12:00:00+00:00"));
    }
}
