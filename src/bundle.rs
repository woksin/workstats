//! Bundles: the evidence one machine exports so another can merge it, and the
//! `export` and `merge` commands around them. This holds the commands' clap
//! shapes and the import hook the pipeline calls; the format and the merge
//! land in a later change.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use crate::cli::ReportArguments;
use crate::model::{Diagnostics, GitCommit, Session};
use crate::paths::ProjectAliases;

#[derive(Debug, Args)]
pub(crate) struct ExportArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "FILE",
        help = "Where to write the bundle; '-' writes stdout"
    )]
    pub(crate) output: Option<PathBuf>,
    #[arg(
        long,
        value_name = "NAME",
        help = "Name for this machine in the bundle"
    )]
    pub(crate) label: Option<String>,
    #[arg(
        long,
        help = "Include changed file paths, which are left out by default"
    )]
    pub(crate) include_paths: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MergeArguments {
    #[arg(value_name = "FILE", required = true, help = "Bundles to merge")]
    pub(crate) files: Vec<PathBuf>,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(long, help = "Also include this machine's own history")]
    pub(crate) with_local: bool,
}

/// Folds `--import` bundles into the sessions and commits read locally.
/// Called by the pipeline after Git has been scanned and before repository
/// labels are made unique, so imported repositories are labelled with the rest.
/// Until bundles exist, asking for an import is refused rather than ignored.
pub(crate) fn merge_imports(
    imports: &[PathBuf],
    _sessions: &mut Vec<Session>,
    _commits: &mut Vec<GitCommit>,
    _aliases: &ProjectAliases,
    _diagnostics: &mut Diagnostics,
) -> Result<()> {
    if imports.is_empty() {
        return Ok(());
    }
    bail!("--import is not yet implemented")
}

pub(crate) fn run_export(_arguments: ExportArguments) -> Result<()> {
    bail!("`workstats export` is not yet implemented")
}

pub(crate) fn run_merge(_arguments: MergeArguments) -> Result<()> {
    bail!("`workstats merge` is not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_imports_change_nothing_and_some_are_refused_until_they_exist() {
        let aliases = ProjectAliases::default();
        let mut diagnostics = Diagnostics::default();
        let (mut sessions, mut commits) = (Vec::new(), Vec::new());
        merge_imports(&[], &mut sessions, &mut commits, &aliases, &mut diagnostics).unwrap();
        let error = merge_imports(
            &[PathBuf::from("bundle.json")],
            &mut sessions,
            &mut commits,
            &aliases,
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not yet implemented"));
    }
}
