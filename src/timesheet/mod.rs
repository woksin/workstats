//! `workstats timesheet`: suggested hours per day and engagement, rounded the
//! way a timesheet is, with a ledger for manual entries, overrides and locks.
//! This module owns the command's clap shape and the plain `timesheet` run:
//! collect the human timeline, compute suggested entries, filter, and present.
//! The subcommands that change the ledger are dispatched from `run`.

pub(crate) mod actions;
pub(crate) mod compute;
pub(crate) mod ledger;
pub(crate) mod lock;
pub(crate) mod model;
pub(crate) mod presets;
pub(crate) mod render;
pub(crate) mod round;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use regex::Regex;
use serde::Deserialize;

use crate::cli::{OutputFormat, ReportArguments};
use crate::describe;
use crate::document::{render_html, render_markdown};
use crate::engagement::{self, Engagements, UNASSIGNED};
use crate::model::Diagnostics;
use crate::paths::{Config, home_dir, load_config};
use crate::report::{Collected, Purpose, collect};
use model::{
    Detail, ExportPreset, Rounding, SplitRule, Timesheet, TimesheetSettings, TotalsBy,
    UnassignedMode,
};
use presets::Person;

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
    #[arg(long, help = "Replace the snapshot of a period that is already locked")]
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

/// The `timesheet` block of the config. Raw JSON in `Config`, checked here, so
/// a misspelt key is refused by name instead of the whole file being ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimesheetConfig {
    increment: Option<String>,
    rounding: Option<String>,
    min_entry: Option<String>,
    drop_below: Option<String>,
    daily_cap: Option<String>,
    split: Option<String>,
    unassigned: Option<String>,
    person: Option<PersonConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonConfig {
    email: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
}

/// A flag, else the config's value, else the built-in default.
fn pick<'a>(
    flag: (&'static str, &'a Option<String>),
    config: (&'static str, &'a Option<String>),
) -> Option<(&'static str, &'a str)> {
    flag.1
        .as_deref()
        .map(|value| (flag.0, value))
        .or_else(|| config.1.as_deref().map(|value| (config.0, value)))
}

/// A duration in whole seconds: `15m`, `1h30m`, `90s`. Zero (`0`, `0m`) is
/// allowed because it means "off" for the minimum and the drop threshold; the
/// callers that cannot be zero say so.
fn span_seconds(name: &str, value: &str) -> Result<u64> {
    const LIMIT_SECONDS: f64 = 366.0 * 24.0 * 3600.0;
    let invalid = || anyhow::anyhow!("invalid {name} {value:?}: use 30s, 15m, 2h or 1h30m");
    let trimmed = value.trim();
    if trimmed == "0" {
        return Ok(0);
    }
    let whole = Regex::new(r"(?i)^(?:\d+(?:\.\d+)?[smh])+$").expect("static regex");
    if !whole.is_match(trimmed) {
        return Err(invalid());
    }
    let part = Regex::new(r"(?i)(\d+(?:\.\d+)?)([smh])").expect("static regex");
    let mut seconds = 0.0;
    for captures in part.captures_iter(trimmed) {
        let amount: f64 = captures[1].parse().map_err(|_| invalid())?;
        seconds += amount
            * match captures[2].to_ascii_lowercase().as_str() {
                "h" => 3600.0,
                "m" => 60.0,
                _ => 1.0,
            };
    }
    if !seconds.is_finite() || seconds > LIMIT_SECONDS {
        bail!("{name} {value:?} must be at most 8784h (366 days)");
    }
    if (seconds - seconds.round()).abs() > 1e-9 {
        bail!("{name} {value:?} must be a whole number of seconds");
    }
    Ok(seconds.round() as u64)
}

fn enum_value<T: ValueEnum>(name: &str, value: &str) -> Result<T> {
    T::from_str(value.trim(), true).map_err(|_| {
        let accepted = T::value_variants()
            .iter()
            .filter_map(|variant| variant.to_possible_value())
            .map(|value| value.get_name().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("invalid {name} {value:?}; expected one of {accepted}")
    })
}

/// Everything the flags and the config decide before anything is scanned.
pub(crate) struct Resolved {
    pub(crate) settings: TimesheetSettings,
    pub(crate) person: Person,
}

/// Flag > config > built-in default, validated. The increment is whole
/// seconds and at least one; the daily cap is a multiple of it, so every
/// estimate stays a whole number of increments.
pub(crate) fn resolve(options: &TimesheetOptions, config: &Config) -> Result<Resolved> {
    let file: TimesheetConfig = match &config.timesheet {
        Some(value) => {
            serde_json::from_value(value.clone()).context("invalid \"timesheet\" configuration")?
        }
        None => TimesheetConfig::default(),
    };
    let increment = match pick(
        ("--increment", &options.increment),
        ("timesheet.increment", &file.increment),
    ) {
        Some((name, value)) => span_seconds(name, value)?,
        None => TimesheetSettings::default().increment_seconds,
    };
    if increment == 0 {
        bail!("the increment must be at least one second");
    }
    let optional_span = |flag: (&'static str, &Option<String>),
                         config: (&'static str, &Option<String>)| {
        pick(flag, config)
            .map(|(name, value)| span_seconds(name, value))
            .transpose()
    };
    let min_entry = optional_span(
        ("--min-entry", &options.min_entry),
        ("timesheet.min_entry", &file.min_entry),
    )?
    .unwrap_or(0);
    let drop_below = optional_span(
        ("--drop-below", &options.drop_below),
        ("timesheet.drop_below", &file.drop_below),
    )?
    .unwrap_or(0);
    let daily_cap = pick(
        ("--daily-cap", &options.daily_cap),
        ("timesheet.daily_cap", &file.daily_cap),
    )
    .map(|(name, value)| {
        let seconds = span_seconds(name, value)?;
        if seconds == 0 {
            bail!("{name} must be more than zero; leave it out for no cap");
        }
        if seconds % increment != 0 {
            bail!(
                "{name} ({}) must be a multiple of the increment ({})",
                compute::span_text(seconds),
                compute::span_text(increment)
            );
        }
        Ok(seconds)
    })
    .transpose()?;
    // The enum flags are typed by clap; only the config holds them as text.
    let rounding = match (options.rounding, &file.rounding) {
        (Some(rounding), _) => rounding,
        (None, Some(value)) => enum_value::<Rounding>("timesheet.rounding", value)?,
        (None, None) => Rounding::default(),
    };
    let split = match (options.split, &file.split) {
        (Some(split), _) => split,
        (None, Some(value)) => enum_value::<SplitRule>("timesheet.split", value)?,
        (None, None) => SplitRule::default(),
    };
    let unassigned = match (options.unassigned, &file.unassigned) {
        (Some(mode), _) => mode,
        (None, Some(value)) => enum_value::<UnassignedMode>("timesheet.unassigned", value)?,
        (None, None) => UnassignedMode::default(),
    };
    let person = file.person.unwrap_or_default();
    Ok(Resolved {
        settings: TimesheetSettings {
            increment_seconds: increment,
            rounding,
            min_entry_seconds: min_entry,
            drop_below_seconds: drop_below,
            daily_cap_seconds: daily_cap,
            split,
            unassigned,
            detail: options.detail,
        },
        person: Person {
            email: person.email.unwrap_or_default(),
            first_name: person.first_name.unwrap_or_default(),
            last_name: person.last_name.unwrap_or_default(),
        },
    })
}

/// Whether the report flags name a window of their own.
fn has_window(report: &ReportArguments) -> bool {
    report.month.is_some()
        || report.year.is_some()
        || report.week.is_some()
        || report.since.is_some()
        || report.until.is_some()
}

/// `--engagement acme`, `(unassigned)` or `unassigned`.
fn is_unassigned(key: &str) -> bool {
    key == UNASSIGNED || key == "unassigned"
}

/// Applies `--engagement`, `--billable-only` and `--unassigned hide` after the
/// whole computation: a day is rounded and capped over all its work, so
/// filtering never changes any figure that is still shown. Returns what was
/// left out.
pub(crate) fn filter(
    timesheet: &mut Timesheet,
    options: &TimesheetOptions,
    unassigned: UnassignedMode,
) -> render::Hidden {
    let wanted: Vec<&str> = options.engagement.iter().map(String::as_str).collect();
    let keep = |engagement: &str, billable: Option<bool>| {
        let is_unassigned_entry = engagement == UNASSIGNED;
        let explicitly = wanted
            .iter()
            .any(|key| *key == engagement || (is_unassigned(key) && is_unassigned_entry));
        (wanted.is_empty() || explicitly)
            && (!options.billable_only || billable.unwrap_or(true))
            && (unassigned == UnassignedMode::Show || !is_unassigned_entry || explicitly)
    };
    let mut hidden = render::Hidden::default();
    let entries = std::mem::take(&mut timesheet.entries);
    for entry in entries {
        if keep(&entry.engagement, Some(entry.billable)) {
            timesheet.entries.push(entry);
        } else {
            hidden.entries += 1;
            hidden.seconds += entry.final_seconds;
        }
    }
    // Dropped entries have no billing flag; only the engagement filters apply.
    timesheet
        .dropped
        .retain(|entry| keep(&entry.engagement, None));
    timesheet.drift.retain(|row| keep(&row.engagement, None));
    if hidden.entries > 0 {
        if !wanted.is_empty() {
            hidden
                .reasons
                .push(format!("--engagement {}", wanted.join(",")));
        }
        if options.billable_only {
            hidden.reasons.push("--billable-only".to_string());
        }
        if unassigned == UnassignedMode::Hide {
            hidden.reasons.push("--unassigned hide".to_string());
        }
    }
    hidden
}

pub(crate) fn run(arguments: TimesheetArguments) -> Result<()> {
    let TimesheetArguments {
        action,
        options,
        report,
    } = arguments;
    // Each action takes only its own arguments and returns; only the plain
    // `workstats timesheet` form reaches `run_report`.
    if let Some(action) = action {
        return actions::run(action);
    }
    run_report(options, report)
}

/// A computed timesheet and what it was computed from.
pub(crate) struct Live {
    pub(crate) collected: Collected,
    pub(crate) resolved: Resolved,
    pub(crate) computation: compute::Computation,
    /// The ledger as it was read for this computation.
    pub(crate) ledger: ledger::Ledger,
    /// No window flag was given, so `--month current` was assumed.
    pub(crate) default_window: bool,
    /// What each described entry's digest said; filled when any of
    /// `--describe`, `--summarize-with` or `--digest` is given.
    pub(crate) digests: Vec<describe::Digest>,
}

/// Whether the output lists an entry, by the filters that hide entries. Used
/// to leave unlisted entries undescribed: nothing is read or summarised for a
/// row nobody will see. (`--unassigned hide` is not considered, so `lock`,
/// which lists everything, describes everything.)
fn is_listed(options: &TimesheetOptions, entry: &model::TimesheetEntry) -> bool {
    let wanted = options.engagement.is_empty()
        || options.engagement.iter().any(|key| {
            *key == entry.engagement || (is_unassigned(key) && entry.engagement == UNASSIGNED)
        });
    wanted && (!options.billable_only || entry.billable)
}

/// Refuses what means nothing to a timesheet, reads the config, the ledger
/// and the history, and computes the entries with the ledger applied.
/// Everything that can be refused is refused before the history is scanned.
/// Shared by the plain report and by `lock`, so a lock freezes exactly the
/// figures the report shows. `ignore_locks` shows the live computation for
/// locked periods.
pub(crate) fn compute_live(
    options: &TimesheetOptions,
    mut report: ReportArguments,
    ignore_locks: bool,
) -> Result<Live> {
    // Flags that shape a report's rows mean nothing here, and ignoring them
    // silently is how a number gets read as something it is not.
    for (given, flag) in [
        (
            report.group_by.is_some() || report.by_repo || report.matrix || report.by_dir,
            "--group-by",
        ),
        (report.period.is_some(), "--period"),
        (report.compare.is_some(), "--compare"),
        (report.explain_human_time, "--explain-human-time"),
        (
            report.explain_repository_attribution,
            "--explain-repository-attribution",
        ),
    ] {
        if given {
            bail!(
                "{flag} does not apply to `workstats timesheet`; it groups by day and engagement"
            );
        }
    }

    // Read early, so a bad flag or config value fails before a long scan.
    let mut diagnostics = Diagnostics::default();
    let config = load_config(report.config.as_deref(), &mut diagnostics);
    let resolved = resolve(options, &config)?;
    let plan = describe::Plan::parse(
        &options.describe,
        options.summarize_with.as_deref(),
        options
            .summarize_timeout
            .as_deref()
            .map(|value| span_seconds("--summarize-timeout", value))
            .transpose()?,
        options.digest,
    )?;
    let configured = Engagements::compile(
        config.engagements.as_ref(),
        &config.project_aliases,
        &home_dir(),
    )?;
    for key in &options.engagement {
        if !is_unassigned(key) && configured.get(key).is_none() {
            let known: Vec<_> = configured.keys().collect();
            bail!(
                "unknown engagement {key:?} in --engagement; configured: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
    }
    // An unreadable ledger stops the run: ignoring it would change hours that
    // may already have been submitted.
    let ledger = ledger::Ledger::load(&ledger::default_path())?;

    let default_window = !has_window(&report);
    if default_window {
        report.month = Some("current".to_string());
    }
    let describe_context = describe::Context::from_report(&report);
    // A timesheet shows no goals; their warnings would otherwise be counted
    // below as trouble reading history.
    report.no_goals = true;
    let collected = collect(report, Purpose::Query)?;

    let current = lock::LockSettings::current(
        &resolved.settings,
        collected.settings.gap_cap,
        collected.settings.human_idle,
        collected.settings.review_credit,
        engagement::active().fingerprint(),
        &ledger.fingerprint(),
    );
    let context = ledger::Context {
        ledger: &ledger,
        engagements: engagement::active(),
        ignore_locks,
        current,
    };
    let mut computation = compute::compute(&compute::Input {
        timeline: &collected.timeline,
        engagements: engagement::active(),
        settings: &resolved.settings,
        window: collected.window,
        report_human_seconds: collected.report.summary.human_estimated_seconds,
        ledger: Some(&context),
    })?;
    // Descriptions are read after the figures are final and are never part of
    // them: they are not cached, and only a lock keeps the one it was given.
    let digests = describe::for_timesheet(
        &plan,
        &describe_context,
        &collected,
        &resolved.settings,
        &mut computation.timesheet,
        |entry| is_listed(options, entry),
    );
    drop(context);
    Ok(Live {
        collected,
        resolved,
        computation,
        ledger,
        default_window,
        digests,
    })
}

/// The report this command makes of its own: refused flags are refused before
/// anything is scanned, and nothing is printed until everything has been
/// computed.
fn run_report(options: TimesheetOptions, report: ReportArguments) -> Result<()> {
    let explicit_format = report.output_format;
    if options.export.is_some()
        && let Some(format) = explicit_format
        && format != OutputFormat::Csv
    {
        bail!(
            "--export writes CSV and cannot be combined with --format {}; drop one of them",
            format.name()
        );
    }
    let Live {
        collected,
        resolved,
        mut computation,
        default_window,
        digests,
        ..
    } = compute_live(&options, report, options.ignore_locks)?;

    // `--digest` shows what `--summarize-with` would be given, and stops.
    if options.digest {
        for warning in &computation.timesheet.warnings {
            eprintln!("workstats: {warning}");
        }
        let stdout = io::stdout();
        let mut out = stdout.lock();
        serde_json::to_writer_pretty(&mut out, &digests)?;
        writeln!(out)?;
        return Ok(());
    }

    let format = match (explicit_format, options.export) {
        (Some(format), _) => format,
        (None, Some(_)) => OutputFormat::Csv,
        (None, None) => collected
            .report
            .inputs
            .config_defaults
            .get("format")
            .and_then(|name| OutputFormat::from_str(name, true).ok())
            .unwrap_or(OutputFormat::Table),
    };

    let hidden = filter(
        &mut computation.timesheet,
        &options,
        resolved.settings.unassigned,
    );

    let mut extra_warnings = Vec::new();
    let reading = collected.report.diagnostics.warning_count;
    if reading > 0 {
        extra_warnings.push(format!(
            "{reading} warning(s) while reading history; run `workstats` for the details"
        ));
    }
    let view = render::View {
        computation: &computation,
        show_evidence: !options.no_evidence,
        show_description: !options.describe.is_empty() || options.summarize_with.is_some(),
        totals_by: options.totals_by.unwrap_or_default(),
        default_window,
        hidden: &hidden,
        extra_warnings: &extra_warnings,
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Table => write!(out, "{}", render::render_text(&render::document(&view)))?,
        OutputFormat::Markdown => write!(out, "{}", render_markdown(&render::document(&view)))?,
        OutputFormat::Html => write!(out, "{}", render_html(&render::document(&view)))?,
        OutputFormat::Json => {
            serde_json::to_writer_pretty(&mut out, &render::json(&view)?)?;
            writeln!(out)?;
        }
        OutputFormat::Csv => {
            presets::write_csv(
                &mut out,
                &computation.timesheet.entries,
                options.export.unwrap_or(ExportPreset::Generic),
                engagement::active(),
                &resolved.person,
                view.show_evidence,
            )?;
            // The CSV is for a pipe or a file; what a reader of the table
            // would have seen below it goes to stderr so it is not lost.
            for warning in view.warnings() {
                eprintln!("workstats: {warning}");
            }
        }
    }
    Ok(())
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

    fn config(timesheet: serde_json::Value) -> Config {
        Config {
            timesheet: Some(timesheet),
            ..Config::default()
        }
    }

    fn options(arguments: &[&str]) -> TimesheetOptions {
        parse(arguments).unwrap().options
    }

    #[test]
    fn settings_come_from_the_flag_then_the_config_then_the_defaults() {
        let nothing = resolve(&options(&[]), &Config::default()).unwrap();
        assert_eq!(TimesheetSettings::default(), nothing.settings);

        let file = config(serde_json::json!({
            "increment": "30m", "rounding": "balanced", "daily_cap": "8h",
            "min_entry": "0m", "split": "signals", "unassigned": "hide",
            "person": {"email": "me@example.com", "first_name": "Ada"}
        }));
        let from_file = resolve(&options(&[]), &file).unwrap();
        assert_eq!(1800, from_file.settings.increment_seconds);
        assert_eq!(Rounding::Balanced, from_file.settings.rounding);
        assert_eq!(Some(8 * 3600), from_file.settings.daily_cap_seconds);
        assert_eq!(SplitRule::Signals, from_file.settings.split);
        assert_eq!(UnassignedMode::Hide, from_file.settings.unassigned);
        assert_eq!("me@example.com", from_file.person.email);

        let flagged = resolve(
            &options(&[
                "--increment",
                "6m",
                "--rounding",
                "up",
                "--split",
                "agent",
                "--daily-cap",
                "6h",
            ]),
            &file,
        )
        .unwrap();
        assert_eq!(360, flagged.settings.increment_seconds);
        assert_eq!(Rounding::Up, flagged.settings.rounding);
        assert_eq!(SplitRule::Agent, flagged.settings.split);
        assert_eq!(Some(6 * 3600), flagged.settings.daily_cap_seconds);
    }

    #[test]
    fn bad_settings_are_refused_naming_the_flag_or_the_config_key() {
        let message = |arguments: &[&str], config: &Config| {
            format!("{:#}", resolve(&options(arguments), config).err().unwrap())
        };
        let none = Config::default();
        assert!(message(&["--increment", "soon"], &none).contains("--increment"));
        assert!(message(&["--increment", "0m"], &none).contains("increment"));
        assert!(message(&["--increment", "7m", "--daily-cap", "1h"], &none).contains("multiple"));
        assert!(message(&["--daily-cap", "0"], &none).contains("--daily-cap"));
        assert!(message(&["--increment", "1.5s"], &none).contains("whole number of seconds"));
        let file = config(serde_json::json!({"rounding": "sideways"}));
        assert!(message(&[], &file).contains("timesheet.rounding"));
        let file = config(serde_json::json!({"incremnt": "15m"}));
        assert!(message(&[], &file).contains("timesheet"));
        let file = config(serde_json::json!({"daily_cap": "10m", "increment": "15m"}));
        assert!(message(&[], &file).contains("timesheet.daily_cap"));
    }

    #[test]
    fn durations_may_be_compound_and_must_be_whole_seconds() {
        for (value, seconds) in [
            ("15m", 900),
            ("1h30m", 5400),
            ("90s", 90),
            ("2H", 7200),
            ("0", 0),
            ("0m", 0),
            ("1.5m", 90),
        ] {
            assert_eq!(seconds, span_seconds("--x", value).unwrap(), "{value}");
        }
        for value in ["", "15", "m", "1h 30m", "-5m", "1.5s", "9999h"] {
            assert!(span_seconds("--x", value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn zero_is_off_for_the_minimum_and_the_drop_threshold() {
        let settings = resolve(
            &options(&["--min-entry", "0", "--drop-below", "0s"]),
            &Config::default(),
        )
        .unwrap()
        .settings;
        assert_eq!(0, settings.min_entry_seconds);
        assert_eq!(0, settings.drop_below_seconds);
    }

    #[test]
    fn a_window_flag_replaces_the_default_month() {
        assert!(!has_window(&parse(&[]).unwrap().report));
        for flag in [
            ["--month", "2026-08"],
            ["--week", "last"],
            ["--since", "2026-08"],
            ["--year", "2026"],
        ] {
            assert!(has_window(&parse(&flag).unwrap().report), "{flag:?}");
        }
    }

    #[test]
    fn export_conflicts_with_an_explicit_non_csv_format_at_run_time() {
        let error = run(parse(&["--export", "toggl", "--format", "json"]).unwrap()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("--export") && message.contains("--format json"),
            "{message}"
        );
    }

    #[test]
    fn flags_that_mean_nothing_to_a_timesheet_are_refused() {
        for flag in [
            ["--group-by", "repo"],
            ["--period", "month"],
            ["--compare", "previous"],
        ] {
            let error = run(parse(&flag).unwrap()).unwrap_err();
            assert!(error.to_string().contains(flag[0]), "{flag:?}: {error}");
        }
    }

    fn entry(engagement: &str, billable: bool, seconds: u64) -> model::TimesheetEntry {
        use crate::timesheet::model::{EntryStatus, Evidence};
        model::TimesheetEntry {
            date: chrono::NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(),
            engagement: engagement.to_string(),
            detail: None,
            label: engagement.to_string(),
            client: None,
            billable,
            raw_seconds: seconds as f64,
            estimated_seconds: seconds,
            manual_seconds: 0,
            override_seconds: None,
            final_seconds: seconds,
            first_start: None,
            last_end: None,
            evidence: Evidence::default(),
            rate: None,
            currency: None,
            amount: None,
            notes: Vec::new(),
            description: None,
            status: EntryStatus::Suggested,
            adjustments: Vec::new(),
            lock_drift_seconds: None,
        }
    }

    fn sheet() -> Timesheet {
        Timesheet {
            window: model::TimesheetWindow::default(),
            settings: TimesheetSettings::default(),
            entries: vec![
                entry("acme", true, 3600),
                entry("internal", false, 1800),
                entry(UNASSIGNED, false, 900),
            ],
            dropped: Vec::new(),
            cross_check: Vec::new(),
            warnings: Vec::new(),
            methodology: model::TimesheetMethodology {
                status: "suggested",
                split_rule: String::new(),
                rounding: String::new(),
            },
            drift: Vec::new(),
            applied_locks: Vec::new(),
        }
    }

    #[test]
    fn filters_leave_out_entries_and_say_what_they_left_out() {
        let mut timesheet = sheet();
        let hidden = filter(
            &mut timesheet,
            &options(&["--billable-only"]),
            UnassignedMode::Show,
        );
        assert_eq!(1, timesheet.entries.len());
        assert_eq!((2, 2700), (hidden.entries, hidden.seconds));
        assert_eq!(vec!["--billable-only"], hidden.reasons);

        let mut timesheet = sheet();
        let hidden = filter(
            &mut timesheet,
            &options(&["--engagement", "internal"]),
            UnassignedMode::Show,
        );
        assert_eq!("internal", timesheet.entries[0].engagement);
        assert_eq!(2, hidden.entries);

        let mut timesheet = sheet();
        let hidden = filter(&mut timesheet, &options(&[]), UnassignedMode::Hide);
        assert_eq!(2, timesheet.entries.len());
        assert_eq!(vec!["--unassigned hide"], hidden.reasons);

        // Naming the unassigned work explicitly overrides hiding it.
        let mut timesheet = sheet();
        filter(
            &mut timesheet,
            &options(&["--engagement", "unassigned"]),
            UnassignedMode::Hide,
        );
        assert_eq!(UNASSIGNED, timesheet.entries[0].engagement);

        let mut timesheet = sheet();
        let hidden = filter(&mut timesheet, &options(&[]), UnassignedMode::Show);
        assert_eq!(3, timesheet.entries.len());
        assert_eq!(0, hidden.entries);
    }
}
