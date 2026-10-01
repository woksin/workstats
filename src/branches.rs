//! Branch attribution: filling in the branch a session or commit belongs to
//! when the provider did not record one. This is the stub: `enrich` changes
//! nothing, so only branches a provider recorded are known.

use crate::model::{Diagnostics, GitCommit, Session};

/// Fills branches the providers did not record from Git (the checkout's HEAD
/// reflog and current HEAD for sessions; the integration-branch rule for
/// commits). Runs after the Git scan, before anything is aggregated.
// Called from `report::scan_window` once the pipeline split lands.
#[allow(dead_code)]
pub fn enrich(
    _sessions: &mut [Session],
    _commits: &mut [GitCommit],
    _agent_commits: &mut [GitCommit],
    _diagnostics: &mut Diagnostics,
) {
}
