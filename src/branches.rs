//! Branch attribution: filling in the branch a session or commit belongs to
//! when the provider did not record one. See `docs/branches.md` for the rules
//! as users meet them; the comments here say why they are what they are.
//!
//! Everything is read from the local repository, never the network, and no
//! more than three `git` processes are spawned per checkout:
//!
//!  1. `for-each-ref`: the local branches, the checked-out one and the target
//!     of `refs/remotes/origin/HEAD` (what the remote calls its default).
//!  2. `log --source … --not <integration>`: the branch each commit that is
//!     *not* on the integration branch is reachable from. Commits only on the
//!     integration branch are the ones this cannot place.
//!  3. `log -g --grep-reflog='^checkout: moving from ' …` on `HEAD`: when the
//!     checkout switched branches. Git does the filtering, so the reflog's
//!     other entries (`commit: <your message>`, `rebase: …`) are never
//!     delivered to this process, let alone parsed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;

use crate::git::{COMMITTER_DATE_SKEW, git_executable};
use crate::model::{BranchMark, BranchSource, Diagnostics, GitCommit, Session};

/// Tried in order when the config does not name the integration branches.
const DEFAULT_INTEGRATION: &[&str] = &["main", "master", "trunk", "develop"];
/// Marks one session may hold, as the providers' own limit.
const MAXIMUM_MARKS: usize = 256;
/// The longest branch name kept, in bytes.
const MAXIMUM_NAME_BYTES: usize = 256;

/// The integration branch candidates from the `branches` config block, in
/// order of preference: `{"integration": ["main", "release"]}` (a single string
/// is accepted too). A block that cannot be read is an error naming the key.
fn integration_names(config: Option<&Value>) -> Result<Vec<String>> {
    let defaults = || {
        DEFAULT_INTEGRATION
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    };
    let block = match config {
        None | Some(Value::Null) => return Ok(defaults()),
        Some(Value::Object(block)) => block,
        Some(_) => bail!("invalid \"branches\" configuration: expected an object"),
    };
    if let Some(unknown) = block.keys().find(|key| key.as_str() != "integration") {
        bail!(
            "invalid \"branches\" configuration: unknown key \"{unknown}\" (expected integration)"
        );
    }
    let invalid = || {
        anyhow::anyhow!(
            "invalid \"branches\" configuration: \"integration\" must be a branch name or a list of names"
        )
    };
    let names: Vec<String> = match block.get("integration") {
        None | Some(Value::Null) => return Ok(defaults()),
        Some(Value::String(name)) => vec![name.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string).ok_or_else(invalid))
            .collect::<Result<_>>()?,
        Some(_) => return Err(invalid()),
    };
    if names.len() > 32 || names.iter().any(|name| !valid_name(name)) {
        return Err(invalid());
    }
    Ok(names)
}

/// A name worth keeping: not empty, bounded, and free of control characters,
/// which have no business in a ref and would break the report's tables.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAXIMUM_NAME_BYTES && !name.chars().any(char::is_control)
}

/// Whether a name a provider or `log --branch` records can be a branch: bounded,
/// free of control characters, not `HEAD` or `@` (what a detached checkout
/// reports), and not a name Git forbids. Branch names obey Git's ref rules,
/// which exclude spaces, `~ ^ : ? * [ \`, `..`, `@{`, a leading `-` and a
/// trailing `/`, `.` or `.lock`, so a name that breaks them is not a branch.
/// All-digit and all-hex names (ticket numbers, `deadbeef`) are legal branch
/// names and are kept.
pub(crate) fn recordable_branch(name: &str) -> bool {
    valid_name(name)
        && name != "HEAD"
        && name != "@"
        && !name.starts_with('-')
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains(['~', '^', ':', '?', '*', '[', '\\', ' '])
}

/// Whether a reflog destination reads as a branch rather than a detached HEAD.
/// `git checkout <sha>` logs the typed argument where a branch name would be,
/// so on top of [`recordable_branch`] a bare commit id is told apart by its
/// shape. This is only for the reflog: a provider records `HEAD` for a
/// detached checkout, never a SHA, so it does not get this test. A tag checked
/// out by name cannot be told apart and reads as a branch: the one blur this
/// rule accepts.
fn plausible_branch(name: &str) -> bool {
    let detached_id = name.len() >= 7 && name.chars().all(|c| c.is_ascii_hexdigit());
    recordable_branch(name) && !detached_id
}

/// Spawns `git`, counts the processes and reports a failure once, in words.
struct Runner<'a> {
    exe: PathBuf,
    diagnostics: &'a mut Diagnostics,
    spawned: usize,
}

impl Runner<'_> {
    fn run(&mut self, root: &Path, arguments: &[String]) -> Option<String> {
        self.spawned += 1;
        let output = match Command::new(&self.exe)
            .arg("--no-pager")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                self.fail(root, &error.to_string());
                return None;
            }
        };
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let error = String::from_utf8_lossy(&output.stderr);
        // A repository with no commits has no HEAD to read. That is an empty
        // answer, not a fault worth a warning.
        if error.contains("does not have any commits yet")
            || error.contains("ambiguous argument 'HEAD'")
        {
            return None;
        }
        self.fail(root, error.trim());
        None
    }

    fn succeeds(&mut self, root: &Path, arguments: &[String]) -> bool {
        self.spawned += 1;
        Command::new(&self.exe)
            .arg("-C")
            .arg(root)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn fail(&mut self, root: &Path, reason: &str) {
        self.diagnostics.git_errors += 1;
        self.diagnostics.warn(format!(
            "Git branch lookup failed for {}: {}",
            root.display(),
            reason.chars().take(200).collect::<String>()
        ));
    }
}

/// What `for-each-ref` says about a checkout.
fn refs_arguments() -> Vec<String> {
    [
        "for-each-ref",
        "--format=%(HEAD)%09%(refname)%09%(symref)",
        "refs/heads",
        "refs/remotes/origin/HEAD",
    ]
    .map(String::from)
    .to_vec()
}

/// Which branch every commit that is not on `integration` is reachable from.
/// `HEAD` follows `--branches` so that, where the two reach the same commit, a
/// branch name wins over the word `HEAD`; it is named at all only for a
/// detached HEAD, which an attached one adds nothing to.
fn source_arguments(
    integration: Option<&str>,
    since: DateTime<Utc>,
    include_head: bool,
) -> Vec<String> {
    let mut arguments: Vec<String> = [
        "log",
        "--no-merges",
        "--source",
        // Hash and the ref it was reached from; never a subject or an author.
        "--format=%H%x09%S",
    ]
    .map(String::from)
    .to_vec();
    // Bounded like the commit listing in `git.rs`, and widened the same way:
    // Git compares the committer date and the report windows on the author's.
    arguments.push(format!(
        "--since={}",
        (since - COMMITTER_DATE_SKEW).to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    arguments.push("--branches".to_string());
    if include_head {
        arguments.push("HEAD".to_string());
    }
    if let Some(integration) = integration {
        arguments.push("--not".to_string());
        // Fully qualified, so a tag with the branch's name cannot stand in.
        arguments.push(format!("refs/heads/{integration}"));
    }
    arguments
}

/// The HEAD reflog, reduced by Git itself to the branch switches. `%gd` with
/// `--date=unix` is `HEAD@{<epoch>}` and `%gs` is the reflog subject; because
/// of `--grep-reflog` the only subjects that come back begin
/// `checkout: moving from `, and they hold nothing but two ref names.
fn reflog_arguments() -> Vec<String> {
    [
        "-c",
        "log.showSignature=false",
        "log",
        "-g",
        "--grep-reflog=^checkout: moving from ",
        "--date=unix",
        "--format=%gd%x09%gs",
        "HEAD",
    ]
    .map(String::from)
    .to_vec()
}

#[derive(Debug, Default)]
struct Refs {
    branches: HashSet<String>,
    /// The branch this checkout has out; `None` when detached.
    current: Option<String>,
    integration: Option<String>,
}

/// Reads `refs_arguments` output. The integration branch is the first of
/// `names` that exists locally; failing that, the local branch `origin/HEAD`
/// points at (local metadata, so no fetch is involved).
fn parse_refs(output: &str, names: &[String]) -> Refs {
    let mut refs = Refs::default();
    let mut origin_head = None;
    for line in output.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(marker), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let symref = fields.next().unwrap_or_default();
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            if !valid_name(branch) {
                continue;
            }
            if marker == "*" {
                refs.current = Some(branch.to_string());
            }
            refs.branches.insert(branch.to_string());
        } else if name == "refs/remotes/origin/HEAD" {
            origin_head = symref
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_string);
        }
    }
    refs.integration = names
        .iter()
        .find(|name| refs.branches.contains(*name))
        .cloned()
        .or_else(|| origin_head.filter(|name| refs.branches.contains(name)));
    refs
}

/// How far back from the moment the reflog is read the branch a checkout left
/// is still believed. Git expires reflog entries by their age when `gc` runs,
/// and entries whose commit nothing reaches any more (the checkout of a
/// squash-merged, deleted feature branch) expire after
/// `gc.reflogExpireUnreachable`, 30 days by default. Switches older than that
/// can therefore be missing anywhere; only the last 30 days are known to be
/// whole. This is the default, not the repository's configured value.
const UNREACHABLE_EXPIRY: Duration = Duration::days(30);

/// One branch switch of one checkout. `None` is a detached HEAD.
#[derive(Debug)]
struct Switch {
    at: DateTime<Utc>,
    from: Option<String>,
    to: Option<String>,
}

/// The branch switches of one checkout, oldest first.
#[derive(Debug, Default)]
struct Reflog {
    switches: Vec<Switch>,
    /// When the reflog was read: what the look-back is measured from.
    read_at: DateTime<Utc>,
}

fn parse_reflog(output: &str, read_at: DateTime<Utc>) -> Reflog {
    let mut switches = Vec::new();
    for line in output.lines() {
        let Some((selector, subject)) = line.split_once('\t') else {
            continue;
        };
        let Some(at) = selector
            .strip_prefix("HEAD@{")
            .and_then(|rest| rest.strip_suffix('}'))
            .and_then(|epoch| epoch.parse::<i64>().ok())
            .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
        else {
            continue;
        };
        let Some((from, to)) = subject
            .strip_prefix("checkout: moving from ")
            .and_then(|rest| rest.split_once(" to "))
        else {
            continue;
        };
        let branch = |name: &str| plausible_branch(name).then(|| name.to_string());
        switches.push(Switch {
            at,
            from: branch(from),
            to: branch(to.trim()),
        });
    }
    // Git lists newest first. Reversing before the stable sort keeps two
    // switches in one second in the order they happened.
    switches.reverse();
    switches.sort_by_key(|switch| switch.at);
    Reflog { switches, read_at }
}

impl Reflog {
    /// The branch the checkout was on at `at`: the destination of the last
    /// switch by then. Before the first switch on record, the branch that
    /// switch left, but only if `at` is within `UNREACHABLE_EXPIRY` of when the
    /// reflog was read: older switches may have expired, so how long the
    /// checkout had been on that branch is not known, and older reads as
    /// unknown. `None` also when detached or when the reflog is empty.
    fn branch_at(&self, at: DateTime<Utc>) -> Option<&str> {
        let first = self.switches.first()?;
        if at < first.at {
            return (at >= self.read_at - UNREACHABLE_EXPIRY)
                .then_some(first.from.as_deref())
                .flatten();
        }
        let passed = self.switches.partition_point(|switch| switch.at <= at);
        self.switches[passed - 1].to.as_deref()
    }

    /// Whether the reflog knows nothing about `at` because it is later than
    /// every switch on record (or there are none): the checkout's branch is
    /// then whatever is checked out now.
    fn is_after_last(&self, at: DateTime<Utc>) -> bool {
        self.switches.last().is_none_or(|switch| switch.at < at)
    }
}

/// The marks for a session that spans `first..=last` in a checkout with
/// `reflog` and `current` checked out. A mark is written only where the branch
/// changes, with the session start as `from: None`. The source is `Reflog` if
/// any mark came from a recorded switch, `Head` if all came from the checkout's
/// current branch.
fn session_marks(
    reflog: &Reflog,
    current: Option<&str>,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
) -> (Vec<BranchMark>, BranchSource) {
    let pick = |at: DateTime<Utc>| -> Option<(&str, BranchSource)> {
        if reflog.is_after_last(at) {
            current.map(|branch| (branch, BranchSource::Head))
        } else {
            reflog
                .branch_at(at)
                .map(|branch| (branch, BranchSource::Reflog))
        }
    };
    let mut points = vec![first];
    points.extend(
        reflog
            .switches
            .iter()
            .map(|switch| switch.at)
            .filter(|at| *at > first && *at <= last),
    );
    points.dedup();

    let mut marks: Vec<BranchMark> = Vec::new();
    let mut source = BranchSource::Head;
    for (index, at) in points.into_iter().enumerate() {
        let Some((branch, how)) = pick(at) else {
            continue;
        };
        if marks.last().is_some_and(|mark| mark.branch == branch) {
            continue;
        }
        if marks.len() >= MAXIMUM_MARKS {
            break;
        }
        if how == BranchSource::Reflog {
            source = BranchSource::Reflog;
        }
        marks.push(BranchMark {
            from: (index > 0).then_some(at),
            branch: branch.to_string(),
        });
    }
    if marks.is_empty() {
        source = BranchSource::None;
    }
    (marks, source)
}

/// A working copy: its top directory.
#[derive(Clone, Debug)]
struct Checkout {
    root: PathBuf,
}

/// The checkout `path` is in, found by looking for `.git` upward and without
/// asking Git. A directory that is gone has none.
fn checkout_of(path: &Path) -> Option<Checkout> {
    if !path.is_dir() {
        return None;
    }
    let root = path
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())?;
    Some(Checkout {
        root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
    })
}

/// What has been asked of Git so far, so nothing is asked twice.
struct Context<'a> {
    git: Runner<'a>,
    names: &'a [String],
    /// When the reflogs are read.
    read_at: DateTime<Utc>,
    refs: HashMap<PathBuf, Option<Rc<Refs>>>,
    reflogs: HashMap<PathBuf, Option<Rc<Reflog>>>,
    checkouts: HashMap<String, Option<Checkout>>,
}

impl Context<'_> {
    fn checkout(&mut self, cwd: &str) -> Option<Checkout> {
        self.checkouts
            .entry(cwd.to_string())
            .or_insert_with(|| checkout_of(Path::new(cwd)))
            .clone()
    }

    fn refs(&mut self, root: &Path) -> Option<Rc<Refs>> {
        if let Some(known) = self.refs.get(root) {
            return known.clone();
        }
        let refs = self
            .git
            .run(root, &refs_arguments())
            .map(|output| Rc::new(parse_refs(&output, self.names)));
        self.refs.insert(root.to_path_buf(), refs.clone());
        refs
    }

    fn reflog(&mut self, root: &Path) -> Option<Rc<Reflog>> {
        if let Some(known) = self.reflogs.get(root) {
            return known.clone();
        }
        let reflog = self
            .git
            .run(root, &reflog_arguments())
            .map(|output| Rc::new(parse_reflog(&output, self.read_at)));
        self.reflogs.insert(root.to_path_buf(), reflog.clone());
        reflog
    }

    /// The branch each commit after `since` that is not on the integration
    /// branch was reached from; `None` for a commit only a detached HEAD holds.
    /// Commits absent from the map are on the integration branch.
    fn sources(
        &mut self,
        root: &Path,
        refs: &Refs,
        since: DateTime<Utc>,
    ) -> Option<HashMap<String, Option<String>>> {
        let detached = refs.current.is_none()
            && self.git.succeeds(
                root,
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"].map(String::from),
            );
        let output = self.git.run(
            root,
            &source_arguments(refs.integration.as_deref(), since, detached),
        )?;
        Some(
            output
                .lines()
                .filter_map(|line| {
                    let (sha, source) = line.split_once('\t')?;
                    // `--branches` makes Git name the source without
                    // `refs/heads/`; `HEAD` is the detached HEAD, which no
                    // branch may be called.
                    let name = source.strip_prefix("refs/heads/").unwrap_or(source);
                    let branch = (name != "HEAD" && valid_name(name)).then(|| name.to_string());
                    Some((sha.to_string(), branch))
                })
                .collect(),
        )
    }
}

/// Fills the branches the providers did not record and attributes every
/// commit. Runs after the Git scan, before anything is aggregated.
///
/// Sessions that recorded a branch are left alone. A failure is reported as a
/// warning and leaves the affected sessions and commits with no branch.
pub fn enrich(
    sessions: &mut [Session],
    commits: &mut [GitCommit],
    agent_commits: &mut [GitCommit],
    config: Option<&Value>,
    diagnostics: &mut Diagnostics,
) {
    // `collect_commits` already says when Git is missing; there is nothing
    // more to add for a report that has no Git to read.
    let Some(exe) = git_executable() else {
        return;
    };
    let names = integration_names(config).unwrap_or_else(|error| {
        diagnostics.warn(format!("{error:#}; using the default integration branches"));
        integration_names(None).unwrap_or_default()
    });
    enrich_with(
        exe,
        &names,
        Utc::now(),
        sessions,
        commits,
        agent_commits,
        diagnostics,
    );
}

/// `enrich` with the pieces named, and the moment the reflogs are read as of;
/// returns how many `git` processes ran.
fn enrich_with(
    exe: PathBuf,
    names: &[String],
    read_at: DateTime<Utc>,
    sessions: &mut [Session],
    commits: &mut [GitCommit],
    agent_commits: &mut [GitCommit],
    diagnostics: &mut Diagnostics,
) -> usize {
    let mut context = Context {
        git: Runner {
            exe,
            diagnostics,
            spawned: 0,
        },
        names,
        read_at,
        refs: HashMap::new(),
        reflogs: HashMap::new(),
        checkouts: HashMap::new(),
    };
    let mut commits: Vec<&mut GitCommit> = commits
        .iter_mut()
        .chain(agent_commits.iter_mut())
        .filter(|commit| commit.branch.is_none())
        .collect();

    // Each commit is looked up through the checkout it was found in.
    let mut groups: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (index, commit) in commits.iter().enumerate() {
        if let Some(checkout) = context.checkout(&commit.cwd) {
            groups.entry(checkout.root).or_default().push(index);
        }
    }

    for (root, indices) in &groups {
        let Some(refs) = context.refs(root) else {
            continue;
        };
        let since = indices
            .iter()
            .map(|index| commits[*index].timestamp)
            .min()
            .unwrap_or_default();
        let Some(sources) = context.sources(root, &refs, since) else {
            continue;
        };
        for index in indices {
            let commit = &mut *commits[*index];
            match sources.get(&commit.sha) {
                Some(Some(branch)) => {
                    commit.branch = Some(branch.clone());
                    commit.branch_source = BranchSource::Unique;
                }
                // Reachable only from a detached HEAD: no branch to name.
                Some(None) => {}
                // On the integration branch (merged, fast-forwarded, or the
                // feature is gone). It is reported there: which checkout made
                // the commit is not something the HEAD reflogs can say without
                // reading their commit entries, and guessing would make the
                // answer depend on directory order. The time spent on the
                // feature is still attributed through the sessions.
                None => {
                    if let Some(integration) = refs.integration.as_deref() {
                        commit.branch = Some(integration.to_string());
                        commit.branch_source = BranchSource::Integration;
                    }
                }
            }
        }
    }

    for session in sessions.iter_mut() {
        if !session.branches.is_empty() {
            continue;
        }
        let (Some(first), Some(last)) = (session.first_seen(), session.last_seen()) else {
            continue;
        };
        let Some(checkout) = context.checkout(&session.cwd) else {
            continue;
        };
        let (Some(refs), Some(reflog)) =
            (context.refs(&checkout.root), context.reflog(&checkout.root))
        else {
            continue;
        };
        let (marks, source) = session_marks(&reflog, refs.current.as_deref(), first, last);
        session.branches = marks;
        session.branch_source = source;
    }
    context.git.spawned
}

/// The integration branch of the repository at `repo`, by the same rule
/// `enrich` uses; `None` when there is none. For the branch report.
pub(crate) fn integration_branch(
    repo: &Path,
    config: Option<&Value>,
    diagnostics: &mut Diagnostics,
) -> Option<String> {
    let exe = git_executable()?;
    let names = integration_names(config).unwrap_or_else(|error| {
        diagnostics.warn(format!("{error:#}; using the default integration branches"));
        integration_names(None).unwrap_or_default()
    });
    let mut runner = Runner {
        exe,
        diagnostics,
        spawned: 0,
    };
    let output = runner.run(repo, &refs_arguments())?;
    parse_refs(&output, &names).integration
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use chrono::TimeZone;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::classify::CategoryTally;
    use crate::model::{ActivityPoint, Authorship};

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A real repository on `main`, driven at chosen times.
    struct Repo {
        _directory: TempDir,
        path: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            // Resolved, so macOS's `/var` symlink does not make the test's
            // paths differ from the ones Git and the checkout lookup report.
            let path = directory.path().canonicalize().unwrap().join("repo");
            fs::create_dir_all(&path).unwrap();
            let repo = Self {
                _directory: directory,
                path,
            };
            repo.run("2026-03-01T08:00:00Z", &["init", "-q", "-b", "main"]);
            repo
        }

        /// Runs `git` in `directory` as of `when`: the author and committer
        /// dates, and so the reflog's, are all that moment.
        fn git(&self, directory: &Path, when: &str, arguments: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(directory)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(arguments)
                .env("GIT_AUTHOR_DATE", when)
                .env("GIT_COMMITTER_DATE", when)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn run(&self, when: &str, arguments: &[&str]) -> String {
            self.git(&self.path, when, arguments)
        }

        /// An empty commit in `directory`; returns its SHA.
        fn commit_in(&self, directory: &Path, when: &str, message: &str) -> String {
            self.git(
                directory,
                when,
                &["commit", "-q", "--allow-empty", "-m", message],
            );
            self.git(directory, when, &["rev-parse", "HEAD"])
        }

        fn commit(&self, when: &str, message: &str) -> String {
            self.commit_in(&self.path, when, message)
        }
    }

    fn commit_record(cwd: &Path, sha: &str, when: &str) -> GitCommit {
        GitCommit {
            sha: sha.to_string(),
            timestamp: at(when),
            repo: "repo".to_string(),
            repo_id: "repo".to_string(),
            repo_member_id: "repo".to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            root: "repo".to_string(),
            additions: 1,
            deletions: 0,
            files: Vec::new(),
            ignored_additions: 0,
            ignored_deletions: 0,
            categories: CategoryTally::default(),
            authorship: Authorship::default(),
            branch: None,
            branch_source: BranchSource::None,
        }
    }

    fn session(cwd: &Path, from: &str, to: &str) -> Session {
        let point = |when: &str| ActivityPoint {
            timestamp: at(when),
            model: "m".to_string(),
        };
        Session {
            provider: "test".to_string(),
            session_id: "s".to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            repo: "repo".to_string(),
            repo_id: "repo".to_string(),
            root: "repo".to_string(),
            points: vec![point(from), point(to)],
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: false,
            source_file: PathBuf::new(),
            branches: Vec::new(),
            branch_source: BranchSource::None,
            pull_requests: Vec::new(),
        }
    }

    /// The moment the fixtures' reflogs are read: the day after they were made.
    fn read_time() -> DateTime<Utc> {
        at("2026-03-02T00:00:00Z")
    }

    fn defaults() -> Vec<String> {
        integration_names(None).unwrap()
    }

    /// Runs `enrich_with` over one human commit list and returns the number of
    /// `git` processes it spawned.
    fn enrich_commits(commits: &mut [GitCommit], sessions: &mut [Session]) -> usize {
        let mut diagnostics = Diagnostics::default();
        let spawned = enrich_with(
            git_executable().unwrap(),
            &defaults(),
            read_time(),
            sessions,
            commits,
            &mut [],
            &mut diagnostics,
        );
        assert!(
            diagnostics.messages.is_empty(),
            "{:?}",
            diagnostics.messages
        );
        spawned
    }

    #[test]
    fn integration_is_the_first_configured_name_that_exists_else_origin_head() {
        let listing = "*\trefs/heads/develop\t\n \trefs/heads/master\t\n \trefs/heads/feat/x\t\n";
        // The default order is main, master, trunk, develop.
        let refs = parse_refs(listing, &defaults());
        assert_eq!(Some("master"), refs.integration.as_deref());
        assert_eq!(Some("develop"), refs.current.as_deref());
        // The config's order wins.
        let refs = parse_refs(listing, &["develop".to_string(), "master".to_string()]);
        assert_eq!(Some("develop"), refs.integration.as_deref());

        // Failing every name, the remote's default is used if it exists locally.
        let listing = "*\trefs/heads/feat/x\t\n \trefs/heads/stable\t\n \
                       \trefs/remotes/origin/HEAD\trefs/remotes/origin/stable\n";
        let refs = parse_refs(listing, &defaults());
        assert_eq!(Some("stable"), refs.integration.as_deref());
        // ...but not if the remote's default was never checked out here.
        let listing =
            "*\trefs/heads/feat/x\t\n \trefs/remotes/origin/HEAD\trefs/remotes/origin/stable\n";
        assert_eq!(None, parse_refs(listing, &defaults()).integration);
        // A detached HEAD has no current branch.
        assert_eq!(
            None,
            parse_refs(" \trefs/heads/main\t\n", &defaults()).current
        );
    }

    #[test]
    fn the_branches_config_is_read_by_key() {
        let names = |value: Value| integration_names(Some(&value));
        assert_eq!(
            vec!["release"],
            names(json!({"integration": "release"})).unwrap()
        );
        assert_eq!(
            vec!["a", "b"],
            names(json!({"integration": ["a", "b"]})).unwrap()
        );
        assert_eq!(defaults(), names(json!({})).unwrap());
        for bad in [
            json!({"integration": 3}),
            json!({"integrations": ["a"]}),
            json!(7),
        ] {
            let error = format!("{:#}", names(bad).unwrap_err());
            assert!(error.contains("branches"), "{error}");
        }
    }

    #[test]
    fn reflog_destinations_that_are_not_branches_are_not_names() {
        for name in ["main", "feat/x", "ACME-1", "release/1.2"] {
            assert!(plausible_branch(name), "{name}");
        }
        for name in ["1a2b3c4", "HEAD", "HEAD~2", "a^b", "x..y", "bad name", ""] {
            assert!(!plausible_branch(name), "{name}");
        }
    }

    #[test]
    fn recorded_branch_names_reject_what_git_forbids_but_keep_digits_and_hex() {
        for name in [
            "main",
            "feat/x",
            "20260301",
            "1234567",
            "deadbeef",
            "4521873",
            "release/1.2",
        ] {
            assert!(recordable_branch(name), "{name}");
        }
        for name in [
            "HEAD",
            "@",
            "",
            "fix login",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[b",
            "a\\b",
            "x..y",
            "a@{1}",
            "-rf",
            "x.lock",
            "x/",
            "x.",
            "a\nb",
        ] {
            assert!(!recordable_branch(name), "{name:?}");
        }
        assert!(!recordable_branch(&"b".repeat(257)));
        // The reflog alone also refuses a bare commit id.
        assert!(!plausible_branch("deadbeef"));
        assert!(!plausible_branch("20260301"));
    }

    #[test]
    fn the_reflog_read_asks_git_to_filter_and_asks_for_no_subject() {
        let arguments = reflog_arguments();
        assert!(arguments.contains(&"--grep-reflog=^checkout: moving from ".to_string()));
        assert!(arguments.contains(&"-g".to_string()));
        assert!(arguments.contains(&"--format=%gd%x09%gs".to_string()));
        // No placeholder that carries a commit's own words or identity.
        let format = arguments
            .iter()
            .find(|a| a.starts_with("--format="))
            .unwrap();
        for placeholder in ["%s", "%b", "%B", "%an", "%ae", "%cn", "%ce"] {
            assert!(!format.contains(placeholder), "{placeholder}");
        }
        let sources = source_arguments(Some("main"), at("2026-03-01T00:00:00Z"), false).join(" ");
        assert!(sources.contains("--format=%H%x09%S"));
        assert!(sources.contains("--not refs/heads/main"));
    }

    #[test]
    fn git_delivers_only_branch_switches_and_never_a_commit_message() {
        let repo = Repo::new();
        repo.commit("2026-03-01T09:00:00Z", "SECRET client plan");
        // A message that imitates a switch is still a `commit:` entry.
        repo.commit(
            "2026-03-01T09:30:00Z",
            "checkout: moving from secret to leak",
        );
        repo.run("2026-03-01T10:00:00Z", &["checkout", "-q", "-b", "feat/x"]);

        // The very arguments the parser's input is read with.
        let arguments = reflog_arguments();
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = repo.run("2026-03-01T11:00:00Z", &arguments);
        assert!(!output.contains("SECRET"), "{output}");
        assert!(!output.contains("leak"), "{output}");
        assert!(!output.contains("commit"), "{output}");
        let reflog = parse_reflog(&output, read_time());
        assert_eq!(1, reflog.switches.len());
        assert_eq!(Some("feat/x"), reflog.switches[0].to.as_deref());
        assert_eq!(Some("main"), reflog.switches[0].from.as_deref());
    }

    #[test]
    fn a_feature_branch_commit_is_unique_and_integration_work_falls_back_to_main() {
        let repo = Repo::new();
        let base = repo.commit("2026-03-01T09:00:00Z", "base");
        repo.run("2026-03-01T10:00:00Z", &["checkout", "-q", "-b", "feat/x"]);
        let work = repo.commit("2026-03-01T11:00:00Z", "work");

        let mut commits = vec![
            commit_record(&repo.path, &base, "2026-03-01T09:00:00Z"),
            commit_record(&repo.path, &work, "2026-03-01T11:00:00Z"),
        ];
        let spawned = enrich_commits(&mut commits, &mut []);

        assert_eq!(Some("main"), commits[0].branch.as_deref());
        assert_eq!(BranchSource::Integration, commits[0].branch_source);
        assert_eq!(Some("feat/x"), commits[1].branch.as_deref());
        assert_eq!(BranchSource::Unique, commits[1].branch_source);
        assert!(spawned <= 3, "{spawned} git processes for one checkout");
    }

    #[test]
    fn a_merged_and_deleted_branch_reads_as_main_but_its_session_keeps_the_feature() {
        let repo = Repo::new();
        let base = repo.commit("2026-03-01T09:00:00Z", "base");
        repo.run(
            "2026-03-01T10:00:00Z",
            &["checkout", "-q", "-b", "feat/gone"],
        );
        let work = repo.commit("2026-03-01T11:00:00Z", "work");
        repo.run("2026-03-01T12:00:00Z", &["checkout", "-q", "main"]);
        repo.run(
            "2026-03-01T12:05:00Z",
            &["merge", "-q", "--no-ff", "feat/gone", "-m", "merge"],
        );
        repo.run("2026-03-01T12:10:00Z", &["branch", "-q", "-D", "feat/gone"]);

        let mut commits = vec![
            commit_record(&repo.path, &base, "2026-03-01T09:00:00Z"),
            commit_record(&repo.path, &work, "2026-03-01T11:00:00Z"),
        ];
        let mut sessions = vec![session(
            &repo.path,
            "2026-03-01T10:30:00Z",
            "2026-03-01T11:30:00Z",
        )];
        let spawned = enrich_commits(&mut commits, &mut sessions);

        // A commit already merged into the integration branch is reported on it.
        for commit in &commits {
            assert_eq!(Some("main"), commit.branch.as_deref());
            assert_eq!(BranchSource::Integration, commit.branch_source);
        }
        // The time spent on the feature is still attributed through the session.
        assert_eq!("feat/gone", sessions[0].branches[0].branch);
        assert_eq!(BranchSource::Reflog, sessions[0].branch_source);
        assert!(spawned <= 3, "{spawned}");
    }

    #[test]
    fn a_squash_merged_and_deleted_feature_reads_as_main() {
        let repo = Repo::new();
        repo.commit("2026-03-01T09:00:00Z", "base");
        repo.run(
            "2026-03-01T10:00:00Z",
            &["checkout", "-q", "-b", "feat/squashed"],
        );
        let one = repo.commit("2026-03-01T11:00:00Z", "one");
        let two = repo.commit("2026-03-01T11:30:00Z", "two");
        repo.run("2026-03-01T12:00:00Z", &["checkout", "-q", "main"]);
        repo.run(
            "2026-03-01T12:05:00Z",
            &["merge", "-q", "--squash", "feat/squashed"],
        );
        let squash = repo.commit("2026-03-01T12:06:00Z", "squash");
        repo.run(
            "2026-03-01T12:10:00Z",
            &["branch", "-q", "-D", "feat/squashed"],
        );

        // The original commits are on no branch any more.
        let mut commits = vec![
            commit_record(&repo.path, &one, "2026-03-01T11:00:00Z"),
            commit_record(&repo.path, &two, "2026-03-01T11:30:00Z"),
            commit_record(&repo.path, &squash, "2026-03-01T12:06:00Z"),
        ];
        enrich_commits(&mut commits, &mut []);
        for commit in &commits {
            assert_eq!(Some("main"), commit.branch.as_deref());
            assert_eq!(BranchSource::Integration, commit.branch_source);
        }
    }

    #[test]
    fn commits_from_parallel_worktrees_read_as_main_and_each_session_keeps_its_branch() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:30:00Z", "base");
        let first = repo.path.with_file_name("first");
        let second = repo.path.with_file_name("second");
        for worktree in [&first, &second] {
            repo.run(
                "2026-03-01T09:00:00Z",
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    worktree.to_str().unwrap(),
                ],
            );
        }
        repo.git(
            &first,
            "2026-03-01T09:05:00Z",
            &["checkout", "-q", "-b", "feat/a"],
        );
        repo.git(
            &second,
            "2026-03-01T09:05:00Z",
            &["checkout", "-q", "-b", "feat/b"],
        );
        let a = repo.commit_in(&first, "2026-03-01T10:00:00Z", "a");
        let b = repo.commit_in(&second, "2026-03-01T10:30:00Z", "b");
        // Both land on main, and their branches are deleted once the
        // worktrees let go of them.
        repo.run(
            "2026-03-01T11:00:00Z",
            &["merge", "-q", "--no-ff", "feat/a", "-m", "ma"],
        );
        repo.run(
            "2026-03-01T11:01:00Z",
            &["merge", "-q", "--no-ff", "feat/b", "-m", "mb"],
        );
        repo.git(
            &first,
            "2026-03-01T11:02:00Z",
            &["checkout", "-q", "--detach"],
        );
        repo.git(
            &second,
            "2026-03-01T11:02:00Z",
            &["checkout", "-q", "--detach"],
        );
        repo.run(
            "2026-03-01T11:03:00Z",
            &["branch", "-q", "-D", "feat/a", "feat/b"],
        );

        // The commits are found from the main checkout only, as `git.rs` does.
        let mut commits = vec![
            commit_record(&repo.path, &a, "2026-03-01T10:00:00Z"),
            commit_record(&repo.path, &b, "2026-03-01T10:30:00Z"),
        ];
        // The worktrees are known to the run through a session in each.
        let mut sessions = vec![
            session(&first, "2026-03-01T10:00:00Z", "2026-03-01T10:10:00Z"),
            session(&second, "2026-03-01T10:00:00Z", "2026-03-01T10:10:00Z"),
        ];
        enrich_commits(&mut commits, &mut sessions);

        for commit in &commits {
            assert_eq!(Some("main"), commit.branch.as_deref());
            assert_eq!(BranchSource::Integration, commit.branch_source);
        }
        // Each session sees only its own checkout's switches.
        assert_eq!("feat/a", sessions[0].branches[0].branch);
        assert_eq!("feat/b", sessions[1].branches[0].branch);
        assert_eq!(BranchSource::Reflog, sessions[0].branch_source);
    }

    #[test]
    fn a_worktree_session_gets_its_own_branch_and_one_checkout_is_not_ambiguous() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:30:00Z", "base");
        let worktree = repo.path.with_file_name("wt");
        repo.run(
            "2026-03-01T09:00:00Z",
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/wt",
                worktree.to_str().unwrap(),
            ],
        );
        let work = repo.commit_in(&worktree, "2026-03-01T10:00:00Z", "work");
        repo.run(
            "2026-03-01T11:00:00Z",
            &["merge", "-q", "--ff-only", "feat/wt"],
        );

        // The main checkout never left main; the worktree has no switch on
        // record, so its answer is the branch it has out now.
        let mut sessions = vec![
            session(&worktree, "2026-03-01T10:00:00Z", "2026-03-01T10:10:00Z"),
            session(&repo.path, "2026-03-01T10:00:00Z", "2026-03-01T10:10:00Z"),
        ];
        let mut commits = vec![commit_record(&worktree, &work, "2026-03-01T10:00:00Z")];
        enrich_commits(&mut commits, &mut sessions);

        assert_eq!("feat/wt", sessions[0].branches[0].branch);
        assert_eq!(BranchSource::Head, sessions[0].branch_source);
        assert_eq!(None, sessions[0].branches[0].from);
        assert_eq!("main", sessions[1].branches[0].branch);
        // The commit is on main now, so it is not unique and reads as main.
        assert_eq!(Some("main"), commits[0].branch.as_deref());
        assert_eq!(BranchSource::Integration, commits[0].branch_source);
    }

    #[test]
    fn a_commit_made_in_a_feature_worktree_and_fast_forwarded_into_main_reads_as_main() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:30:00Z", "base");
        let worktree = repo.path.with_file_name("wt");
        repo.run(
            "2026-03-01T09:00:00Z",
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/wt",
                worktree.to_str().unwrap(),
            ],
        );
        let work = repo.commit_in(&worktree, "2026-03-01T10:00:00Z", "work");
        repo.run(
            "2026-03-01T11:00:00Z",
            &["merge", "-q", "--ff-only", "feat/wt"],
        );

        // Found through either checkout, the answer is the same.
        let mut commits = vec![
            commit_record(&repo.path, &work, "2026-03-01T10:00:00Z"),
            commit_record(&worktree, &work, "2026-03-01T10:00:00Z"),
        ];
        enrich_commits(&mut commits, &mut []);
        for commit in &commits {
            assert_eq!(Some("main"), commit.branch.as_deref());
            assert_eq!(BranchSource::Integration, commit.branch_source);
        }
    }

    #[test]
    fn a_commit_made_while_its_own_checkout_was_on_main_stays_on_main() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:30:00Z", "base");
        let worktree = repo.path.with_file_name("feature");
        repo.run(
            "2026-03-01T09:00:00Z",
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                worktree.to_str().unwrap(),
            ],
        );
        repo.git(
            &worktree,
            "2026-03-01T09:05:00Z",
            &["checkout", "-q", "-b", "feat/x"],
        );
        // A hotfix straight on main while the worktree has a feature out.
        let hotfix = repo.commit("2026-03-01T10:00:00Z", "hotfix");
        let mut commits = vec![commit_record(&repo.path, &hotfix, "2026-03-01T10:00:00Z")];
        // The worktree is known to the run through a session in it.
        let mut sessions = vec![session(
            &worktree,
            "2026-03-01T10:00:00Z",
            "2026-03-01T10:10:00Z",
        )];
        enrich_commits(&mut commits, &mut sessions);
        assert_eq!(Some("main"), commits[0].branch.as_deref());
        assert_eq!(BranchSource::Integration, commits[0].branch_source);
    }

    #[test]
    fn the_look_back_is_measured_from_the_read_time_not_from_the_oldest_switch() {
        let read_at = Utc.timestamp_opt(400 * 86_400, 0).unwrap();
        let reflog_with = |switch_at: DateTime<Utc>| Reflog {
            switches: vec![Switch {
                at: switch_at,
                from: Some("feat/old".to_string()),
                to: Some("main".to_string()),
            }],
            read_at,
        };

        // Oldest kept switch is recent: before it, the branch it left is
        // believed for 30 days back from the read time, and no further, even
        // though that is well inside 90 days of the switch.
        let recent = read_at - Duration::days(10);
        let reflog = reflog_with(recent);
        assert_eq!(
            Some("feat/old"),
            reflog.branch_at(read_at - Duration::days(30))
        );
        assert_eq!(
            Some("feat/old"),
            reflog.branch_at(recent - Duration::seconds(1))
        );
        assert_eq!(
            None,
            reflog.branch_at(read_at - UNREACHABLE_EXPIRY - Duration::seconds(1))
        );
        // The reviewer's case: between switch - 90 days and read time - 30 days.
        assert_eq!(None, reflog.branch_at(recent - Duration::days(60)));
        assert_eq!(Some("main"), reflog.branch_at(recent));

        // An old oldest switch (gc keeps reflogs that long): everything before
        // it is older than the look-back, so there is no first-branch guess.
        let old = read_at - Duration::days(100);
        let reflog = reflog_with(old);
        assert_eq!(None, reflog.branch_at(old - Duration::days(1)));
        assert_eq!(Some("main"), reflog.branch_at(old));

        // So a session that old gets no marks from the reflog.
        let (marks, source) = session_marks(
            &reflog_with(recent),
            Some("main"),
            recent - Duration::days(60),
            recent - Duration::days(59),
        );
        assert!(marks.is_empty());
        assert_eq!(BranchSource::None, source);
    }

    #[test]
    fn a_session_that_crosses_a_switch_gets_one_mark_at_the_switch() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:00:00Z", "base");
        repo.run(
            "2026-03-01T10:00:00Z",
            &["checkout", "-q", "-b", "feat/one"],
        );
        repo.run("2026-03-01T12:00:00Z", &["checkout", "-q", "main"]);

        let mut sessions = vec![
            // Before, across and after the switches.
            session(&repo.path, "2026-03-01T09:00:00Z", "2026-03-01T13:00:00Z"),
            // Entirely after the last switch: the branch that is out now.
            session(&repo.path, "2026-03-01T14:00:00Z", "2026-03-01T15:00:00Z"),
        ];
        enrich_commits(&mut [], &mut sessions);

        let marks: Vec<_> = sessions[0]
            .branches
            .iter()
            .map(|mark| (mark.from, mark.branch.as_str()))
            .collect();
        assert_eq!(
            vec![
                (None, "main"),
                (Some(at("2026-03-01T10:00:00Z")), "feat/one"),
                (Some(at("2026-03-01T12:00:00Z")), "main"),
            ],
            marks
        );
        assert_eq!(BranchSource::Reflog, sessions[0].branch_source);
        assert_eq!(
            Some("feat/one"),
            sessions[0].branch_at(at("2026-03-01T11:00:00Z"))
        );
        assert_eq!(1, sessions[1].branches.len());
        assert_eq!("main", sessions[1].branches[0].branch);
        assert_eq!(BranchSource::Head, sessions[1].branch_source);
    }

    #[test]
    fn a_recorded_branch_is_never_replaced_and_a_non_repository_is_skipped() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:00:00Z", "base");
        let mut recorded = session(&repo.path, "2026-03-01T09:00:00Z", "2026-03-01T10:00:00Z");
        recorded.branches = vec![BranchMark {
            from: None,
            branch: "provider/said".to_string(),
        }];
        recorded.branch_source = BranchSource::Recorded;
        let elsewhere = tempfile::tempdir().unwrap();
        let mut sessions = vec![
            recorded,
            session(
                elsewhere.path(),
                "2026-03-01T09:00:00Z",
                "2026-03-01T10:00:00Z",
            ),
            session(
                &repo.path.with_file_name("gone"),
                "2026-03-01T09:00:00Z",
                "2026-03-01T10:00:00Z",
            ),
        ];
        let spawned = enrich_commits(&mut [], &mut sessions);

        assert_eq!("provider/said", sessions[0].branches[0].branch);
        assert_eq!(BranchSource::Recorded, sessions[0].branch_source);
        assert!(sessions[1].branches.is_empty());
        assert!(sessions[2].branches.is_empty());
        assert_eq!(0, spawned, "nothing to look up, so no git process");
    }

    #[test]
    fn commits_only_on_a_detached_head_are_left_without_a_branch() {
        let repo = Repo::new();
        repo.commit("2026-03-01T08:00:00Z", "base");
        repo.run("2026-03-01T09:00:00Z", &["checkout", "-q", "--detach"]);
        let loose = repo.commit("2026-03-01T10:00:00Z", "loose");

        let mut commits = vec![commit_record(&repo.path, &loose, "2026-03-01T10:00:00Z")];
        enrich_commits(&mut commits, &mut []);
        assert_eq!(None, commits[0].branch);
        assert_eq!(BranchSource::None, commits[0].branch_source);
    }

    #[test]
    fn with_no_integration_branch_every_branch_commit_is_unique_and_none_falls_back() {
        let repo = Repo::new();
        repo.run("2026-03-01T08:00:00Z", &["checkout", "-q", "-b", "odd"]);
        let only = repo.commit("2026-03-01T09:00:00Z", "only");
        let mut commits = vec![commit_record(&repo.path, &only, "2026-03-01T09:00:00Z")];
        enrich_commits(&mut commits, &mut []);
        assert_eq!(Some("odd"), commits[0].branch.as_deref());
        assert_eq!(BranchSource::Unique, commits[0].branch_source);
    }

    #[test]
    fn marks_are_capped_and_a_session_without_time_is_left_alone() {
        let switches: Vec<Switch> = (0..400)
            .map(|index| Switch {
                at: Utc.timestamp_opt(1_000 + index, 0).unwrap(),
                from: Some(format!("b{}", index % 2)),
                to: Some(format!("b{}", (index + 1) % 2)),
            })
            .collect();
        let reflog = Reflog {
            switches,
            read_at: Utc.timestamp_opt(5_000, 0).unwrap(),
        };
        let (marks, source) = session_marks(
            &reflog,
            Some("b0"),
            Utc.timestamp_opt(900, 0).unwrap(),
            Utc.timestamp_opt(5_000, 0).unwrap(),
        );
        assert_eq!(MAXIMUM_MARKS, marks.len());
        assert_eq!(BranchSource::Reflog, source);
        assert!(
            marks
                .windows(2)
                .all(|pair| pair[0].branch != pair[1].branch)
        );

        let (marks, source) = session_marks(
            &Reflog::default(),
            None,
            Utc.timestamp_opt(0, 0).unwrap(),
            Utc.timestamp_opt(1, 0).unwrap(),
        );
        assert!(marks.is_empty());
        assert_eq!(BranchSource::None, source);
    }

    #[test]
    fn an_empty_repository_is_quiet() {
        let repo = Repo::new();
        let mut sessions = vec![session(
            &repo.path,
            "2026-03-01T09:00:00Z",
            "2026-03-01T10:00:00Z",
        )];
        // `Repo::new` made no commit: HEAD is unborn.
        enrich_commits(&mut [], &mut sessions);
        assert!(sessions[0].branches.is_empty());
    }
}
