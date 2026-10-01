//! `workstats branch` and `workstats pr`: the effort behind one branch or pull
//! request. This holds the commands' clap shapes; the report lands in a later
//! change.

use anyhow::{Result, bail};
use clap::Args;

use crate::cli::ReportArguments;

#[derive(Debug, Args)]
pub(crate) struct BranchArguments {
    #[arg(
        value_name = "NAME",
        help = "Branch to report (default: the current branch)"
    )]
    pub(crate) name: Option<String>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "REF",
        help = "Branch it was cut from (default: the integration branch)"
    )]
    pub(crate) base: Option<String>,
    #[arg(long, help = "One row per local branch")]
    pub(crate) all: bool,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "SOURCES",
        help = "Add descriptions from commits and/or sessions[=PROVIDERS]; read only when asked"
    )]
    pub(crate) describe: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct PrArguments {
    #[arg(
        value_name = "NAME",
        help = "Branch to report (default: the current branch)"
    )]
    pub(crate) name: Option<String>,
    #[arg(
        long,
        value_name = "N",
        help = "Pull request number, resolved through the sessions that mentioned it"
    )]
    pub(crate) number: Option<u64>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "REF",
        help = "Branch it was cut from (default: the integration branch)"
    )]
    pub(crate) base: Option<String>,
}

pub(crate) fn run_branch(_arguments: BranchArguments) -> Result<()> {
    bail!("`workstats branch` is not yet implemented")
}

pub(crate) fn run_pr(_arguments: PrArguments) -> Result<()> {
    bail!("`workstats pr` is not yet implemented")
}
