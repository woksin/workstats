//! `workstats calendar`: a year grid of human time per day, drawn in the
//! terminal. This holds the command's clap shape; the grid lands in a later
//! change.

use anyhow::{Result, bail};
use clap::Args;

use crate::cli::ReportArguments;

#[derive(Debug, Args)]
pub(crate) struct CalendarArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

pub(crate) fn run(_arguments: CalendarArguments) -> Result<()> {
    bail!("`workstats calendar` is not yet implemented")
}
