//! `workstats now`: today and the week so far in one short line, cheap enough
//! for a prompt or status bar. This holds the command's clap shape; the
//! snapshot, template and refresh behaviour land in a later change.

use anyhow::{Result, bail};
use clap::Args;

use crate::cli::ReportArguments;

#[derive(Debug, Args)]
pub(crate) struct NowArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "STR",
        help = "Output template, for example \"{human} · {agent} agent\" (config: \"now.template\")"
    )]
    pub(crate) template: Option<String>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Reuse the last result if it is younger than this (default: 60s; config: \"now.max_age\")"
    )]
    pub(crate) max_age: Option<String>,
    #[arg(
        long,
        help = "Print the last result at once and refresh in the background"
    )]
    pub(crate) no_wait: bool,
    #[arg(
        long,
        value_name = "DUR",
        help = "How recent a session must be to count as active (default: 10m; config: \"now.active_within\")"
    )]
    pub(crate) active_within: Option<String>,
    #[arg(long, help = "Print nothing and exit 0 when it fails, for prompts")]
    pub(crate) quiet_errors: bool,
    #[arg(long, hide = true)]
    pub(crate) refresh_only: bool,
}

pub(crate) fn run(_arguments: NowArguments) -> Result<()> {
    bail!("`workstats now` is not yet implemented")
}
