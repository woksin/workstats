//! Issue keys read from branch names. The rules are configuration read once at
//! startup and shared, like the category registry: every interval, signal,
//! token and commit that is grouped by `issue` or `feature` is read through the
//! same rules, so one branch can never be two issues in one report.
//!
//! Only the branch name is read. Commit subjects are deliberately not a source
//! (grouping must not depend on `--describe`), and nothing here touches Git.

use std::sync::OnceLock;

use anyhow::{Result, bail};
use regex::{Regex, RegexBuilder};
use serde_json::Value;

/// How many patterns, projects or prefixes one block may hold. The same bound
/// `configured_rules` puts on source rules, so a config cannot make every
/// branch name cost an unbounded number of regex runs.
const MAXIMUM_ENTRIES: usize = 32;
/// The longest a single pattern or prefix may be, in bytes.
const MAXIMUM_ENTRY_BYTES: usize = 256;
/// A compiled pattern may not grow past this; the `regex` crate's default is
/// ten times larger and a branch-name pattern never needs it.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// The patterns used when the config names none. Jira-style keys are matched
/// case-sensitively on purpose: `release-2` must not become an issue. The
/// GitHub form comes first so `GH-12` is issue `#12`, as GitHub numbers it,
/// rather than a Jira-style key called `GH-12`.
const DEFAULT_PATTERNS: &[&str] = &[
    r"(?i)\bgh-(?P<num>\d+)",
    r"(?P<key>[A-Z][A-Z0-9]+-\d+)",
    r"#(?P<num>\d+)",
    r"^(?P<num>\d+)-",
];

/// The prefixes stripped from a branch to make its slug when the config names
/// none. `*` stands for one path segment (`users/*/` is `users/ada/`).
const DEFAULT_STRIP_PREFIXES: &[&str] = &[
    "feature/",
    "feat/",
    "fix/",
    "bugfix/",
    "hotfix/",
    "chore/",
    "refactor/",
    "users/*/",
];

/// What a feature is when its branch names no issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fallback {
    /// The branch with its prefixes stripped.
    Slug,
    /// The branch exactly as named.
    Branch,
}

#[derive(Debug)]
pub struct IssueRules {
    /// Tried in order; the first that matches names the issue. Every one has a
    /// `key` or a `num` group, checked when it is compiled.
    patterns: Vec<Regex>,
    /// Each anchored at the start of the branch; see `strip_prefixes`.
    prefixes: Vec<Regex>,
    fallback: Fallback,
}

impl Default for IssueRules {
    /// The rules in force when nothing is configured.
    fn default() -> Self {
        // The defaults are constants checked by the tests below; a failure here
        // is a bug in this file, not in anyone's configuration.
        Self::from_config(None).expect("the default issue rules compile")
    }
}

impl IssueRules {
    /// Compiles the `issues` config block. A pattern that does not compile, a
    /// block over its limits, a key nobody reads or a wrongly-typed value is an
    /// error naming the key: a misspelled rule set that quietly matched nothing
    /// would report every branch as having no issue.
    pub fn from_config(config: Option<&Value>) -> Result<Self> {
        let empty = serde_json::Map::new();
        let block = match config {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(block)) => block,
            Some(other) => bail!(
                "invalid \"issues\" configuration: expected an object, got {}",
                kind(other)
            ),
        };
        if let Some(unknown) = block.keys().find(|key| {
            !matches!(
                key.as_str(),
                "patterns" | "projects" | "strip_prefixes" | "fallback"
            )
        }) {
            bail!(
                "invalid \"issues\" configuration: unknown key \"{unknown}\" \
                 (expected patterns, projects, strip_prefixes or fallback)"
            );
        }

        let patterns = strings(block, "patterns")?.unwrap_or_else(|| {
            DEFAULT_PATTERNS
                .iter()
                .map(|text| (*text).to_string())
                .collect()
        });
        let projects = strings(block, "projects")?.unwrap_or_default();
        let prefixes = strings(block, "strip_prefixes")?.unwrap_or_else(|| {
            DEFAULT_STRIP_PREFIXES
                .iter()
                .map(|text| (*text).to_string())
                .collect()
        });
        let fallback = match block.get("fallback") {
            None | Some(Value::Null) => Fallback::Slug,
            Some(Value::String(name)) if name == "slug" => Fallback::Slug,
            Some(Value::String(name)) if name == "branch" => Fallback::Branch,
            Some(other) => bail!(
                "invalid \"issues\" configuration: \"fallback\" must be \"slug\" or \"branch\", \
                 got {other}"
            ),
        };

        let mut compiled = Vec::new();
        for (index, pattern) in patterns.iter().enumerate() {
            let key = format!("\"patterns\"[{index}]");
            let regex = compile(&key, pattern)?;
            // A pattern with neither group has nothing to name the issue by.
            let names: Vec<&str> = regex.capture_names().flatten().collect();
            if !names.iter().any(|name| matches!(*name, "key" | "num")) {
                bail!(
                    "invalid \"issues\" configuration: {key} needs a (?P<key>…) or (?P<num>…) \
                     group to name the issue"
                );
            }
            compiled.push(regex);
        }
        if !projects.is_empty() {
            for (index, project) in projects.iter().enumerate() {
                let valid = !project.is_empty()
                    && project.len() <= 64
                    && project
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && project.starts_with(|c: char| c.is_ascii_alphabetic());
                if !valid {
                    bail!(
                        "invalid \"issues\" configuration: \"projects\"[{index}] must be letters, \
                         digits or '_', starting with a letter"
                    );
                }
            }
            // After the configured patterns, so an explicit rule still wins.
            let alternatives: Vec<String> = projects
                .iter()
                .map(|project| regex::escape(project))
                .collect();
            let pattern = format!(r"(?i)\b(?P<key>(?:{})-\d+)", alternatives.join("|"));
            compiled.push(compile("\"projects\"", &pattern)?);
        }

        let mut stripping = Vec::new();
        for (index, prefix) in prefixes.iter().enumerate() {
            let key = format!("\"strip_prefixes\"[{index}]");
            if prefix.is_empty() {
                bail!("invalid \"issues\" configuration: {key} is empty");
            }
            // `*` is one path segment; everything else is literal.
            let pattern = format!(
                "^{}",
                prefix
                    .split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join("[^/]+")
            );
            stripping.push(compile(&key, &pattern)?);
        }

        Ok(Self {
            patterns: compiled,
            prefixes: stripping,
            fallback,
        })
    }

    /// The issue key the branch names, if any: `ACME-123` for a key group
    /// (upper-cased), `#45` for a number group.
    pub fn issue(&self, branch: &str) -> Option<String> {
        for pattern in &self.patterns {
            let Some(captures) = pattern.captures(branch) else {
                continue;
            };
            if let Some(key) = captures.name("key").filter(|key| !key.as_str().is_empty()) {
                return Some(key.as_str().to_uppercase());
            }
            if let Some(number) = captures.name("num").filter(|n| !n.as_str().is_empty()) {
                // `#007` and `#7` are one issue.
                let digits = number.as_str();
                let number = digits
                    .parse::<u64>()
                    .map_or_else(|_| digits.to_string(), |value| value.to_string());
                return Some(format!("#{number}"));
            }
        }
        None
    }

    /// The issue key when there is one, otherwise the branch slug (or the whole
    /// branch, with `"fallback": "branch"`). The integration branch is its own
    /// name either way, because no prefix applies to it.
    pub fn feature(&self, branch: &str) -> String {
        if let Some(issue) = self.issue(branch) {
            return issue;
        }
        match self.fallback {
            Fallback::Branch => branch.to_string(),
            Fallback::Slug => self.slug(branch),
        }
    }

    /// The branch with its configured prefixes removed, repeatedly, so
    /// `users/ada/feature/login` is `login`. Never empty: a branch that is
    /// nothing but prefix keeps its name.
    fn slug(&self, branch: &str) -> String {
        let mut slug = branch;
        'again: loop {
            for prefix in &self.prefixes {
                if let Some(found) = prefix.find(slug)
                    && found.end() < slug.len()
                {
                    slug = &slug[found.end()..];
                    continue 'again;
                }
            }
            return slug.to_string();
        }
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// A list of strings under `key`, within the block's limits.
fn strings(block: &serde_json::Map<String, Value>, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = block.get(key) else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        bail!(
            "invalid \"issues\" configuration: \"{key}\" must be a list of strings, got {}",
            kind(value)
        );
    };
    if items.len() > MAXIMUM_ENTRIES {
        bail!(
            "invalid \"issues\" configuration: \"{key}\" holds at most {MAXIMUM_ENTRIES} entries"
        );
    }
    let mut strings = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Value::String(text) = item else {
            bail!(
                "invalid \"issues\" configuration: \"{key}\"[{index}] must be a string, got {}",
                kind(item)
            );
        };
        if text.len() > MAXIMUM_ENTRY_BYTES {
            bail!(
                "invalid \"issues\" configuration: \"{key}\"[{index}] is longer than \
                 {MAXIMUM_ENTRY_BYTES} bytes"
            );
        }
        strings.push(text.clone());
    }
    Ok(Some(strings))
}

fn compile(key: &str, pattern: &str) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|error| anyhow::anyhow!("invalid \"issues\" configuration: {key}: {error}"))
}

static ACTIVE: OnceLock<IssueRules> = OnceLock::new();

/// The rules every branch is read with; the defaults before `install`.
pub fn active() -> &'static IssueRules {
    ACTIVE.get_or_init(IssueRules::default)
}

/// Installs the configured rules. Must run before anything is attributed.
pub fn install(rules: IssueRules) -> Result<()> {
    ACTIVE
        .set(rules)
        .map_err(|_| anyhow::anyhow!("the issue rules were already in use"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rules(config: &Value) -> IssueRules {
        IssueRules::from_config(Some(config)).unwrap()
    }

    #[test]
    fn the_default_rules_read_the_common_branch_conventions() {
        let rules = IssueRules::default();
        for (branch, issue) in [
            ("feature/ACME-123-login", Some("ACME-123")),
            ("ACME-9", Some("ACME-9")),
            ("fix/PLAT-42_retry", Some("PLAT-42")),
            ("feature/#45-widget", Some("#45")),
            ("bugfix/gh-12-typo", Some("#12")),
            ("GH-12", Some("#12")),
            ("45-fix-login", Some("#45")),
            ("007-agent", Some("#7")),
            // Lower-case Jira-style keys are not guessed at.
            ("release-2", None),
            ("acme-123-login", None),
            ("main", None),
            ("users/ada/spike", None),
        ] {
            assert_eq!(issue.map(str::to_string), rules.issue(branch), "{branch}");
        }
    }

    #[test]
    fn projects_make_listed_keys_case_insensitive_and_nothing_else() {
        let rules = rules(&json!({"projects": ["ACME", "plat"]}));
        assert_eq!(Some("ACME-123".to_string()), rules.issue("acme-123-login"));
        assert_eq!(
            Some("PLAT-7".to_string()),
            rules.issue("feature/Plat-7-thing")
        );
        // Unlisted lower-case keys and look-alikes still do not match.
        assert_eq!(None, rules.issue("release-2"));
        assert_eq!(None, rules.issue("other-5-x"));
        assert_eq!(None, rules.issue("xacme-1"));
    }

    #[test]
    fn an_explicit_pattern_beats_the_projects_matcher_and_the_first_match_wins() {
        let rules = rules(&json!({
            "patterns": [r"^(?P<num>\d+)-", r"(?P<key>[A-Z]+-\d+)"],
            "projects": ["acme"]
        }));
        assert_eq!(Some("#12".to_string()), rules.issue("12-acme-3"));
        assert_eq!(Some("ACME-3".to_string()), rules.issue("x/acme-3"));
    }

    #[test]
    fn features_are_the_issue_or_the_slug() {
        let rules = IssueRules::default();
        assert_eq!("ACME-1", rules.feature("feature/ACME-1-x"));
        assert_eq!("login-page", rules.feature("feature/login-page"));
        assert_eq!("login", rules.feature("users/ada/feat/login"));
        assert_eq!("main", rules.feature("main"));
        // A branch that is only a prefix keeps its name.
        assert_eq!("feature/", rules.feature("feature/"));
    }

    #[test]
    fn the_branch_fallback_keeps_the_whole_name() {
        let rules = rules(&json!({"fallback": "branch"}));
        assert_eq!("feature/login", rules.feature("feature/login"));
        assert_eq!("ACME-1", rules.feature("feature/ACME-1"));
    }

    #[test]
    fn custom_prefixes_replace_the_defaults_and_star_is_one_segment() {
        let rules = rules(&json!({"strip_prefixes": ["team/*/"]}));
        assert_eq!("login", rules.feature("team/blue/login"));
        assert_eq!("feature/login", rules.feature("feature/login"));
        assert_eq!("red/x", rules.feature("team/blue/red/x"));
    }

    #[test]
    fn a_bad_configuration_is_refused_by_the_key_that_is_wrong() {
        let refused =
            |config: Value| format!("{:#}", IssueRules::from_config(Some(&config)).unwrap_err());

        let error = refused(json!({"patterns": ["ok(?P<key>x)", "("]}));
        assert!(error.contains("patterns"), "{error}");
        assert!(error.contains("[1]"), "{error}");

        let error = refused(json!({"patterns": ["no groups"]}));
        assert!(error.contains("patterns"), "{error}");
        assert!(error.contains("key"), "{error}");

        let error = refused(json!({"patterns": vec!["(?P<num>1)"; 33]}));
        assert!(error.contains("patterns"), "{error}");
        assert!(error.contains("32"), "{error}");

        let long = format!("(?P<key>{})", "a".repeat(300));
        let error = refused(json!({"patterns": [long]}));
        assert!(error.contains("patterns"), "{error}");
        assert!(error.contains("256"), "{error}");

        let error = refused(json!({"projects": ["ok", "no spaces"]}));
        assert!(error.contains("projects"), "{error}");

        let error = refused(json!({"strip_prefix": ["x/"]}));
        assert!(error.contains("strip_prefix"), "{error}");

        let error = refused(json!({"fallback": "nonsense"}));
        assert!(error.contains("fallback"), "{error}");

        let error = refused(json!({"patterns": "oops"}));
        assert!(error.contains("patterns"), "{error}");

        let error = refused(json!(7));
        assert!(error.contains("issues"), "{error}");
    }

    #[test]
    fn a_pattern_that_explodes_is_refused_not_run() {
        let error = format!(
            "{:#}",
            IssueRules::from_config(Some(
                &json!({"patterns": ["(?P<key>(((a{100}){100}){100}))"]})
            ))
            .unwrap_err()
        );
        assert!(error.contains("patterns"), "{error}");
    }
}
