//! The one place `aggregate` asks "which branch, issue, feature or engagement
//! is this?". It sits between the aggregation code and the modules that own the
//! answers (`issues`, `engagement`), so those can grow without `aggregate.rs`
//! changing again.

use crate::engagement;
use crate::issues;

/// What an engagement can be matched on: the repository, the checkout and the
/// branch the work happened on.
// `repo_id` and `cwd` are read by the engagement matcher once it lands.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Ctx<'a> {
    pub repo_id: &'a str,
    pub cwd: &'a str,
    pub branch: Option<&'a str>,
}

/// Shown when a value is not known.
pub const UNKNOWN: &str = "—";

/// The branch itself, or `—` when none is known.
pub fn branch_label(branch: Option<&str>) -> String {
    branch.map_or_else(|| UNKNOWN.to_string(), str::to_string)
}

/// The issue key (`ACME-123`, `#45`) the branch names, or `—`.
pub fn issue_label(branch: Option<&str>) -> String {
    branch
        .and_then(|branch| issues::active().issue(branch))
        .unwrap_or_else(|| UNKNOWN.to_string())
}

/// The issue key when there is one, otherwise the branch slug, or `—`.
pub fn feature_label(branch: Option<&str>) -> String {
    branch.map_or_else(
        || UNKNOWN.to_string(),
        |branch| issues::active().feature(branch),
    )
}

/// The engagement the work bills to, or `(unassigned)`.
pub fn engagement_label(context: &Ctx<'_>) -> String {
    engagement::active().label_for(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_branches_read_as_a_dash_and_known_ones_pass_through() {
        assert_eq!("—", branch_label(None));
        assert_eq!("feat/x", branch_label(Some("feat/x")));
        assert_eq!("—", issue_label(None));
        assert_eq!("—", feature_label(None));
    }

    #[test]
    fn the_stub_rules_assign_nothing() {
        let context = Ctx {
            repo_id: "repo",
            cwd: "/repo",
            branch: Some("main"),
        };
        assert_eq!("(unassigned)", engagement_label(&context));
        assert_eq!("—", issue_label(Some("main")));
        assert_eq!("main", feature_label(Some("main")));
    }
}
