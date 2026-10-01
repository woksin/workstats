//! Bundles: the evidence one machine exports so another can merge it, and the
//! `export` and `merge` commands around them.
//!
//! A bundle is evidence, not a conclusion. It carries the same structural
//! facts the transcript cache holds (activity timestamps, models, token counts,
//! branch names) plus per-commit line tallies, and nothing else: no prompts, no
//! commit subjects, no titles, no absolute paths, and no file paths unless the
//! exporter asked for them. Because it is evidence, the importing machine runs
//! its own pipeline over the union, so human time is recomputed from all the
//! signals together and an hour spent on the laptop and the desktop is counted
//! once. Summing two reports could never promise that.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use clap::Args;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::classify::{CategoryTally, active_registry};
use crate::cli::{ReportArguments, ReportWindow};
use crate::model::{
    ActivityPoint, Authorship, BranchMark, BranchSource, Diagnostics, ExactInterval, GitCommit,
    PrLink, Session, TokenEvent,
};
use crate::paths::{ProjectAliases, default_config_path, disambiguated_repository_label};
use crate::report::{self, Collected, Presentation, Purpose};
use crate::timeutil::parse_duration;

/// The `format` every bundle states, so a file that is something else is
/// refused by name rather than half-read.
const FORMAT: &str = "workstats-bundle";
const VERSION: u32 = 1;
/// A bundle is read whole. Past this a file is not a bundle someone exported
/// by hand-sized history; it is refused rather than allowed to exhaust memory.
const MAX_BUNDLE_BYTES: u64 = 1 << 30;
const MAX_LABEL_CHARS: usize = 64;
const MAX_KEY_BYTES: usize = 512;
const MAX_SUBDIR_BYTES: usize = 512;
/// The same bounds the providers apply to a session's branch marks.
const MAX_BRANCH_MARKS: usize = 256;
const MAX_BRANCH_BYTES: usize = 256;
const MACHINE_FILE: &str = "machine.json";

#[derive(Debug, Args)]
pub(crate) struct ExportArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "FILE",
        help = "Where to write the bundle; '-' writes stdout (default: workstats-bundle-<label>.json)"
    )]
    pub(crate) output: Option<PathBuf>,
    #[arg(
        long,
        value_name = "NAME",
        help = "Name for this machine in the bundle; remembered in machine.json beside the config"
    )]
    pub(crate) label: Option<String>,
    #[arg(
        long,
        help = "Include changed file paths, which are left out by default"
    )]
    pub(crate) include_paths: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MergeArguments {
    #[arg(value_name = "FILE", required = true, help = "Bundles to merge")]
    pub(crate) files: Vec<PathBuf>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(long, help = "Also include this machine's own history")]
    pub(crate) with_local: bool,
}

// ---------------------------------------------------------------------------
// The format.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Bundle {
    format: String,
    version: u32,
    exported_at: DateTime<Utc>,
    workstats_version: String,
    machine: BundleMachine,
    #[serde(default)]
    person: BundlePerson,
    #[serde(default)]
    window: BundleWindow,
    #[serde(default)]
    settings: BundleSettings,
    #[serde(default)]
    repositories: Vec<BundleRepository>,
    #[serde(default)]
    sessions: Vec<BundleSession>,
    #[serde(default)]
    commits: Vec<BundleCommit>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleMachine {
    id: String,
    label: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BundlePerson {
    #[serde(default)]
    authors: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BundleWindow {
    #[serde(default)]
    since: Option<DateTime<Utc>>,
    #[serde(default)]
    until: Option<DateTime<Utc>>,
}

/// What the exporter measured with. Informational: the importer's own settings
/// decide the merged report, and a difference is only noted.
#[derive(Debug, Default, Serialize, Deserialize)]
struct BundleSettings {
    #[serde(default)]
    human_idle: Option<String>,
    #[serde(default)]
    review_credit: Option<String>,
    #[serde(default)]
    gap_cap: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleRepository {
    key: String,
    label: String,
    portable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleSession {
    provider: String,
    session_id: String,
    repo: String,
    #[serde(default)]
    subdir: String,
    #[serde(default)]
    is_subagent: bool,
    #[serde(default)]
    branch_source: BranchSource,
    #[serde(default)]
    branches: Vec<BranchMark>,
    #[serde(default)]
    points: Vec<ActivityPoint>,
    #[serde(default)]
    human_points: Vec<ActivityPoint>,
    #[serde(default)]
    exact_intervals: Vec<ExactInterval>,
    #[serde(default)]
    token_events: Vec<TokenEvent>,
    #[serde(default)]
    pull_requests: Vec<PrLink>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleCommit {
    repo: String,
    /// The exporter's own `project:` grouping for this commit's repository,
    /// when it had one. Used only when the importer has no alias of its own
    /// that claims the repository, so a product the exporter grouped stays
    /// grouped with its sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    sha: String,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    ignored_additions: u64,
    #[serde(default)]
    ignored_deletions: u64,
    /// Category name to `[additions, deletions]`, only for categories touched.
    #[serde(default)]
    categories: BTreeMap<String, [u64; 2]>,
    #[serde(default)]
    agent_authored: bool,
    #[serde(default)]
    agent_assisted: bool,
    #[serde(default)]
    autofix_assisted: bool,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    branch_source: BranchSource,
    /// Changed file paths: `null` unless the exporter passed `--include-paths`.
    #[serde(default)]
    files: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// This machine: machine.json.
// ---------------------------------------------------------------------------

/// Who this machine is in a bundle. The id is random and never leaves the
/// machine except inside a bundle; it only keeps one machine's `local:`
/// repository keys apart from another's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Machine {
    pub(crate) id: String,
    pub(crate) label: String,
}

#[derive(Serialize, Deserialize)]
struct MachineFile {
    version: u32,
    id: String,
    #[serde(default)]
    label: Option<String>,
}

/// `machine.json` lives beside the config so it follows the same
/// `WORKSTATS_CONFIG`/`--config` choice as everything else that is the user's.
fn machine_path(config: &Path) -> PathBuf {
    config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(MACHINE_FILE)
}

/// A label is shown in reports and becomes part of a synthetic path, so it is
/// short, printable and free of path separators and the `@` that joins it.
fn validate_label(label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty()
        || label.chars().count() > MAX_LABEL_CHARS
        || label
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | '@'))
    {
        bail!(
            "a machine label must be 1 to {MAX_LABEL_CHARS} printable characters without '/', '\\' or '@'; pass one with --label"
        );
    }
    Ok(label.to_string())
}

fn environment_label() -> Option<String> {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .filter_map(|name| env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// 128 random bits as hex. The standard library seeds `RandomState` from the
/// operating system, which is enough for what this is: an identifier that must
/// not collide between two machines, not a secret. The seeds are mixed with the
/// clock and process id through SHA-256 so two calls in one process differ too.
fn random_machine_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut seed = Vec::with_capacity(32);
    for round in 0u8..4 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(round);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        seed.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    let digest = Sha256::digest(&seed);
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_machine_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Reads this machine's identity, creating `machine.json` the first time. A
/// file that cannot be read is an error and is never regenerated: a new id
/// would silently turn every `local:` repository this machine ever exported
/// into a different repository.
///
/// The label is `requested`, else the stored one, else `fallback` (the
/// environment's host name). A requested label is remembered.
fn ensure_machine(
    path: &Path,
    requested: Option<&str>,
    fallback: Option<String>,
) -> Result<Machine> {
    let stored = if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let file: MachineFile = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is not valid JSON; fix or delete it (deleting gives this machine a new id)",
                path.display()
            )
        })?;
        if file.version != 1 || !valid_machine_id(&file.id) {
            bail!(
                "{} is not a version 1 machine file with a 32-character hex id; fix or delete it",
                path.display()
            );
        }
        Some(file)
    } else {
        None
    };
    let id = stored
        .as_ref()
        .map_or_else(random_machine_id, |file| file.id.clone());
    let stored_label = stored
        .as_ref()
        .and_then(|file| file.label.as_deref())
        .map(validate_label)
        .transpose()
        .with_context(|| format!("{} holds an unusable label", path.display()))?;
    let label = match (requested, stored_label.clone(), fallback) {
        (Some(requested), _, _) => validate_label(requested)?,
        (None, Some(stored), _) => stored,
        (None, None, Some(host)) => validate_label(&host)
            .context("the host name is not usable as a machine label; pass one with --label")?,
        (None, None, None) => bail!(
            "this machine has no label yet and HOSTNAME/COMPUTERNAME is not set; pass --label NAME"
        ),
    };
    if stored.is_none() || stored_label.as_deref() != Some(label.as_str()) {
        let encoded = serde_json::to_vec_pretty(&MachineFile {
            version: 1,
            id: id.clone(),
            label: Some(label.clone()),
        })?;
        write_atomic(path, &encoded)?;
    }
    Ok(Machine { id, label })
}

/// Written through a temporary file in the same directory so an interrupted
/// write cannot leave half a file where a whole one was expected.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot write to {}", parent.display()))?;
    file.write_all(bytes)?;
    file.flush()?;
    crate::durable::persist(file, path)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Export.
// ---------------------------------------------------------------------------

pub(crate) fn run_export(arguments: ExportArguments) -> Result<()> {
    let ExportArguments {
        mut report,
        output,
        label,
        include_paths,
    } = arguments;
    // A bundle is one machine's own evidence. Folding another machine's in
    // would make this machine vouch for it, and a second export of the same
    // history would not be a bundle of this one.
    if !report.import.is_empty() {
        bail!(
            "`workstats export` writes this machine's own history; merge bundles with `workstats merge` instead of exporting with --import"
        );
    }
    let config = report.config.clone().unwrap_or_else(default_config_path);
    // Before the scan: a missing label is cheap to report and expensive to
    // find out after reading all of history.
    let machine = ensure_machine(
        &machine_path(&config),
        label.as_deref(),
        environment_label(),
    )?;
    // A bundle carries no goals.
    report.no_goals = true;
    let collected = report::collect(report, Purpose::Query)?;
    let bundle = build_bundle(&collected_input(&collected), &machine, include_paths);
    let encoded = serde_json::to_vec(&bundle)?;
    let summary = format!(
        "{} sessions, {} commits, {} repositories",
        bundle.sessions.len(),
        bundle.commits.len(),
        bundle.repositories.len()
    );
    let local_only: Vec<&str> = bundle
        .repositories
        .iter()
        .filter(|repository| !repository.portable)
        .map(|repository| repository.label.as_str())
        .collect();
    match output.as_deref() {
        Some(path) if path == Path::new("-") => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&encoded)?;
            stdout.write_all(b"\n")?;
            eprintln!("Exported {summary} from {}", machine.label);
        }
        other => {
            let path = other.map_or_else(
                || PathBuf::from(default_output_name(&machine.label)),
                Path::to_path_buf,
            );
            write_atomic(&path, &encoded)?;
            eprintln!("Wrote {} ({summary})", path.display());
        }
    }
    if !local_only.is_empty() {
        eprintln!(
            "{} repositories have no shared remote and cannot be matched with other machines' history: {}",
            local_only.len(),
            sample(&local_only)
        );
    }
    Ok(())
}

fn default_output_name(label: &str) -> String {
    let safe: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("workstats-bundle-{safe}.json")
}

/// A short list for a message: the first few names and how many more.
fn sample(names: &[&str]) -> String {
    const SHOWN: usize = 5;
    let mut shown = names
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        shown.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    shown
}

/// What a bundle is built from: everything a run collected, nothing derived.
struct ExportInput<'a> {
    sessions: &'a [Session],
    commits: &'a [GitCommit],
    agent_commits: &'a [GitCommit],
    window: ReportWindow,
    authors: &'a [String],
    human_idle: Duration,
    review_credit: Duration,
    gap_cap: Duration,
    now: DateTime<Utc>,
}

fn collected_input(collected: &Collected) -> ExportInput<'_> {
    ExportInput {
        sessions: &collected.sessions,
        commits: &collected.commits,
        agent_commits: &collected.agent_commits,
        window: collected.window,
        authors: &collected.report.inputs.authors,
        human_idle: collected.settings.human_idle,
        review_credit: collected.settings.review_credit,
        gap_cap: collected.settings.gap_cap,
        now: collected.settings.now,
    }
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.num_seconds();
    if seconds > 0 && seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// A repository identity another machine can recognise: a configured project
/// or a remote. A remote that names a directory on this disk (`file://` and
/// plain-path remotes) is an absolute path in disguise and is not portable.
fn portable_key(natural_id: &str) -> bool {
    if natural_id.starts_with("project:") {
        return true;
    }
    let Some(identity) = natural_id.strip_prefix("remote:") else {
        return false;
    };
    let bytes = identity.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !(identity.starts_with('/') || identity.contains('\\') || drive)
}

/// The label a repository had before a collision made it `name [1a2b3c4d]`.
/// The importer disambiguates again from its own identities, so exporting the
/// decorated form would stack two suffixes.
fn plain_label(label: &str, repo_id: &str) -> String {
    let tag = disambiguated_repository_label("", repo_id);
    label
        .strip_suffix(tag.as_str())
        .unwrap_or(label)
        .to_string()
}

fn printable(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_LABEL_CHARS * 2)
        .collect()
}

/// The last path segment of a remote identity, which is the label the local
/// resolver gives the same repository.
fn remote_label(key: &str) -> String {
    key.strip_prefix("remote:")
        .and_then(|identity| identity.rsplit(['/', '\\']).find(|part| !part.is_empty()))
        .map_or_else(|| key.to_string(), printable)
}

/// The directory a session ran in, relative to the root of its checkout, with
/// `/` separators. Empty when the directory is the root, is gone, or is not in
/// a checkout: an absolute path is never kept, and neither is a guess.
fn subdir_of(cwd: &str) -> String {
    let path = Path::new(cwd);
    if !path.is_dir() {
        return String::new();
    }
    let Some(root) = path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
    else {
        return String::new();
    };
    let Ok(relative) = path.strip_prefix(root) else {
        return String::new();
    };
    let parts: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    clean_subdir(&parts.join("/"))
}

/// A subdirectory from a bundle is untrusted text that becomes part of a
/// display path: only plain relative components survive.
fn clean_subdir(raw: &str) -> String {
    let parts: Vec<String> = raw
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .map(|part| part.chars().filter(|c| !c.is_control()).collect::<String>())
        .collect();
    let joined = parts.join("/");
    if joined.len() > MAX_SUBDIR_BYTES {
        String::new()
    } else {
        joined
    }
}

/// Assigns each repository that has no portable identity a key of the form
/// `local:<machine id>:<label>`. Two different repositories with the same label
/// get distinct keys, because merging them would be a silent loss.
fn local_keys(machine: &Machine, labels: BTreeMap<String, String>) -> HashMap<String, String> {
    let mut used = BTreeSet::new();
    let mut keys = HashMap::new();
    for (natural_id, label) in labels {
        let base = format!("local:{}:{}", machine.id, label);
        let mut key = base.clone();
        let mut suffix = 2;
        while !used.insert(key.clone()) {
            key = format!("{base}-{suffix}");
            suffix += 1;
        }
        keys.insert(natural_id, key);
    }
    keys
}

/// The id a session carries in a bundle. Providers build their ids from the
/// history layout (`<id>:<path relative to the history root>`, `<id>:<cwd>`),
/// and those paths hold usernames and client directory names. A bundle exports
/// only a hash of the provider and the id: 128 bits, so it cannot collide in
/// practice, and stable, so the same session in a synced folder on two machines
/// still deduplicates. Importing hashes the local ids the same way.
fn opaque_session_id(provider: &str, session_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(provider.as_bytes());
    digest.update([0]);
    digest.update(session_id.as_bytes());
    digest
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn build_bundle(input: &ExportInput<'_>, machine: &Machine, include_paths: bool) -> Bundle {
    // First pass: which repositories have no portable identity, and what are
    // they called. Sessions know their repository by `repo_id`; commits by the
    // natural `repo_member_id`, which is what a remote key is made from.
    let mut unportable: BTreeMap<String, String> = BTreeMap::new();
    for session in input.sessions {
        if !portable_key(&session.repo_id) {
            unportable
                .entry(session.repo_id.clone())
                .or_insert_with(|| printable(&plain_label(&session.repo, &session.repo_id)));
        }
    }
    for commit in input.commits.iter().chain(input.agent_commits) {
        if !portable_key(&commit.repo_member_id) {
            unportable
                .entry(commit.repo_member_id.clone())
                .or_insert_with(|| printable(&plain_label(&commit.repo, &commit.repo_id)));
        }
    }
    let keys = local_keys(machine, unportable);
    let key_for = |natural_id: &str| -> String {
        keys.get(natural_id)
            .cloned()
            .unwrap_or_else(|| natural_id.to_string())
    };

    let mut repositories: BTreeMap<String, BundleRepository> = BTreeMap::new();
    let mut remember = |key: &str, label: String| {
        repositories
            .entry(key.to_string())
            .or_insert_with(|| BundleRepository {
                key: key.to_string(),
                label,
                portable: !key.starts_with("local:"),
            });
    };

    let mut sessions: Vec<BundleSession> = input
        .sessions
        .iter()
        .map(|session| {
            let key = key_for(&session.repo_id);
            remember(
                &key,
                if key.starts_with("remote:") {
                    remote_label(&key)
                } else {
                    printable(&plain_label(&session.repo, &session.repo_id))
                },
            );
            BundleSession {
                provider: session.provider.clone(),
                session_id: opaque_session_id(&session.provider, &session.session_id),
                repo: key,
                subdir: subdir_of(&session.cwd),
                is_subagent: session.is_subagent,
                branch_source: session.branch_source,
                branches: session.branches.clone(),
                points: session.points.clone(),
                human_points: session.human_points.clone(),
                exact_intervals: session.exact_intervals.clone(),
                token_events: session.token_events.clone(),
                pull_requests: session.pull_requests.clone(),
            }
        })
        .collect();
    sessions.sort_by(|a, b| {
        (&a.provider, &a.session_id, &a.repo, &a.subdir).cmp(&(
            &b.provider,
            &b.session_id,
            &b.repo,
            &b.subdir,
        ))
    });

    let registry = active_registry();
    let mut commits: Vec<BundleCommit> = input
        .commits
        .iter()
        .chain(input.agent_commits)
        .map(|commit| {
            let key = key_for(&commit.repo_member_id);
            remember(
                &key,
                if key.starts_with("remote:") {
                    remote_label(&key)
                } else {
                    printable(&plain_label(&commit.repo, &commit.repo_id))
                },
            );
            let project = commit
                .repo_id
                .starts_with("project:")
                .then(|| commit.repo_id.clone());
            if let Some(project) = &project {
                remember(
                    project,
                    printable(&plain_label(&commit.repo, &commit.repo_id)),
                );
            }
            let mut categories = BTreeMap::new();
            for index in 0..registry.len() {
                let lines = commit.categories.get(index);
                if lines.touched() > 0 {
                    categories.insert(
                        registry.name(index).to_string(),
                        [lines.additions, lines.deletions],
                    );
                }
            }
            BundleCommit {
                repo: key,
                project,
                sha: commit.sha.clone(),
                timestamp: commit.timestamp,
                additions: commit.additions,
                deletions: commit.deletions,
                ignored_additions: commit.ignored_additions,
                ignored_deletions: commit.ignored_deletions,
                categories,
                agent_authored: commit.authorship.is_agent_authored(),
                agent_assisted: commit.authorship.is_agent_assisted(),
                autofix_assisted: commit.authorship.is_autofix_assisted(),
                branch: commit.branch.clone(),
                branch_source: commit.branch_source,
                files: include_paths.then(|| commit.files.clone()),
            }
        })
        .collect();
    commits.sort_by(|a, b| (&a.repo, &a.sha).cmp(&(&b.repo, &b.sha)));

    Bundle {
        format: FORMAT.to_string(),
        version: VERSION,
        exported_at: input.now,
        workstats_version: env!("CARGO_PKG_VERSION").to_string(),
        machine: BundleMachine {
            id: machine.id.clone(),
            label: machine.label.clone(),
        },
        person: BundlePerson {
            authors: input.authors.to_vec(),
        },
        window: BundleWindow {
            since: input.window.0,
            until: input.window.1,
        },
        settings: BundleSettings {
            human_idle: Some(duration_text(input.human_idle)),
            review_credit: Some(duration_text(input.review_credit)),
            gap_cap: Some(duration_text(input.gap_cap)),
        },
        repositories: repositories.into_values().collect(),
        sessions,
        commits,
    }
}

// ---------------------------------------------------------------------------
// Import.
// ---------------------------------------------------------------------------

/// Everything the import needs to know about the run it joins, so it can make
/// the same choices the local readers made: the window, the filters and the
/// importer's own settings.
pub(crate) struct ImportRequest<'a> {
    pub(crate) files: &'a [PathBuf],
    pub(crate) window: ReportWindow,
    pub(crate) repo_filter: Option<&'a str>,
    /// `--path`/`--path-exclude` were given. Bundles carry no file paths by
    /// default, so the filters cannot apply to imported commits; the run says so.
    pub(crate) path_filtered: bool,
    /// The authors the local Git scan used; empty when Git was not read.
    pub(crate) local_authors: &'a [String],
    pub(crate) human_idle: Duration,
    pub(crate) review_credit: Duration,
    pub(crate) gap_cap: Duration,
    pub(crate) provider_enabled: &'a dyn Fn(&str) -> bool,
}

/// What the header of a file says about it, read before the rest so a report
/// or an unrelated file is refused by what it is, not by which field failed.
#[derive(Deserialize)]
struct Header {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    version: Option<u64>,
    #[serde(default)]
    methodology: Option<IgnoredAny>,
}

fn read_bundle(path: &Path) -> Result<Bundle> {
    let length = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if length > MAX_BUNDLE_BYTES {
        bail!(
            "{} is larger than the {} MiB a bundle can be",
            path.display(),
            MAX_BUNDLE_BYTES >> 20
        );
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let header: Header = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{} is not a workstats bundle (invalid JSON)",
            path.display()
        )
    })?;
    if header.methodology.is_some() {
        bail!(
            "{} is a report, and a report is a conclusion; export a bundle with `workstats export` and merge that",
            path.display()
        );
    }
    if header.format.as_deref() != Some(FORMAT) {
        bail!(
            "{} is not a workstats bundle; write one with `workstats export`",
            path.display()
        );
    }
    match header.version {
        Some(version) if version == u64::from(VERSION) => {}
        other => bail!(
            "{} is bundle version {}; this workstats reads version {VERSION}",
            path.display(),
            other.map_or_else(|| "unknown".to_string(), |version| version.to_string())
        ),
    }
    let bundle: Bundle = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid workstats bundle", path.display()))?;
    validate(&bundle)
        .with_context(|| format!("{} is not a valid workstats bundle", path.display()))?;
    Ok(bundle)
}

fn valid_key(key: &str) -> bool {
    key.len() <= MAX_KEY_BYTES
        && !key.chars().any(char::is_control)
        && ["remote:", "local:", "project:"]
            .iter()
            .any(|prefix| key.starts_with(prefix) && key.len() > prefix.len())
}

fn validate(bundle: &Bundle) -> Result<()> {
    if !valid_machine_id(&bundle.machine.id) {
        bail!("the machine id is not a 32-character hex string");
    }
    let mut known = HashSet::new();
    for repository in &bundle.repositories {
        if !valid_key(&repository.key) {
            bail!(
                "repository key {:?} is not remote:, local: or project:",
                repository.key
            );
        }
        known.insert(repository.key.as_str());
    }
    for session in &bundle.sessions {
        if !crate::cli::valid_provider_identifier(&session.provider, false) {
            bail!(
                "session provider {:?} is not a valid identifier",
                session.provider
            );
        }
        if !known.contains(session.repo.as_str()) {
            bail!(
                "a session names repository {:?}, which the bundle does not list",
                session.repo
            );
        }
    }
    for commit in &bundle.commits {
        if !known.contains(commit.repo.as_str()) {
            bail!(
                "a commit names repository {:?}, which the bundle does not list",
                commit.repo
            );
        }
        if commit.sha.is_empty()
            || commit.sha.len() > 64
            || !commit.sha.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("a commit has a malformed id");
        }
    }
    Ok(())
}

/// Lowercased, trimmed, sorted and de-duplicated, so two spellings of the same
/// person compare equal.
fn normalized_authors(authors: &[String]) -> Vec<String> {
    authors
        .iter()
        .map(|author| author.trim().to_lowercase())
        .filter(|author| !author.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Where an imported repository ends up in this run: the importer's own
/// project alias when it has one for the remote, else what the bundle says.
struct Placement {
    repo_id: String,
    label: String,
}

fn place(
    key: &str,
    project: Option<&str>,
    labels: &HashMap<&str, &str>,
    aliases: &ProjectAliases,
) -> Placement {
    // The importer's configuration decides, exactly as it does for a local
    // checkout of the same remote.
    if key.starts_with("remote:")
        && let Some((repo_id, label)) = aliases.alias_for_natural_id(key)
    {
        return Placement { repo_id, label };
    }
    let choice = project
        .filter(|project| labels.contains_key(project))
        .unwrap_or(key);
    Placement {
        repo_id: choice.to_string(),
        label: labels
            .get(choice)
            .map_or_else(|| remote_label(choice), |label| printable(label)),
    }
}

fn path_safe(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect()
}

/// A name for the working directory of imported work. It is not a directory on
/// this machine, so nothing resolves or scans it as one.
fn synthetic_cwd(label: &str, machine: &str, subdir: &str) -> String {
    let base = format!("{}@{}", path_safe(label), path_safe(machine));
    if subdir.is_empty() {
        base
    } else {
        format!("{base}/{subdir}")
    }
}

/// `--repo` as the local readers apply it: a case-insensitive substring of the
/// label, its disambiguated form, or where the work was. Imported work has no
/// path on this machine, so the repository key and subdirectory stand in for it.
fn repo_matches(filter: Option<&str>, placement: &Placement, key: &str, subdir: &str) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let needle = filter.to_lowercase();
    placement.label.to_lowercase().contains(&needle)
        || disambiguated_repository_label(&placement.label, &placement.repo_id)
            .to_lowercase()
            .contains(&needle)
        || key.to_lowercase().contains(&needle)
        || subdir.to_lowercase().contains(&needle)
}

/// Appends what `incoming` has that `target` lacks, counting duplicates the way
/// a union of multisets does: an item present twice on both sides stays twice,
/// not four times and not once. Two machines that synced one history folder
/// hold the same events; one machine's genuinely repeated event is kept.
fn union_by<T, K: Hash + Eq>(
    target: &mut Vec<T>,
    incoming: Vec<T>,
    key: impl Fn(&T) -> K,
) -> usize {
    let mut available: HashMap<K, usize> = HashMap::new();
    for item in target.iter() {
        *available.entry(key(item)).or_default() += 1;
    }
    let mut added = 0;
    for item in incoming {
        match available.get_mut(&key(&item)) {
            Some(count) if *count > 0 => *count -= 1,
            _ => {
                target.push(item);
                added += 1;
            }
        }
    }
    added
}

fn usage_key(event: &TokenEvent) -> (DateTime<Utc>, String, [u64; 4]) {
    (
        event.timestamp,
        event.model.clone(),
        [
            event.usage.input_tokens,
            event.usage.output_tokens,
            event.usage.cache_read_tokens,
            event.usage.cache_creation_tokens,
        ],
    )
}

/// Folds `incoming` into `target`, which keeps its own identity (repository,
/// working directory, role): what it already had wins, and only evidence it
/// lacked is added.
fn union_session(target: &mut Session, incoming: Session) {
    let points = union_by(&mut target.points, incoming.points, |point| {
        (point.timestamp, point.model.clone())
    });
    let human = union_by(&mut target.human_points, incoming.human_points, |point| {
        (point.timestamp, point.model.clone())
    });
    let intervals = union_by(
        &mut target.exact_intervals,
        incoming.exact_intervals,
        |interval| (interval.start, interval.end, interval.model.clone()),
    );
    let tokens = union_by(&mut target.token_events, incoming.token_events, usage_key);
    union_by(&mut target.pull_requests, incoming.pull_requests, |link| {
        (link.number, link.repository.clone())
    });
    // Sorted only when something arrived, so a session that gained nothing is
    // exactly what it was.
    if points > 0 {
        target.points.sort_by_key(|point| point.timestamp);
    }
    if human > 0 {
        target.human_points.sort_by_key(|point| point.timestamp);
    }
    if intervals > 0 {
        target
            .exact_intervals
            .sort_by_key(|interval| interval.start);
    }
    if tokens > 0 {
        target.token_events.sort_by_key(|event| event.timestamp);
    }
    if target.branches.is_empty() && !incoming.branches.is_empty() {
        target.branches = incoming.branches;
        target.branch_source = incoming.branch_source;
    }
}

/// Branch marks the way the providers bound them. A mark that breaks a bound is
/// dropped and counted rather than trusted. `branch_at` reads marks in time
/// order with the `from: None` mark first, so the marks are sorted that way: a
/// second `None` mark and a mark that repeats the branch before it are dropped
/// and counted too, because neither changes anything and an unsorted list would
/// put work on the wrong branch.
fn bounded_marks(marks: Vec<BranchMark>, dropped: &mut usize) -> Vec<BranchMark> {
    let mut valid: Vec<BranchMark> = Vec::new();
    for mark in marks {
        let ok = !mark.branch.is_empty()
            && mark.branch.len() <= MAX_BRANCH_BYTES
            && !mark.branch.chars().any(char::is_control);
        if ok {
            valid.push(mark);
        } else {
            *dropped += 1;
        }
    }
    // Stable, and `None` orders before every `Some`.
    valid.sort_by_key(|mark| mark.from);
    let mut kept: Vec<BranchMark> = Vec::new();
    for mark in valid {
        let extra_start = mark.from.is_none() && !kept.is_empty();
        let repeat = kept.last().is_some_and(|last| last.branch == mark.branch);
        if extra_start || repeat || kept.len() >= MAX_BRANCH_MARKS {
            *dropped += 1;
        } else {
            kept.push(mark);
        }
    }
    kept
}

/// Folds `--import` bundles into the sessions and commits read locally.
/// Called by the pipeline after Git has been scanned and before repository
/// labels are made unique, so imported repositories are labelled with the rest.
///
/// Nothing is summed. Sessions and commits join the local ones and the normal
/// pipeline computes everything from the union.
pub(crate) fn merge_imports(
    request: &ImportRequest<'_>,
    sessions: &mut Vec<Session>,
    commits: &mut Vec<GitCommit>,
    agent_commits: &mut Vec<GitCommit>,
    aliases: &ProjectAliases,
    diagnostics: &mut Diagnostics,
) -> Result<()> {
    if request.files.is_empty() {
        return Ok(());
    }
    let mut bundles = Vec::new();
    for path in request.files {
        bundles.push(read_bundle(path)?);
    }

    // One person's machines only. Human time is computed for one person from
    // all their signals; a team needs it per person and then summed, which
    // this does not do.
    let first = normalized_authors(&bundles[0].person.authors);
    for bundle in &bundles[1..] {
        let authors = normalized_authors(&bundle.person.authors);
        if authors != first {
            bail!(
                "bundles from `{}` and `{}` were exported for different authors ({} and {}); team merges are not supported yet",
                bundles[0].machine.label,
                bundle.machine.label,
                describe_authors(&first),
                describe_authors(&authors),
            );
        }
    }
    let local = normalized_authors(request.local_authors);
    if !local.is_empty() && !first.is_empty() && local != first {
        diagnostics.warn(format!(
            "this run read Git history for {} but the imported bundles were exported for {}; the merged hours are one person's, so check that these are your machines",
            describe_authors(&local),
            describe_authors(&first)
        ));
    }
    if request.path_filtered {
        diagnostics.warn(
            "--path and --path-exclude do not apply to imported commits, which carry no file paths unless exported with --include-paths",
        );
    }

    // The sessions already present, by (provider, session id). Sources join one
    // at a time, so two entries of one source never merge with each other: the
    // history that wrote them kept them apart.
    let mut index: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (position, session) in sessions.iter().enumerate() {
        index
            .entry((
                session.provider.clone(),
                opaque_session_id(&session.provider, &session.session_id),
            ))
            .or_default()
            .push(position);
    }
    let mut seen_commits: HashSet<(String, String)> = commits
        .iter()
        .chain(agent_commits.iter())
        .map(|commit| (commit.repo_member_id.clone(), commit.sha.clone()))
        .collect();
    let registry = active_registry();
    let other = registry
        .index_of("other")
        .unwrap_or_else(|| registry.len().saturating_sub(1));

    for bundle in bundles {
        let machine = printable(&bundle.machine.label);
        let machine = if machine.trim().is_empty() {
            bundle.machine.id[..8].to_string()
        } else {
            machine
        };
        note_coverage(request, &bundle, &machine, diagnostics);
        note_settings(request, &bundle, &machine, diagnostics);

        let labels: HashMap<&str, &str> = bundle
            .repositories
            .iter()
            .map(|repository| (repository.key.as_str(), repository.label.as_str()))
            .collect();
        let root = format!("imported/{}", path_safe(&machine));
        let unshared: Vec<&str> = bundle
            .repositories
            .iter()
            .filter(|repository| repository.key.starts_with("local:"))
            .map(|repository| repository.label.as_str())
            .collect();
        if !unshared.is_empty() {
            diagnostics.warn(format!(
                "`{machine}` has {} repositories with no shared remote ({}); they cannot be matched with other machines' history, so they are counted as separate repositories and their commits are not deduplicated",
                unshared.len(),
                sample(&unshared)
            ));
        }

        let (mut new_sessions, mut merged_sessions, mut dropped_marks) = (0, 0, 0usize);
        let mut added: Vec<((String, String), usize)> = Vec::new();
        for item in bundle.sessions {
            if !(request.provider_enabled)(&item.provider) {
                continue;
            }
            let placement = place(&item.repo, None, &labels, aliases);
            let subdir = clean_subdir(&item.subdir);
            if !repo_matches(request.repo_filter, &placement, &item.repo, &subdir) {
                continue;
            }
            let mut incoming = Session {
                provider: item.provider,
                session_id: item.session_id,
                cwd: synthetic_cwd(&placement.label, &machine, &subdir),
                repo: placement.label.clone(),
                repo_id: placement.repo_id.clone(),
                root: root.clone(),
                points: item.points,
                exact_intervals: item.exact_intervals,
                human_points: item.human_points,
                token_events: item.token_events,
                is_subagent: item.is_subagent,
                source_file: PathBuf::new(),
                branches: bounded_marks(item.branches, &mut dropped_marks),
                branch_source: item.branch_source,
                pull_requests: item.pull_requests,
            };
            if incoming.branches.is_empty() {
                incoming.branch_source = BranchSource::None;
            }
            let key = (incoming.provider.clone(), incoming.session_id.clone());
            let target = index.get(&key).and_then(|candidates| {
                candidates
                    .iter()
                    .copied()
                    .find(|&position| sessions[position].repo_id == incoming.repo_id)
                    .or_else(|| candidates.first().copied())
            });
            match target {
                Some(position) => {
                    union_session(&mut sessions[position], incoming);
                    merged_sessions += 1;
                }
                None => {
                    sessions.push(incoming);
                    added.push((key, sessions.len() - 1));
                    new_sessions += 1;
                }
            }
        }
        for (key, position) in added {
            index.entry(key).or_default().push(position);
        }

        let (mut new_commits, mut known_commits, mut without_files) = (0, 0, 0);
        let mut unknown_categories: BTreeSet<String> = BTreeSet::new();
        for item in bundle.commits {
            let placement = place(&item.repo, item.project.as_deref(), &labels, aliases);
            if !repo_matches(request.repo_filter, &placement, &item.repo, "") {
                continue;
            }
            // The local copy wins: it was read from the repository itself.
            if !seen_commits.insert((item.repo.clone(), item.sha.clone())) {
                known_commits += 1;
                continue;
            }
            let mut tally = CategoryTally::default();
            for (name, [additions, deletions]) in &item.categories {
                let slot = registry.index_of(name).unwrap_or_else(|| {
                    unknown_categories.insert(name.clone());
                    other
                });
                tally.add(slot, *additions, *deletions);
            }
            let mut authorship = if item.agent_authored {
                Authorship::agent()
            } else {
                Authorship::default()
            };
            // `Authorship` records assistance only by reading a trailer, so the
            // flags are restored through the identities it recognises.
            if item.agent_assisted {
                authorship.note_co_author("<noreply@anthropic.com>");
            }
            if item.autofix_assisted {
                authorship.note_co_author("github-code-quality[bot]@");
            }
            let files = item.files.unwrap_or_default();
            if files.is_empty() && item.additions + item.deletions > 0 {
                without_files += 1;
            }
            let commit = GitCommit {
                sha: item.sha,
                timestamp: item.timestamp,
                repo: placement.label.clone(),
                repo_id: placement.repo_id,
                repo_member_id: item.repo,
                cwd: synthetic_cwd(&placement.label, &machine, ""),
                root: root.clone(),
                additions: item.additions,
                deletions: item.deletions,
                files,
                ignored_additions: item.ignored_additions,
                ignored_deletions: item.ignored_deletions,
                categories: tally,
                authorship,
                branch: item.branch.filter(|branch| {
                    branch.len() <= MAX_BRANCH_BYTES && !branch.chars().any(char::is_control)
                }),
                branch_source: item.branch_source,
            };
            if commit.authorship.is_agent_authored() {
                agent_commits.push(commit);
            } else {
                commits.push(commit);
            }
            new_commits += 1;
        }

        diagnostics.note(format!(
            "imported `{machine}`: {new_sessions} new and {merged_sessions} already-known sessions, {new_commits} new and {known_commits} already-known commits"
        ));
        if !unknown_categories.is_empty() {
            diagnostics.note(format!(
                "`{machine}` uses categories this configuration does not define ({}); their lines are counted as other",
                unknown_categories.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if without_files > 0 {
            diagnostics.note(format!(
                "{without_files} commits imported from `{machine}` carry no file paths, so they add no files to the file counts; export with --include-paths for those"
            ));
        }
        if dropped_marks > 0 {
            diagnostics.warn(format!(
                "{dropped_marks} branch marks in the bundle from `{machine}` were dropped for being empty, too long, containing control characters, starting the session twice or repeating the branch before them"
            ));
        }
    }
    Ok(())
}

fn describe_authors(authors: &[String]) -> String {
    if authors.is_empty() {
        "no author".to_string()
    } else {
        format!("[{}]", authors.join(", "))
    }
}

/// Says when a bundle covers less than the window asked for, because a figure
/// that quietly lacks a machine's hours reads as a lighter week.
fn note_coverage(
    request: &ImportRequest<'_>,
    bundle: &Bundle,
    machine: &str,
    diagnostics: &mut Diagnostics,
) {
    if let Some(since) = bundle.window.since
        && request.window.0.is_none_or(|asked| asked < since)
    {
        diagnostics.note(format!(
            "the bundle from `{machine}` starts at {}; earlier hours on that machine are not in it",
            since.format("%Y-%m-%d")
        ));
    }
    if let Some(until) = bundle.window.until
        && request.window.1.is_none_or(|asked| asked > until)
    {
        diagnostics.note(format!(
            "the bundle from `{machine}` ends at {}; later hours on that machine are not in it",
            until.format("%Y-%m-%d")
        ));
    }
}

/// The importer's settings decide the report; a bundle measured with others is
/// noted so a difference in hours is not a mystery.
fn note_settings(
    request: &ImportRequest<'_>,
    bundle: &Bundle,
    machine: &str,
    diagnostics: &mut Diagnostics,
) {
    let differs = |theirs: &Option<String>, ours: Duration| {
        theirs
            .as_deref()
            .and_then(|text| parse_duration(text).ok())
            .is_some_and(|theirs| theirs != ours)
    };
    if differs(&bundle.settings.human_idle, request.human_idle)
        || differs(&bundle.settings.review_credit, request.review_credit)
        || differs(&bundle.settings.gap_cap, request.gap_cap)
    {
        let show = |value: &Option<String>| value.clone().unwrap_or_else(|| "?".to_string());
        diagnostics.note(format!(
            "`{machine}` was exported with human_idle {}, review_credit {} and gap_cap {}; this run's settings ({}, {}, {}) apply to the merged hours",
            show(&bundle.settings.human_idle),
            show(&bundle.settings.review_credit),
            show(&bundle.settings.gap_cap),
            duration_text(request.human_idle),
            duration_text(request.review_credit),
            duration_text(request.gap_cap),
        ));
    }
}

/// `workstats merge FILE...`: a report over the bundles alone, or over them and
/// this machine's own history with `--with-local`.
pub(crate) fn run_merge(arguments: MergeArguments) -> Result<()> {
    let MergeArguments {
        files,
        mut report,
        with_local,
    } = arguments;
    report.import.extend(files);
    if !with_local {
        // The bundles are the whole input. Skipping the local readers is the
        // only thing these flags do here: imports are never suppressed by them.
        report.no_ai = true;
        report.no_git = true;
        report.no_default_events = true;
    }
    report::run(report, Presentation::Print, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TokenUsage;

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-03-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minute)
    }

    fn point(minute: i64) -> ActivityPoint {
        ActivityPoint {
            timestamp: at(minute),
            model: "m".into(),
        }
    }

    fn session(id: &str, repo_id: &str, label: &str, minutes: &[i64]) -> Session {
        Session {
            provider: "pi".into(),
            session_id: id.into(),
            cwd: "/nowhere/at/all".into(),
            repo: label.into(),
            repo_id: repo_id.into(),
            root: "~/code".into(),
            points: minutes.iter().map(|m| point(*m)).collect(),
            exact_intervals: Vec::new(),
            human_points: minutes.iter().map(|m| point(*m)).collect(),
            token_events: Vec::new(),
            is_subagent: false,
            source_file: PathBuf::new(),
            branches: Vec::new(),
            branch_source: BranchSource::None,
            pull_requests: Vec::new(),
        }
    }

    fn commit(
        member: &str,
        repo_id: &str,
        label: &str,
        sha: &str,
        authorship: Authorship,
    ) -> GitCommit {
        let mut categories = CategoryTally::default();
        categories.add(active_registry().index_of("source").unwrap(), 8, 2);
        GitCommit {
            sha: sha.into(),
            timestamp: at(5),
            repo: label.into(),
            repo_id: repo_id.into(),
            repo_member_id: member.into(),
            cwd: "/nowhere/at/all".into(),
            root: "~/code".into(),
            additions: 8,
            deletions: 2,
            files: vec!["src/lib.rs".into()],
            ignored_additions: 0,
            ignored_deletions: 0,
            categories,
            authorship,
            branch: Some("feat/x".into()),
            branch_source: BranchSource::Unique,
        }
    }

    fn machine(id_digit: char, label: &str) -> Machine {
        Machine {
            id: id_digit.to_string().repeat(32),
            label: label.into(),
        }
    }

    fn input<'a>(
        sessions: &'a [Session],
        commits: &'a [GitCommit],
        agent: &'a [GitCommit],
        authors: &'a [String],
    ) -> ExportInput<'a> {
        ExportInput {
            sessions,
            commits,
            agent_commits: agent,
            window: (None, None),
            authors,
            human_idle: Duration::hours(1),
            review_credit: Duration::minutes(30),
            gap_cap: Duration::minutes(5),
            now: at(0),
        }
    }

    fn write(directory: &Path, name: &str, bundle: &Bundle) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, serde_json::to_vec(bundle).unwrap()).unwrap();
        path
    }

    struct Merged {
        sessions: Vec<Session>,
        commits: Vec<GitCommit>,
        agent_commits: Vec<GitCommit>,
        diagnostics: Diagnostics,
    }

    fn merge(
        files: &[PathBuf],
        local_sessions: Vec<Session>,
        local_commits: Vec<GitCommit>,
        repo_filter: Option<&str>,
    ) -> Result<Merged> {
        let allow_all = |_: &str| true;
        let mut merged = Merged {
            sessions: local_sessions,
            commits: local_commits,
            agent_commits: Vec::new(),
            diagnostics: Diagnostics::default(),
        };
        merge_imports(
            &ImportRequest {
                files,
                window: (None, None),
                repo_filter,
                path_filtered: false,
                local_authors: &[],
                human_idle: Duration::hours(1),
                review_credit: Duration::minutes(30),
                gap_cap: Duration::minutes(5),
                provider_enabled: &allow_all,
            },
            &mut merged.sessions,
            &mut merged.commits,
            &mut merged.agent_commits,
            &ProjectAliases::default(),
            &mut merged.diagnostics,
        )?;
        Ok(merged)
    }

    fn authors(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn an_exported_session_id_is_a_stable_opaque_hash_of_provider_and_id() {
        let id = "uuid:-Users-alice-secret-client-api/uuid.jsonl";
        let opaque = opaque_session_id("claude", id);
        assert_eq!(32, opaque.len());
        assert!(opaque.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!opaque.contains("alice"));
        // The same session on another machine hashes the same; another
        // provider's identical id, or another id, does not.
        assert_eq!(opaque, opaque_session_id("claude", id));
        assert_ne!(opaque, opaque_session_id("pi", id));
        assert_ne!(opaque, opaque_session_id("claude", "uuid"));
        // A boundary between provider and id cannot be shifted.
        assert_ne!(opaque_session_id("ab", "c"), opaque_session_id("a", "bc"));
    }

    #[test]
    fn a_built_bundle_exports_no_raw_session_id() {
        let mut local = session("uuid:/Users/alice/work/api", "remote:x/api", "api", &[1, 2]);
        local.cwd = "/Users/alice/work/api".into();
        let bundle = build_bundle(
            &ExportInput {
                sessions: &[local.clone()],
                commits: &[],
                agent_commits: &[],
                authors: &[],
                window: (None, None),
                now: at(0),
                human_idle: Duration::hours(1),
                review_credit: Duration::minutes(30),
                gap_cap: Duration::minutes(5),
            },
            &machine('a', "laptop"),
            false,
        );
        let text = serde_json::to_string(&bundle).unwrap();
        assert!(!text.contains("alice"), "{text}");
        assert_eq!(
            opaque_session_id(&local.provider, &local.session_id),
            bundle.sessions[0].session_id
        );
    }

    fn mark(from: Option<i64>, branch: &str) -> BranchMark {
        BranchMark {
            from: from.map(at),
            branch: branch.into(),
        }
    }

    #[test]
    fn imported_marks_are_sorted_with_one_start_and_no_repeats() {
        let mut dropped = 0;
        let marks = bounded_marks(
            vec![
                mark(Some(30), "c"),
                mark(None, "a"),
                mark(Some(10), "b"),
                // A second start, a repeat of the branch before it, an invalid
                // name: each is dropped and counted.
                mark(None, "z"),
                mark(Some(20), "b"),
                mark(Some(40), ""),
            ],
            &mut dropped,
        );
        let names: Vec<&str> = marks.iter().map(|mark| mark.branch.as_str()).collect();
        assert_eq!(vec!["a", "b", "c"], names);
        assert_eq!(None, marks[0].from);
        assert_eq!(3, dropped);
        // So the branch in force follows the clock, which an unsorted list
        // would have got wrong.
        assert_eq!(Some("a"), crate::model::branch_at(&marks, at(5)));
        assert_eq!(Some("b"), crate::model::branch_at(&marks, at(15)));
        assert_eq!(Some("c"), crate::model::branch_at(&marks, at(35)));
    }

    #[test]
    fn a_new_machine_file_holds_a_random_id_and_keeps_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/machine.json");
        let first = ensure_machine(&path, Some("laptop"), None).unwrap();
        assert!(valid_machine_id(&first.id));
        assert_eq!("laptop", first.label);
        // Neither the id nor the label changes when nothing is asked for.
        let again = ensure_machine(&path, None, Some("ignored-host".into())).unwrap();
        assert_eq!(first, again);
        // A requested label replaces the stored one and the id stays.
        let renamed = ensure_machine(&path, Some("desktop"), None).unwrap();
        assert_eq!(first.id, renamed.id);
        assert_eq!("desktop", ensure_machine(&path, None, None).unwrap().label);
        assert_ne!(random_machine_id(), random_machine_id());
    }

    #[test]
    fn the_host_name_labels_a_machine_only_when_nothing_else_does() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("machine.json");
        let error = ensure_machine(&path, None, None).unwrap_err().to_string();
        assert!(error.contains("--label"), "{error}");
        assert!(!path.exists(), "a failed label must not leave an id behind");
        assert_eq!(
            "build-host",
            ensure_machine(&path, None, Some("build-host".into()))
                .unwrap()
                .label
        );
        for bad in ["", "a/b", "a\\b", "a@b", "tab\there", &"x".repeat(65)] {
            assert!(validate_label(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_unreadable_machine_file_is_an_error_not_a_new_machine() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("machine.json");
        fs::write(&path, "not json").unwrap();
        assert!(ensure_machine(&path, Some("laptop"), None).is_err());
        assert_eq!("not json", fs::read_to_string(&path).unwrap());
        fs::write(&path, r#"{"version":1,"id":"short","label":"x"}"#).unwrap();
        assert!(ensure_machine(&path, Some("laptop"), None).is_err());
    }

    #[test]
    fn only_remotes_and_projects_are_portable() {
        assert!(portable_key("remote:github.com/acme/api"));
        assert!(portable_key("project:acme"));
        assert!(!portable_key("git:/home/me/api/.git"));
        assert!(!portable_key("path:/home/me/scratch"));
        // Remotes that name a directory on this disk are paths in disguise.
        assert!(!portable_key("remote:/srv/git/api"));
        assert!(!portable_key("remote:C:/git/api"));
        assert!(!portable_key("remote:\\\\server\\share\\api"));
    }

    #[test]
    fn a_collision_suffix_is_removed_before_export() {
        let decorated = disambiguated_repository_label("api", "remote:host/a/api");
        assert_eq!("api", plain_label(&decorated, "remote:host/a/api"));
        assert_eq!("api", plain_label("api", "remote:host/a/api"));
        assert_eq!(decorated, plain_label(&decorated, "remote:host/b/api"));
    }

    #[test]
    fn subdirectories_are_relative_plain_and_bounded() {
        assert_eq!("a/b", clean_subdir("a/b"));
        assert_eq!("a/b", clean_subdir("/a/../b/./"));
        assert_eq!("a/b", clean_subdir("a\\..\\b"));
        assert_eq!("", clean_subdir(&"x/".repeat(400)));
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("repo/src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(directory.path().join("repo/.git")).unwrap();
        assert_eq!("src/deep", subdir_of(&nested.to_string_lossy()));
        assert_eq!(
            "",
            subdir_of(&directory.path().join("repo").to_string_lossy())
        );
        // Outside a checkout, or gone, there is nothing relative to report.
        assert_eq!("", subdir_of(&directory.path().to_string_lossy()));
        assert_eq!(
            "",
            subdir_of(&directory.path().join("gone").to_string_lossy())
        );
    }

    #[test]
    fn a_union_keeps_what_each_side_has_and_counts_repeats_once_per_side() {
        let mut target = vec![1, 1, 2];
        let added = union_by(&mut target, vec![1, 2, 2, 3, 1], |value| *value);
        // Present twice on both sides stays twice; extras on the incoming side arrive.
        assert_eq!(vec![1, 1, 2, 2, 3], target);
        assert_eq!(2, added);
    }

    #[test]
    fn the_same_session_in_two_bundles_is_one_session_with_the_union_of_its_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let people = authors(&["ada@example.com"]);
        let key = "remote:github.com/acme/api";
        let mut laptop_session = session("s1", key, "api", &[0, 10]);
        laptop_session.token_events = vec![TokenEvent {
            timestamp: at(10),
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 6,
                ..TokenUsage::default()
            },
        }];
        let mut synced = session("s1", key, "api", &[10, 20]);
        synced.token_events = laptop_session.token_events.clone();
        let first = build_bundle(
            &input(&[laptop_session], &[], &[], &people),
            &machine('a', "laptop"),
            false,
        );
        let second = build_bundle(
            &input(&[synced], &[], &[], &people),
            &machine('b', "desktop"),
            false,
        );
        let files = [
            write(directory.path(), "a.json", &first),
            write(directory.path(), "b.json", &second),
        ];
        let merged = merge(&files, Vec::new(), Vec::new(), None).unwrap();
        assert_eq!(1, merged.sessions.len());
        let only = &merged.sessions[0];
        let minutes: Vec<_> = only.points.iter().map(|p| p.timestamp).collect();
        assert_eq!(vec![at(0), at(10), at(20)], minutes);
        assert_eq!(
            1,
            only.token_events.len(),
            "the shared token event is counted once"
        );
        assert_eq!("api", only.repo);
        assert_eq!(key, only.repo_id);
        assert!(only.cwd.starts_with("api@laptop"), "{}", only.cwd);
        assert!(!Path::new(&only.cwd).is_dir());
    }

    #[test]
    fn a_session_the_machine_already_has_gains_evidence_and_keeps_its_identity() {
        let directory = tempfile::tempdir().unwrap();
        let key = "remote:github.com/acme/api";
        let bundle = build_bundle(
            &input(&[session("s1", key, "api", &[0, 30])], &[], &[], &[]),
            &machine('a', "laptop"),
            false,
        );
        let file = write(directory.path(), "a.json", &bundle);
        let mut local = session("s1", key, "api", &[0, 10]);
        local.cwd = "/home/me/api".into();
        let merged = merge(&[file], vec![local], Vec::new(), None).unwrap();
        assert_eq!(1, merged.sessions.len());
        assert_eq!("/home/me/api", merged.sessions[0].cwd);
        assert_eq!(3, merged.sessions[0].points.len());
    }

    #[test]
    fn a_commit_is_counted_once_across_bundles_and_the_local_copy_wins() {
        let directory = tempfile::tempdir().unwrap();
        let key = "remote:github.com/acme/api";
        let theirs = commit(key, key, "api", "abc123", Authorship::default());
        let first = build_bundle(
            &input(&[], std::slice::from_ref(&theirs), &[], &[]),
            &machine('a', "laptop"),
            false,
        );
        let second = build_bundle(
            &input(&[], std::slice::from_ref(&theirs), &[], &[]),
            &machine('b', "desktop"),
            false,
        );
        let files = [
            write(directory.path(), "a.json", &first),
            write(directory.path(), "b.json", &second),
        ];
        assert_eq!(
            1,
            merge(&files, Vec::new(), Vec::new(), None)
                .unwrap()
                .commits
                .len()
        );

        let mut local = commit(key, key, "api", "abc123", Authorship::default());
        local.additions = 99;
        let merged = merge(&files, Vec::new(), vec![local], None).unwrap();
        assert_eq!(1, merged.commits.len());
        assert_eq!(99, merged.commits[0].additions);
    }

    #[test]
    fn authorship_and_categories_survive_the_trip_and_unknown_categories_become_other() {
        let directory = tempfile::tempdir().unwrap();
        let key = "remote:github.com/acme/api";
        let mut assisted = Authorship::default();
        assisted.note_co_author("<noreply@anthropic.com>");
        let mut autofix = Authorship::default();
        autofix.note_co_author("github-code-quality[bot]@");
        let commits = vec![
            commit(key, key, "api", "a1", Authorship::default()),
            commit(key, key, "api", "a2", assisted),
            commit(key, key, "api", "a3", autofix),
        ];
        let agent = vec![commit(key, key, "api", "a4", Authorship::agent())];
        let mut bundle = build_bundle(
            &input(&[], &commits, &agent, &[]),
            &machine('a', "laptop"),
            false,
        );
        for exported in &mut bundle.commits {
            exported.categories.insert("novel".into(), [3, 1]);
        }
        let file = write(directory.path(), "a.json", &bundle);
        let merged = merge(&[file], Vec::new(), Vec::new(), None).unwrap();
        assert_eq!(3, merged.commits.len());
        assert_eq!(1, merged.agent_commits.len());
        let by_sha = |sha: &str| merged.commits.iter().find(|c| c.sha == sha).unwrap();
        assert!(by_sha("a2").authorship.is_agent_assisted());
        assert!(!by_sha("a2").authorship.is_autofix_assisted());
        assert!(by_sha("a3").authorship.is_autofix_assisted());
        assert!(!by_sha("a1").authorship.is_agent_assisted());
        assert!(merged.agent_commits[0].authorship.is_agent_authored());
        let registry = active_registry();
        let source = registry.index_of("source").unwrap();
        let other = registry.index_of("other").unwrap();
        assert_eq!(8, by_sha("a1").categories.get(source).additions);
        assert_eq!(3, by_sha("a1").categories.get(other).additions);
        assert!(
            merged
                .diagnostics
                .notes
                .iter()
                .any(|n| n.contains("novel") && n.contains("other"))
        );
        // Branch data came along, and the default bundle carries no file paths.
        assert_eq!(Some("feat/x"), by_sha("a1").branch.as_deref());
        assert!(by_sha("a1").files.is_empty());
        assert!(
            merged
                .diagnostics
                .notes
                .iter()
                .any(|n| n.contains("no file paths"))
        );
    }

    #[test]
    fn repositories_without_a_remote_are_kept_apart_per_machine_and_warned_about() {
        let directory = tempfile::tempdir().unwrap();
        let build = |digit: char, label: &str| {
            let sessions = [session(
                &format!("s-{digit}"),
                "git:/home/me/scratch/.git",
                "scratch",
                &[0, 5],
            )];
            let commits = [commit(
                "git:/home/me/scratch/.git",
                "git:/home/me/scratch/.git",
                "scratch",
                "abc123",
                Authorship::default(),
            )];
            build_bundle(
                &input(&sessions, &commits, &[], &[]),
                &machine(digit, label),
                false,
            )
        };
        let (one, two) = (build('a', "laptop"), build('b', "desktop"));
        assert!(
            one.repositories[0]
                .key
                .starts_with(&format!("local:{}:", "a".repeat(32)))
        );
        assert!(!one.repositories[0].portable);
        let files = [
            write(directory.path(), "a.json", &one),
            write(directory.path(), "b.json", &two),
        ];
        let merged = merge(&files, Vec::new(), Vec::new(), None).unwrap();
        // Same label and same sha: still two of each, because
        // nothing says they are the same repository.
        assert_eq!(2, merged.sessions.len());
        assert_eq!(2, merged.commits.len());
        assert_ne!(merged.sessions[0].repo_id, merged.sessions[1].repo_id);
        assert_eq!(
            2,
            merged
                .diagnostics
                .messages
                .iter()
                .filter(|m| m.contains("no shared remote"))
                .count()
        );
        // No local path survives into the bundle.
        let text = serde_json::to_string(&one).unwrap();
        assert!(!text.contains("/home/me"), "{text}");
    }

    #[test]
    fn two_local_repositories_with_one_label_stay_two_repositories() {
        let sessions = [
            session("s1", "git:/a/api/.git", "api", &[0]),
            session("s2", "git:/b/api/.git", "api", &[0]),
        ];
        let bundle = build_bundle(
            &input(&sessions, &[], &[], &[]),
            &machine('a', "laptop"),
            false,
        );
        assert_eq!(2, bundle.repositories.len());
        assert_ne!(bundle.sessions[0].repo, bundle.sessions[1].repo);
    }

    #[test]
    fn a_report_is_refused_by_what_it_is() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        fs::write(
            &path,
            r#"{"methodology":{"human_work":"x"},"summary":{},"rows":[]}"#,
        )
        .unwrap();
        let error = merge(&[path], Vec::new(), Vec::new(), None)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("a report is a conclusion"), "{error}");
        assert!(error.contains("workstats export"), "{error}");
    }

    #[test]
    fn files_that_are_not_bundles_of_this_version_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let refused = |body: &str| {
            let path = directory.path().join("x.json");
            fs::write(&path, body).unwrap();
            merge(&[path], Vec::new(), Vec::new(), None)
                .err()
                .unwrap()
                .to_string()
        };
        assert!(refused("nonsense").contains("not a workstats bundle"));
        assert!(refused("{}").contains("not a workstats bundle"));
        assert!(refused(r#"{"format":"workstats-bundle","version":2}"#).contains("version 2"));
        assert!(refused(r#"{"format":"workstats-bundle"}"#).contains("version unknown"));
        let mut bundle = build_bundle(&input(&[], &[], &[], &[]), &machine('a', "laptop"), false);
        bundle.sessions.push(BundleSession {
            provider: "pi".into(),
            session_id: "s".into(),
            repo: "remote:unlisted".into(),
            subdir: String::new(),
            is_subagent: false,
            branch_source: BranchSource::None,
            branches: Vec::new(),
            points: Vec::new(),
            human_points: Vec::new(),
            exact_intervals: Vec::new(),
            token_events: Vec::new(),
            pull_requests: Vec::new(),
        });
        let path = write(directory.path(), "bad.json", &bundle);
        let error = merge(&[path], Vec::new(), Vec::new(), None).err().unwrap();
        assert!(format!("{error:#}").contains("does not list"), "{error:#}");
    }

    #[test]
    fn bundles_for_different_people_are_not_merged() {
        let directory = tempfile::tempdir().unwrap();
        let ada = build_bundle(
            &input(&[], &[], &[], &authors(&["Ada@example.com"])),
            &machine('a', "laptop"),
            false,
        );
        let same = build_bundle(
            &input(&[], &[], &[], &authors(&["ada@example.com "])),
            &machine('b', "desktop"),
            false,
        );
        let grace = build_bundle(
            &input(&[], &[], &[], &authors(&["grace@example.com"])),
            &machine('c', "other"),
            false,
        );
        let (a, b, c) = (
            write(directory.path(), "a.json", &ada),
            write(directory.path(), "b.json", &same),
            write(directory.path(), "c.json", &grace),
        );
        // Spelling and case are not a different person.
        merge(&[a.clone(), b], Vec::new(), Vec::new(), None).unwrap();
        let error = merge(&[a, c], Vec::new(), Vec::new(), None)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("team merges are not supported yet"),
            "{error}"
        );
    }

    #[test]
    fn the_repo_filter_and_provider_filter_apply_to_imported_work() {
        let people = authors(&["ada@example.com"]);
        let directory = tempfile::tempdir().unwrap();
        let mut codex = session("s2", "remote:github.com/acme/web", "web", &[0]);
        codex.provider = "codex".into();
        let sessions = [
            session("s1", "remote:github.com/acme/api", "api", &[0]),
            codex,
        ];
        let commits = [
            commit(
                "remote:github.com/acme/api",
                "remote:github.com/acme/api",
                "api",
                "a1",
                Authorship::default(),
            ),
            commit(
                "remote:github.com/acme/web",
                "remote:github.com/acme/web",
                "web",
                "b1",
                Authorship::default(),
            ),
        ];
        let bundle = build_bundle(
            &input(&sessions, &commits, &[], &people),
            &machine('a', "laptop"),
            false,
        );
        let file = write(directory.path(), "a.json", &bundle);
        let filtered = merge(
            std::slice::from_ref(&file),
            Vec::new(),
            Vec::new(),
            Some("API"),
        )
        .unwrap();
        assert_eq!(1, filtered.sessions.len());
        assert_eq!("api", filtered.sessions[0].repo);
        assert_eq!(1, filtered.commits.len());
        // The owner part of the key matches too, as a path would locally.
        let owner = merge(
            std::slice::from_ref(&file),
            Vec::new(),
            Vec::new(),
            Some("acme/web"),
        )
        .unwrap();
        assert_eq!(1, owner.sessions.len());
        // A provider the run excludes is not imported.
        let mut merged = Merged {
            sessions: Vec::new(),
            commits: Vec::new(),
            agent_commits: Vec::new(),
            diagnostics: Diagnostics::default(),
        };
        let only_pi = |provider: &str| provider == "pi";
        merge_imports(
            &ImportRequest {
                files: &[file],
                window: (None, None),
                repo_filter: None,
                path_filtered: true,
                local_authors: &authors(&["someone@else.com"]),
                human_idle: Duration::hours(2),
                review_credit: Duration::minutes(30),
                gap_cap: Duration::minutes(5),
                provider_enabled: &only_pi,
            },
            &mut merged.sessions,
            &mut merged.commits,
            &mut merged.agent_commits,
            &ProjectAliases::default(),
            &mut merged.diagnostics,
        )
        .unwrap();
        assert_eq!(1, merged.sessions.len());
        assert_eq!("pi", merged.sessions[0].provider);
        // The path filter cannot apply, and a different local author and
        // different settings are said out loud rather than ignored.
        let warnings = merged.diagnostics.messages.join("\n");
        assert!(warnings.contains("--path"), "{warnings}");
        assert!(warnings.contains("someone@else.com"), "{warnings}");
        assert!(
            merged
                .diagnostics
                .notes
                .iter()
                .any(|n| n.contains("human_idle 1h"))
        );
    }

    #[test]
    fn a_bundle_that_covers_less_than_the_window_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let mut bundle = build_bundle(&input(&[], &[], &[], &[]), &machine('a', "laptop"), false);
        bundle.window = BundleWindow {
            since: Some(at(0)),
            until: Some(at(60)),
        };
        let file = write(directory.path(), "a.json", &bundle);
        let merged = merge(&[file], Vec::new(), Vec::new(), None).unwrap();
        let notes = merged.diagnostics.notes.join("\n");
        assert!(notes.contains("starts at"), "{notes}");
        assert!(notes.contains("ends at"), "{notes}");
    }

    #[test]
    fn a_project_alias_on_the_importer_regroups_a_remote() {
        use crate::paths::{Config, ProjectAliasConfig};
        let config = Config {
            project_aliases: BTreeMap::from([(
                "acme".to_string(),
                ProjectAliasConfig {
                    label: "Acme Product".into(),
                    remotes: vec!["https://github.com/acme/api.git".into()],
                    paths: Vec::new(),
                },
            )]),
            ..Config::default()
        };
        let aliases = config.compiled_project_aliases(Path::new("/home")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let key = "remote:github.com/acme/api";
        let bundle = build_bundle(
            &input(
                &[session("s1", key, "api", &[0])],
                &[commit(key, key, "api", "a1", Authorship::default())],
                &[],
                &[],
            ),
            &machine('a', "laptop"),
            false,
        );
        let file = write(directory.path(), "a.json", &bundle);
        let allow_all = |_: &str| true;
        let (mut sessions, mut commits, mut agents) = (Vec::new(), Vec::new(), Vec::new());
        merge_imports(
            &ImportRequest {
                files: &[file],
                window: (None, None),
                repo_filter: None,
                path_filtered: false,
                local_authors: &[],
                human_idle: Duration::hours(1),
                review_credit: Duration::minutes(30),
                gap_cap: Duration::minutes(5),
                provider_enabled: &allow_all,
            },
            &mut sessions,
            &mut commits,
            &mut agents,
            &aliases,
            &mut Diagnostics::default(),
        )
        .unwrap();
        assert_eq!("project:acme", sessions[0].repo_id);
        assert_eq!("Acme Product", sessions[0].repo);
        assert_eq!("project:acme", commits[0].repo_id);
        // The natural identity stays, so a commit is still deduplicated by it.
        assert_eq!(key, commits[0].repo_member_id);
    }

    #[test]
    fn an_exporter_side_project_groups_its_commits_with_its_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let key = "remote:github.com/acme/api";
        let sessions = [session("s1", "project:acme", "Acme Product", &[0])];
        let commits = [commit(
            key,
            "project:acme",
            "Acme Product",
            "a1",
            Authorship::default(),
        )];
        let bundle = build_bundle(
            &input(&sessions, &commits, &[], &[]),
            &machine('a', "laptop"),
            false,
        );
        let file = write(directory.path(), "a.json", &bundle);
        let merged = merge(&[file], Vec::new(), Vec::new(), None).unwrap();
        assert_eq!("project:acme", merged.sessions[0].repo_id);
        assert_eq!("project:acme", merged.commits[0].repo_id);
        assert_eq!(merged.sessions[0].repo, merged.commits[0].repo);
    }

    #[test]
    fn file_paths_are_exported_only_when_asked() {
        let key = "remote:github.com/acme/api";
        let commits = [commit(key, key, "api", "a1", Authorship::default())];
        let without = build_bundle(
            &input(&[], &commits, &[], &[]),
            &machine('a', "laptop"),
            false,
        );
        assert!(without.commits[0].files.is_none());
        assert!(!serde_json::to_string(&without).unwrap().contains("lib.rs"));
        let with = build_bundle(
            &input(&[], &commits, &[], &[]),
            &machine('a', "laptop"),
            true,
        );
        assert_eq!(Some(vec!["src/lib.rs".to_string()]), with.commits[0].files);
    }

    #[test]
    fn imported_branch_marks_are_bounded_like_the_providers_bound_them() {
        let mut dropped = 0;
        let marks = vec![
            BranchMark {
                from: None,
                branch: "ok".into(),
            },
            BranchMark {
                from: Some(at(1)),
                branch: "bad\nname".into(),
            },
            BranchMark {
                from: Some(at(2)),
                branch: "x".repeat(300),
            },
            BranchMark {
                from: Some(at(3)),
                branch: String::new(),
            },
        ];
        assert_eq!(1, bounded_marks(marks, &mut dropped).len());
        assert_eq!(3, dropped);
    }

    #[test]
    fn the_default_output_name_is_a_safe_file_name() {
        assert_eq!(
            "workstats-bundle-my_laptop.json",
            default_output_name("my laptop")
        );
    }
}
