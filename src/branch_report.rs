//! `workstats branch` and `workstats pr`: the effort behind one branch or pull
//! request, computed from one collected run.
//!
//! Nothing here scans anything of its own. The run is `report::collect` over a
//! window chosen from the branch's history, and every figure is a sum of the
//! pieces that run produced, filtered to the branch and the repository:
//!
//! * **Human time** is the sum of the `human_intervals` pieces labelled with the
//!   branch. Those pieces already partition the human timeline, so work done at
//!   the same moment on another branch is not in them, and the figure is never
//!   the window's total.
//! * **Agent wall time and parallel agent time** come from the `ai_intervals`
//!   with the same filter: the union of them, and their plain sum.
//! * **Tokens, models and list value** come from each session's token events,
//!   placed on a branch by `Session::branch_at`.
//! * **Commits** are the ones `git log --no-merges BASE..BRANCH` names, matched
//!   by SHA against the commits the run retained (the user's own) and the agent
//!   commits it found.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, SecondsFormat, TimeZone, Utc};
use clap::Args;
use serde::Serialize;

use crate::branches;
use crate::cli::{OutputFormat, ReportArguments, report_window, scan_directory};
use crate::describe;
use crate::document::{
    self, Block, Column, Document, Paragraphs, Table, TextStyle, escape_markdown, render_html,
    render_markdown,
};
use crate::git::git_executable;
use crate::model::{Diagnostics, GitCommit, Interval, Session, TokenUsage};
use crate::output::{compact_tokens, hours, number, safe_text};
use crate::paths::{PathResolver, home_dir, load_config};
use crate::pricing::{self, RATES_AS_OF, RateSource};
use crate::report::{Collected, Purpose, collect};
use crate::timeutil::{clip_interval, union_seconds};

const NOTE: &str = "Human time is an estimate from prompts, session edges and commits, not a stopwatch; agent figures come from local histories. List value is what the tokens would cost at published list prices, not what was billed.";

#[derive(Debug, Args)]
pub(crate) struct BranchArguments {
    #[arg(
        value_name = "NAME",
        help = "Branch to report (default: the current branch)"
    )]
    pub(crate) name: Option<String>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "REF",
        help = "Branch it was cut from (default: the integration branch)"
    )]
    pub(crate) base: Option<String>,
    #[arg(long, help = "One row per local branch")]
    pub(crate) all: bool,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "SOURCES",
        help = "Add descriptions from commits and/or sessions[=PROVIDERS]; read only when asked"
    )]
    pub(crate) describe: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct PrArguments {
    #[arg(
        value_name = "NAME",
        help = "Branch to report (default: the current branch)"
    )]
    pub(crate) name: Option<String>,
    #[arg(
        long,
        value_name = "N",
        help = "Pull request number, resolved through the sessions that mentioned it"
    )]
    pub(crate) number: Option<u64>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "REF",
        help = "Branch it was cut from (default: the integration branch)"
    )]
    pub(crate) base: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "SOURCES",
        help = "Add descriptions from commits and/or sessions[=PROVIDERS]; read only when asked"
    )]
    pub(crate) describe: Vec<String>,
}

pub(crate) fn run_branch(arguments: BranchArguments) -> Result<()> {
    run(Request {
        command: Kind::Branch,
        scope: if arguments.all {
            if arguments.name.is_some() {
                bail!("--all reports every local branch; drop the branch name");
            }
            Scope::All
        } else {
            Scope::Named(arguments.name, None)
        },
        base: arguments.base,
        describe: arguments.describe,
        report: arguments.report,
    })
}

pub(crate) fn run_pr(arguments: PrArguments) -> Result<()> {
    let scope = match (arguments.name, arguments.number) {
        (None, Some(number)) => Scope::Number(number),
        (name, number) => Scope::Named(name, number),
    };
    run(Request {
        command: Kind::Pr,
        scope,
        base: arguments.base,
        describe: arguments.describe,
        report: arguments.report,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Branch,
    Pr,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Pr => "pr",
        }
    }

    /// A PR description is pasted somewhere that renders Markdown, so that is
    /// what `pr` prints unless told otherwise.
    fn default_format(self) -> OutputFormat {
        match self {
            Self::Branch => OutputFormat::Table,
            Self::Pr => OutputFormat::Markdown,
        }
    }
}

enum Scope {
    /// The named branch, or the current one; with the pull request number the
    /// user said it belongs to, if any.
    Named(Option<String>, Option<u64>),
    All,
    /// Whatever branches the sessions that mentioned this pull request were on.
    Number(u64),
}

struct Request {
    command: Kind,
    scope: Scope,
    base: Option<String>,
    describe: Vec<String>,
    report: ReportArguments,
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

/// The few read-only `git` calls a branch report needs, run in the
/// repository's top level so a worktree and its main checkout agree.
struct Git {
    exe: PathBuf,
    /// Where the user pointed, which may be a worktree; it owns the `HEAD` that
    /// names the current branch.
    directory: PathBuf,
    top: PathBuf,
}

impl Git {
    fn open(directory: &Path) -> Result<Self> {
        let exe = git_executable()
            .context("git was not found; `workstats branch` and `workstats pr` read the repository's branches")?;
        let output = Command::new(&exe)
            .arg("-C")
            .arg(directory)
            .args(["rev-parse", "--show-toplevel"])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("could not run git in {}", directory.display()))?;
        if !output.status.success() {
            bail!("{} is not inside a Git repository", directory.display());
        }
        let top = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        Ok(Self {
            exe,
            directory: directory.to_path_buf(),
            top,
        })
    }

    fn text_in(&self, directory: &Path, arguments: &[&str]) -> Result<String> {
        let output = Command::new(&self.exe)
            .arg("--no-pager")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .context("could not run git")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                arguments.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn text(&self, arguments: &[&str]) -> Result<String> {
        self.text_in(&self.top, arguments)
    }

    /// The commit a revision names, or `None` when it names nothing.
    fn commit_of(&self, revision: &str) -> Option<String> {
        if revision.starts_with('-') {
            return None;
        }
        let spelled = format!("{revision}^{{commit}}");
        self.text(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spelled,
        ])
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    }

    fn local_branches(&self) -> Result<Vec<String>> {
        let output = self.text(&["for-each-ref", "--format=%(refname)", "refs/heads"])?;
        Ok(output
            .lines()
            .filter_map(|line| line.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect())
    }

    /// The branch `HEAD` is on in the directory the user pointed at.
    fn current_branch(&self) -> Result<String> {
        match self.text_in(&self.directory, &["symbolic-ref", "--short", "HEAD"]) {
            Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
            _ => bail!("HEAD is not on a branch (detached); pass the branch name"),
        }
    }

    fn exists(&self, branch: &str) -> bool {
        self.commit_of(&format!("refs/heads/{branch}")).is_some()
    }
}

/// The branch a report is measured against.
struct Base {
    label: String,
    commit: String,
}

/// What Git says about one branch's history relative to its base.
#[derive(Default)]
struct BranchHistory {
    /// The author time of the commit the branch was cut from.
    fork_point: Option<DateTime<Utc>>,
    /// `BASE..BRANCH`, without merges, as (sha, author time).
    commits: Vec<(String, DateTime<Utc>)>,
    /// When the branch ref was created, if its reflog still says.
    created: Option<DateTime<Utc>>,
}

impl BranchHistory {
    /// The earliest moment this history can account for.
    fn earliest(&self) -> Option<(DateTime<Utc>, &'static str)> {
        let candidates = [
            self.fork_point.map(|at| (at, "fork point")),
            self.commits
                .iter()
                .map(|(_, at)| *at)
                .min()
                .map(|at| (at, "first commit")),
            self.created.map(|at| (at, "branch creation")),
        ];
        candidates.into_iter().flatten().min_by_key(|(at, _)| *at)
    }
}

fn epoch(text: &str) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(text.trim().parse().ok()?, 0).single()
}

fn branch_history(
    git: &Git,
    base: Option<&Base>,
    branch: &str,
    warnings: &mut Vec<String>,
) -> BranchHistory {
    let reference = format!("refs/heads/{branch}");
    let mut history = BranchHistory::default();
    // The reflog goes back only as far as Git kept it; when it is gone, the
    // fork point and the commits still bound the window.
    if let Ok(text) = git.text(&["reflog", "show", "--date=unix", "--format=%gd", &reference]) {
        history.created = text
            .lines()
            .rev()
            .find_map(|line| line.rsplit_once("@{")?.1.strip_suffix('}').and_then(epoch));
    }
    let Some(base) = base else {
        return history;
    };
    if let Ok(text) = git.text(&["merge-base", &base.commit, &reference])
        && let Some(commit) = text.split_whitespace().next()
        && let Ok(at) = git.text(&["log", "-1", "--format=%at", commit])
    {
        history.fork_point = epoch(&at);
    }
    let range = format!("{}..{reference}", base.commit);
    match git.text(&["log", "--no-merges", "--format=%H%x09%at", &range]) {
        Ok(text) => {
            history.commits = text
                .lines()
                .filter_map(|line| {
                    let (sha, at) = line.split_once('\t')?;
                    Some((sha.to_string(), epoch(at)?))
                })
                .collect();
        }
        Err(error) => warnings.push(format!("commits of the branch were not listed: {error:#}")),
    }
    history
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// One branch and the window its figures are summed over.
struct Target {
    name: String,
    history: BranchHistory,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    since_source: &'static str,
}

fn run(request: Request) -> Result<()> {
    let Request {
        command,
        scope,
        base: base_argument,
        describe,
        report: mut arguments,
    } = request;
    if arguments.no_git {
        bail!(
            "--no-git cannot be used with `workstats {}`; it reads the repository's branches and commits",
            command.name()
        );
    }
    if arguments.compare.is_some() {
        bail!(
            "--compare is not available with `workstats {}`; it reports one branch or pull request",
            command.name()
        );
    }
    let flag_format = arguments.output_format;
    // Loaded here only for the directory and the default format; the run
    // itself loads it again and reports anything wrong with it.
    let config = load_config(arguments.config.as_deref(), &mut Diagnostics::default());
    let defaults = config.config_defaults(&home_dir())?;
    let format = flag_format
        .or(defaults.format)
        .unwrap_or_else(|| command.default_format());
    if format == OutputFormat::Csv {
        bail!(
            "--format csv is not available for `workstats {}`; use table, json, markdown, or html",
            command.name()
        );
    }
    let directory = scan_directory(
        arguments.directory.as_deref(),
        env::var_os("WORKSTATS_DIR").map(PathBuf::from),
        defaults.dir.clone(),
        env::current_dir().ok(),
    )?;
    let git = Git::open(&directory)?;
    let mut warnings: Vec<String> = Vec::new();

    // The base: --base, else the integration branch by the same rule the
    // attribution uses.
    let mut git_notes = Diagnostics::default();
    let base = match base_argument.as_deref() {
        Some(reference) => {
            let commit = git
                .commit_of(reference)
                .with_context(|| format!("--base {reference:?} does not name a commit"))?;
            Some(Base {
                label: reference.to_string(),
                commit,
            })
        }
        None => branches::integration_branch(&git.top, config.branches.as_ref(), &mut git_notes)
            .and_then(|name| {
                git.commit_of(&format!("refs/heads/{name}"))
                    .map(|commit| Base {
                        label: name,
                        commit,
                    })
            }),
    };
    warnings.extend(git_notes.messages);

    // The window the user fixed, if any. `--month`, `--week` and `--year` fix
    // both ends; `--since` and `--until` each fix one.
    let (user_since, user_until) = report_window(&arguments, Utc::now())?;

    let mut names: Vec<String> = match &scope {
        Scope::Named(name, _) => {
            let name = match name {
                Some(name) => name.clone(),
                None => git.current_branch()?,
            };
            if !git.exists(&name) {
                bail!("there is no local branch named {name:?}");
            }
            vec![name]
        }
        Scope::All => git.local_branches()?,
        Scope::Number(_) => Vec::new(),
    };
    // A branch is not measured against itself.
    let base_branch = |name: &str| base.as_ref().is_some_and(|base| base.label == name);
    let mut histories: BTreeMap<String, BranchHistory> = BTreeMap::new();
    for name in &names {
        let against = if base_branch(name) {
            None
        } else {
            base.as_ref()
        };
        histories.insert(
            name.clone(),
            branch_history(&git, against, name, &mut warnings),
        );
    }
    if let Scope::Named(..) = &scope
        && names.len() == 1
        && histories[&names[0]].fork_point.is_none()
        && user_since.is_none()
    {
        bail!(
            "{} has no fork point to measure from: {}; pass --base REF or a window (--since, --month, --week)",
            safe_text(&names[0]),
            match &base {
                None => "no integration branch was found".to_string(),
                Some(base) if base_branch(&names[0]) =>
                    format!("it is the base branch ({})", safe_text(&base.label)),
                Some(base) => format!("it shares no history with {}", safe_text(&base.label)),
            }
        );
    }

    // One collect, started early enough to hold everything the branch did: the
    // earliest of its fork point, its first commit and its creation. A pull
    // request looked up by number is not known to belong to any branch yet, so
    // it reads all history unless a window is given.
    if user_since.is_none()
        && !matches!(scope, Scope::Number(_))
        && let Some(earliest) = histories
            .values()
            .filter_map(|history| history.earliest().map(|(at, _)| at))
            .min()
    {
        // `--since` takes a local date, so start at the day's beginning.
        arguments.since = Some(earliest.with_timezone(&Local).date_naive().to_string());
    }
    // The agent pass is what lets the agent's commits be listed apart from the
    // user's own; it does not touch the human estimate.
    if arguments.agent_commits.is_none() {
        arguments.agent_commits = Some(String::new());
    }
    let plan = describe::Plan::parse(&describe, None, None, false)?;
    let describe_context = describe::Context::from_report(&arguments);
    // The branch report shows no goals.
    arguments.no_goals = true;
    let collected = collect(arguments, Purpose::Query)?;

    let aliases = collected
        .settings
        .config
        .compiled_project_aliases(&home_dir())?;
    let repo_id = PathResolver::with_context(Vec::new(), aliases, Vec::new(), home_dir())
        .describe(&git.top.to_string_lossy())
        .3;

    let mut link: Option<u64> = None;
    match &scope {
        Scope::Number(number) => {
            link = Some(*number);
            names = branches_for_pull_request(
                &collected,
                &repo_id,
                *number,
                base.as_ref().map(|base| base.label.as_str()),
            );
            if names.is_empty() {
                bail!(
                    "no session in this repository linked pull request #{number}; pass the branch name, or widen the window"
                );
            }
            for name in &names {
                if git.exists(name) {
                    let against = if base_branch(name) {
                        None
                    } else {
                        base.as_ref()
                    };
                    histories.insert(
                        name.clone(),
                        branch_history(&git, against, name, &mut warnings),
                    );
                } else {
                    warnings.push(format!(
                        "branch {} no longer exists locally, so its commits and fork point are unknown",
                        safe_text(name)
                    ));
                }
            }
        }
        Scope::Named(_, Some(number)) => {
            link = Some(*number);
            let linked = branches_for_pull_request(
                &collected,
                &repo_id,
                *number,
                base.as_ref().map(|base| base.label.as_str()),
            );
            if !linked.contains(&names[0]) {
                warnings.push(format!(
                    "no session in this repository linked pull request #{number} to {}",
                    safe_text(&names[0])
                ));
            }
        }
        _ => {}
    }

    let targets: Vec<Target> = names
        .iter()
        .map(|name| {
            target(
                &collected,
                &repo_id,
                name,
                histories.remove(name).unwrap_or_default(),
                user_since,
                user_until,
            )
        })
        .collect();

    let mut entries: Vec<Entry> = Vec::new();
    let mut omitted = 0;
    for target in &targets {
        let figures = figures(&collected, &repo_id, &[target]);
        if matches!(scope, Scope::All) && figures.is_empty() {
            omitted += 1;
            continue;
        }
        entries.push(Entry {
            branch: target.name.clone(),
            base: (!base_branch(&target.name))
                .then(|| base.as_ref().map(|base| base.label.clone()))
                .flatten(),
            window: EntryWindow {
                since: target.since.map(iso),
                until: target.until.map(iso),
                since_source: target.since_source,
            },
            figures,
            description: describe_target(
                &plan,
                &describe_context,
                &collected,
                &repo_id,
                target,
                &mut warnings,
            ),
        });
    }
    let combined = (entries.len() > 1 && !matches!(scope, Scope::All)).then(|| {
        let refs: Vec<&Target> = targets.iter().collect();
        Combined {
            branches: names.clone(),
            figures: figures(&collected, &repo_id, &refs),
        }
    });

    if let Some(stale) = stale_rates(&collected, &entries, combined.as_ref()) {
        warnings.push(stale);
    }
    for entry in &entries {
        if !entry.figures.unpriced_models.is_empty() {
            warnings.push(format!(
                "{} has tokens for models with no list price ({}); they are counted in tokens but not in list value",
                safe_text(&entry.branch),
                entry.figures.unpriced_models.join(", ")
            ));
        }
    }
    warnings.extend(collected.report.diagnostics.messages.iter().cloned());

    let output = Output {
        command: command.name(),
        status: "estimate",
        note: NOTE,
        pull_request: link,
        window: ScopeWindow {
            since: collected.window.0.map(iso),
            until: collected.window.1.map(iso),
        },
        window_human_seconds: collected.report.summary.human_estimated_seconds,
        rates_as_of: RATES_AS_OF,
        branches: entries,
        combined,
        omitted_branches: omitted,
        warnings,
    };
    print_output(&output, command, format, matches!(scope, Scope::All))
}

/// The opt-in description of one branch: the subjects of its own commits and
/// the titles of the sessions that were on it, read only for what `plan` asks
/// for. Commits an agent authored are never asked about (`describe` checks).
fn describe_target(
    plan: &describe::Plan,
    context: &describe::Context,
    collected: &Collected,
    repo_id: &str,
    target: &Target,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if !plan.is_active() {
        return None;
    }
    let (commits, sessions) = target_pieces(collected, repo_id, target);
    describe::for_branch(plan, context, &commits, &sessions, warnings).text()
}

/// The user's own commits on the branch and the sessions with work on it
/// inside its window, each oldest first so a description reads in the order
/// things happened.
fn target_pieces<'a>(
    collected: &'a Collected,
    repo_id: &str,
    target: &Target,
) -> (Vec<&'a GitCommit>, Vec<&'a Session>) {
    let listed: HashSet<&str> = target
        .history
        .commits
        .iter()
        .map(|(sha, _)| sha.as_str())
        .collect();
    let mut commits: Vec<&GitCommit> = collected
        .commits
        .iter()
        .filter(|commit| commit.repo_id == repo_id && listed.contains(commit.sha.as_str()))
        .collect();
    commits.sort_by_key(|commit| commit.timestamp);

    let mut on_branch: HashSet<(&str, &str)> = collected
        .timeline
        .ai_intervals
        .iter()
        .filter(|piece| piece.repo_id == repo_id && piece.branch.as_deref() == Some(&target.name))
        .filter(|piece| clip_interval(piece, target.since, target.until).is_some())
        .map(|piece| (piece.provider.as_str(), piece.session_id.as_str()))
        .collect();
    for session in collected
        .sessions
        .iter()
        .filter(|session| session.repo_id == repo_id)
    {
        let tokens_on_branch = session.token_events.iter().any(|event| {
            session.branch_at(event.timestamp) == Some(&target.name)
                && target.since.is_none_or(|since| event.timestamp >= since)
                && target.until.is_none_or(|until| event.timestamp < until)
        });
        if tokens_on_branch {
            on_branch.insert((session.provider.as_str(), session.session_id.as_str()));
        }
    }
    let mut sessions: Vec<&Session> = collected
        .sessions
        .iter()
        .filter(|session| {
            session.repo_id == repo_id
                && on_branch.contains(&(session.provider.as_str(), session.session_id.as_str()))
        })
        .collect();
    sessions.sort_by_key(|session| session.first_seen());
    (commits, sessions)
}

/// The window and starting point for `name`: the user's `--since` if given,
/// otherwise the earliest of the fork point, the first commit, the branch's
/// creation and the first moment a session was on it in this repository.
fn target(
    collected: &Collected,
    repo_id: &str,
    name: &str,
    history: BranchHistory,
    user_since: Option<DateTime<Utc>>,
    user_until: Option<DateTime<Utc>>,
) -> Target {
    let first_session = first_tagged(collected, repo_id, name);
    let (since, since_source) = match user_since {
        Some(since) => (Some(since), "window"),
        None => {
            let from_git = history.earliest();
            match (from_git, first_session) {
                (Some((git, _)), Some(session)) if session < git => {
                    (Some(session), "first session on the branch")
                }
                (Some((git, source)), _) => (Some(git), source),
                (None, Some(session)) => (Some(session), "first session on the branch"),
                (None, None) => (collected.window.0, "window"),
            }
        }
    };
    Target {
        name: name.to_string(),
        history,
        since,
        until: user_until,
        since_source,
    }
}

/// The first signal or agent interval tagged with `name` in this repository.
fn first_tagged(collected: &Collected, repo_id: &str, name: &str) -> Option<DateTime<Utc>> {
    let signals = collected
        .timeline
        .human_signals
        .iter()
        .filter(|signal| signal.repo_id == repo_id && signal.branch.as_deref() == Some(name))
        .map(|signal| signal.timestamp);
    let pieces = collected
        .timeline
        .human_intervals
        .iter()
        .chain(&collected.timeline.ai_intervals)
        .filter(|piece| piece.repo_id == repo_id && piece.branch.as_deref() == Some(name))
        .map(|piece| piece.start);
    signals.chain(pieces).min()
}

/// The branches the sessions that linked pull request `number` were on, at the
/// moment of the link where it has one, and at the end of the session where it
/// does not. The base branch is never the answer: a pull request is not made
/// from it.
fn branches_for_pull_request(
    collected: &Collected,
    repo_id: &str,
    number: u64,
    base: Option<&str>,
) -> Vec<String> {
    let mut found = BTreeSet::new();
    for session in collected
        .sessions
        .iter()
        .filter(|session| session.repo_id == repo_id)
    {
        for link in session
            .pull_requests
            .iter()
            .filter(|link| link.number == number)
        {
            let branch = link
                .at
                .or_else(|| session.last_seen())
                .and_then(|at| session.branch_at(at))
                .or_else(|| session.branches.last().map(|mark| mark.branch.as_str()));
            if let Some(branch) = branch
                && Some(branch) != base
            {
                found.insert(branch.to_string());
            }
        }
    }
    found.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Figures
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Figures {
    human_seconds: f64,
    agent_wall_seconds: f64,
    parallel_agent_seconds: f64,
    sessions: usize,
    providers: Vec<String>,
    tokens: Tokens,
    models: Vec<ModelFigures>,
    /// What the tokens would cost at list prices; models with no price are
    /// left out of it and named in `unpriced_models`.
    list_value_usd: f64,
    unpriced_models: Vec<String>,
    commits: Commits,
}

impl Figures {
    fn is_empty(&self) -> bool {
        self.human_seconds <= 0.0
            && self.agent_wall_seconds <= 0.0
            && self.tokens.total == 0
            && self.commits.own + self.commits.agent == 0
    }
}

#[derive(Default, Serialize)]
struct Tokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    total: u64,
}

#[derive(Serialize)]
struct ModelFigures {
    model: String,
    tokens: u64,
    list_value_usd: Option<f64>,
}

#[derive(Default, Serialize)]
struct Commits {
    /// The user's own commits on the branch.
    own: usize,
    additions: u64,
    deletions: u64,
    /// Commits an agent authored.
    agent: usize,
    agent_additions: u64,
    agent_deletions: u64,
    /// Commits on the branch that are neither: other people's, or outside the
    /// window the run read.
    other: usize,
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn figures(collected: &Collected, repo_id: &str, targets: &[&Target]) -> Figures {
    let find = |branch: &str| targets.iter().find(|target| target.name == branch);
    let clip = |piece: &Interval| -> Option<Interval> {
        let target = find(piece.branch.as_deref()?)?;
        clip_interval(piece, target.since, target.until)
    };
    let in_repo = |piece: &&Interval| piece.repo_id == repo_id;

    // Human time: the pieces themselves, never the window total.
    let human_seconds: f64 = collected
        .timeline
        .human_intervals
        .iter()
        .filter(in_repo)
        .filter_map(clip)
        .map(|piece| piece.seconds())
        .sum();
    let agent: Vec<Interval> = collected
        .timeline
        .ai_intervals
        .iter()
        .filter(in_repo)
        .filter_map(clip)
        .collect();
    let parallel_agent_seconds: f64 = agent.iter().map(Interval::seconds).sum();

    let mut sessions: BTreeSet<(String, String)> = agent
        .iter()
        .map(|piece| (piece.provider.clone(), piece.session_id.clone()))
        .collect();
    let mut usage: BTreeMap<String, TokenUsage> = BTreeMap::new();
    for session in collected
        .sessions
        .iter()
        .filter(|session| session.repo_id == repo_id)
    {
        for event in &session.token_events {
            let Some(target) = session.branch_at(event.timestamp).and_then(find) else {
                continue;
            };
            if target.since.is_some_and(|since| event.timestamp < since)
                || target.until.is_some_and(|until| event.timestamp >= until)
            {
                continue;
            }
            sessions.insert((session.provider.clone(), session.session_id.clone()));
            let total = usage.entry(event.model.clone()).or_default();
            total.input_tokens = total.input_tokens.saturating_add(event.usage.input_tokens);
            total.output_tokens = total
                .output_tokens
                .saturating_add(event.usage.output_tokens);
            total.cache_read_tokens = total
                .cache_read_tokens
                .saturating_add(event.usage.cache_read_tokens);
            total.cache_creation_tokens = total
                .cache_creation_tokens
                .saturating_add(event.usage.cache_creation_tokens);
        }
    }
    let mut tokens = Tokens::default();
    let mut models = Vec::new();
    let mut list_value_usd = 0.0;
    let mut unpriced_models = Vec::new();
    for (model, usage) in &usage {
        tokens.input = tokens.input.saturating_add(usage.input_tokens);
        tokens.output = tokens.output.saturating_add(usage.output_tokens);
        tokens.cache_read = tokens.cache_read.saturating_add(usage.cache_read_tokens);
        tokens.cache_creation = tokens
            .cache_creation
            .saturating_add(usage.cache_creation_tokens);
        let value = pricing::list_value_usd(model, usage, &collected.settings.rate_overrides);
        match value {
            Some(value) => list_value_usd += value,
            None => unpriced_models.push(model.clone()),
        }
        models.push(ModelFigures {
            model: model.clone(),
            tokens: usage.total(),
            list_value_usd: value.map(round3),
        });
    }
    tokens.total = tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_creation);
    models.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.model.cmp(&right.model))
    });

    // Commits: the branch's own, by SHA, split by who wrote them.
    let listed: HashSet<&str> = targets
        .iter()
        .flat_map(|target| target.history.commits.iter().map(|(sha, _)| sha.as_str()))
        .collect();
    let mut commits = Commits::default();
    let mut seen = HashSet::new();
    for commit in collected
        .commits
        .iter()
        .filter(|commit| commit.repo_id == repo_id && listed.contains(commit.sha.as_str()))
    {
        if seen.insert(commit.sha.as_str()) {
            commits.own += 1;
            commits.additions += commit.additions;
            commits.deletions += commit.deletions;
        }
    }
    for commit in collected
        .agent_commits
        .iter()
        .filter(|commit| commit.repo_id == repo_id && listed.contains(commit.sha.as_str()))
    {
        if seen.insert(commit.sha.as_str()) {
            commits.agent += 1;
            commits.agent_additions += commit.additions;
            commits.agent_deletions += commit.deletions;
        }
    }
    commits.other = listed.len().saturating_sub(seen.len());

    Figures {
        human_seconds: round3(human_seconds),
        agent_wall_seconds: round3(union_seconds(&agent)),
        parallel_agent_seconds: round3(parallel_agent_seconds),
        sessions: sessions.len(),
        providers: sessions
            .iter()
            .map(|(provider, _)| provider_name(provider))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        tokens,
        models,
        list_value_usd: round3(list_value_usd),
        unpriced_models,
        commits,
    }
}

fn provider_name(provider: &str) -> String {
    match provider {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "copilot" => "Copilot CLI",
        "copilot-vscode" => "Copilot Chat",
        "gemini" => "Gemini CLI",
        "opencode" => "OpenCode",
        "pi" => "Pi",
        other => other,
    }
    .to_string()
}

/// The stale-rates warning, when a built-in price contributed to a list value
/// that is shown.
fn stale_rates(
    collected: &Collected,
    entries: &[Entry],
    combined: Option<&Combined>,
) -> Option<String> {
    let overrides = &collected.settings.rate_overrides;
    let built_in = entries
        .iter()
        .map(|entry| &entry.figures)
        .chain(combined.map(|combined| &combined.figures))
        .flat_map(|figures| figures.models.iter())
        .any(|model| {
            overrides
                .resolve(&model.model)
                .is_some_and(|rate| rate.source == RateSource::BuiltIn)
        });
    built_in
        .then(|| pricing::stale_rates_warning(collected.settings.now.date_naive()))
        .flatten()
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Entry {
    branch: String,
    base: Option<String>,
    window: EntryWindow,
    #[serde(flatten)]
    figures: Figures,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Serialize)]
struct EntryWindow {
    since: Option<String>,
    until: Option<String>,
    /// What decided the start: the fork point, the first commit, the branch's
    /// creation, the first session on the branch, or a window the user gave.
    since_source: &'static str,
}

/// Several branches reported as one, for a pull request whose sessions moved
/// between branches. Computed over the pooled pieces, so parallel agents on
/// two of the branches are still one wall-clock span.
#[derive(Serialize)]
struct Combined {
    branches: Vec<String>,
    #[serde(flatten)]
    figures: Figures,
}

#[derive(Serialize)]
struct ScopeWindow {
    since: Option<String>,
    until: Option<String>,
}

#[derive(Serialize)]
struct Output {
    command: &'static str,
    status: &'static str,
    note: &'static str,
    pull_request: Option<u64>,
    /// The window the run read, which holds more than any one branch.
    window: ScopeWindow,
    /// Every human second in that window, on any branch or repository. It is
    /// context, not the branch's time.
    window_human_seconds: f64,
    rates_as_of: &'static str,
    branches: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    combined: Option<Combined>,
    /// With `--all`: branches that showed no work, left out of the rows.
    omitted_branches: usize,
    warnings: Vec<String>,
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn day(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%Y-%m-%d").to_string()
}

fn print_output(output: &Output, command: Kind, format: OutputFormat, all: bool) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(output)?),
        OutputFormat::Markdown if command == Kind::Pr => print!("{}", pr_block(output)),
        OutputFormat::Markdown => print!("{}", render_markdown(&document(output, command, all))),
        OutputFormat::Html => print!("{}", render_html(&document(output, command, all))),
        _ => print!("{}", render_text(&document(output, command, all))),
    }
    Ok(())
}

fn summary(output: &Output) -> Option<(&Figures, String)> {
    match (&output.combined, output.branches.first()) {
        (Some(combined), _) => Some((&combined.figures, combined.branches.join(", "))),
        (None, Some(entry)) => Some((&entry.figures, entry.branch.clone())),
        (None, None) => None,
    }
}

fn money(value: f64) -> String {
    if value >= 100.0 {
        format!("${value:.0}")
    } else {
        format!("${value:.2}")
    }
}

fn lines(commits: &Commits) -> String {
    format!(
        "+{}/\u{2212}{}",
        number(commits.additions),
        number(commits.deletions)
    )
}

fn commit_words(commits: &Commits) -> String {
    let mut text = format!(
        "{} {}",
        number(commits.own),
        if commits.own == 1 {
            "commit"
        } else {
            "commits"
        }
    );
    if commits.own > 0 {
        text.push(' ');
        text.push_str(&lines(commits));
    }
    if commits.agent > 0 {
        text.push_str(&format!(
            ", {} agent {}",
            number(commits.agent),
            if commits.agent == 1 {
                "commit"
            } else {
                "commits"
            }
        ));
    }
    if commits.other > 0 {
        text.push_str(&format!(
            ", {} not counted (other authors, or outside the window)",
            number(commits.other)
        ));
    }
    text
}

fn models_words(figures: &Figures) -> Option<String> {
    if figures.models.is_empty() {
        return None;
    }
    let shown: Vec<&str> = figures
        .models
        .iter()
        .take(3)
        .map(|model| model.model.as_str())
        .collect();
    let more = figures.models.len().saturating_sub(shown.len());
    Some(if more > 0 {
        format!("models {} and {more} more", shown.join(", "))
    } else {
        format!("models {}", shown.join(", "))
    })
}

/// Names the rates a list value used, for the line that shows one.
fn rates_words(output: &Output) -> String {
    format!("rates as of {}", output.rates_as_of)
}

/// The one line a pull request carries: effort, in the order the spec gives.
fn effort_line(output: &Output, figures: &Figures) -> String {
    let mut parts = vec![
        format!("~{} human", hours(figures.human_seconds)),
        format!("{} agent wall", hours(figures.agent_wall_seconds)),
    ];
    if figures.parallel_agent_seconds - figures.agent_wall_seconds >= 60.0 {
        parts.push(format!(
            "{} summed across parallel agents",
            hours(figures.parallel_agent_seconds)
        ));
    }
    parts.push(if figures.sessions == 0 {
        "no AI sessions".to_string()
    } else {
        format!(
            "{} {} ({})",
            number(figures.sessions),
            if figures.sessions == 1 {
                "session"
            } else {
                "sessions"
            },
            figures.providers.join(", ")
        )
    });
    parts.extend(models_words(figures));
    if figures.list_value_usd > 0.0 {
        parts.push(format!(
            "list value \u{2248} {} ({})",
            money(figures.list_value_usd),
            rates_words(output)
        ));
    }
    parts.push(commit_words(&figures.commits));
    parts.join(" \u{b7} ")
}

/// The Markdown block for a pull request description. Every value goes through
/// `escape_markdown`, so a branch called `#123` or `@team` does not link or
/// notify anyone; the bold markers are the only Markdown written unescaped.
fn pr_block(output: &Output) -> String {
    let Some((figures, branches)) = summary(output) else {
        return "**Effort (estimated):** no work found for this branch\n".to_string();
    };
    let escape = |text: &str| escape_markdown(&safe_text(text));
    let since = output
        .branches
        .iter()
        .filter_map(|entry| entry.window.since.as_deref())
        .min()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| day(at.with_timezone(&Utc)));
    let until = output
        .branches
        .iter()
        .filter_map(|entry| entry.window.until.as_deref())
        .max()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| day(at.with_timezone(&Utc)))
        .unwrap_or_else(|| day(Utc::now()));
    let mut block = format!(
        "**Effort (estimated):** {}\n",
        escape(&effort_line(output, figures))
    );
    block.push_str(&format!(
        "\n**Scope:** {}{}{}\n",
        escape(&format!(
            "{} {}",
            if output.branches.len() > 1 {
                "branches"
            } else {
                "branch"
            },
            branches
        )),
        match &since {
            Some(since) => escape(&format!(" \u{b7} {since} \u{2192} {until}")),
            None => String::new(),
        },
        match output.pull_request {
            Some(number) => escape(&format!(" \u{b7} pull request #{number}")),
            None => String::new(),
        }
    ));
    // `--describe` text is one bounded line per branch. One branch needs no
    // label; several get one line each, so the reader can tell whose it is.
    let described: Vec<&Entry> = output
        .branches
        .iter()
        .filter(|entry| entry.description.is_some())
        .collect();
    match described.as_slice() {
        [] => {}
        [entry] if output.branches.len() == 1 => {
            block.push_str(&format!(
                "\n**Description:** {}\n",
                escape(entry.description.as_deref().unwrap_or_default())
            ));
        }
        entries => {
            block.push_str("\n**Description:**\n");
            for entry in entries {
                block.push_str(&format!(
                    "\n- {}: {}",
                    escape(&entry.branch),
                    escape(entry.description.as_deref().unwrap_or_default())
                ));
            }
            block.push('\n');
        }
    }
    let mut footnote = NOTE.to_string();
    for warning in output
        .warnings
        .iter()
        .filter(|warning| warning.starts_with("built-in list rates"))
    {
        footnote.push(' ');
        footnote.push_str(warning);
    }
    block.push_str(&format!("\n_{}_\n", escape(&footnote)));
    block
}

fn document(output: &Output, command: Kind, all: bool) -> Document {
    let title = match command {
        Kind::Pr => "Pull request effort (estimated)",
        Kind::Branch if all => "Effort per branch (estimated)",
        Kind::Branch => "Branch effort (estimated)",
    };
    let mut blocks = Vec::new();
    if all {
        let columns = vec![
            Column::text("Branch"),
            Column::number("Human"),
            Column::number("Agent wall"),
            Column::number("Sessions"),
            Column::number("Tokens"),
            Column::number("List value"),
            Column::number("Commits"),
            Column::text("Lines"),
        ];
        let rows = output
            .branches
            .iter()
            .map(|entry| {
                let figures = &entry.figures;
                vec![
                    entry.branch.clone(),
                    hours(figures.human_seconds),
                    hours(figures.agent_wall_seconds),
                    number(figures.sessions),
                    compact_tokens(figures.tokens.total),
                    if figures.list_value_usd > 0.0 {
                        money(figures.list_value_usd)
                    } else {
                        "\u{2014}".to_string()
                    },
                    number(figures.commits.own),
                    if figures.commits.own > 0 {
                        lines(&figures.commits)
                    } else {
                        "\u{2014}".to_string()
                    },
                ]
            })
            .collect();
        let mut table = Table::new(columns, rows);
        let human: f64 = output
            .branches
            .iter()
            .map(|entry| entry.figures.human_seconds)
            .sum();
        table.total = Some(vec![
            "Sum of branches".to_string(),
            hours(human),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ]);
        blocks.push(Block::Table(table));
        blocks.push(Block::Paragraph(format!(
            "All human work in the window, on any branch or repository: {}. Each row counts only the pieces labelled with that branch in this repository, so the rows add up to no more than that.",
            hours(output.window_human_seconds)
        )));
        if output.omitted_branches > 0 {
            blocks.push(Block::Paragraph(format!(
                "{} local {} showed no work in the window and {} not listed.",
                output.omitted_branches,
                if output.omitted_branches == 1 {
                    "branch"
                } else {
                    "branches"
                },
                if output.omitted_branches == 1 {
                    "is"
                } else {
                    "are"
                }
            )));
        }
    } else if let Some((figures, branches)) = summary(output) {
        let mut facts = vec![(
            if output.branches.len() > 1 {
                "Branches"
            } else {
                "Branch"
            }
            .to_string(),
            branches,
        )];
        if let Some(number) = output.pull_request {
            facts.push(("Pull request".to_string(), format!("#{number}")));
        }
        if let Some(entry) = output.branches.first() {
            if let Some(base) = &entry.base {
                facts.push(("Base".to_string(), base.clone()));
            }
            let since = entry
                .window
                .since
                .as_deref()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|at| day(at.with_timezone(&Utc)));
            let until = entry
                .window
                .until
                .as_deref()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|at| day(at.with_timezone(&Utc)))
                .unwrap_or_else(|| "now".to_string());
            if let Some(since) = since {
                facts.push((
                    "Window".to_string(),
                    format!(
                        "{since} \u{2192} {until} (from the {})",
                        entry.window.since_source
                    ),
                ));
            }
            if let Some(description) = &entry.description {
                facts.push(("Description".to_string(), description.clone()));
            }
        }
        facts.push(("Human time".to_string(), hours(figures.human_seconds)));
        facts.push((
            "Agent wall time".to_string(),
            hours(figures.agent_wall_seconds),
        ));
        facts.push((
            "Parallel agent time".to_string(),
            hours(figures.parallel_agent_seconds),
        ));
        facts.push((
            "Sessions".to_string(),
            if figures.sessions == 0 {
                "0".to_string()
            } else {
                format!(
                    "{} ({})",
                    number(figures.sessions),
                    figures.providers.join(", ")
                )
            },
        ));
        facts.push((
            "Tokens".to_string(),
            format!(
                "{} (in {}, out {}, cache read {}, cache write {})",
                compact_tokens(figures.tokens.total),
                compact_tokens(figures.tokens.input),
                compact_tokens(figures.tokens.output),
                compact_tokens(figures.tokens.cache_read),
                compact_tokens(figures.tokens.cache_creation)
            ),
        ));
        if figures.list_value_usd > 0.0 {
            facts.push((
                "List value".to_string(),
                format!(
                    "\u{2248} {} ({})",
                    money(figures.list_value_usd),
                    rates_words(output)
                ),
            ));
        }
        facts.push(("Commits".to_string(), commit_words(&figures.commits)));
        blocks.push(Block::Facts(facts));
        if !figures.models.is_empty() {
            blocks.push(Block::Section("Models".to_string()));
            blocks.push(Block::Table(Table::new(
                vec![
                    Column::text("Model"),
                    Column::number("Tokens"),
                    Column::number("List value"),
                ],
                figures
                    .models
                    .iter()
                    .map(|model| {
                        vec![
                            model.model.clone(),
                            compact_tokens(model.tokens),
                            model
                                .list_value_usd
                                .map_or_else(|| "unpriced".to_string(), money),
                        ]
                    })
                    .collect(),
            )));
        }
    } else {
        blocks.push(Block::Paragraph(
            "No work found for this branch.".to_string(),
        ));
    }
    blocks.push(Block::Paragraph(output.note.to_string()));
    if !output.warnings.is_empty() {
        blocks.push(Block::Section("Warnings".to_string()));
        blocks.push(Block::List(output.warnings.clone()));
    }
    Document {
        title: title.to_string(),
        blocks,
    }
}

/// The terminal rendering of a `Document`: plain lines, aligned columns. The
/// same text the other formats carry, under the same control-character filter.
fn render_text(document: &Document) -> String {
    document::render_text(
        document,
        &TextStyle {
            underline_sections: false,
            paragraphs: Paragraphs::Indented,
            indent: "  ",
            rule: '\u{2500}',
            rule_above_total: false,
            tight_narrow_columns: false,
        },
    )
}
