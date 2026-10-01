//! Engagements: which client or contract a piece of work bills to. The rules
//! are configuration read once at startup and shared, so grouping a report by
//! `engagement` and building a timesheet read the same answer.
//!
//! Engagements sit beside `project_aliases` and do not replace them: an alias
//! says these repositories are one product, an engagement says this work bills
//! to this client. Matching goes tier by tier and the first tier with a match
//! wins; see [`Engagements::label_for`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::attribution::{self, Ctx};
use crate::paths::{ProjectAliasConfig, expand_path, remote_repository};

/// What an interval, signal, token or commit with no engagement is labelled.
pub const UNASSIGNED: &str = "(unassigned)";

const MAX_ENGAGEMENTS: usize = 64;
const MAX_MATCHERS: usize = 64;
const MAX_TEXT_BYTES: usize = 256;

/// How an engagement is named in a vendor CSV. Anything left out falls back to
/// the engagement's own label and client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportMeta {
    pub project: Option<String>,
    pub client: Option<String>,
    pub task: Option<String>,
    pub tags: Vec<String>,
}

/// One compiled engagement.
#[derive(Clone, Debug)]
pub struct Engagement {
    pub key: String,
    pub label: String,
    pub client: Option<String>,
    pub billable: bool,
    pub rate: Option<f64>,
    pub currency: Option<String>,
    pub export: ExportMeta,
}

#[derive(Debug, Default)]
pub struct Engagements {
    /// In key order, so every tie below resolves the same way on every run.
    items: Vec<Engagement>,
    issue_prefixes: HashMap<String, usize>,
    branch_globs: Vec<(GlobSet, usize)>,
    /// `project:<alias>` and `remote:<identity>` repository ids.
    repo_ids: HashMap<String, usize>,
    remote_globs: Vec<(GlobSet, usize)>,
    paths: Vec<(PathBuf, usize)>,
    fallback: Option<usize>,
    fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EngagementConfig {
    label: Option<String>,
    client: Option<String>,
    billable: Option<bool>,
    rate: Option<f64>,
    currency: Option<String>,
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    remotes: Vec<String>,
    #[serde(default)]
    remote_globs: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    issue_prefixes: Vec<String>,
    export: Option<ExportConfig>,
    #[serde(default)]
    fallback: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportConfig {
    project: Option<String>,
    client: Option<String>,
    task: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

impl Engagements {
    /// Compiles and validates the `engagements` config block. `aliases` are
    /// the config's project aliases: an engagement may name one in `projects`,
    /// and may not claim a remote an alias already absorbed. Every error names
    /// the engagement and the key at fault.
    pub fn compile(
        config: Option<&Value>,
        aliases: &BTreeMap<String, ProjectAliasConfig>,
        home: &Path,
    ) -> Result<Self> {
        compile(config, aliases, home).context("invalid \"engagements\" configuration")
    }

    /// The engagement key the context belongs to, or `(unassigned)`.
    ///
    /// The tiers, first match wins:
    /// 1. `issue_prefixes`, applied to the issue the branch names;
    /// 2. `branches` globs;
    /// 3. `projects` and `remotes` (the repository id);
    /// 4. `remote_globs`;
    /// 5. `paths`, the longest prefix of the working directory;
    /// 6. the one engagement with `fallback: true`.
    ///
    /// Inside a tier two engagements can overlap only through globs, and the
    /// one whose key sorts first wins, so the answer never depends on order of
    /// arrival.
    pub fn label_for(&self, context: &Ctx<'_>) -> String {
        if self.items.is_empty() {
            return UNASSIGNED.to_string();
        }
        let issue = context
            .branch
            .map(|branch| attribution::issue_label(Some(branch)))
            .filter(|issue| issue != attribution::UNKNOWN);
        self.match_with_issue(context, issue.as_deref())
    }

    fn match_with_issue(&self, context: &Ctx<'_>, issue: Option<&str>) -> String {
        self.index_of(context, issue).map_or_else(
            || UNASSIGNED.to_string(),
            |index| self.items[index].key.clone(),
        )
    }

    fn index_of(&self, context: &Ctx<'_>, issue: Option<&str>) -> Option<usize> {
        if let Some((prefix, number)) = issue.and_then(|issue| issue.rsplit_once('-'))
            && !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && let Some(index) = self.issue_prefixes.get(&prefix.to_ascii_uppercase())
        {
            return Some(*index);
        }
        if let Some(branch) = context.branch
            && let Some((_, index)) = self
                .branch_globs
                .iter()
                .find(|(set, _)| set.is_match(branch))
        {
            return Some(*index);
        }
        if let Some(index) = self.repo_ids.get(context.repo_id) {
            return Some(*index);
        }
        if let Some(identity) = context.repo_id.strip_prefix("remote:")
            && let Some((_, index)) = self
                .remote_globs
                .iter()
                .find(|(set, _)| set.is_match(identity))
        {
            return Some(*index);
        }
        let cwd = Path::new(context.cwd);
        let longest = self
            .paths
            .iter()
            .filter(|(prefix, _)| cwd.starts_with(prefix))
            .max_by_key(|(prefix, _)| prefix.components().count());
        if let Some((_, index)) = longest {
            return Some(*index);
        }
        self.fallback
    }

    /// The engagement with this key.
    pub fn get(&self, key: &str) -> Option<&Engagement> {
        self.items.iter().find(|engagement| engagement.key == key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|engagement| engagement.key.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether any engagement is matched by a branch or an issue, so labelling
    /// work needs the branches it was done on.
    pub fn uses_branches(&self) -> bool {
        !self.issue_prefixes.is_empty() || !self.branch_globs.is_empty()
    }

    /// A digest of the configuration the engagements were compiled from, so a
    /// lock can tell that the rules changed after it was taken.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn compile(
    config: Option<&Value>,
    aliases: &BTreeMap<String, ProjectAliasConfig>,
    home: &Path,
) -> Result<Engagements> {
    let Some(value) = config else {
        return Ok(Engagements::default());
    };
    let Value::Object(map) = value else {
        bail!("it must be an object keyed by engagement id");
    };
    if map.len() > MAX_ENGAGEMENTS {
        bail!("at most {MAX_ENGAGEMENTS} engagements are supported");
    }
    let valid_key = Regex::new(r"^[a-z][a-z0-9_-]{0,63}$").expect("static regex");
    let valid_prefix = Regex::new(r"^[A-Za-z][A-Za-z0-9_]{0,31}$").expect("static regex");

    // The remotes a project alias already absorbed, by identity.
    let mut absorbed: HashMap<String, &str> = HashMap::new();
    for (alias, configured) in aliases {
        for remote in &configured.remotes {
            if let Some((_, identity)) = remote_repository(remote, Path::new("/")) {
                absorbed.insert(identity, alias);
            }
        }
    }

    let sorted: BTreeMap<&String, &Value> = map.iter().collect();
    let mut result = Engagements::default();
    let mut claimed_prefixes: HashMap<String, String> = HashMap::new();
    let mut claimed_repo_ids: HashMap<String, String> = HashMap::new();
    let mut claimed_paths: Vec<(PathBuf, String)> = Vec::new();
    for (key, raw) in sorted {
        if !valid_key.is_match(key) {
            bail!(
                "engagement key {key:?} must start with a lowercase letter and use lowercase letters, numbers, '_' or '-' (at most 64 characters)"
            );
        }
        let configured: EngagementConfig =
            serde_json::from_value(raw.clone()).with_context(|| format!("engagements.{key}"))?;
        let index = result.items.len();
        let at = |field: &str| format!("engagements.{key}.{field}");

        let label = match &configured.label {
            Some(label) => text(&at("label"), label)?,
            None => key.clone(),
        };
        let client = configured
            .client
            .as_deref()
            .map(|client| text(&at("client"), client))
            .transpose()?;
        let rate = match configured.rate {
            Some(rate) if !rate.is_finite() || rate < 0.0 => {
                bail!("{}: must be zero or more, got {rate}", at("rate"))
            }
            other => other,
        };
        let currency = match &configured.currency {
            Some(currency) => {
                let currency = currency.trim().to_ascii_uppercase();
                if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
                    bail!(
                        "{}: expects a three-letter ISO code, such as USD or NOK",
                        at("currency")
                    );
                }
                Some(currency)
            }
            None => None,
        };
        if rate.is_some() && currency.is_none() {
            bail!(
                "{}: a rate needs a currency, so amounts are never added across currencies by guesswork",
                at("currency")
            );
        }
        // A rate with no say on billing means it bills; a client with no rate
        // is internal unless it says otherwise.
        let billable = configured.billable.unwrap_or(rate.is_some());
        let export = match &configured.export {
            Some(export) => {
                if export.tags.len() > 16 {
                    bail!("{}: at most 16 tags are supported", at("export.tags"));
                }
                ExportMeta {
                    project: export
                        .project
                        .as_deref()
                        .map(|value| text(&at("export.project"), value))
                        .transpose()?,
                    client: export
                        .client
                        .as_deref()
                        .map(|value| text(&at("export.client"), value))
                        .transpose()?,
                    task: export
                        .task
                        .as_deref()
                        .map(|value| text(&at("export.task"), value))
                        .transpose()?,
                    tags: export
                        .tags
                        .iter()
                        .map(|tag| text(&at("export.tags"), tag))
                        .collect::<Result<_>>()?,
                }
            }
            None => ExportMeta::default(),
        };

        let mut matchers = 0;
        for (field, length) in [
            ("projects", configured.projects.len()),
            ("remotes", configured.remotes.len()),
            ("remote_globs", configured.remote_globs.len()),
            ("paths", configured.paths.len()),
            ("branches", configured.branches.len()),
            ("issue_prefixes", configured.issue_prefixes.len()),
        ] {
            if length > MAX_MATCHERS {
                bail!(
                    "{}: at most {MAX_MATCHERS} entries are supported",
                    at(field)
                );
            }
            matchers += length;
        }
        if matchers == 0 && !configured.fallback {
            bail!(
                "engagements.{key}: needs at least one of projects, remotes, remote_globs, paths, branches, issue_prefixes, or \"fallback\": true"
            );
        }

        for prefix in &configured.issue_prefixes {
            if !valid_prefix.is_match(prefix) {
                bail!(
                    "{}: {prefix:?} must be letters, digits or '_', starting with a letter (for example \"ACME\")",
                    at("issue_prefixes")
                );
            }
            let prefix = prefix.to_ascii_uppercase();
            if let Some(other) = claimed_prefixes.insert(prefix.clone(), key.clone()) {
                bail!("engagements {other:?} and {key:?} both claim the issue prefix {prefix:?}");
            }
            result.issue_prefixes.insert(prefix, index);
        }
        for project in &configured.projects {
            if !aliases.contains_key(project) {
                bail!(
                    "{}: {project:?} is not a project alias; define it under \"project_aliases\" first",
                    at("projects")
                );
            }
            claim_repo_id(
                &mut claimed_repo_ids,
                &mut result.repo_ids,
                format!("project:{project}"),
                key,
                index,
                &format!("project {project:?}"),
            )?;
        }
        for remote in &configured.remotes {
            let Some((_, identity)) = remote_repository(remote, Path::new("/")) else {
                bail!("{}: {remote:?} is not a usable git remote", at("remotes"));
            };
            if let Some(alias) = absorbed.get(&identity) {
                bail!(
                    "{}: {remote:?} is already part of project alias {alias:?}; reference alias {alias:?} in \"projects\" instead",
                    at("remotes")
                );
            }
            claim_repo_id(
                &mut claimed_repo_ids,
                &mut result.repo_ids,
                format!("remote:{identity}"),
                key,
                index,
                &format!("remote {remote:?}"),
            )?;
        }
        if !configured.branches.is_empty() {
            let set = glob_set(&at("branches"), &configured.branches, false)?;
            result.branch_globs.push((set, index));
        }
        if !configured.remote_globs.is_empty() {
            // Hosts and organisations are case-insensitive in practice.
            let set = glob_set(&at("remote_globs"), &configured.remote_globs, true)?;
            result.remote_globs.push((set, index));
        }
        for path in &configured.paths {
            if path.trim().is_empty() || path.len() > 1024 {
                bail!(
                    "{}: a path must not be empty or over 1024 bytes",
                    at("paths")
                );
            }
            let canonical = canonical_path(&expand_path(path, home));
            // Nested paths are allowed (a consultant keeps `~/work/acme` inside
            // `~/work`) and the longest prefix wins. Only an identical path
            // would make the answer a coin toss.
            if let Some((_, other)) = claimed_paths
                .iter()
                .find(|(claimed, _)| *claimed == canonical)
            {
                bail!("engagements {other:?} and {key:?} both claim the path {path:?}");
            }
            claimed_paths.push((canonical.clone(), key.clone()));
            result.paths.push((canonical, index));
        }
        if configured.fallback {
            if let Some(other) = result.fallback.map(|other| &result.items[other].key) {
                bail!("engagements {other:?} and {key:?} are both the fallback; only one can be");
            }
            result.fallback = Some(index);
        }
        result.items.push(Engagement {
            key: key.clone(),
            label,
            client,
            billable,
            rate,
            currency,
            export,
        });
    }
    let canonical = serde_json::to_string(value).unwrap_or_default();
    let digest: String = Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    result.fingerprint = format!("sha256:{digest}");
    Ok(result)
}

fn claim_repo_id(
    claimed: &mut HashMap<String, String>,
    index: &mut HashMap<String, usize>,
    repo_id: String,
    key: &str,
    position: usize,
    what: &str,
) -> Result<()> {
    if let Some(other) = claimed.insert(repo_id.clone(), key.to_string()) {
        bail!("engagements {other:?} and {key:?} both claim the {what}");
    }
    index.insert(repo_id, position);
    Ok(())
}

/// Display text from the config: short, one line, no control characters, so it
/// can be printed and exported without a second look.
fn text(field: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.chars().any(char::is_control) {
        bail!(
            "{field}: must be 1 to {MAX_TEXT_BYTES} bytes on one line, without control characters"
        );
    }
    Ok(value.to_string())
}

fn glob_set(field: &str, patterns: &[String], case_insensitive: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() || pattern.len() > MAX_TEXT_BYTES {
            bail!("{field}: a pattern must be 1 to {MAX_TEXT_BYTES} bytes");
        }
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .with_context(|| format!("{field}: invalid pattern {pattern:?}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("{field}: invalid patterns"))
}

/// The real path when it exists, otherwise the path with `.` and `..` folded,
/// as the project aliases do: a configured directory that does not exist yet
/// still has to compare against the canonical working directories sessions
/// carry.
fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(result) = path.canonicalize() {
        return result;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
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

static ACTIVE: OnceLock<Engagements> = OnceLock::new();

/// The engagements every piece of work is matched against; none before
/// `install`.
pub fn active() -> &'static Engagements {
    ACTIVE.get_or_init(Engagements::default)
}

/// Installs the configured engagements. Must run before anything is
/// attributed.
pub fn install(engagements: Engagements) -> Result<()> {
    ACTIVE
        .set(engagements)
        .map_err(|_| anyhow::anyhow!("the engagements were already in use"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn compile_json(value: Value) -> Result<Engagements> {
        compile_with_aliases(value, &BTreeMap::new())
    }

    fn compile_with_aliases(
        value: Value,
        aliases: &BTreeMap<String, ProjectAliasConfig>,
    ) -> Result<Engagements> {
        Engagements::compile(Some(&value), aliases, Path::new("/home/dev"))
    }

    fn alias(remotes: &[&str]) -> ProjectAliasConfig {
        serde_json::from_value(json!({"label": "Product", "remotes": remotes})).unwrap()
    }

    fn error_of(value: Value) -> String {
        format!("{:#}", compile_json(value).unwrap_err())
    }

    /// A path under the system temporary directory, spelt the way a session's
    /// canonical working directory is, so the tests hold on every platform.
    fn under(relative: &str) -> String {
        let mut path = canonical_path(&std::env::temp_dir()).join("engagement-tests");
        for part in relative.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
        }
        path.to_string_lossy().into_owned()
    }

    fn context<'a>(repo_id: &'a str, cwd: &'a str, branch: Option<&'a str>) -> Ctx<'a> {
        Ctx {
            repo_id,
            cwd,
            branch,
        }
    }

    #[test]
    fn nothing_configured_is_everything_unassigned() {
        let engagements = Engagements::compile(None, &BTreeMap::new(), Path::new("/")).unwrap();
        assert!(engagements.is_empty());
        assert_eq!(
            UNASSIGNED,
            engagements.label_for(&context("repo", "/work", Some("main")))
        );
    }

    #[test]
    fn the_tiers_are_tried_in_order_and_the_first_match_wins() {
        let aliases = BTreeMap::from([("cratis".to_string(), alias(&["git@h:x/cratis.git"]))]);
        let engagements = compile_with_aliases(
            json!({
                "byissue": {"issue_prefixes": ["ACME"]},
                "bybranch": {"branches": ["client/*"]},
                "byproject": {"projects": ["cratis"]},
                "byremote": {"remotes": ["git@github.com:acme/api.git"]},
                "byglob": {"remote_globs": ["github.com/acme/*"]},
                "bypath": {"paths": [under("work")]},
                "other": {"fallback": true},
            }),
            &aliases,
        )
        .unwrap();
        let label = |repo_id: &str, cwd: &str, branch: Option<&str>, issue: Option<&str>| {
            engagements.match_with_issue(&context(repo_id, cwd, branch), issue)
        };
        // Everything matches something; the issue prefix is the strongest.
        assert_eq!(
            "byissue",
            label(
                "project:cratis",
                &under("work/x"),
                Some("client/y"),
                Some("ACME-12")
            )
        );
        assert_eq!(
            "bybranch",
            label(
                "project:cratis",
                &under("work/x"),
                Some("client/y"),
                Some("ZZZ-1")
            )
        );
        assert_eq!(
            "byproject",
            label("project:cratis", &under("work/x"), None, None)
        );
        assert_eq!(
            "byremote",
            label("remote:github.com/acme/api", &under("work/x"), None, None)
        );
        assert_eq!(
            "byglob",
            label("remote:GitHub.com/ACME/other", &under("work/x"), None, None)
        );
        assert_eq!("bypath", label("local:x", &under("work/x"), None, None));
        assert_eq!("other", label("local:x", &under("elsewhere"), None, None));
    }

    #[test]
    fn without_a_fallback_unmatched_work_is_unassigned() {
        let engagements = compile_json(json!({"a": {"paths": [under("work/a")]}})).unwrap();
        assert_eq!(
            UNASSIGNED,
            engagements.label_for(&context("local:x", &under("work/b"), None))
        );
    }

    #[test]
    fn the_longest_path_prefix_wins_even_across_engagements() {
        let engagements = compile_json(json!({
            "all": {"paths": [under("work")]},
            "acme": {"paths": [under("work/acme")]},
        }))
        .unwrap();
        let label = |cwd: &str| engagements.label_for(&context("local:x", cwd, None));
        assert_eq!("acme", label(&under("work/acme/api/src")));
        assert_eq!("acme", label(&under("work/acme")));
        assert_eq!(
            "all",
            label(&under("work/acmeco")),
            "a prefix is a whole component"
        );
        assert_eq!("all", label(&under("work/other")));
    }

    #[test]
    fn issue_prefixes_ignore_case_and_need_a_number() {
        let engagements = compile_json(json!({"a": {"issue_prefixes": ["acme"]}})).unwrap();
        let ctx = context("local:x", "/w", Some("b"));
        assert_eq!("a", engagements.match_with_issue(&ctx, Some("ACME-7")));
        assert_eq!(
            UNASSIGNED,
            engagements.match_with_issue(&ctx, Some("ACME-x"))
        );
        assert_eq!(UNASSIGNED, engagements.match_with_issue(&ctx, Some("#45")));
        assert_eq!(UNASSIGNED, engagements.match_with_issue(&ctx, None));
    }

    #[test]
    fn an_engagement_with_no_way_to_match_is_refused_by_name() {
        let message = error_of(json!({"lonely": {"label": "Nobody"}}));
        assert!(message.contains("engagements.lonely"), "{message}");
        assert!(message.contains("fallback"), "{message}");
    }

    #[test]
    fn keys_must_follow_the_alias_rule() {
        let message = error_of(json!({"Bad Key": {"fallback": true}}));
        assert!(message.contains("\"Bad Key\""), "{message}");
    }

    #[test]
    fn a_misspelt_field_names_the_engagement_and_the_field() {
        let message = error_of(json!({"acme": {"paths": ["/w"], "ratee": 1}}));
        assert!(message.contains("engagements.acme"), "{message}");
        assert!(message.contains("ratee"), "{message}");
    }

    #[test]
    fn rates_need_a_currency_and_sensible_values() {
        let message = error_of(json!({"a": {"fallback": true, "rate": 100}}));
        assert!(message.contains("engagements.a.currency"), "{message}");
        let message = error_of(json!({"a": {"fallback": true, "rate": -1, "currency": "NOK"}}));
        assert!(message.contains("engagements.a.rate"), "{message}");
        let message = error_of(json!({"a": {"fallback": true, "rate": 1, "currency": "NOKK"}}));
        assert!(message.contains("engagements.a.currency"), "{message}");
        let message = error_of(json!({"a": {"fallback": true, "currency": "n0k"}}));
        assert!(message.contains("three-letter"), "{message}");
    }

    #[test]
    fn a_rate_means_billable_unless_the_engagement_says_otherwise() {
        let engagements = compile_json(json!({
            "paid": {"fallback": true, "rate": 1450, "currency": "nok"},
            "free": {"paths": ["/a"], "rate": 10, "currency": "EUR", "billable": false},
            "plain": {"paths": ["/b"]},
        }))
        .unwrap();
        let paid = engagements.get("paid").unwrap();
        assert!(paid.billable);
        assert_eq!(Some("NOK"), paid.currency.as_deref());
        assert!(!engagements.get("free").unwrap().billable);
        assert!(!engagements.get("plain").unwrap().billable);
        assert_eq!("plain", engagements.get("plain").unwrap().label);
    }

    #[test]
    fn the_same_claim_in_two_engagements_is_an_error_naming_both() {
        for (a, b, what) in [
            (
                json!({"issue_prefixes": ["ACME"]}),
                json!({"issue_prefixes": ["acme"]}),
                "issue prefix",
            ),
            (
                json!({"remotes": ["git@github.com:acme/api.git"]}),
                json!({"remotes": ["https://github.com/acme/api"]}),
                "remote",
            ),
            (
                json!({"paths": [under("w/a")]}),
                json!({"paths": [format!("{}/", under("w/a"))]}),
                "path",
            ),
        ] {
            let message = error_of(json!({"one": a, "two": b}));
            assert!(
                message.contains("\"one\"")
                    && message.contains("\"two\"")
                    && message.contains(what),
                "{message}"
            );
        }
    }

    #[test]
    fn two_engagements_cannot_claim_one_project_alias() {
        let aliases = BTreeMap::from([("p".to_string(), alias(&["git@h:x/p.git"]))]);
        let error = compile_with_aliases(
            json!({"one": {"projects": ["p"]}, "two": {"projects": ["p"]}}),
            &aliases,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("project \"p\""));
    }

    #[test]
    fn nested_paths_are_allowed() {
        compile_json(json!({
            "all": {"paths": [under("work")]},
            "acme": {"paths": [under("work/acme")]},
        }))
        .unwrap();
    }

    #[test]
    fn a_remote_an_alias_absorbed_must_be_reached_through_the_alias() {
        let aliases = BTreeMap::from([(
            "product".to_string(),
            alias(&["git@github.com:acme/api.git"]),
        )]);
        let error = compile_with_aliases(
            json!({"acme": {"remotes": ["https://github.com/acme/api"]}}),
            &aliases,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("engagements.acme.remotes"), "{message}");
        assert!(
            message.contains("reference alias \"product\" in \"projects\""),
            "{message}"
        );
    }

    #[test]
    fn projects_must_name_an_alias() {
        let error = compile_json(json!({"a": {"projects": ["ghost"]}})).unwrap_err();
        assert!(format!("{error:#}").contains("engagements.a.projects"));
    }

    #[test]
    fn two_fallbacks_are_an_error() {
        let message = error_of(json!({"a": {"fallback": true}, "b": {"fallback": true}}));
        assert!(message.contains("fallback"), "{message}");
    }

    #[test]
    fn bad_globs_and_prefixes_are_refused_naming_the_key() {
        let message = error_of(json!({"a": {"branches": ["[unclosed"]}}));
        assert!(message.contains("engagements.a.branches"), "{message}");
        let message = error_of(json!({"a": {"issue_prefixes": ["9X"]}}));
        assert!(
            message.contains("engagements.a.issue_prefixes"),
            "{message}"
        );
    }

    #[test]
    fn the_config_must_be_an_object() {
        let error =
            Engagements::compile(Some(&json!([])), &BTreeMap::new(), Path::new("/")).unwrap_err();
        assert!(format!("{error:#}").contains("engagements"));
    }

    #[test]
    fn the_fingerprint_follows_the_configuration() {
        let a = compile_json(json!({"a": {"fallback": true}})).unwrap();
        let b = compile_json(json!({"a": {"fallback": true}})).unwrap();
        let c = compile_json(json!({"a": {"fallback": true, "label": "A"}})).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert!(a.fingerprint().starts_with("sha256:"));
    }

    #[test]
    fn export_names_are_kept_for_the_presets() {
        let engagements = compile_json(json!({
            "a": {"fallback": true, "client": "ACME AS",
                  "export": {"project": "Platform", "task": "Dev", "tags": ["dev", "x"]}},
        }))
        .unwrap();
        let export = &engagements.get("a").unwrap().export;
        assert_eq!(Some("Platform"), export.project.as_deref());
        assert_eq!(None, export.client.as_deref());
        assert_eq!(vec!["dev", "x"], export.tags);
    }
}
