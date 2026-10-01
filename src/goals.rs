//! Weekly-hours and list-value-cap progress. The report carries a `GoalReport`
//! only when goals are configured and not disabled with `--no-goals`; this is
//! the shell the full implementation fills in.

use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GoalReport {}
