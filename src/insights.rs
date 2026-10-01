//! `workstats insights` and `workstats digest`: focus, patterns and leverage
//! computed from a collected run, with no new reads. This holds the commands'
//! clap shapes; the computation lands in a later change.

use anyhow::{Result, bail};
use clap::Args;

use crate::cli::ReportArguments;

#[derive(Debug, Args)]
pub(crate) struct InsightsArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_delimiter = ',',
        value_parser = ["focus", "leverage", "heatmap", "models"],
        help = "Only these sections (default: all)"
    )]
    pub(crate) section: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DigestArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

pub(crate) fn run_insights(_arguments: InsightsArguments) -> Result<()> {
    bail!("`workstats insights` is not yet implemented")
}

pub(crate) fn run_digest(_arguments: DigestArguments) -> Result<()> {
    bail!("`workstats digest` is not yet implemented")
}
