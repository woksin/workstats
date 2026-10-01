mod aggregate;
mod ai;
mod allocate;
mod attribution;
mod branch_report;
mod branches;
mod bundle;
mod cache;
mod calendar;
mod classify;
mod cli;
mod commands;
mod compare;
mod describe;
mod document;
mod engagement;
mod git;
mod goals;
mod insights;
mod issues;
mod model;
mod now;
mod output;
mod paths;
mod pricing;
mod progress;
mod report;
mod sources;
mod timesheet;
mod timeutil;
mod tui;
mod update;

use clap::Parser;

use cli::{Arguments, Command};
use commands::{classify_paths, print_sources, record_event, run_update_command};
use report::{Presentation, run, run_allocation};

fn main() {
    let Arguments { command, report } = Arguments::parse();
    let result = match command {
        Some(Command::Ui(command)) => run(*command, Presentation::Explore, None),
        Some(Command::Sources(command)) => print_sources(&command),
        // `--config` reads naturally on either side of the subcommand, and a
        // flag that is silently ignored on one side is the same trap as the
        // grouping aliases that used to discard `--group-by` (AUDIT V).
        Some(Command::Classify(command)) => classify_paths(&command, report.config.as_deref()),
        Some(Command::Record(command)) => record_event(&command),
        Some(Command::Update(command)) => run_update_command(&command),
        Some(Command::Allocate(command)) => run_allocation(*command),
        Some(Command::Timesheet(command)) => timesheet::run(*command),
        Some(Command::Branch(command)) => branch_report::run_branch(*command),
        Some(Command::Pr(command)) => branch_report::run_pr(*command),
        Some(Command::Insights(command)) => insights::run_insights(*command),
        Some(Command::Digest(command)) => insights::run_digest(*command),
        Some(Command::Now(command)) => now::run(*command),
        Some(Command::Export(command)) => bundle::run_export(*command),
        Some(Command::Merge(command)) => bundle::run_merge(*command),
        Some(Command::Calendar(command)) => calendar::run(*command),
        None => run(report, Presentation::Print, None),
    };
    if let Err(error) = result {
        eprintln!("workstats: {error:#}");
        std::process::exit(2);
    }
}
