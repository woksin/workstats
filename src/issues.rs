//! Issue keys read from branch names. The rules are configuration read once at
//! startup and shared, like the category registry. This is the stub: no rules
//! are configured, so no branch names an issue and a feature is its branch.

use std::sync::OnceLock;

use anyhow::Result;
use serde_json::Value;

#[derive(Debug, Default)]
pub struct IssueRules {}

impl IssueRules {
    /// Compiles the `issues` config block. The stub accepts anything.
    // Called from `report::prepare` once the pipeline split lands.
    #[allow(dead_code)]
    pub fn from_config(_config: Option<&Value>) -> Result<Self> {
        Ok(Self::default())
    }

    /// The issue key the branch names, if any.
    pub fn issue(&self, _branch: &str) -> Option<String> {
        None
    }

    /// The issue key when there is one, otherwise the branch slug.
    pub fn feature(&self, branch: &str) -> String {
        self.issue(branch).unwrap_or_else(|| branch.to_string())
    }
}

static ACTIVE: OnceLock<IssueRules> = OnceLock::new();

/// The rules every branch is read with; the empty rules before `install`.
pub fn active() -> &'static IssueRules {
    ACTIVE.get_or_init(IssueRules::default)
}

/// Installs the configured rules. Must run before anything is attributed.
// Called from `report::prepare` once the pipeline split lands.
#[allow(dead_code)]
pub fn install(rules: IssueRules) -> Result<()> {
    ACTIVE
        .set(rules)
        .map_err(|_| anyhow::anyhow!("the issue rules were already in use"))
}
