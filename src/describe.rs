//! Opt-in descriptions for timesheet entries and branch reports: commit
//! subjects, tool-generated session titles, and an external summarizer. They
//! are read only when asked for, never cached, and never put in bundles; see
//! `docs/privacy.md` for the boundary.
//!
//! * `--describe commits` runs one extra `git log --no-walk --stdin
//!   --format=%H%x09%s` per repository over the SHAs of the user's own commits.
//!   Subjects only, bounded, sanitised. A commit an agent authored is never
//!   asked about: only `GitCommit::human_signal` (which is `None` for those)
//!   admits a commit here.
//! * `--describe sessions[=PROVIDERS]` reads titles through
//!   `ai::titles::read_titles`.
//! * `--summarize-with CMD` hands a digest to a command the user chose.
//!
//! The commands that describe things call `for_timesheet` (the timesheet) or
//! `for_branch` (a branch or pull-request report); both go through the same
//! `Catalog`, so a subject is read once however many entries mention it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{Local, NaiveDate};
use serde::Serialize;

use crate::ai::titles::{self, DEFAULT_PROVIDERS, SessionKey, TITLE_PROVIDERS};
use crate::cli::ReportArguments;
use crate::engagement;
use crate::git::git_executable;
use crate::model::{GitCommit, HumanSignal, Session};
use crate::output::safe_text;
use crate::report::Collected;
use crate::sources::{default_codex_database, normalize_provider};
use crate::timesheet::compute::{Key, entry_key};
use crate::timesheet::model::{EntryStatus, Timesheet, TimesheetEntry, TimesheetSettings};

/// A subject is a label, not a message.
pub(crate) const MAX_SUBJECT_CHARS: usize = 200;
/// What a summarizer's answer may be, after it is made one safe line.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 500;
/// How much of a summarizer's output is read at all.
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
/// The most subjects and titles one digest carries; the counts say how many
/// there were.
const MAX_DIGEST_SUBJECTS: usize = 50;
const MAX_DIGEST_TITLES: usize = 20;
/// The default for `--summarize-timeout`.
pub(crate) const DEFAULT_SUMMARIZE_TIMEOUT_SECONDS: u64 = 60;

/// What the user asked to have described, validated before anything is read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) commits: bool,
    /// The providers whose titles are read; empty means `sessions` was not
    /// asked for.
    pub(crate) session_providers: BTreeSet<String>,
    pub(crate) summarize_with: Option<String>,
    pub(crate) timeout: Duration,
    /// Print the digests and run nothing.
    pub(crate) digest_only: bool,
}

impl Plan {
    /// Reads `--describe` values, `--summarize-with`, the timeout in seconds
    /// and `--digest`. `commits`, `sessions` and `sessions=A+B` are accepted
    /// (a `+`, not a comma, separates providers, because the flag itself is
    /// comma-separated); repeating `sessions=...` adds providers. A bare
    /// `sessions` is every provider with a title source except Codex.
    pub(crate) fn parse(
        describe: &[String],
        summarize_with: Option<&str>,
        timeout_seconds: Option<u64>,
        digest: bool,
    ) -> Result<Self> {
        let mut plan = Self {
            timeout: Duration::from_secs(
                timeout_seconds.unwrap_or(DEFAULT_SUMMARIZE_TIMEOUT_SECONDS),
            ),
            digest_only: digest,
            ..Self::default()
        };
        if timeout_seconds == Some(0) {
            bail!("--summarize-timeout must be more than zero");
        }
        for token in describe {
            let token = token.trim().to_ascii_lowercase();
            let (name, providers) = match token.split_once('=') {
                Some((name, providers)) => (name, Some(providers)),
                None => (token.as_str(), None),
            };
            match (name, providers) {
                ("commits", None) => plan.commits = true,
                ("commits", Some(_)) => bail!("--describe commits takes no providers"),
                ("sessions", None) => plan
                    .session_providers
                    .extend(DEFAULT_PROVIDERS.iter().map(|name| name.to_string())),
                ("sessions", Some(providers)) => {
                    let mut any = false;
                    for provider in providers.split('+').filter(|name| !name.is_empty()) {
                        let provider = normalize_provider(provider);
                        if !TITLE_PROVIDERS.contains(&provider.as_str()) {
                            bail!(
                                "no session titles are read for provider {provider:?}; --describe sessions=PROVIDERS accepts {}",
                                TITLE_PROVIDERS.join(", ")
                            );
                        }
                        plan.session_providers.insert(provider);
                        any = true;
                    }
                    if !any {
                        bail!(
                            "--describe sessions= needs at least one provider, separated by +, for example sessions=claude+pi"
                        );
                    }
                }
                _ => bail!(
                    "unknown --describe source {token:?}; use commits, sessions or sessions=PROVIDERS"
                ),
            }
        }
        if let Some(command) = summarize_with {
            if command.trim().is_empty() {
                bail!("--summarize-with needs a command");
            }
            plan.summarize_with = Some(command.to_string());
        }
        Ok(plan)
    }

    /// Whether anything is to be read or run.
    pub(crate) fn is_active(&self) -> bool {
        self.commits
            || !self.session_providers.is_empty()
            || self.summarize_with.is_some()
            || self.digest_only
    }

    fn sessions(&self) -> bool {
        !self.session_providers.is_empty()
    }
}

/// Run-level facts the readers need that a `Session` or `GitCommit` does not
/// carry.
#[derive(Clone, Debug, Default)]
pub(crate) struct Context {
    pub(crate) codex_db: Option<PathBuf>,
}

impl Context {
    pub(crate) fn from_report(arguments: &ReportArguments) -> Self {
        Self {
            codex_db: Some(
                arguments
                    .codex_db
                    .clone()
                    .unwrap_or_else(default_codex_database),
            ),
        }
    }
}

/// The text found for one entry or branch, before it is joined or summarised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Material {
    pub(crate) commit_subjects: Vec<String>,
    pub(crate) session_titles: Vec<String>,
}

impl Material {
    /// What `--describe` alone puts in the description column: titles, then
    /// subjects, on one line. `None` when nothing was found.
    pub(crate) fn text(&self) -> Option<String> {
        let parts: Vec<&str> = self
            .session_titles
            .iter()
            .chain(&self.commit_subjects)
            .map(String::as_str)
            .collect();
        (!parts.is_empty()).then(|| bounded_line(&parts.join("; "), MAX_DESCRIPTION_CHARS))
    }
}

/// One line, control and direction characters replaced, at most `limit`
/// characters (the last one `…` when it was cut).
fn bounded_line(text: &str, limit: usize) -> String {
    let line = safe_text(&text.split_whitespace().collect::<Vec<_>>().join(" "));
    if line.chars().count() <= limit {
        return line;
    }
    let mut cut: String = line.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Everything read for a set of commits and sessions: subjects by commit,
/// titles by session. Built once, queried per entry.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    subjects: HashMap<(String, String), String>,
    titles: HashMap<SessionKey, String>,
}

impl Catalog {
    /// Reads what `plan` asks for over these commits and sessions. A source
    /// that cannot be read adds a line to `warnings` and costs its own text.
    pub(crate) fn load(
        plan: &Plan,
        context: &Context,
        commits: &[&GitCommit],
        sessions: &[&Session],
        warnings: &mut Vec<String>,
    ) -> Self {
        let mut catalog = Self::default();
        if plan.commits {
            catalog.read_subjects(commits, warnings);
        }
        if plan.sessions() {
            catalog.titles = titles::read_titles(
                sessions,
                &plan.session_providers,
                context.codex_db.as_deref(),
                warnings,
            );
        }
        catalog
    }

    fn read_subjects(&mut self, commits: &[&GitCommit], warnings: &mut Vec<String>) {
        // Only commits the user wrote: `human_signal` is `None` for a commit
        // an agent authored, so such a commit's SHA never reaches Git here.
        let mut by_repository: BTreeMap<&str, Vec<&GitCommit>> = BTreeMap::new();
        for commit in commits {
            if commit.human_signal().is_some() {
                by_repository.entry(&commit.cwd).or_default().push(commit);
            }
        }
        if by_repository.is_empty() {
            return;
        }
        let Some(git) = git_executable() else {
            warnings.push("--describe commits: Git was not found; no commit subjects".to_string());
            return;
        };
        let mut absent = 0;
        for (cwd, commits) in by_repository {
            if !Path::new(cwd).is_dir() {
                absent += commits.len();
                continue;
            }
            let shas: Vec<&str> = commits.iter().map(|commit| commit.sha.as_str()).collect();
            match read_subjects(&git, Path::new(cwd), &shas) {
                Ok(found) => {
                    for commit in commits {
                        if let Some(subject) = found.get(&commit.sha) {
                            self.subjects.insert(
                                (commit.repo_member_id.clone(), commit.sha.clone()),
                                subject.clone(),
                            );
                        }
                    }
                }
                Err(reason) => warnings.push(format!(
                    "--describe commits: no subjects for {cwd}: {reason}"
                )),
            }
        }
        if absent > 0 {
            warnings.push(format!(
                "--describe commits: {absent} commit(s) belong to repositories that are not on this machine (imported bundles); no subjects for them"
            ));
        }
    }

    /// The subjects and titles for these commits and sessions, in the order
    /// given and without repeats.
    pub(crate) fn material(&self, commits: &[&GitCommit], sessions: &[&Session]) -> Material {
        let mut seen = HashSet::new();
        let commit_subjects = commits
            .iter()
            .filter_map(|commit| {
                self.subjects
                    .get(&(commit.repo_member_id.clone(), commit.sha.clone()))
            })
            .filter(|subject| seen.insert(subject.as_str()))
            .cloned()
            .collect();
        let mut seen = HashSet::new();
        let session_titles = sessions
            .iter()
            .filter_map(|session| {
                self.titles
                    .get(&(session.provider.clone(), session.session_id.clone()))
            })
            .filter(|title| seen.insert(title.as_str()))
            .cloned()
            .collect();
        Material {
            commit_subjects,
            session_titles,
        }
    }
}

/// The subjects of `shas` in one repository: a single `git log --no-walk
/// --stdin` pass printing the hash and the subject, nothing else. A SHA that
/// is not hexadecimal is not passed (a line starting with `-` would be read
/// as an option).
fn read_subjects(
    git: &Path,
    repository: &Path,
    shas: &[&str],
) -> std::result::Result<HashMap<String, String>, String> {
    let valid: Vec<&str> = shas
        .iter()
        .copied()
        .filter(|sha| (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()))
        .collect();
    if valid.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new(git)
        .arg("--no-pager")
        .arg("-C")
        .arg(repository)
        .args(["log", "--no-walk", "--stdin", "--format=%H%x09%s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    let input = valid.join("\n") + "\n";
    // Written off this thread: Git may answer before it has read everything.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let _ = writer.join();
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        return Err(bounded_line(reason.trim(), 200));
    }
    let mut found = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((sha, subject)) = line.split_once('\t') else {
            continue;
        };
        if !valid.contains(&sha) {
            continue;
        }
        // Sanitised before it is bounded, so a cut cannot split a replacement.
        let subject = bounded_line(subject, MAX_SUBJECT_CHARS);
        if !subject.is_empty() {
            found.insert(sha.to_string(), subject);
        }
    }
    Ok(found)
}

/// The text of a branch or pull request's description: the subjects of its
/// commits and the titles of its sessions. This is the call `branch` and `pr`
/// make; they pass the human commits and the sessions that fall on the branch,
/// in the order they want them listed. Reads nothing the plan did not ask for.
pub(crate) fn for_branch(
    plan: &Plan,
    context: &Context,
    commits: &[&GitCommit],
    sessions: &[&Session],
    warnings: &mut Vec<String>,
) -> Material {
    Catalog::load(plan, context, commits, sessions, warnings).material(commits, sessions)
}

/// What is handed to `--summarize-with` on standard input, and what `--digest`
/// prints: names and counts, plus subjects and titles only when asked for.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Digest {
    pub(crate) version: u32,
    pub(crate) date: String,
    pub(crate) engagement: String,
    /// The `--detail` key of the entry, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    pub(crate) hours: f64,
    pub(crate) repos: Vec<String>,
    pub(crate) branches: Vec<String>,
    pub(crate) issues: Vec<String>,
    pub(crate) counts: DigestCounts,
    /// Present only with `--describe commits`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) commit_subjects: Option<Vec<String>>,
    /// Present only with `--describe sessions`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_titles: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct DigestCounts {
    pub(crate) prompts: usize,
    pub(crate) commits: usize,
    pub(crate) sessions: usize,
}

impl Digest {
    /// The subjects and titles are attached only for the sources the plan
    /// asked for, and capped.
    pub(crate) fn with_material(mut self, plan: &Plan, material: &Material) -> Self {
        if plan.commits {
            self.commit_subjects = Some(
                material
                    .commit_subjects
                    .iter()
                    .take(MAX_DIGEST_SUBJECTS)
                    .cloned()
                    .collect(),
            );
        }
        if plan.sessions() {
            self.session_titles = Some(
                material
                    .session_titles
                    .iter()
                    .take(MAX_DIGEST_TITLES)
                    .cloned()
                    .collect(),
            );
        }
        self
    }
}

/// Fills the description of every shown timesheet entry that has activity
/// behind it, and returns each entry's digest (what `--digest` prints).
/// Entries that are locked (their description is the snapshot's) or manual
/// (no activity) are left alone, as are entries `shown` rejects: nothing is
/// read or summarised for a row the output will not list.
///
/// A failing summarizer is a warning on the timesheet and leaves that entry
/// without a description; the others proceed.
pub(crate) fn for_timesheet(
    plan: &Plan,
    context: &Context,
    collected: &Collected,
    settings: &TimesheetSettings,
    timesheet: &mut Timesheet,
    shown: impl Fn(&TimesheetEntry) -> bool,
) -> Vec<Digest> {
    if !plan.is_active() {
        return Vec::new();
    }
    let engagements = engagement::active();
    let mut index: HashMap<(NaiveDate, Key), usize> = HashMap::new();
    for (position, entry) in timesheet.entries.iter().enumerate() {
        if !matches!(entry.status, EntryStatus::Locked | EntryStatus::Manual) && shown(entry) {
            index.insert(
                (entry.date, (entry.engagement.clone(), entry.detail.clone())),
                position,
            );
        }
    }
    if index.is_empty() {
        return Vec::new();
    }
    let place = |signal: &HumanSignal| {
        let key = entry_key(
            engagements,
            settings.detail,
            &signal.repo_id,
            &signal.cwd,
            signal.branch.as_deref(),
            &signal.repo,
        );
        let date = signal.timestamp.with_timezone(&Local).date_naive();
        index.get(&(date, key)).copied()
    };

    let sessions_by_key: HashMap<SessionKey, &Session> = collected
        .sessions
        .iter()
        .rev()
        .map(|session| {
            (
                (session.provider.clone(), session.session_id.clone()),
                session,
            )
        })
        .collect();
    let mut session_keys: Vec<BTreeSet<SessionKey>> =
        vec![BTreeSet::new(); timesheet.entries.len()];
    for signal in &collected.timeline.human_signals {
        if signal.kind == "commit" {
            continue;
        }
        if let Some(position) = place(signal) {
            session_keys[position].insert((signal.provider.clone(), signal.session_id.clone()));
        }
    }
    let mut commit_indexes: Vec<Vec<usize>> = vec![Vec::new(); timesheet.entries.len()];
    for (position, commit) in collected.commits.iter().enumerate() {
        if let Some(signal) = commit.human_signal()
            && let Some(entry) = place(&signal)
        {
            commit_indexes[entry].push(position);
        }
    }
    for indexes in &mut commit_indexes {
        indexes.sort_by_key(|position| collected.commits[*position].timestamp);
    }

    let wanted_sessions: Vec<&Session> = session_keys
        .iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|key| sessions_by_key.get(key).copied())
        .collect();
    let wanted_commits: Vec<&GitCommit> = commit_indexes
        .iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|position| &collected.commits[*position])
        .collect();
    let mut warnings = Vec::new();
    let catalog = Catalog::load(
        plan,
        context,
        &wanted_commits,
        &wanted_sessions,
        &mut warnings,
    );

    let mut positions: Vec<usize> = index.values().copied().collect();
    positions.sort_unstable();
    let mut digests = Vec::new();
    let mut summaries = Vec::new();
    for position in positions {
        let commits: Vec<&GitCommit> = commit_indexes[position]
            .iter()
            .map(|index| &collected.commits[*index])
            .collect();
        let sessions: Vec<&Session> = session_keys[position]
            .iter()
            .filter_map(|key| sessions_by_key.get(key).copied())
            .collect();
        let material = catalog.material(&commits, &sessions);
        let entry = &timesheet.entries[position];
        let digest = Digest {
            version: 1,
            date: entry.date.to_string(),
            engagement: entry.engagement.clone(),
            detail: entry.detail.clone(),
            hours: (entry.final_seconds as f64 / 3600.0 * 100.0).round() / 100.0,
            repos: entry.evidence.repos.clone(),
            branches: entry.evidence.branches.clone(),
            issues: entry.evidence.issues.clone(),
            counts: DigestCounts {
                prompts: entry.evidence.prompts,
                commits: entry.evidence.commits,
                sessions: entry.evidence.sessions,
            },
            commit_subjects: None,
            session_titles: None,
        }
        .with_material(plan, &material);
        if !plan.digest_only && plan.summarize_with.is_none() {
            summaries.push((position, material.text()));
        }
        digests.push((position, digest));
    }

    if let Some(command) = plan.summarize_with.as_deref().filter(|_| !plan.digest_only) {
        eprintln!(
            "workstats: running --summarize-with for {} entr{} (up to {}s each)",
            digests.len(),
            if digests.len() == 1 { "y" } else { "ies" },
            plan.timeout.as_secs()
        );
        for (position, digest) in &digests {
            match summarize(command, plan.timeout, digest) {
                Ok(text) => summaries.push((*position, Some(text))),
                Err(reason) => warnings.push(format!(
                    "--summarize-with produced no description for {} {}: {reason}",
                    digest.date, digest.engagement
                )),
            }
        }
    }
    for (position, text) in summaries {
        timesheet.entries[position].description = text;
    }
    timesheet.warnings.extend(warnings);
    digests.into_iter().map(|(_, digest)| digest).collect()
}

/// Runs the summarizer for one digest and returns its description: the first
/// 500 characters of standard output, on one line, made safe. An error says
/// why there is none.
pub(crate) fn summarize(
    command: &str,
    timeout: Duration,
    digest: &Digest,
) -> std::result::Result<String, String> {
    let input = serde_json::to_vec(digest).map_err(|error| error.to_string())?;
    let (output, error_output) = run_command(command, timeout, input)?;
    let description = bounded_line(&String::from_utf8_lossy(&output), MAX_DESCRIPTION_CHARS);
    if description.is_empty() {
        let detail = bounded_line(&String::from_utf8_lossy(&error_output), 200);
        return Err(if detail.is_empty() {
            "the command printed nothing".to_string()
        } else {
            format!("the command printed nothing; standard error said: {detail}")
        });
    }
    Ok(description)
}

fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut shell = Command::new("cmd");
        // Raw, so cmd sees the text as typed rather than re-quoted.
        shell.arg("/C").raw_arg(command);
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    }
}

/// Reads at most `MAX_OUTPUT_BYTES` of a pipe and then drains the rest, so a
/// chatty command is never blocked on a full pipe.
fn collect_pipe(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut pipe).take(MAX_OUTPUT_BYTES).read_to_end(&mut kept);
        let _ = sender.send(kept);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    });
    receiver
}

/// Stops the command and anything it started. On Unix the command leads its
/// own process group, so the whole group is signalled.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -KILL -- -{}", child.id()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Runs `command` through the platform shell with `input` on standard input
/// and the user's environment, and returns what it printed on standard output
/// and error. Non-zero exit and timeout are errors.
fn run_command(
    command: &str,
    timeout: Duration,
    input: Vec<u8>,
) -> std::result::Result<(Vec<u8>, Vec<u8>), String> {
    let mut process = shell(command);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not start the command: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    // Written off this thread, and closed when done; a command that never
    // reads its input is not a reason to hang.
    thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stdout = collect_pipe(child.stdout.take().ok_or("no standard output")?);
    let stderr = collect_pipe(child.stderr.take().ok_or("no standard error")?);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("could not wait for the command: {error}")),
        }
    };
    // A grandchild that kept the pipes open must not hold the run up.
    let grace = Duration::from_secs(2);
    let output = stdout.recv_timeout(grace).unwrap_or_default();
    let error_output = stderr.recv_timeout(grace).unwrap_or_default();
    if !status.success() {
        let detail = bounded_line(&String::from_utf8_lossy(&error_output), 200);
        return Err(if detail.is_empty() {
            format!("the command failed ({status})")
        } else {
            format!("the command failed ({status}): {detail}")
        });
    }
    Ok((output, error_output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(describe: &[&str]) -> Result<Plan> {
        let values: Vec<String> = describe.iter().map(ToString::to_string).collect();
        Plan::parse(&values, None, None, false)
    }

    #[test]
    fn describe_sources_parse_and_codex_needs_naming() {
        let plan = parse(&["commits", "sessions"]).unwrap();
        assert!(plan.commits);
        assert!(plan.session_providers.contains("claude"));
        assert!(!plan.session_providers.contains("codex"));

        let codex = parse(&["sessions=codex"]).unwrap();
        assert!(!codex.commits);
        assert_eq!(
            BTreeSet::from(["codex".to_string()]),
            codex.session_providers
        );

        let several = parse(&["sessions=claude+pi", "sessions=codex"]).unwrap();
        assert_eq!(3, several.session_providers.len());
        assert!(!parse(&[]).unwrap().is_active());
    }

    #[test]
    fn bad_describe_values_are_refused_by_name() {
        for bad in ["prompts", "sessions=gemini", "sessions=", "commits=x"] {
            assert!(parse(&[bad]).is_err(), "{bad} should be refused");
        }
        let message = parse(&["sessions=nope"]).unwrap_err().to_string();
        assert!(message.contains("claude"), "{message}");
        assert!(Plan::parse(&[], Some("  "), None, false).is_err());
        assert!(Plan::parse(&[], Some("cat"), Some(0), false).is_err());
    }

    #[test]
    fn lines_are_one_safe_bounded_line() {
        let line = bounded_line("a\u{1b}[0m\n\tb\u{202e}", 100);
        assert!(!line.chars().any(char::is_control));
        assert!(!line.contains('\u{202e}'));
        let long = bounded_line(&"y ".repeat(1000), MAX_DESCRIPTION_CHARS);
        assert_eq!(MAX_DESCRIPTION_CHARS, long.chars().count());
        assert!(long.ends_with('…'));
    }

    #[test]
    fn material_joins_titles_before_subjects() {
        let material = Material {
            commit_subjects: vec!["Fix login".into()],
            session_titles: vec!["Auth work".into()],
        };
        assert_eq!(Some("Auth work; Fix login".to_string()), material.text());
        assert_eq!(None, Material::default().text());
    }

    fn digest() -> Digest {
        Digest {
            version: 1,
            date: "2026-03-02".into(),
            engagement: "acme".into(),
            detail: None,
            hours: 1.5,
            repos: vec!["api".into()],
            branches: vec![],
            issues: vec![],
            counts: DigestCounts {
                prompts: 3,
                commits: 1,
                sessions: 1,
            },
            commit_subjects: None,
            session_titles: None,
        }
    }

    #[test]
    fn the_optional_digest_fields_appear_only_for_the_sources_asked_for() {
        let material = Material {
            commit_subjects: vec!["Subject".into()],
            session_titles: vec!["Title".into()],
        };
        let bare = serde_json::to_value(digest()).unwrap();
        assert!(bare.get("commit_subjects").is_none());
        assert!(bare.get("session_titles").is_none());
        assert!(bare.get("detail").is_none());

        let plan = parse(&["commits"]).unwrap();
        let value = serde_json::to_value(digest().with_material(&plan, &material)).unwrap();
        assert_eq!(serde_json::json!(["Subject"]), value["commit_subjects"]);
        assert!(value.get("session_titles").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_summarizer_reads_the_digest_and_its_output_becomes_one_line() {
        let text = summarize(
            "tr -d '\\n' | head -c 20; printf '\\nsecond \\033[31mline\\n'",
            Duration::from_secs(10),
            &digest(),
        )
        .unwrap();
        assert!(text.starts_with("{\"version\":1,\"date\""), "{text}");
        assert!(!text.chars().any(char::is_control));
        assert!(!text.contains('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_or_silent_or_slow_summarizer_is_an_error() {
        let failure =
            summarize("echo oops >&2; exit 3", Duration::from_secs(10), &digest()).unwrap_err();
        assert!(failure.contains("oops"), "{failure}");
        let silent = summarize("true", Duration::from_secs(10), &digest()).unwrap_err();
        assert!(silent.contains("printed nothing"), "{silent}");
        let started = Instant::now();
        let slow = summarize("sleep 30", Duration::from_millis(300), &digest()).unwrap_err();
        assert!(slow.contains("timed out"), "{slow}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
