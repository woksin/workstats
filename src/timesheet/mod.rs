//! `workstats timesheet`: suggested hours per day and engagement, rounded the
//! way a timesheet is, with a ledger for manual entries, overrides and locks.
//! This module owns the command's clap shape; the behaviour lands in later
//! changes, and until then every form of the command says so and stops.

pub(crate) mod ledger;
pub(crate) mod model;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::cli::ReportArguments;
use model::{Detail, ExportPreset, Rounding, SplitRule, TotalsBy, UnassignedMode};

/// `workstats timesheet [OPTIONS]` or `workstats timesheet <ACTION>`. The
/// report and timesheet flags belong to the first form; the actions take only
/// their own arguments, so mixing the two is refused by clap.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct TimesheetArguments {
    #[command(subcommand)]
    pub(crate) action: Option<TimesheetAction>,
    #[command(flatten)]
    pub(crate) options: TimesheetOptions,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

/// The flags that shape the computed timesheet itself.
#[derive(Clone, Debug, Default, Args)]
pub(crate) struct TimesheetOptions {
    #[arg(
        long,
        value_name = "DUR",
        help = "Rounding increment (default: 15m; config: \"timesheet.increment\")"
    )]
    pub(crate) increment: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "How durations are rounded (default: nearest)"
    )]
    pub(crate) rounding: Option<Rounding>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Raise any entry below this to it (default: off)"
    )]
    pub(crate) min_entry: Option<String>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Drop entries whose raw time is below this (default: off)"
    )]
    pub(crate) drop_below: Option<String>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Most hours a day may total; a multiple of the increment (default: none)"
    )]
    pub(crate) daily_cap: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "How a work block is shared between engagements (default: nearest)"
    )]
    pub(crate) split: Option<SplitRule>,
    #[arg(
        long,
        value_enum,
        help = "Break each engagement down by issue, feature, branch or repo"
    )]
    pub(crate) detail: Option<Detail>,
    #[arg(long, value_name = "KEY", action = clap::ArgAction::Append, help = "Only this engagement; repeatable")]
    pub(crate) engagement: Vec<String>,
    #[arg(long, help = "Only billable engagements")]
    pub(crate) billable_only: bool,
    #[arg(
        long,
        value_enum,
        help = "List or hide work that matches no engagement (default: show)"
    )]
    pub(crate) unassigned: Option<UnassignedMode>,
    // Not `--by`: the report flags already use that as an alias of
    // `--group-by`, and two flags of one name cannot share a command.
    #[arg(
        long = "totals-by",
        value_enum,
        help = "Subtotal by day or week (default: day)"
    )]
    pub(crate) totals_by: Option<TotalsBy>,
    #[arg(
        long,
        value_enum,
        value_name = "PRESET",
        help = "Write a vendor CSV; implies --format csv"
    )]
    pub(crate) export: Option<ExportPreset>,
    #[arg(
        long,
        value_name = "SOURCES",
        value_delimiter = ',',
        help = "Add descriptions from commits and/or sessions[=PROVIDERS]; read only when asked"
    )]
    pub(crate) describe: Vec<String>,
    #[arg(
        long,
        value_name = "CMD",
        help = "Pipe each entry's digest to this command and use its output as the description"
    )]
    pub(crate) summarize_with: Option<String>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Time allowed for --summarize-with (default: 60s)"
    )]
    pub(crate) summarize_timeout: Option<String>,
    #[arg(long, help = "Print the digest that would be sent and run nothing")]
    pub(crate) digest: bool,
    #[arg(long, help = "Show the live computation for locked periods")]
    pub(crate) ignore_locks: bool,
    #[arg(long, help = "Leave the evidence columns out")]
    pub(crate) no_evidence: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TimesheetAction {
    /// Add hours by hand to a day and engagement
    Add(AddArguments),
    /// Override the estimate for a day and engagement; 0 suppresses it
    Set(SetArguments),
    /// Remove an override
    Unset(UnsetArguments),
    /// Remove a manual entry by id
    Rm(RemoveArguments),
    /// List manual entries and overrides
    Entries(EntriesArguments),
    /// Freeze a period's figures as submitted
    Lock(Box<LockArguments>),
    /// Remove a lock
    Unlock(UnlockArguments),
    /// List locks
    Locks,
}

#[derive(Debug, Args)]
pub(crate) struct AddArguments {
    #[arg(
        value_name = "DATE",
        help = "YYYY-MM-DD, today, yesterday, or mon..sun (the most recent)"
    )]
    pub(crate) date: String,
    #[arg(value_name = "ENGAGEMENT")]
    pub(crate) engagement: String,
    #[arg(value_name = "DURATION", help = "For example 1h30m or 90m")]
    pub(crate) duration: String,
    #[arg(value_name = "NOTE")]
    pub(crate) note: Option<String>,
    #[arg(long, value_name = "HH:MM", help = "Start time, for CSV exports")]
    pub(crate) start: Option<String>,
    #[arg(
        long,
        conflicts_with = "non_billable",
        help = "Billable regardless of the engagement"
    )]
    pub(crate) billable: bool,
    #[arg(long, help = "Not billable regardless of the engagement")]
    pub(crate) non_billable: bool,
    #[arg(long, help = "Write into a locked day")]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SetArguments {
    #[arg(value_name = "DATE")]
    pub(crate) date: String,
    #[arg(value_name = "ENGAGEMENT")]
    pub(crate) engagement: String,
    #[arg(value_name = "DURATION")]
    pub(crate) duration: String,
    #[arg(value_name = "NOTE")]
    pub(crate) note: Option<String>,
    #[arg(long, help = "Write into a locked day")]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UnsetArguments {
    #[arg(value_name = "DATE")]
    pub(crate) date: String,
    #[arg(value_name = "ENGAGEMENT")]
    pub(crate) engagement: String,
    #[arg(long, help = "Write into a locked day")]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RemoveArguments {
    #[arg(value_name = "ID")]
    pub(crate) id: String,
    #[arg(long, help = "Write into a locked day")]
    pub(crate) force: bool,
}

/// The window the ledger listing covers. The same words as the report's, but
/// only these four: nothing here reads a history.
#[derive(Debug, Args)]
pub(crate) struct EntriesArguments {
    #[arg(
        long,
        conflicts_with_all = ["week", "since", "until"],
        help = "One calendar month: YYYY-MM, current, or last"
    )]
    pub(crate) month: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["month", "since", "until"],
        help = "One ISO week: YYYY-Www, current, or last"
    )]
    pub(crate) week: Option<String>,
    #[arg(long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    pub(crate) since: Option<String>,
    #[arg(long, help = "Inclusive YYYY-MM or YYYY-MM-DD")]
    pub(crate) until: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Ledger file (default: beside the config)"
    )]
    pub(crate) ledger: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct LockArguments {
    // Not named `period`: the report flags already have a `--period`, and two
    // arguments of one name cannot share a command.
    #[arg(value_name = "PERIOD", help = "YYYY-MM, YYYY-Www, or A..B")]
    pub(crate) target: String,
    #[arg(long, help = "Lock over existing drift or a lock already held")]
    pub(crate) force: bool,
    #[command(flatten)]
    pub(crate) options: TimesheetOptions,
    #[command(flatten)]
    pub(crate) report: ReportArguments,
}

#[derive(Debug, Args)]
pub(crate) struct UnlockArguments {
    #[arg(value_name = "PERIOD")]
    pub(crate) period: String,
}

pub(crate) fn run(_arguments: TimesheetArguments) -> Result<()> {
    bail!("`workstats timesheet` is not yet implemented")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Arguments, Command};

    fn parse(arguments: &[&str]) -> Result<TimesheetArguments, clap::Error> {
        let mut full = vec!["workstats", "timesheet"];
        full.extend_from_slice(arguments);
        Arguments::try_parse_from(full).map(|parsed| match parsed.command {
            Some(Command::Timesheet(timesheet)) => *timesheet,
            other => panic!("expected timesheet, got {other:?}"),
        })
    }

    #[test]
    fn report_and_timesheet_flags_parse_together() {
        let parsed = parse(&[
            "--month",
            "2026-08",
            "--increment",
            "15m",
            "--rounding",
            "balanced",
            "--detail",
            "issue",
            "--engagement",
            "acme",
            "--engagement",
            "internal",
            "--export",
            "toggl",
            "--describe",
            "commits,sessions=codex",
        ])
        .unwrap();
        assert!(parsed.action.is_none());
        assert_eq!(Some("2026-08"), parsed.report.month.as_deref());
        assert_eq!(Some(Rounding::Balanced), parsed.options.rounding);
        assert_eq!(Some(Detail::Issue), parsed.options.detail);
        assert_eq!(vec!["acme", "internal"], parsed.options.engagement);
        assert_eq!(Some(ExportPreset::Toggl), parsed.options.export);
        assert_eq!(vec!["commits", "sessions=codex"], parsed.options.describe);
    }

    #[test]
    fn every_action_parses_with_its_own_arguments() {
        for arguments in [
            &[
                "add",
                "yesterday",
                "acme",
                "1h30m",
                "Steering",
                "--start",
                "09:00",
                "--billable",
            ][..],
            &["set", "mon", "acme", "0", "--force"],
            &["unset", "2026-08-12", "acme"],
            &["rm", "m7f3a2c1"],
            &["entries", "--month", "2026-08"],
            &[
                "lock",
                "2026-08",
                "--month",
                "2026-08",
                "--increment",
                "30m",
                "--force",
            ],
            &["unlock", "2026-08"],
            &["locks"],
        ] {
            let parsed = parse(arguments).unwrap_or_else(|error| panic!("{arguments:?}: {error}"));
            assert!(parsed.action.is_some(), "{arguments:?}");
        }
    }

    #[test]
    fn an_action_refuses_the_computation_flags() {
        assert!(parse(&["locks", "--increment", "15m"]).is_err());
        assert!(parse(&["--increment", "15m", "locks"]).is_err());
    }

    #[test]
    fn a_manual_entry_cannot_be_both_billable_and_not() {
        assert!(parse(&["add", "today", "acme", "1h", "--billable", "--non-billable"]).is_err());
    }

    #[test]
    fn the_command_says_it_is_not_implemented_yet() {
        let error = run(parse(&[]).unwrap()).unwrap_err();
        assert!(error.to_string().contains("not yet implemented"));
    }
}
