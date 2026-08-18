//! Navigation, sorting, filtering and saved views.
//!
//! `App` owns everything the explorer knows and is the only thing the renderer
//! reads. It holds no borrow of the report it was built from, so the drawing
//! code needs no lifetime of its own.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::PathBuf;

use super::diff::{DiffRequest, DiffView};
use super::event::Action;
use super::search::{Index, Target};
use super::state::{
    Column, Dataset, Entry, Field, Grain, Level, LevelKind, Mode, SavedView, SavedViews, Sort,
    columns, default_sort, default_views_path, key_of,
};
use super::{diff, search};
use crate::model::{GitCommit, Report};

/// The most hits the search overlay will ever show. Anything past this is
/// noise a fuzzy needle cannot usefully rank.
const MAX_SEARCH_HITS: usize = 200;
/// A ceiling on the search index so a decade of history in a monorepo cannot
/// turn opening the explorer into an allocation storm.
const MAX_SEARCH_TARGETS: usize = 200_000;

/// One row of the search overlay.
pub struct SearchRow {
    pub label: String,
    /// `"repo"`, `"commit"` or `"file"`.
    pub kind: &'static str,
    /// Matched character positions in `label`, for highlighting.
    pub indices: Vec<usize>,
    /// Where accepting this hit jumps to.
    path: Vec<String>,
}

pub struct App {
    data: Dataset,
    /// Never empty: the overview is the floor.
    stack: Vec<Level>,
    grain: Grain,
    sort: Sort,
    filter: String,
    mode: Mode,
    input: String,
    status: Option<String>,
    help: bool,
    views: SavedViews,
    views_path: PathBuf,
    views_selected: usize,
    index: Index,
    hits: Vec<SearchRow>,
    search_selected: usize,
    /// Display only. Dropped the moment the diff level is left; never written
    /// anywhere. See the module documentation in `tui/mod.rs`.
    diff: Option<DiffView>,
    diff_offset: usize,
    rows: Vec<Entry>,
    viewport: usize,
    quit: bool,
}

impl App {
    pub fn new(report: &Report, commits: Vec<GitCommit>) -> Self {
        let data = Dataset::build(report, commits);
        let grain = Grain::default();
        let views_path = default_views_path();
        let mut app = Self {
            index: Index::build(search_targets(&data, grain)),
            views: SavedViews::load(&views_path),
            data,
            stack: vec![Level::new(LevelKind::Overview, "", "workstats")],
            grain,
            sort: default_sort(LevelKind::Overview),
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            help: false,
            views_path,
            views_selected: 0,
            hits: Vec::new(),
            search_selected: 0,
            diff: None,
            diff_offset: 0,
            rows: Vec::new(),
            viewport: 10,
            quit: false,
        };
        app.rebuild();
        app
    }

    // ---- what the renderer reads -----------------------------------------

    pub fn level(&self) -> LevelKind {
        self.stack
            .last()
            .map_or(LevelKind::Overview, |level| level.kind)
    }

    pub fn breadcrumb(&self) -> Vec<String> {
        self.stack.iter().map(|level| level.label.clone()).collect()
    }

    pub fn columns(&self) -> &'static [Column] {
        columns(self.level())
    }

    pub fn rows(&self) -> &[Entry] {
        &self.rows
    }

    pub fn selected(&self) -> usize {
        self.stack.last().map_or(0, |level| level.selected)
    }

    pub fn offset(&self) -> usize {
        self.stack.last().map_or(0, |level| level.offset)
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn grain(&self) -> Grain {
        self.grain
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn help_visible(&self) -> bool {
        self.help
    }

    pub fn summary(&self) -> &[(String, String)] {
        &self.data.summary
    }

    pub fn saved_views(&self) -> &[SavedView] {
        &self.views.views
    }

    pub fn views_selected(&self) -> usize {
        self.views_selected
    }

    pub fn search_hits(&self) -> &[SearchRow] {
        &self.hits
    }

    pub fn search_selected(&self) -> usize {
        self.search_selected
    }

    pub fn diff(&self) -> Option<&DiffView> {
        self.diff.as_ref()
    }

    pub fn diff_offset(&self) -> usize {
        self.diff_offset
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The table body height, in rows. The app has no other way to learn it,
    /// and a page key that does not match what the reader sees is worse than
    /// no page key at all.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
    }

    /// Keeps the renderer's scroll position across frames and across a trip
    /// down into a child level and back.
    pub fn set_offset(&mut self, offset: usize) {
        if let Some(level) = self.stack.last_mut() {
            level.offset = offset;
        }
    }

    // ---- what the event loop drives ---------------------------------------

    pub fn apply(&mut self, action: Action) {
        if action != Action::Nothing {
            self.status = None;
        }
        match self.mode {
            Mode::Normal => self.apply_normal(action),
            Mode::Filter => self.apply_filter(action),
            Mode::Search => self.apply_search(action),
            Mode::SaveView => self.apply_save_view(action),
            Mode::Views => self.apply_views(action),
        }
    }

    fn apply_normal(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true,
            Action::Descend => self.descend(),
            Action::Ascend => self.ascend(),
            Action::Cancel => self.cancel(),
            Action::Move(delta) => self.move_by(delta),
            Action::Page(pages) => self.move_by(pages * self.viewport as isize),
            Action::First => self.move_by(isize::MIN / 2),
            Action::Last => self.move_by(isize::MAX / 2),
            Action::SortColumn(column) => self.sort_by(column),
            Action::SortNext => self.shift_sort(1),
            Action::SortPrevious => self.shift_sort(-1),
            Action::ToggleSortOrder => {
                self.sort.descending = !self.sort.descending;
                self.rebuild();
            }
            Action::ToggleGrain => self.toggle_grain(),
            Action::ToggleHelp => self.help = !self.help,
            Action::BeginFilter => self.mode = Mode::Filter,
            Action::BeginSearch => {
                self.mode = Mode::Search;
                self.input.clear();
                self.hits.clear();
                self.search_selected = 0;
            }
            Action::BeginSaveView => {
                self.mode = Mode::SaveView;
                self.input.clear();
            }
            Action::OpenViews => {
                if self.views.views.is_empty() {
                    self.status = Some("no saved views yet — press w to save one".to_string());
                } else {
                    self.mode = Mode::Views;
                    self.views_selected = 0;
                }
            }
            Action::Nothing
            | Action::Input(_)
            | Action::Backspace
            | Action::Accept
            | Action::DeleteView => {}
        }
    }

    fn apply_filter(&mut self, action: Action) {
        match action {
            Action::Input(character) => {
                self.filter.push(character);
                self.rebuild();
            }
            Action::Backspace => {
                self.filter.pop();
                self.rebuild();
            }
            Action::Accept => self.mode = Mode::Normal,
            Action::Cancel => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.rebuild();
            }
            Action::Move(delta) => self.move_by(delta),
            Action::Page(pages) => self.move_by(pages * self.viewport as isize),
            Action::Descend => {
                self.mode = Mode::Normal;
                self.descend();
            }
            Action::Quit => self.quit = true,
            _ => {}
        }
    }

    fn apply_search(&mut self, action: Action) {
        match action {
            Action::Input(character) => {
                self.input.push(character);
                self.run_search();
            }
            Action::Backspace => {
                self.input.pop();
                self.run_search();
            }
            Action::Move(delta) => {
                self.search_selected = step(self.search_selected, delta, self.hits.len());
            }
            Action::Page(pages) => {
                let delta = pages * self.viewport as isize;
                self.search_selected = step(self.search_selected, delta, self.hits.len());
            }
            Action::Accept | Action::Descend => {
                if let Some(path) = self
                    .hits
                    .get(self.search_selected)
                    .map(|hit| hit.path.clone())
                {
                    self.jump_to(&path);
                }
                self.close_overlay();
            }
            Action::Cancel => self.close_overlay(),
            Action::Quit => self.quit = true,
            _ => {}
        }
    }

    fn apply_save_view(&mut self, action: Action) {
        match action {
            Action::Input(character) => self.input.push(character),
            Action::Backspace => {
                self.input.pop();
            }
            Action::Accept => {
                let name = self.input.trim().to_string();
                self.save_current_view(&name);
                self.close_overlay();
            }
            Action::Cancel => self.close_overlay(),
            Action::Quit => self.quit = true,
            _ => {}
        }
    }

    fn apply_views(&mut self, action: Action) {
        match action {
            Action::Move(delta) => {
                self.views_selected = step(self.views_selected, delta, self.views.views.len());
            }
            Action::Accept | Action::Descend => {
                self.open_saved_view(self.views_selected);
                self.close_overlay();
            }
            Action::DeleteView => self.delete_saved_view(self.views_selected),
            Action::Cancel => self.close_overlay(),
            Action::Quit => self.quit = true,
            _ => {}
        }
    }

    // ---- navigation --------------------------------------------------------

    /// Esc peels one layer at a time: an overlay, then the filter, then the
    /// level. Anything else and a reader loses their place by pressing Esc once
    /// too often.
    fn cancel(&mut self) {
        if self.help {
            self.help = false;
        } else if !self.filter.is_empty() {
            self.filter.clear();
            self.rebuild();
        } else {
            self.ascend();
        }
    }

    fn close_overlay(&mut self) {
        self.mode = Mode::Normal;
        self.input.clear();
    }

    fn descend(&mut self) {
        let Some(child) = self.level().child() else {
            return;
        };
        let chosen = self.rows.get(self.selected()).map(|row| {
            let label = row
                .fields
                .first()
                .map_or_else(|| row.id.clone(), |field| field.text.clone());
            (row.id.clone(), label)
        });
        let Some((key, label)) = chosen else {
            self.status = Some(format!("no {} to open", self.level().label()));
            return;
        };
        if child == LevelKind::Diff && !self.open_diff(&key) {
            return;
        }
        self.stack.push(Level::new(child, key, label));
        self.sort = default_sort(child);
        self.filter.clear();
        self.rebuild();
    }

    fn ascend(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }
        self.stack.pop();
        // The diff is display-only, so it stops existing the moment it stops
        // being on screen.
        self.diff = None;
        self.diff_offset = 0;
        self.sort = default_sort(self.level());
        self.filter.clear();
        self.rebuild();
    }

    /// Reads one file's contents through `git show` for display. This is the
    /// only path in the whole tool that does so; the result is never cached,
    /// reported, or written to a saved view.
    fn open_diff(&mut self, sha: &str) -> bool {
        let request = DiffRequest {
            cwd: PathBuf::from(key_of(&self.stack, LevelKind::Repo)),
            sha: sha.to_string(),
            path: key_of(&self.stack, LevelKind::File).to_string(),
        };
        match diff::load(&request) {
            Ok(view) => {
                self.diff = Some(view);
                self.diff_offset = 0;
                true
            }
            Err(error) => {
                self.status = Some(format!("{error:#}"));
                false
            }
        }
    }

    /// Replays a path of row identities from the overview downwards. A key that
    /// no longer exists stops the descent rather than guessing at a neighbour.
    pub fn jump_to(&mut self, path: &[String]) {
        self.stack.truncate(1);
        self.diff = None;
        self.filter.clear();
        self.sort = default_sort(LevelKind::Overview);
        self.rebuild();
        for key in path {
            let Some(child) = self.level().child() else {
                break;
            };
            let Some(position) = self.rows.iter().position(|row| row.id == *key) else {
                self.status = Some(format!("{key} is not in this report any more"));
                break;
            };
            let label = self.rows[position]
                .fields
                .first()
                .map_or_else(|| key.clone(), |field| field.text.clone());
            if let Some(level) = self.stack.last_mut() {
                level.selected = position;
            }
            self.stack.push(Level::new(child, key.clone(), label));
            self.sort = default_sort(child);
            self.rebuild();
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.level() == LevelKind::Diff {
            let length = self.diff.as_ref().map_or(0, |view| view.lines.len());
            self.diff_offset = step(self.diff_offset, delta, length);
            return;
        }
        let length = self.rows.len();
        if let Some(level) = self.stack.last_mut() {
            level.selected = step(level.selected, delta, length);
        }
    }

    // ---- sorting and filtering ---------------------------------------------

    /// A measurement is most interesting from the top, a name from the start,
    /// so the first press of a column key picks the direction that reads best.
    fn sort_by(&mut self, column: usize) {
        let Some(definition) = self.columns().get(column) else {
            return;
        };
        if self.sort.column == column {
            self.sort.descending = !self.sort.descending;
        } else {
            self.sort = Sort {
                column,
                descending: definition.numeric,
            };
        }
        self.rebuild();
    }

    fn shift_sort(&mut self, delta: isize) {
        let width = self.columns().len();
        if width == 0 {
            return;
        }
        let next = (self.sort.column as isize + delta).rem_euclid(width as isize) as usize;
        self.sort_by(next);
    }

    fn toggle_grain(&mut self) {
        self.grain = self.grain.toggled();
        // A period key belongs to one grain, so everything below the repo level
        // would name a bucket that no longer exists.
        self.stack.truncate(self.stack.len().min(2));
        self.diff = None;
        self.index = Index::build(search_targets(&self.data, self.grain));
        self.sort = default_sort(self.level());
        self.rebuild();
        self.status = Some(format!("period: {}", self.grain.label()));
    }

    fn rebuild(&mut self) {
        let mut rows = self.data.rows(&self.stack, self.grain);
        if !self.filter.is_empty() {
            rows.retain(|row| search::score(&self.filter, &row.haystack()).is_some());
        }
        if self.sort.column >= self.columns().len() {
            self.sort.column = 0;
        }
        let sort = self.sort;
        rows.sort_by(|left, right| {
            let ordering = compare(left.fields.get(sort.column), right.fields.get(sort.column));
            if sort.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        if let Some(level) = self.stack.last_mut() {
            level.selected = level.selected.min(last);
            level.offset = level.offset.min(last);
        }
    }

    fn run_search(&mut self) {
        let hits: Vec<SearchRow> = self
            .index
            .query(&self.input, MAX_SEARCH_HITS)
            .into_iter()
            .filter_map(|hit| {
                self.index.target(hit.target).map(|target| SearchRow {
                    label: target.label.clone(),
                    kind: target.kind,
                    indices: hit.indices,
                    path: target.path.clone(),
                })
            })
            .collect();
        self.hits = hits;
        self.search_selected = 0;
    }

    // ---- saved views --------------------------------------------------------

    fn save_current_view(&mut self, name: &str) {
        // A saved view records where the reader was, never the diff they were
        // reading: restoring one must not read a file unasked.
        let path: Vec<String> = self
            .stack
            .iter()
            .skip(1)
            .filter(|level| level.kind != LevelKind::Diff)
            .map(|level| level.key.clone())
            .collect();
        let view = SavedView {
            name: name.to_string(),
            path,
            grain: self.grain,
            sort: self.sort,
            filter: self.filter.clone(),
        };
        if let Err(error) = self.views.insert(view) {
            self.status = Some(format!("{error:#}"));
            return;
        }
        self.status = match self.views.save(&self.views_path) {
            Ok(()) => Some(format!("saved view \"{name}\"")),
            Err(error) => Some(format!("{error:#}")),
        };
    }

    fn open_saved_view(&mut self, index: usize) {
        let Some(view) = self.views.views.get(index).cloned() else {
            return;
        };
        if view.grain != self.grain {
            self.grain = view.grain;
            self.index = Index::build(search_targets(&self.data, self.grain));
        }
        self.jump_to(&view.path);
        self.sort = view.sort;
        self.filter = view.filter;
        self.rebuild();
    }

    fn delete_saved_view(&mut self, index: usize) {
        let Some(view) = self.views.remove(index) else {
            return;
        };
        self.views_selected = self
            .views_selected
            .min(self.views.views.len().saturating_sub(1));
        self.status = match self.views.save(&self.views_path) {
            Ok(()) => Some(format!("deleted view \"{}\"", view.name)),
            Err(error) => Some(format!("{error:#}")),
        };
        if self.views.views.is_empty() {
            self.close_overlay();
        }
    }
}

/// Moves `current` by `delta` and keeps it inside `0..length`, saturating at
/// both ends rather than wrapping — wrapping past the last row reads as a bug.
fn step(current: usize, delta: isize, length: usize) -> usize {
    if length == 0 {
        return 0;
    }
    let last = length as isize - 1;
    (current as isize).saturating_add(delta).clamp(0, last) as usize
}

fn compare(left: Option<&Field>, right: Option<&Field>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match (left.value, right.value) {
            (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
            _ => left.text.to_lowercase().cmp(&right.text.to_lowercase()),
        },
        _ => Ordering::Equal,
    }
}

/// Repositories, commits, and each distinct file, with the drill-down path that
/// reaches them. Files are deduplicated to their most recent commit: the same
/// path appearing once per commit would bury everything else in the results.
fn search_targets(data: &Dataset, grain: Grain) -> Vec<Target> {
    let mut targets = Vec::new();
    for repo in &data.repos {
        targets.push(Target {
            kind: "repo",
            label: format!("{} ({})", repo.label, repo.root),
            path: vec![repo.key.clone()],
        });
    }
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for commit in data.commits().iter().rev() {
        if targets.len() >= MAX_SEARCH_TARGETS {
            break;
        }
        let period = commit.period(grain).to_string();
        let dominant = dominant_category(data, commit);
        targets.push(Target {
            kind: "commit",
            label: format!("{} {}", commit.short_sha, commit.summary),
            path: vec![
                commit.repo_key.clone(),
                period.clone(),
                dominant.to_string(),
                commit.sha.clone(),
            ],
        });
        for file in &commit.files {
            if !seen.insert((commit.repo_key.as_str(), file.path.as_str())) {
                continue;
            }
            targets.push(Target {
                kind: "file",
                label: file.path.clone(),
                path: vec![
                    commit.repo_key.clone(),
                    period.clone(),
                    data.category_name(file.category).to_string(),
                    commit.sha.clone(),
                    file.path.clone(),
                ],
            });
        }
    }
    targets
}

/// The category a commit's own path should descend through, so a search hit
/// lands on a category level that actually contains it.
fn dominant_category<'a>(data: &'a Dataset, commit: &super::state::CommitRecord) -> &'a str {
    let index = (0..data.categories.len())
        .max_by_key(|index| commit.categories.get(*index).touched())
        .unwrap_or(0);
    data.category_name(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::sample_commit;

    /// The directory has to outlive the app, so it is handed back with it. A
    /// test must never write to the real config directory.
    fn app() -> (App, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let data = Dataset::from_commits(vec![
            sample_commit("aaaaaaaaaaaa", "/repos/widget", &[("src/lib.rs", 10, 2)]),
            sample_commit(
                "bbbbbbbbbbbb",
                "/repos/widget",
                &[("src/lib.rs", 1, 1), ("tests/lib.rs", 20, 0)],
            ),
            sample_commit("cccccccccccc", "/repos/gadget", &[("README.md", 5, 0)]),
        ]);
        let mut app = App {
            index: Index::build(search_targets(&data, Grain::Month)),
            data,
            stack: vec![Level::new(LevelKind::Overview, "", "workstats")],
            grain: Grain::Month,
            sort: default_sort(LevelKind::Overview),
            filter: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            help: false,
            views: SavedViews::default(),
            views_path: directory.path().join("views.json"),
            views_selected: 0,
            hits: Vec::new(),
            search_selected: 0,
            diff: None,
            diff_offset: 0,
            rows: Vec::new(),
            viewport: 10,
            quit: false,
        };
        app.rebuild();
        (app, directory)
    }

    #[test]
    fn enter_descends_and_escape_climbs_back_to_the_same_row() {
        let (mut app, _directory) = app();
        assert_eq!(LevelKind::Overview, app.level());
        // Two repositories, busiest first.
        assert_eq!(2, app.rows().len());

        app.apply(Action::Descend);
        assert_eq!(LevelKind::Repo, app.level());
        assert_eq!(vec!["workstats", "widget"], app.breadcrumb());
        app.apply(Action::Ascend);

        app.apply(Action::Move(1));
        let chosen = app.rows()[app.selected()].id.clone();
        app.apply(Action::Descend);
        app.apply(Action::Ascend);
        assert_eq!(LevelKind::Overview, app.level());
        assert_eq!(chosen, app.rows()[app.selected()].id);
        // The overview is the floor; climbing past it is a no-op, not a panic.
        app.apply(Action::Ascend);
        assert_eq!(LevelKind::Overview, app.level());
    }

    #[test]
    fn the_selection_never_leaves_the_rows() {
        let (mut app, _directory) = app();
        app.apply(Action::Last);
        assert_eq!(app.rows().len() - 1, app.selected());
        app.apply(Action::Move(50));
        assert_eq!(app.rows().len() - 1, app.selected());
        app.apply(Action::First);
        assert_eq!(0, app.selected());
        app.apply(Action::Move(-50));
        assert_eq!(0, app.selected());
    }

    #[test]
    fn a_column_key_sorts_and_pressing_it_again_reverses() {
        let (mut app, _directory) = app();
        app.apply(Action::SortColumn(0));
        assert_eq!(
            Sort {
                column: 0,
                descending: false
            },
            app.sort()
        );
        let ascending: Vec<String> = app.rows().iter().map(|row| row.id.clone()).collect();
        app.apply(Action::SortColumn(0));
        assert!(app.sort().descending);
        let descending: Vec<String> = app.rows().iter().map(|row| row.id.clone()).collect();
        assert_eq!(
            ascending.iter().rev().collect::<Vec<_>>(),
            descending.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_column_index_past_the_level_is_ignored_rather_than_panicking() {
        let (mut app, _directory) = app();
        app.apply(Action::SortColumn(40));
        assert_eq!(default_sort(LevelKind::Overview), app.sort());
    }

    #[test]
    fn the_filter_narrows_the_level_and_escape_restores_it() {
        let (mut app, _directory) = app();
        let all = app.rows().len();
        app.apply(Action::BeginFilter);
        assert_eq!(Mode::Filter, app.mode());
        for character in "gadget".chars() {
            app.apply(Action::Input(character));
        }
        assert_eq!(1, app.rows().len());
        app.apply(Action::Cancel);
        assert_eq!(Mode::Normal, app.mode());
        assert_eq!(all, app.rows().len());
    }

    #[test]
    fn switching_the_period_grain_drops_the_levels_it_invalidates() {
        let (mut app, _directory) = app();
        app.apply(Action::Descend);
        app.apply(Action::Descend);
        assert_eq!(LevelKind::Period, app.level());
        app.apply(Action::ToggleGrain);
        assert_eq!(Grain::Day, app.grain());
        assert_eq!(LevelKind::Repo, app.level());
        assert!(!app.rows().is_empty());
    }

    #[test]
    fn a_saved_view_round_trips_through_the_config_directory() {
        let (mut app, _directory) = app();
        app.apply(Action::Descend);
        app.apply(Action::Descend);
        let before = app.breadcrumb();

        app.apply(Action::BeginSaveView);
        for character in "widget".chars() {
            app.apply(Action::Input(character));
        }
        app.apply(Action::Accept);
        assert_eq!(1, app.saved_views().len());

        app.apply(Action::Ascend);
        app.apply(Action::Ascend);
        assert_eq!(LevelKind::Overview, app.level());

        app.apply(Action::OpenViews);
        assert_eq!(Mode::Views, app.mode());
        app.apply(Action::Accept);
        assert_eq!(before, app.breadcrumb());
        assert_eq!(Mode::Normal, app.mode());
    }

    #[test]
    fn help_and_escape_peel_one_layer_at_a_time() {
        let (mut app, _directory) = app();
        app.apply(Action::Descend);
        app.apply(Action::ToggleHelp);
        assert!(app.help_visible());
        app.apply(Action::Cancel);
        assert!(!app.help_visible());
        assert_eq!(LevelKind::Repo, app.level());
        app.apply(Action::Cancel);
        assert_eq!(LevelKind::Overview, app.level());
    }
}
