//! Engagements: which client or contract a piece of work bills to. The rules
//! are configuration read once at startup and shared. This is the stub: no
//! engagements are configured, so everything is `(unassigned)`.

use std::sync::OnceLock;

use anyhow::Result;
use serde_json::Value;

use crate::attribution::Ctx;

/// What an interval, signal, token or commit with no engagement is labelled.
pub const UNASSIGNED: &str = "(unassigned)";

#[derive(Debug, Default)]
pub struct Engagements {}

impl Engagements {
    /// Compiles and validates the `engagements` config block. The stub accepts
    /// anything.
    // Called from `report::prepare` once the pipeline split lands.
    #[allow(dead_code)]
    pub fn from_config(_config: Option<&Value>) -> Result<Self> {
        Ok(Self::default())
    }

    /// The engagement key the context belongs to, or `(unassigned)`.
    pub fn label_for(&self, _context: &Ctx<'_>) -> String {
        UNASSIGNED.to_string()
    }
}

static ACTIVE: OnceLock<Engagements> = OnceLock::new();

/// The engagements every piece of work is matched against; none before
/// `install`.
pub fn active() -> &'static Engagements {
    ACTIVE.get_or_init(Engagements::default)
}

/// Installs the configured engagements. Must run before anything is
/// attributed.
// Called from `report::prepare` once the pipeline split lands.
#[allow(dead_code)]
pub fn install(engagements: Engagements) -> Result<()> {
    ACTIVE
        .set(engagements)
        .map_err(|_| anyhow::anyhow!("the engagements were already in use"))
}
