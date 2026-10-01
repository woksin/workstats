//! `workstats insights` and `workstats digest`: focus, patterns and leverage
//! computed from a collected run, with no new reads.
//!
//! Everything here is a function of `Collected`: the human timeline (the same
//! non-overlapping pieces the report sums), the agent intervals, the sessions
//! with their token events, and the human commits. Nothing is scanned again,
//! so a figure here cannot disagree with the report over the same window
//! except by being cut differently, and each cut is stated where it is made.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use clap::Args;
use serde::Serialize;
use serde_json::Value;

use crate::aggregate::{Timeline, foreground_session_output};
use crate::attribution::{self, Ctx};
use crate::cli::{OutputFormat, ReportArguments, ReportWindow};
use crate::compare::Comparison;
use crate::document::{Block, Column, Document, Table, render_html, render_markdown};
use crate::goals::GoalReport;
use crate::model::{Diagnostics, GitCommit, Interval, Session, TokenUsage};
use crate::output::{
    compact_tokens, hours, number, percent, push_comparison, safe_text, warning_lines,
};
use crate::paths::{Config, home_dir};
use crate::pricing::{self, RATES_AS_OF, RateOverrides, RateSource};
use crate::report::{Collected, Purpose, collect};
use crate::timeutil::window_label;

/// The default window of `insights`, in days, today included.
const DEFAULT_WINDOW_DAYS: i64 = 28;
/// A work block shorter than this is a "short block": too brief to be focused
/// work, long enough to be more than a glance.
const SHORT_BLOCK_SECONDS: f64 = 30.0 * 60.0;
/// How many rows of each ranking a digest shows unless `--top` asks for fewer.
const DIGEST_ROWS: usize = 10;
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

const NOTE: &str = "Human work is an estimate from prompts, session edges and commits, not a stopwatch; agent figures come from local histories. List value is what the tokens would cost at published list prices, not what was billed.";

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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub(crate) fn run_insights(arguments: InsightsArguments) -> Result<()> {
    let InsightsArguments {
        report: mut arguments,
        section,
    } = arguments;
    let sections = Sections::parse(&section);
    if arguments.compare.is_some() {
        bail!(
            "--compare is not available with `workstats insights`; use `workstats digest`, which compares the week with the one before"
        );
    }
    let flag_format = arguments.output_format;
    let top = row_limit(arguments.top);
    refuse_csv("insights", flag_format)?;
    let defaulted = !has_window(&arguments);
    if defaulted {
        let first = Local::now().date_naive() - Duration::days(DEFAULT_WINDOW_DAYS - 1);
        arguments.since = Some(first.to_string());
    }
    let collected = collect(arguments, Purpose::Query)?;
    let format = resolve_format("insights", flag_format, &collected.settings.config)?;
    let analysis = Analysis::new(&collected, &Local, sections.heatmap)?;
    let window = WindowInfo::new(collected.window, defaulted);
    let shows_value = sections.leverage || sections.models;
    let extra = analysis.warnings(shows_value);
    let mut warnings = collected.report.diagnostics.messages.clone();
    warnings.extend(extra.iter().cloned());
    match format {
        OutputFormat::Json => {
            let output = InsightsOutput {
                window,
                note: NOTE,
                focus: sections.focus.then_some(&analysis.focus),
                patterns: sections.heatmap.then_some(&analysis.patterns),
                leverage: sections.leverage.then_some(&analysis.leverage),
                models: sections.models.then_some(&analysis.models),
                warnings,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        other => {
            let document = insights_document(
                &window,
                &analysis,
                sections,
                top,
                &collected.report.diagnostics,
                &extra,
                other != OutputFormat::Table,
            );
            print_document(&document, other);
        }
    }
    Ok(())
}

pub(crate) fn run_digest(arguments: DigestArguments) -> Result<()> {
    let mut arguments = arguments.report;
    let flag_format = arguments.output_format;
    let limit = row_limit(arguments.top).min(DIGEST_ROWS);
    refuse_csv("digest", flag_format)?;
    let defaulted = !has_window(&arguments);
    if defaulted {
        arguments.week = Some("last".to_string());
    }
    // A comparison needs both ends of the window, so an open-ended one is left
    // alone and the digest says so instead of failing.
    let bounded = arguments.month.is_some()
        || arguments.year.is_some()
        || arguments.week.is_some()
        || (arguments.since.is_some() && arguments.until.is_some());
    if arguments.compare.is_none() && bounded {
        arguments.compare = Some("previous".to_string());
    }
    let collected = collect(arguments, Purpose::Query)?;
    let format = resolve_format("digest", flag_format, &collected.settings.config)?;
    let analysis = Analysis::new(&collected, &Local, false)?;
    let window = WindowInfo::new(collected.window, defaulted);
    let rankings = Rankings::new(&collected, limit);
    let extra = analysis.warnings(true);
    let mut warnings = collected.report.diagnostics.messages.clone();
    warnings.extend(extra.iter().cloned());
    match format {
        OutputFormat::Json => {
            let output = DigestOutput {
                window,
                note: NOTE,
                comparison: collected.report.comparison.as_ref(),
                top_repos: &rankings.repos,
                top_features: &rankings.features,
                focus: &analysis.focus,
                leverage: &analysis.leverage,
                goals: collected.report.goals.as_ref(),
                warnings,
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        other => {
            let document = digest_document(
                &window,
                &collected.report.comparison,
                &rankings,
                &analysis,
                collected.report.goals.as_ref(),
                &collected.report.diagnostics,
                &extra,
                other != OutputFormat::Table,
            );
            print_document(&document, other);
        }
    }
    Ok(())
}

/// `--top`, where 0 means every row.
fn row_limit(top: usize) -> usize {
    if top == 0 { usize::MAX } else { top }
}

fn has_window(arguments: &ReportArguments) -> bool {
    arguments.month.is_some()
        || arguments.year.is_some()
        || arguments.week.is_some()
        || arguments.since.is_some()
        || arguments.until.is_some()
}

/// CSV has one flat table of rows, and neither command is one.
fn refuse_csv(command: &str, format: Option<OutputFormat>) -> Result<()> {
    if format == Some(OutputFormat::Csv) {
        bail!(
            "--format csv is not available for `workstats {command}`; use table, json, markdown, or html"
        );
    }
    Ok(())
}

/// The flag, else the config's `defaults.format`, else the table.
fn resolve_format(
    command: &str,
    flag: Option<OutputFormat>,
    config: &Config,
) -> Result<OutputFormat> {
    let format = match flag {
        Some(format) => format,
        None => config
            .config_defaults(&home_dir())?
            .format
            .unwrap_or(OutputFormat::Table),
    };
    if format == OutputFormat::Csv {
        bail!(
            "defaults.format \"csv\" (from config) is not available for `workstats {command}`; pass --format table, json, markdown, or html"
        );
    }
    Ok(format)
}

fn print_document(document: &Document, format: OutputFormat) {
    match format {
        OutputFormat::Markdown => print!("{}", render_markdown(document)),
        OutputFormat::Html => print!("{}", render_html(document)),
        _ => print!("{}", render_text(document)),
    }
}

#[derive(Clone, Copy)]
struct Sections {
    focus: bool,
    heatmap: bool,
    leverage: bool,
    models: bool,
}

impl Sections {
    fn parse(names: &[String]) -> Self {
        if names.is_empty() {
            return Self {
                focus: true,
                heatmap: true,
                leverage: true,
                models: true,
            };
        }
        let has = |name: &str| names.iter().any(|candidate| candidate == name);
        Self {
            focus: has("focus"),
            heatmap: has("heatmap"),
            leverage: has("leverage"),
            models: has("models"),
        }
    }
}

// ---------------------------------------------------------------------------
// Output shapes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WindowInfo {
    since: Option<String>,
    until: Option<String>,
    label: String,
    /// True when no window flag was given and the command chose one.
    defaulted: bool,
}

impl WindowInfo {
    fn new(window: ReportWindow, defaulted: bool) -> Self {
        let (since, until) = window;
        let day = |value: DateTime<Utc>| value.with_timezone(&Local).date_naive();
        let label = match (since, until) {
            (Some(since), Some(until)) => window_label(since, until),
            (Some(since), None) => format!("since {}", day(since)),
            (None, Some(until)) => format!("until {}", day(until - Duration::seconds(1))),
            (None, None) => "all history".to_string(),
        };
        let label = if defaulted {
            match (since, until) {
                (Some(_), None) => format!("the last {DEFAULT_WINDOW_DAYS} days ({label})"),
                _ => format!("{label} (default window)"),
            }
        } else {
            label
        };
        Self {
            since: since.map(|value| value.to_rfc3339()),
            until: until.map(|value| value.to_rfc3339()),
            label,
            defaulted,
        }
    }
}

#[derive(Serialize)]
struct InsightsOutput<'a> {
    window: WindowInfo,
    note: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<&'a Focus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patterns: Option<&'a Patterns>,
    #[serde(skip_serializing_if = "Option::is_none")]
    leverage: Option<&'a Leverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<&'a Models>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct DigestOutput<'a> {
    window: WindowInfo,
    note: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison: Option<&'a Comparison>,
    top_repos: &'a Ranking,
    top_features: &'a Ranking,
    focus: &'a Focus,
    leverage: &'a Leverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    goals: Option<&'a GoalReport>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct Focus {
    aggregate: FocusAggregate,
    days: Vec<FocusDay>,
}

#[derive(Serialize)]
struct FocusAggregate {
    human_seconds: f64,
    block_count: usize,
    short_block_count: usize,
    short_block_threshold_seconds: f64,
    longest_block_seconds: f64,
    longest_block_start: Option<String>,
    longest_repo_stretch_seconds: f64,
    longest_repo_stretch_repo: Option<String>,
    average_block_seconds: Option<f64>,
    context_switches: usize,
    context_switches_per_block: Option<f64>,
    context_switches_per_human_hour: Option<f64>,
}

#[derive(Serialize)]
struct FocusDay {
    date: String,
    human_seconds: f64,
    block_count: usize,
    short_block_count: usize,
    longest_block_seconds: f64,
    longest_repo_stretch_seconds: f64,
    context_switches: usize,
}

#[derive(Serialize)]
struct Patterns {
    /// Monday first, matching the matrix rows.
    weekdays: [&'static str; 7],
    /// Human seconds by local weekday (row) and hour of day (column).
    human_seconds: Vec<[f64; 24]>,
    /// Seconds any agent was active (overlap counted once), same shape.
    agent_wall_seconds: Vec<[f64; 24]>,
    active_days: usize,
    night: NightFigures,
    weekend: WeekendFigures,
}

#[derive(Serialize)]
struct NightFigures {
    from: String,
    to: String,
    human_seconds: f64,
    share_of_human: Option<f64>,
    /// Nights with any human time, each named by the date its evening began.
    days: usize,
}

#[derive(Serialize)]
struct WeekendFigures {
    weekdays: Vec<&'static str>,
    human_seconds: f64,
    share_of_human: Option<f64>,
    days: usize,
}

#[derive(Serialize)]
struct Leverage {
    human_seconds: f64,
    agent_wall_seconds: f64,
    parallel_agent_seconds: f64,
    /// Agent hours of wall clock for each human hour.
    agent_wall_per_human_hour: Option<f64>,
    parallel_agent_per_human_hour: Option<f64>,
    commits: usize,
    changed_lines: u64,
    tokens: u64,
    /// Priced models only; see `unpriced_models`.
    list_value_usd: f64,
    unpriced_models: Vec<String>,
    tokens_per_commit: Option<f64>,
    tokens_per_100_lines: Option<f64>,
    tokens_per_human_hour: Option<f64>,
    list_value_per_commit: Option<f64>,
    list_value_per_100_lines: Option<f64>,
    list_value_per_human_hour: Option<f64>,
    foreground_sessions_with_commits: usize,
    foreground_sessions_without_commits: usize,
    /// Of the foreground sessions in repositories that produced commits.
    sessions_without_commits_share: Option<f64>,
    rates_as_of: &'static str,
}

#[derive(Serialize)]
struct Models {
    providers: Vec<ProviderRow>,
    models: Vec<ModelRow>,
    rates_as_of: &'static str,
    /// The `model_rates` keys that priced a model here; empty means the
    /// built-in table did.
    rate_overrides: Vec<String>,
}

#[derive(Serialize)]
struct ProviderRow {
    provider: String,
    sessions: usize,
    subagent_sessions: usize,
    sessions_with_commits: usize,
    agent_wall_seconds: f64,
    tokens: u64,
    list_value_usd: Option<f64>,
    /// Human time whose nearest signal came from this provider. `git` is the
    /// commits themselves.
    human_seconds: f64,
}

#[derive(Serialize)]
struct ModelRow {
    provider: String,
    model: String,
    sessions: usize,
    subagent_sessions: usize,
    sessions_with_commits: usize,
    agent_wall_seconds: f64,
    tokens: u64,
    list_value_usd: Option<f64>,
    priced: bool,
    human_seconds: f64,
}

#[derive(Serialize)]
struct Ranking {
    rows: Vec<RankedRow>,
    /// Rows left out by the limit, so nothing disappears silently.
    omitted: usize,
}

#[derive(Serialize)]
struct RankedRow {
    name: String,
    human_seconds: f64,
    share_of_human: Option<f64>,
    agent_wall_seconds: f64,
    commits: usize,
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// What the computations read, borrowed from a `Collected` or built by hand in
/// a test.
struct Input<'a> {
    timeline: &'a Timeline,
    sessions: &'a [Session],
    commits: &'a [GitCommit],
    agent_commits: &'a [GitCommit],
    window: ReportWindow,
    human_idle: Duration,
    rates: &'a RateOverrides,
    engagements_configured: bool,
    patterns: PatternSettings,
    today: NaiveDate,
}

/// Night hours and weekend days, from the `insights` config block.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PatternSettings {
    /// Minutes since local midnight.
    night_from: u32,
    night_to: u32,
    weekend: BTreeSet<u32>,
}

impl Default for PatternSettings {
    fn default() -> Self {
        Self {
            night_from: 22 * 60,
            night_to: 6 * 60,
            weekend: BTreeSet::from([5, 6]),
        }
    }
}

impl PatternSettings {
    fn from_config(value: Option<&Value>) -> Result<Self> {
        let mut settings = Self::default();
        let Some(value) = value else {
            return Ok(settings);
        };
        let Some(object) = value.as_object() else {
            bail!("insights must be an object with optional \"night\" and \"weekend\" keys");
        };
        for (key, entry) in object {
            match key.as_str() {
                "night" => {
                    let times: Vec<&str> = entry
                        .as_array()
                        .map(|items| items.iter().filter_map(Value::as_str).collect())
                        .filter(|items: &Vec<&str>| {
                            items.len() == 2 && entry.as_array().is_some_and(|a| a.len() == 2)
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "insights.night must be two \"HH:MM\" times, for example [\"22:00\", \"06:00\"]"
                            )
                        })?;
                    settings.night_from = parse_clock("insights.night", times[0])?;
                    settings.night_to = parse_clock("insights.night", times[1])?;
                    if settings.night_from == settings.night_to {
                        bail!("insights.night must start and end at different times");
                    }
                }
                "weekend" => {
                    let names = entry
                        .as_array()
                        .filter(|items| items.len() <= 7)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "insights.weekend must be a list of at most seven weekday names, for example [\"sat\", \"sun\"]"
                            )
                        })?;
                    let mut days = BTreeSet::new();
                    for name in names {
                        let name = name.as_str().unwrap_or_default();
                        let weekday: Weekday = name.trim().parse().map_err(|_| {
                            anyhow::anyhow!(
                                "insights.weekend has {name:?}, which is not a weekday (mon, tue, wed, thu, fri, sat, sun)"
                            )
                        })?;
                        days.insert(weekday.num_days_from_monday());
                    }
                    settings.weekend = days;
                }
                other => bail!("unknown insights key \"{other}\"; expected night or weekend"),
            }
        }
        Ok(settings)
    }

    fn is_night(&self, minute_of_day: u32) -> bool {
        if self.night_from < self.night_to {
            (self.night_from..self.night_to).contains(&minute_of_day)
        } else {
            minute_of_day >= self.night_from || minute_of_day < self.night_to
        }
    }

    /// True when the night runs past midnight, so its small hours belong to the
    /// evening before.
    fn wraps(&self) -> bool {
        self.night_from > self.night_to
    }
}

fn parse_clock(key: &str, text: &str) -> Result<u32> {
    let invalid = || anyhow::anyhow!("{key} has {text:?}, which is not a time as HH:MM");
    let (hours, minutes) = text.trim().split_once(':').ok_or_else(invalid)?;
    let (hours, minutes): (u32, u32) = (
        hours.parse().map_err(|_| invalid())?,
        minutes.parse().map_err(|_| invalid())?,
    );
    if hours > 23 || minutes > 59 {
        return Err(invalid());
    }
    Ok(hours * 60 + minutes)
}

fn clock(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Everything both commands show, computed once.
struct Analysis {
    focus: Focus,
    patterns: Patterns,
    leverage: Leverage,
    models: Models,
    unpriced: Vec<String>,
    stale_rates: Option<String>,
}

impl Analysis {
    fn new<Tz: TimeZone>(collected: &Collected, tz: &Tz, with_patterns: bool) -> Result<Self> {
        let settings = &collected.settings;
        let patterns = if with_patterns {
            PatternSettings::from_config(settings.config.insights.as_ref())?
        } else {
            PatternSettings::default()
        };
        let input = Input {
            timeline: &collected.timeline,
            sessions: &collected.sessions,
            commits: &collected.commits,
            agent_commits: &collected.agent_commits,
            window: collected.window,
            human_idle: settings.human_idle,
            rates: &settings.rate_overrides,
            engagements_configured: settings.config.engagements.is_some(),
            patterns,
            today: settings.now.with_timezone(&Local).date_naive(),
        };
        Ok(Self::from_input(&input, tz))
    }

    fn from_input<Tz: TimeZone>(input: &Input<'_>, tz: &Tz) -> Self {
        let usage = usage(input);
        Self {
            focus: focus(input, tz),
            patterns: patterns(input, tz),
            leverage: usage.leverage,
            models: usage.models,
            unpriced: usage.unpriced,
            stale_rates: usage.stale_rates,
        }
    }

    /// The warnings this analysis raised. The stale-rates one comes with every
    /// output that shows a list value, and only those.
    fn warnings(&self, shows_value: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        if shows_value {
            if !self.unpriced.is_empty() {
                warnings.push(format!(
                    "no published rate for {}; counted in tokens, left out of list value",
                    self.unpriced.join(", ")
                ));
            }
            warnings.extend(self.stale_rates.clone());
        }
        warnings
    }
}

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

/// One work block: the human pieces that share a block id.
struct BlockFigures {
    start: DateTime<Utc>,
    seconds: f64,
    longest_stretch: f64,
    stretch_repo: String,
    switches: usize,
}

fn blocks(input: &Input<'_>) -> Vec<BlockFigures> {
    let mut groups: BTreeMap<&str, Vec<&Interval>> = BTreeMap::new();
    for piece in &input.timeline.human_intervals {
        groups
            .entry(piece.session_id.as_str())
            .or_default()
            .push(piece);
    }
    let engagement = |piece: &Interval| {
        input.engagements_configured.then(|| {
            attribution::engagement_label(&Ctx {
                repo_id: &piece.repo_id,
                cwd: &piece.cwd,
                branch: piece.branch.as_deref(),
            })
        })
    };
    let mut blocks: Vec<BlockFigures> = groups
        .into_values()
        .map(|mut pieces| {
            pieces.sort_by_key(|piece| piece.start);
            let mut figures = BlockFigures {
                start: pieces[0].start,
                seconds: 0.0,
                longest_stretch: 0.0,
                stretch_repo: pieces[0].repo.clone(),
                switches: 0,
            };
            let mut stretch = 0.0;
            let mut stretch_repo = &pieces[0].repo_id;
            let mut previous_engagement = engagement(pieces[0]);
            for (index, piece) in pieces.iter().enumerate() {
                let seconds = piece.seconds();
                figures.seconds += seconds;
                let now_engagement = engagement(piece);
                if index > 0 {
                    if piece.repo_id != *stretch_repo {
                        figures.switches += 1;
                        stretch = 0.0;
                        stretch_repo = &piece.repo_id;
                    } else if now_engagement != previous_engagement {
                        figures.switches += 1;
                    }
                }
                previous_engagement = now_engagement;
                stretch += seconds;
                if stretch > figures.longest_stretch {
                    figures.longest_stretch = stretch;
                    figures.stretch_repo = piece.repo.clone();
                }
            }
            figures
        })
        .collect();
    blocks.sort_by_key(|block| block.start);
    blocks
}

/// Focus figures. A block belongs to the local day its first piece starts on,
/// whole, so a block that runs past midnight is one block on one day rather
/// than two fragments, and the per-day rows add up to the aggregate.
fn focus<Tz: TimeZone>(input: &Input<'_>, tz: &Tz) -> Focus {
    let blocks = blocks(input);
    let mut days: BTreeMap<NaiveDate, FocusDay> = BTreeMap::new();
    for block in &blocks {
        let date = block.start.with_timezone(tz).date_naive();
        let day = days.entry(date).or_insert_with(|| FocusDay {
            date: date.to_string(),
            human_seconds: 0.0,
            block_count: 0,
            short_block_count: 0,
            longest_block_seconds: 0.0,
            longest_repo_stretch_seconds: 0.0,
            context_switches: 0,
        });
        day.human_seconds += block.seconds;
        day.block_count += 1;
        day.short_block_count += usize::from(block.seconds < SHORT_BLOCK_SECONDS);
        day.longest_block_seconds = day.longest_block_seconds.max(block.seconds);
        day.longest_repo_stretch_seconds =
            day.longest_repo_stretch_seconds.max(block.longest_stretch);
        day.context_switches += block.switches;
    }
    let human_seconds: f64 = blocks.iter().map(|block| block.seconds).sum();
    let longest = blocks
        .iter()
        .max_by(|left, right| left.seconds.total_cmp(&right.seconds));
    let stretch = blocks
        .iter()
        .max_by(|left, right| left.longest_stretch.total_cmp(&right.longest_stretch));
    let switches: usize = blocks.iter().map(|block| block.switches).sum();
    let aggregate = FocusAggregate {
        human_seconds: round3(human_seconds),
        block_count: blocks.len(),
        short_block_count: blocks
            .iter()
            .filter(|block| block.seconds < SHORT_BLOCK_SECONDS)
            .count(),
        short_block_threshold_seconds: SHORT_BLOCK_SECONDS,
        longest_block_seconds: round3(longest.map_or(0.0, |block| block.seconds)),
        longest_block_start: longest.map(|block| block.start.to_rfc3339()),
        longest_repo_stretch_seconds: round3(stretch.map_or(0.0, |block| block.longest_stretch)),
        longest_repo_stretch_repo: stretch.map(|block| block.stretch_repo.clone()),
        average_block_seconds: ratio(human_seconds, blocks.len() as f64).map(round3),
        context_switches: switches,
        context_switches_per_block: ratio(switches as f64, blocks.len() as f64).map(round3),
        context_switches_per_human_hour: ratio(switches as f64, human_seconds / 3600.0).map(round3),
    };
    let days = days
        .into_values()
        .map(|mut day| {
            day.human_seconds = round3(day.human_seconds);
            day.longest_block_seconds = round3(day.longest_block_seconds);
            day.longest_repo_stretch_seconds = round3(day.longest_repo_stretch_seconds);
            day
        })
        .collect();
    Focus { aggregate, days }
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// Calls `visit` for each stretch of `[start, end)` that lies inside one local
/// minute, with the local time at its start and its length in seconds. Walking
/// in the zone's own minutes means a daylight-saving change cannot misplace an
/// hour: the skipped hour gets nothing and the repeated one gets both passes.
fn for_each_local_minute<Tz: TimeZone>(
    tz: &Tz,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    mut visit: impl FnMut(&DateTime<Tz>, f64),
) {
    let mut cursor = start;
    while cursor < end {
        let local = cursor.with_timezone(tz);
        let into = i64::from(local.second()) * 1_000_000_000
            + i64::from(local.nanosecond() % 1_000_000_000);
        let next = (cursor + Duration::nanoseconds(60_000_000_000 - into)).min(end);
        let seconds = (next - cursor).num_microseconds().unwrap_or(0) as f64 / 1_000_000.0;
        visit(&local, seconds);
        cursor = next;
    }
}

/// The merged, non-overlapping spans of `intervals`.
fn union_spans<'a>(
    intervals: impl Iterator<Item = &'a Interval>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut spans: Vec<_> = intervals
        .filter(|interval| interval.end > interval.start)
        .map(|interval| (interval.start, interval.end))
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn span_seconds(spans: &[(DateTime<Utc>, DateTime<Utc>)]) -> f64 {
    spans
        .iter()
        .map(|(start, end)| (*end - *start).num_microseconds().unwrap_or(0) as f64 / 1_000_000.0)
        .sum()
}

fn patterns<Tz: TimeZone>(input: &Input<'_>, tz: &Tz) -> Patterns {
    let settings = &input.patterns;
    let mut human = [[0.0; 24]; 7];
    let mut agent = [[0.0; 24]; 7];
    let mut human_total = 0.0;
    let mut night_seconds = 0.0;
    let mut weekend_seconds = 0.0;
    let mut active: BTreeSet<NaiveDate> = BTreeSet::new();
    let mut night_days: BTreeSet<NaiveDate> = BTreeSet::new();
    let mut weekend_days: BTreeSet<NaiveDate> = BTreeSet::new();
    for piece in &input.timeline.human_intervals {
        for_each_local_minute(tz, piece.start, piece.end, |local, seconds| {
            let weekday = local.weekday().num_days_from_monday();
            let minute = local.hour() * 60 + local.minute();
            human[weekday as usize][local.hour() as usize] += seconds;
            human_total += seconds;
            let date = local.date_naive();
            active.insert(date);
            if settings.is_night(minute) {
                night_seconds += seconds;
                // The small hours of a night that began the evening before are
                // part of that evening's night.
                let evening = if settings.wraps() && minute < settings.night_from {
                    date.pred_opt().unwrap_or(date)
                } else {
                    date
                };
                night_days.insert(evening);
            }
            if settings.weekend.contains(&weekday) {
                weekend_seconds += seconds;
                weekend_days.insert(date);
            }
        });
    }
    // Overlapping agents are one stretch of wall clock, as in the report.
    for (start, end) in union_spans(input.timeline.ai_intervals.iter()) {
        for_each_local_minute(tz, start, end, |local, seconds| {
            agent[local.weekday().num_days_from_monday() as usize][local.hour() as usize] +=
                seconds;
        });
    }
    let round_matrix =
        |matrix: [[f64; 24]; 7]| -> Vec<[f64; 24]> { matrix.map(|row| row.map(round3)).to_vec() };
    Patterns {
        weekdays: WEEKDAYS,
        human_seconds: round_matrix(human),
        agent_wall_seconds: round_matrix(agent),
        active_days: active.len(),
        night: NightFigures {
            from: clock(settings.night_from),
            to: clock(settings.night_to),
            human_seconds: round3(night_seconds),
            share_of_human: ratio(night_seconds, human_total).map(round3),
            days: night_days.len(),
        },
        weekend: WeekendFigures {
            weekdays: settings
                .weekend
                .iter()
                .map(|day| WEEKDAYS[*day as usize])
                .collect(),
            human_seconds: round3(weekend_seconds),
            share_of_human: ratio(weekend_seconds, human_total).map(round3),
            days: weekend_days.len(),
        },
    }
}

// ---------------------------------------------------------------------------
// Leverage and models
// ---------------------------------------------------------------------------

type SessionKey = (String, String);

struct UsageFigures {
    leverage: Leverage,
    models: Models,
    unpriced: Vec<String>,
    stale_rates: Option<String>,
}

/// Counts that cannot be priced or attributed to a model.
fn is_placeholder_model(model: &str) -> bool {
    model.is_empty() || matches!(model, "unknown" | "<synthetic>" | "—")
}

fn in_window(window: ReportWindow, at: DateTime<Utc>) -> bool {
    window.0.is_none_or(|bound| at >= bound) && window.1.is_none_or(|bound| at < bound)
}

#[derive(Default)]
struct Accumulator {
    foreground: HashSet<SessionKey>,
    subagents: HashSet<SessionKey>,
    linked: HashSet<SessionKey>,
    intervals: Vec<Interval>,
    tokens: TokenUsage,
    human_seconds: f64,
}

impl Accumulator {
    fn add_session(&mut self, key: &SessionKey, subagent: bool, linked: bool) {
        if subagent {
            self.subagents.insert(key.clone());
        } else {
            self.foreground.insert(key.clone());
            if linked {
                self.linked.insert(key.clone());
            }
        }
    }

    fn wall_seconds(&self) -> f64 {
        span_seconds(&union_spans(self.intervals.iter()))
    }
}

fn usage(input: &Input<'_>) -> UsageFigures {
    let window = input.window;
    let ai = &input.timeline.ai_intervals;
    let human = &input.timeline.human_intervals;

    // The commits the report counted as yours: inside the window, not
    // agent-authored, once per (checkout member, sha).
    let mut seen = HashSet::new();
    let commits: Vec<&GitCommit> = input
        .commits
        .iter()
        .chain(input.agent_commits)
        .filter(|commit| in_window(window, commit.timestamp))
        .filter(|commit| !commit.authorship.is_agent_authored())
        .filter(|commit| seen.insert((commit.repo_member_id.as_str(), commit.sha.as_str())))
        .collect();
    let changed_lines: u64 = commits
        .iter()
        .map(|commit| commit.additions.saturating_add(commit.deletions))
        .sum();

    // Sessions the report counts: with activity in the window, or first seen
    // in it.
    let mut eligible: HashSet<SessionKey> = ai
        .iter()
        .map(|interval| (interval.provider.clone(), interval.session_id.clone()))
        .collect();
    for session in input.sessions {
        if session
            .first_seen()
            .is_some_and(|first| in_window(window, first))
        {
            eligible.insert((session.provider.clone(), session.session_id.clone()));
        }
    }
    let roles: HashMap<SessionKey, bool> = input
        .sessions
        .iter()
        .map(|session| {
            (
                (session.provider.clone(), session.session_id.clone()),
                session.is_subagent,
            )
        })
        .collect();
    let (with_commits, without_commits) = foreground_session_output(
        input.sessions,
        &commits,
        &eligible,
        &roles,
        input.human_idle,
    );
    // Which sessions those were, for the table: the same function asked about
    // one session at a time, against that repository's commits only.
    let mut by_repo: HashMap<&str, Vec<&GitCommit>> = HashMap::new();
    for commit in &commits {
        by_repo
            .entry(commit.repo_id.as_str())
            .or_default()
            .push(commit);
    }
    let mut linked: HashSet<SessionKey> = HashSet::new();
    for session in input.sessions {
        if session.is_subagent {
            continue;
        }
        let Some(repo_commits) = by_repo.get(session.repo_id.as_str()) else {
            continue;
        };
        let (with, _) = foreground_session_output(
            std::slice::from_ref(session),
            repo_commits,
            &eligible,
            &roles,
            input.human_idle,
        );
        if with > 0 {
            linked.insert((session.provider.clone(), session.session_id.clone()));
        }
    }

    let mut per_model: BTreeMap<(String, String), Accumulator> = BTreeMap::new();
    let mut per_provider: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut touch = |provider: &str, model: &str, apply: &dyn Fn(&mut Accumulator)| {
        apply(
            per_model
                .entry((provider.to_string(), model.to_string()))
                .or_default(),
        );
        apply(per_provider.entry(provider.to_string()).or_default());
    };
    for interval in ai {
        let key = (interval.provider.clone(), interval.session_id.clone());
        let subagent = roles.get(&key).copied().unwrap_or(false);
        let is_linked = linked.contains(&key);
        touch(&interval.provider, &interval.model, &|acc| {
            acc.add_session(&key, subagent, is_linked);
            acc.intervals.push(interval.clone());
        });
    }
    for session in input.sessions {
        let key = (session.provider.clone(), session.session_id.clone());
        let is_linked = linked.contains(&key);
        for event in &session.token_events {
            if !in_window(window, event.timestamp) {
                continue;
            }
            touch(&session.provider, &event.model, &|acc| {
                acc.add_session(&key, session.is_subagent, is_linked);
                acc.tokens += event.usage;
            });
        }
    }
    for piece in human {
        let seconds = piece.seconds();
        touch(&piece.provider, &piece.model, &|acc| {
            acc.human_seconds += seconds;
        });
    }

    let mut built_in_used = false;
    let mut overrides_used: BTreeSet<String> = BTreeSet::new();
    let mut unpriced: BTreeSet<String> = BTreeSet::new();
    let mut total_tokens = TokenUsage::default();
    let mut list_value = 0.0;
    let mut model_rows = Vec::new();
    let mut provider_values: BTreeMap<String, Option<f64>> = BTreeMap::new();
    for ((provider, model), acc) in &per_model {
        let value = if is_placeholder_model(model) || acc.tokens.is_zero() {
            None
        } else {
            match input.rates.resolve(model) {
                None => {
                    unpriced.insert(model.clone());
                    None
                }
                Some(resolved) => {
                    match resolved.source {
                        RateSource::BuiltIn => built_in_used = true,
                        RateSource::Override => {
                            overrides_used.extend(resolved.pattern.map(str::to_string));
                        }
                    }
                    pricing::list_value_usd(model, &acc.tokens, input.rates)
                }
            }
        };
        total_tokens += acc.tokens;
        if let Some(value) = value {
            list_value += value;
            let so_far = provider_values
                .get(provider)
                .copied()
                .flatten()
                .unwrap_or(0.0);
            provider_values.insert(provider.clone(), Some(so_far + value));
        }
        model_rows.push(ModelRow {
            provider: provider.clone(),
            model: model.clone(),
            sessions: acc.foreground.len(),
            subagent_sessions: acc.subagents.len(),
            sessions_with_commits: acc.linked.len(),
            agent_wall_seconds: round3(acc.wall_seconds()),
            tokens: acc.tokens.total(),
            list_value_usd: value.map(round6),
            priced: !is_placeholder_model(model) && input.rates.resolve(model).is_some(),
            human_seconds: round3(acc.human_seconds),
        });
    }
    model_rows.sort_by(|left, right| {
        right
            .list_value_usd
            .unwrap_or(0.0)
            .total_cmp(&left.list_value_usd.unwrap_or(0.0))
            .then_with(|| right.tokens.cmp(&left.tokens))
            .then_with(|| right.human_seconds.total_cmp(&left.human_seconds))
            .then_with(|| (&left.provider, &left.model).cmp(&(&right.provider, &right.model)))
    });
    let mut provider_rows: Vec<ProviderRow> = per_provider
        .iter()
        .map(|(provider, acc)| ProviderRow {
            provider: provider.clone(),
            sessions: acc.foreground.len(),
            subagent_sessions: acc.subagents.len(),
            sessions_with_commits: acc.linked.len(),
            agent_wall_seconds: round3(acc.wall_seconds()),
            tokens: acc.tokens.total(),
            list_value_usd: provider_values.get(provider).copied().flatten().map(round6),
            human_seconds: round3(acc.human_seconds),
        })
        .collect();
    provider_rows.sort_by(|left, right| {
        right
            .list_value_usd
            .unwrap_or(0.0)
            .total_cmp(&left.list_value_usd.unwrap_or(0.0))
            .then_with(|| right.tokens.cmp(&left.tokens))
            .then_with(|| right.human_seconds.total_cmp(&left.human_seconds))
            .then_with(|| left.provider.cmp(&right.provider))
    });

    let human_seconds: f64 = human.iter().map(Interval::seconds).sum();
    let agent_wall = span_seconds(&union_spans(ai.iter()));
    let parallel: f64 = ai.iter().map(Interval::seconds).sum();
    let human_hours = human_seconds / 3600.0;
    let tokens = total_tokens.total();
    let per_100_lines = changed_lines as f64 / 100.0;
    let unpriced: Vec<String> = unpriced.into_iter().collect();
    let leverage = Leverage {
        human_seconds: round3(human_seconds),
        agent_wall_seconds: round3(agent_wall),
        parallel_agent_seconds: round3(parallel),
        agent_wall_per_human_hour: ratio(agent_wall / 3600.0, human_hours).map(round3),
        parallel_agent_per_human_hour: ratio(parallel / 3600.0, human_hours).map(round3),
        commits: commits.len(),
        changed_lines,
        tokens,
        list_value_usd: round6(list_value),
        unpriced_models: unpriced.clone(),
        tokens_per_commit: ratio(tokens as f64, commits.len() as f64).map(round3),
        tokens_per_100_lines: ratio(tokens as f64, per_100_lines).map(round3),
        tokens_per_human_hour: ratio(tokens as f64, human_hours).map(round3),
        list_value_per_commit: ratio(list_value, commits.len() as f64).map(round6),
        list_value_per_100_lines: ratio(list_value, per_100_lines).map(round6),
        list_value_per_human_hour: ratio(list_value, human_hours).map(round6),
        foreground_sessions_with_commits: with_commits,
        foreground_sessions_without_commits: without_commits,
        sessions_without_commits_share: ratio(
            without_commits as f64,
            (with_commits + without_commits) as f64,
        )
        .map(round3),
        rates_as_of: RATES_AS_OF,
    };
    UsageFigures {
        leverage,
        models: Models {
            providers: provider_rows,
            models: model_rows,
            rates_as_of: RATES_AS_OF,
            rate_overrides: overrides_used.into_iter().collect(),
        },
        unpriced,
        // Like `allocate`, only numbers that came from the built-in table can
        // be out of date.
        stale_rates: built_in_used
            .then(|| pricing::stale_rates_warning(input.today))
            .flatten(),
    }
}

/// `numerator / denominator`, or `None` when there is nothing to divide by.
fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0 && denominator.is_finite()).then(|| numerator / denominator)
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

// ---------------------------------------------------------------------------
// Digest rankings
// ---------------------------------------------------------------------------

struct Rankings {
    repos: Ranking,
    features: Ranking,
}

impl Rankings {
    fn new(collected: &Collected, limit: usize) -> Self {
        let mut seen = HashSet::new();
        let commits: Vec<&GitCommit> = collected
            .commits
            .iter()
            .chain(&collected.agent_commits)
            .filter(|commit| in_window(collected.window, commit.timestamp))
            .filter(|commit| !commit.authorship.is_agent_authored())
            .filter(|commit| seen.insert((commit.repo_member_id.as_str(), commit.sha.as_str())))
            .collect();
        rankings(&collected.timeline, &commits, limit)
    }
}

fn rankings(timeline: &Timeline, commits: &[&GitCommit], limit: usize) -> Rankings {
    Rankings {
        repos: rank(
            timeline,
            commits,
            limit,
            |piece| (piece.repo_id.clone(), piece.repo.clone()),
            |commit| (commit.repo_id.clone(), commit.repo.clone()),
        ),
        features: rank(
            timeline,
            commits,
            limit,
            |piece| {
                let label = attribution::feature_label(piece.branch.as_deref());
                (label.clone(), label)
            },
            |commit| {
                let label = attribution::feature_label(commit.branch.as_deref());
                (label.clone(), label)
            },
        ),
    }
}

/// Ranks by human time, then agent wall time, naming each row by `key`'s
/// second element.
fn rank(
    timeline: &Timeline,
    commits: &[&GitCommit],
    limit: usize,
    piece_key: impl Fn(&Interval) -> (String, String),
    commit_key: impl Fn(&GitCommit) -> (String, String),
) -> Ranking {
    #[derive(Default)]
    struct Row {
        name: String,
        human: f64,
        agent: Vec<Interval>,
        commits: usize,
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for piece in &timeline.human_intervals {
        let (key, name) = piece_key(piece);
        let row = rows.entry(key).or_default();
        row.name = name;
        row.human += piece.seconds();
    }
    for interval in &timeline.ai_intervals {
        let (key, name) = piece_key(interval);
        let row = rows.entry(key).or_default();
        if row.name.is_empty() {
            row.name = name;
        }
        row.agent.push(interval.clone());
    }
    for commit in commits {
        let (key, name) = commit_key(commit);
        let row = rows.entry(key).or_default();
        if row.name.is_empty() {
            row.name = name;
        }
        row.commits += 1;
    }
    let total: f64 = rows.values().map(|row| row.human).sum();
    let mut ranked: Vec<RankedRow> = rows
        .into_values()
        .map(|row| RankedRow {
            share_of_human: ratio(row.human, total).map(round3),
            agent_wall_seconds: round3(span_seconds(&union_spans(row.agent.iter()))),
            human_seconds: round3(row.human),
            commits: row.commits,
            name: row.name,
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .human_seconds
            .total_cmp(&left.human_seconds)
            .then_with(|| right.agent_wall_seconds.total_cmp(&left.agent_wall_seconds))
            .then_with(|| right.commits.cmp(&left.commits))
            .then_with(|| left.name.cmp(&right.name))
    });
    let omitted = ranked.len().saturating_sub(limit);
    ranked.truncate(limit);
    Ranking {
        rows: ranked,
        omitted,
    }
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

fn na(value: Option<String>) -> String {
    value.unwrap_or_else(|| "n/a".to_string())
}

fn times(value: Option<f64>) -> String {
    na(value.map(|value| format!("{value:.1}×")))
}

fn usd(value: Option<f64>) -> String {
    na(value.map(|value| {
        let cents = (value * 100.0).round() as u64;
        format!("${}.{:02}", number(cents / 100), cents % 100)
    }))
}

fn tokens_text(value: Option<f64>) -> String {
    na(value.map(|value| compact_tokens(value.round() as u64)))
}

fn local_day(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Local).date_naive().to_string())
        .unwrap_or_else(|_| text.to_string())
}

fn facts(rows: &[(&str, String)]) -> Block {
    Block::Facts(
        rows.iter()
            .map(|(label, value)| ((*label).to_string(), value.clone()))
            .collect(),
    )
}

fn focus_blocks(blocks: &mut Vec<Block>, focus: &Focus, top: usize, with_days: bool) {
    let aggregate = &focus.aggregate;
    blocks.push(Block::Section("Focus".to_string()));
    if aggregate.block_count == 0 {
        blocks.push(Block::Paragraph(
            "No human work blocks in this window.".to_string(),
        ));
        return;
    }
    blocks.push(facts(&[
        (
            "Work blocks",
            format!(
                "{} ({} under {} minutes)",
                aggregate.block_count,
                aggregate.short_block_count,
                (aggregate.short_block_threshold_seconds / 60.0).round()
            ),
        ),
        (
            "Longest block",
            format!(
                "{}{}",
                hours(aggregate.longest_block_seconds),
                aggregate
                    .longest_block_start
                    .as_deref()
                    .map(|start| format!(" ({})", local_day(start)))
                    .unwrap_or_default()
            ),
        ),
        (
            "Longest single-repo stretch",
            format!(
                "{}{}",
                hours(aggregate.longest_repo_stretch_seconds),
                aggregate
                    .longest_repo_stretch_repo
                    .as_deref()
                    .map(|repo| format!(" ({repo})"))
                    .unwrap_or_default()
            ),
        ),
        (
            "Average block",
            na(aggregate.average_block_seconds.map(hours)),
        ),
        (
            "Context switches",
            format!(
                "{} ({} per block, {} per human hour)",
                aggregate.context_switches,
                na(aggregate
                    .context_switches_per_block
                    .map(|value| format!("{value:.1}"))),
                na(aggregate
                    .context_switches_per_human_hour
                    .map(|value| format!("{value:.1}")))
            ),
        ),
    ]));
    if !with_days {
        return;
    }
    let skipped = focus.days.len().saturating_sub(top);
    let rows = focus
        .days
        .iter()
        .skip(skipped)
        .map(|day| {
            vec![
                day.date.clone(),
                hours(day.human_seconds),
                day.block_count.to_string(),
                day.short_block_count.to_string(),
                hours(day.longest_block_seconds),
                hours(day.longest_repo_stretch_seconds),
                day.context_switches.to_string(),
            ]
        })
        .collect();
    blocks.push(Block::Table(Table::new(
        vec![
            Column::text("Day"),
            Column::number("Human"),
            Column::number("Blocks"),
            Column::number("Short"),
            Column::number("Longest"),
            Column::number("Repo stretch"),
            Column::number("Switches"),
        ],
        rows,
    )));
    if skipped > 0 {
        blocks.push(Block::Paragraph(format!(
            "Showing the latest {top} of {} days with work.",
            focus.days.len()
        )));
    }
    blocks.push(Block::Paragraph(
        "A block belongs to the day it starts on. Context switches count changes of repository between consecutive pieces of one block, never across blocks.".to_string(),
    ));
}

/// One cell of a heatmap: shaded by the share of the busiest cell.
fn shade(value: f64, peak: f64) -> &'static str {
    if value <= 0.0 || peak <= 0.0 {
        return "·";
    }
    match (value / peak * 4.0).ceil() as u32 {
        0 | 1 => "░",
        2 => "▒",
        3 => "▓",
        _ => "█",
    }
}

fn heatmap_table(matrix: &[[f64; 24]]) -> Table {
    let peak = matrix
        .iter()
        .flat_map(|row| row.iter().copied())
        .fold(0.0, f64::max);
    let mut columns = vec![Column::text("")];
    columns.extend((0..24).map(|hour| Column::text(format!("{hour:02}"))));
    columns.push(Column::number("Total"));
    let rows = matrix
        .iter()
        .zip(WEEKDAYS)
        .map(|(row, name)| {
            let mut cells = vec![name.to_string()];
            cells.extend(row.iter().map(|value| shade(*value, peak).to_string()));
            cells.push(hours(row.iter().sum()));
            cells
        })
        .collect();
    Table::new(columns, rows)
}

fn patterns_blocks(blocks: &mut Vec<Block>, patterns: &Patterns) {
    blocks.push(Block::Section("Heatmap".to_string()));
    blocks.push(Block::Paragraph(
        "Human work by local weekday (rows) and hour of day (columns). · none, ░ ▒ ▓ █ rising toward the busiest hour.".to_string(),
    ));
    blocks.push(Block::Table(heatmap_table(&patterns.human_seconds)));
    let agent_total: f64 = patterns.agent_wall_seconds.iter().flatten().sum();
    if agent_total > 0.0 {
        blocks.push(Block::Paragraph(
            "Agent wall clock, overlapping agents counted once.".to_string(),
        ));
        blocks.push(Block::Table(heatmap_table(&patterns.agent_wall_seconds)));
    }
    let share = |value: Option<f64>| {
        value.map_or_else(String::new, |value| {
            format!(" ({} of human time)", percent(value))
        })
    };
    blocks.push(facts(&[
        (
            "Late night",
            format!(
                "{} between {} and {} on {} night{}{}",
                hours(patterns.night.human_seconds),
                patterns.night.from,
                patterns.night.to,
                patterns.night.days,
                if patterns.night.days == 1 { "" } else { "s" },
                share(patterns.night.share_of_human)
            ),
        ),
        (
            "Weekend",
            format!(
                "{} on {} day{} ({}){}",
                hours(patterns.weekend.human_seconds),
                patterns.weekend.days,
                if patterns.weekend.days == 1 { "" } else { "s" },
                patterns.weekend.weekdays.join(", "),
                share(patterns.weekend.share_of_human)
            ),
        ),
        ("Days with work", patterns.active_days.to_string()),
    ]));
}

fn leverage_blocks(blocks: &mut Vec<Block>, leverage: &Leverage) {
    blocks.push(Block::Section("Leverage".to_string()));
    let sessions =
        leverage.foreground_sessions_with_commits + leverage.foreground_sessions_without_commits;
    blocks.push(facts(&[
        ("Human work", hours(leverage.human_seconds)),
        ("Agent wall clock", hours(leverage.agent_wall_seconds)),
        (
            "Agent wall per human hour",
            times(leverage.agent_wall_per_human_hour),
        ),
        (
            "Parallel agent work per human hour",
            times(leverage.parallel_agent_per_human_hour),
        ),
        (
            "Commits / changed lines",
            format!(
                "{} / {}",
                number(leverage.commits),
                number(leverage.changed_lines)
            ),
        ),
        ("Tokens", compact_tokens(leverage.tokens)),
        ("Tokens per commit", tokens_text(leverage.tokens_per_commit)),
        (
            "Tokens per 100 changed lines",
            tokens_text(leverage.tokens_per_100_lines),
        ),
        (
            "Tokens per human hour",
            tokens_text(leverage.tokens_per_human_hour),
        ),
        (
            "List value (priced models)",
            usd((leverage.list_value_usd > 0.0 || leverage.tokens > 0)
                .then_some(leverage.list_value_usd)),
        ),
        ("List value per commit", usd(leverage.list_value_per_commit)),
        (
            "List value per 100 changed lines",
            usd(leverage.list_value_per_100_lines),
        ),
        (
            "List value per human hour",
            usd(leverage.list_value_per_human_hour),
        ),
        (
            "Sessions without commits",
            match leverage.sessions_without_commits_share {
                Some(share) => format!(
                    "{} of {} ({})",
                    leverage.foreground_sessions_without_commits,
                    sessions,
                    percent(share)
                ),
                None => "n/a (no sessions in repositories with commits)".to_string(),
            },
        ),
    ]));
    blocks.push(Block::Paragraph(format!(
        "List value is the tokens priced at published list rates (as of {}), not what was billed. Sessions without commits had none in the same repository within the idle window; that covers reading, review and uncommitted work.",
        leverage.rates_as_of
    )));
}

fn models_blocks(blocks: &mut Vec<Block>, models: &Models, top: usize) {
    blocks.push(Block::Section("Providers and models".to_string()));
    if models.providers.is_empty() {
        blocks.push(Block::Paragraph(
            "No agent or human activity in this window.".to_string(),
        ));
        return;
    }
    let columns = |first: &str| {
        vec![
            Column::text(first),
            Column::number("Sessions"),
            Column::number("Subagents"),
            Column::number("With commits"),
            Column::number("Agent wall"),
            Column::number("Tokens"),
            Column::number("List value"),
            Column::number("Human"),
        ]
    };
    let provider_rows = models
        .providers
        .iter()
        .map(|row| {
            vec![
                row.provider.clone(),
                number(row.sessions),
                number(row.subagent_sessions),
                number(row.sessions_with_commits),
                hours(row.agent_wall_seconds),
                compact_tokens(row.tokens),
                usd(row.list_value_usd),
                hours(row.human_seconds),
            ]
        })
        .collect();
    blocks.push(Block::Table(Table::new(columns("Provider"), provider_rows)));
    let omitted = models.models.len().saturating_sub(top);
    let model_rows = models
        .models
        .iter()
        .take(top)
        .map(|row| {
            vec![
                format!("{} / {}", row.provider, row.model),
                number(row.sessions),
                number(row.subagent_sessions),
                number(row.sessions_with_commits),
                hours(row.agent_wall_seconds),
                compact_tokens(row.tokens),
                if row.priced {
                    usd(row.list_value_usd)
                } else {
                    "unpriced".to_string()
                },
                hours(row.human_seconds),
            ]
        })
        .collect();
    blocks.push(Block::Table(Table::new(columns("Model"), model_rows)));
    if omitted > 0 {
        blocks.push(Block::Paragraph(format!(
            "{omitted} more model rows are in --format json."
        )));
    }
    blocks.push(Block::Paragraph(
        "A session is counted under every model it used, so model rows can add up to more than the provider's. Human time is attributed through the provider of the nearest prompt, session edge or commit; `git` is the commits themselves. List value is not billed cost.".to_string(),
    ));
}

fn warning_blocks(
    blocks: &mut Vec<Block>,
    diagnostics: &Diagnostics,
    extra: &[String],
    shareable: bool,
) {
    let home = home_dir();
    let mut lines = warning_lines(diagnostics, shareable.then_some(home.as_path()));
    lines.extend(extra.iter().map(|warning| format!("Warning: {warning}")));
    if !lines.is_empty() {
        blocks.push(Block::Section("Warnings".to_string()));
        blocks.push(Block::List(lines));
    }
}

fn insights_document(
    window: &WindowInfo,
    analysis: &Analysis,
    sections: Sections,
    top: usize,
    diagnostics: &Diagnostics,
    extra: &[String],
    shareable: bool,
) -> Document {
    let mut blocks = vec![Block::Paragraph(NOTE.to_string())];
    if sections.focus {
        focus_blocks(&mut blocks, &analysis.focus, top, true);
    }
    if sections.heatmap {
        patterns_blocks(&mut blocks, &analysis.patterns);
    }
    if sections.leverage {
        leverage_blocks(&mut blocks, &analysis.leverage);
    }
    if sections.models {
        models_blocks(&mut blocks, &analysis.models, top);
    }
    warning_blocks(&mut blocks, diagnostics, extra, shareable);
    Document {
        title: format!("workstats insights — {}", window.label),
        blocks,
    }
}

fn ranking_block(blocks: &mut Vec<Block>, title: &str, first: &str, ranking: &Ranking) {
    blocks.push(Block::Section(title.to_string()));
    if ranking.rows.is_empty() {
        blocks.push(Block::Paragraph("Nothing in this window.".to_string()));
        return;
    }
    let rows = ranking
        .rows
        .iter()
        .map(|row| {
            vec![
                row.name.clone(),
                hours(row.human_seconds),
                row.share_of_human
                    .map_or_else(|| "n/a".to_string(), percent),
                hours(row.agent_wall_seconds),
                number(row.commits),
            ]
        })
        .collect();
    blocks.push(Block::Table(Table::new(
        vec![
            Column::text(first),
            Column::number("Human"),
            Column::number("Share"),
            Column::number("Agent wall"),
            Column::number("Commits"),
        ],
        rows,
    )));
    if ranking.omitted > 0 {
        blocks.push(Block::Paragraph(format!(
            "{} more in --format json with a larger --top.",
            ranking.omitted
        )));
    }
}

/// The goals section of a digest: hours per week against the target and
/// each cap's share, the same lines the plain report prints.
fn goal_blocks(goals: &GoalReport) -> Vec<Block> {
    let lines = goals.lines();
    let body = if lines.is_empty() {
        Block::Paragraph("No goal figures fall in this window.".to_string())
    } else {
        Block::List(lines)
    };
    vec![Block::Section("Goals".to_string()), body]
}

#[allow(clippy::too_many_arguments)]
fn digest_document(
    window: &WindowInfo,
    comparison: &Option<Comparison>,
    rankings: &Rankings,
    analysis: &Analysis,
    goals: Option<&GoalReport>,
    diagnostics: &Diagnostics,
    extra: &[String],
    shareable: bool,
) -> Document {
    let mut blocks = vec![Block::Paragraph(NOTE.to_string())];
    match comparison {
        Some(comparison) => push_comparison(&mut blocks, comparison),
        None => blocks.push(Block::Paragraph(
            "No comparison: the window has no end to measure a previous one from; choose --week, --month, --year, or both --since and --until.".to_string(),
        )),
    }
    ranking_block(
        &mut blocks,
        "Top repositories",
        "Repository",
        &rankings.repos,
    );
    ranking_block(&mut blocks, "Top features", "Feature", &rankings.features);
    focus_blocks(&mut blocks, &analysis.focus, DIGEST_ROWS, false);
    leverage_blocks(&mut blocks, &analysis.leverage);
    if let Some(goals) = goals {
        blocks.extend(goal_blocks(goals));
    }
    warning_blocks(&mut blocks, diagnostics, extra, shareable);
    Document {
        title: format!("workstats digest — {}", window.label),
        blocks,
    }
}

// ---------------------------------------------------------------------------
// Terminal rendering
// ---------------------------------------------------------------------------

/// The `Document` as terminal text: the table view of both commands. The
/// renderers for Markdown and HTML live with the document model; this is the
/// third, and it escapes nothing but still passes every string through
/// `safe_text`, as they do.
fn render_text(document: &Document) -> String {
    let mut output = format!("{}\n", safe_text(&document.title));
    for block in &document.blocks {
        output.push('\n');
        match block {
            Block::Section(title) => {
                output.push_str(&format!("{}\n", safe_text(title)));
            }
            Block::Paragraph(text) => push_wrapped(&mut output, &safe_text(text)),
            Block::List(items) => {
                for item in items {
                    output.push_str(&format!("  - {}\n", safe_text(item)));
                }
            }
            Block::Facts(facts) => {
                let width = facts
                    .iter()
                    .map(|(label, _)| safe_text(label).chars().count())
                    .max()
                    .unwrap_or(0);
                for (label, value) in facts {
                    output.push_str(&format!(
                        "  {:<width$}  {}\n",
                        safe_text(label),
                        safe_text(value)
                    ));
                }
            }
            Block::Table(table) => push_text_table(&mut output, table),
        }
    }
    output
}

fn push_wrapped(output: &mut String, text: &str) {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + word.chars().count() >= 96 {
            output.push_str(&format!("  {line}\n"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        output.push_str(&format!("  {line}\n"));
    }
}

fn push_text_table(output: &mut String, table: &Table) {
    let rows: Vec<Vec<String>> = std::iter::once(
        table
            .columns
            .iter()
            .map(|column| safe_text(&column.label))
            .collect::<Vec<_>>(),
    )
    .chain(
        table
            .rows
            .iter()
            .chain(table.total.iter())
            .map(|row| row.iter().map(|cell| safe_text(cell)).collect()),
    )
    .collect();
    let widths: Vec<usize> = (0..table.columns.len())
        .map(|index| {
            rows.iter()
                .map(|row| row.get(index).map_or(0, |cell| cell.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    // Two spaces between columns, one between two narrow ones: a heatmap's
    // 24 one-glyph hours would otherwise run far past a terminal's width.
    let gap = |index: usize| {
        if index == 0 {
            ""
        } else if widths[index] <= 2 && widths[index - 1] <= 2 {
            " "
        } else {
            "  "
        }
    };
    let line = |row: &Vec<String>| {
        let mut text = String::new();
        for (index, column) in table.columns.iter().enumerate() {
            let cell = row.get(index).map_or("", String::as_str);
            text.push_str(gap(index));
            if column.numeric {
                text.push_str(&format!("{cell:>width$}", width = widths[index]));
            } else {
                text.push_str(&format!("{cell:<width$}", width = widths[index]));
            }
        }
        format!("  {}\n", text.trim_end())
    };
    output.push_str(&line(&rows[0]));
    output.push_str(&format!(
        "  {}\n",
        "─".repeat(
            (0..widths.len())
                .map(|index| widths[index] + gap(index).len())
                .sum::<usize>()
        )
    ));
    for row in &rows[1..] {
        output.push_str(&line(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, LocalResult, NaiveDateTime};

    use crate::aggregate::build_report;
    use crate::allocate::{self, AllocationOptions, Basis, GapPolicy, Plan};
    use crate::model::{ActivityPoint, Authorship, BranchSource, ExactInterval, TokenEvent};

    fn utc(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0).unwrap()
    }

    fn piece(start: DateTime<Utc>, minutes: i64, repo: &str, block: &str) -> Interval {
        Interval {
            start,
            end: start + Duration::minutes(minutes),
            provider: "claude".to_string(),
            model: "claude-opus-5".to_string(),
            session_id: block.to_string(),
            cwd: format!("/work/{repo}"),
            repo: repo.to_string(),
            repo_id: format!("repo:{repo}"),
            root: format!("/work/{repo}"),
            branch: None,
        }
    }

    fn agent(start: DateTime<Utc>, minutes: i64, session: &str) -> Interval {
        Interval {
            session_id: session.to_string(),
            ..piece(start, minutes, "a", session)
        }
    }

    fn input<'a>(
        timeline: &'a Timeline,
        sessions: &'a [Session],
        commits: &'a [GitCommit],
        rates: &'a RateOverrides,
    ) -> Input<'a> {
        Input {
            timeline,
            sessions,
            commits,
            agent_commits: &[],
            window: (None, None),
            human_idle: Duration::hours(1),
            rates,
            engagements_configured: false,
            patterns: PatternSettings::default(),
            today: NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
        }
    }

    fn timeline(human: Vec<Interval>, ai: Vec<Interval>) -> Timeline {
        Timeline {
            human_intervals: human,
            human_signals: Vec::new(),
            ai_intervals: ai,
        }
    }

    fn utc_zone() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn commit(
        sha: &str,
        repo: &str,
        at: DateTime<Utc>,
        additions: u64,
        deletions: u64,
    ) -> GitCommit {
        GitCommit {
            repo_member_id: format!("repo:{repo}"),
            repo_id: format!("repo:{repo}"),
            repo: repo.to_string(),
            cwd: format!("/work/{repo}"),
            root: format!("/work/{repo}"),
            sha: sha.to_string(),
            timestamp: at,
            additions,
            deletions,
            ignored_additions: 0,
            ignored_deletions: 0,
            files: Vec::new(),
            categories: Default::default(),
            authorship: Authorship::default(),
            branch: None,
            branch_source: BranchSource::None,
        }
    }

    fn session(
        id: &str,
        repo: &str,
        start: DateTime<Utc>,
        minutes: i64,
        model: &str,
        output_tokens: u64,
    ) -> Session {
        let end = start + Duration::minutes(minutes);
        Session {
            provider: "claude".to_string(),
            session_id: id.to_string(),
            cwd: format!("/work/{repo}"),
            repo: repo.to_string(),
            repo_id: format!("repo:{repo}"),
            root: format!("/work/{repo}"),
            points: vec![
                ActivityPoint {
                    timestamp: start,
                    model: model.to_string(),
                },
                ActivityPoint {
                    timestamp: end,
                    model: model.to_string(),
                },
            ],
            exact_intervals: vec![ExactInterval {
                start,
                end,
                model: model.to_string(),
            }],
            human_points: vec![ActivityPoint {
                timestamp: start,
                model: model.to_string(),
            }],
            token_events: vec![TokenEvent {
                timestamp: start + Duration::minutes(1),
                model: model.to_string(),
                usage: TokenUsage {
                    input_tokens: 1_000,
                    output_tokens,
                    cache_read_tokens: 5_000,
                    cache_creation_tokens: 2_000,
                },
            }],
            is_subagent: false,
            source_file: Default::default(),
            branches: Vec::new(),
            branch_source: BranchSource::None,
            pull_requests: Vec::new(),
        }
    }

    // ---- focus ----

    #[test]
    fn focus_finds_the_longest_block_stretch_and_switches() {
        let human = vec![
            // Block 0, one local day: 60 minutes in `a`, then 30 in `b`, then 30 in `a`.
            piece(utc(3, 9, 0), 60, "a", "work-block:0"),
            piece(utc(3, 10, 0), 30, "b", "work-block:0"),
            piece(utc(3, 10, 30), 30, "a", "work-block:0"),
            // Block 1, short, and starting in the repo block 0 ended in.
            piece(utc(3, 14, 0), 20, "b", "work-block:1"),
            // Block 2 on the next day.
            piece(utc(4, 9, 0), 45, "a", "work-block:2"),
        ];
        let timeline = timeline(human, Vec::new());
        let rates = RateOverrides::default();
        let found = focus(&input(&timeline, &[], &[], &rates), &utc_zone());
        let aggregate = &found.aggregate;
        assert_eq!(3, aggregate.block_count);
        assert_eq!(1, aggregate.short_block_count);
        assert_eq!(7200.0, aggregate.longest_block_seconds);
        assert_eq!(3600.0, aggregate.longest_repo_stretch_seconds);
        assert_eq!(Some("a".to_string()), aggregate.longest_repo_stretch_repo);
        // a→b and b→a inside block 0. Block 1 starts in `b` where block 0 ended
        // in `a`, and that is across a block boundary, so it is not a switch.
        assert_eq!(2, aggregate.context_switches);
        assert_eq!(Some(0.667), aggregate.context_switches_per_block);
        assert_eq!(2, found.days.len());
        assert_eq!("2026-08-03", found.days[0].date);
        assert_eq!(2, found.days[0].block_count);
        assert_eq!(1, found.days[0].short_block_count);
        assert_eq!(7200.0, found.days[0].longest_block_seconds);
        assert_eq!(2, found.days[0].context_switches);
        assert_eq!(2700.0, found.days[1].human_seconds);
        assert_eq!(0, found.days[1].context_switches);
    }

    #[test]
    fn a_block_that_crosses_midnight_is_one_block_on_the_day_it_started() {
        let human = vec![
            piece(utc(3, 23, 30), 30, "a", "work-block:0"),
            piece(utc(4, 0, 0), 45, "a", "work-block:0"),
        ];
        let timeline = timeline(human, Vec::new());
        let rates = RateOverrides::default();
        let found = focus(&input(&timeline, &[], &[], &rates), &utc_zone());
        assert_eq!(1, found.aggregate.block_count);
        assert_eq!(4500.0, found.aggregate.longest_block_seconds);
        assert_eq!(1, found.days.len());
        assert_eq!("2026-08-03", found.days[0].date);
        // The day is the viewer's: the same block starts on the 4th at +02:00.
        let later = focus(
            &input(&timeline, &[], &[], &rates),
            &FixedOffset::east_opt(2 * 3600).unwrap(),
        );
        assert_eq!("2026-08-04", later.days[0].date);
    }

    #[test]
    fn an_empty_timeline_has_no_focus_and_no_divisions() {
        let timeline = timeline(Vec::new(), Vec::new());
        let rates = RateOverrides::default();
        let found = focus(&input(&timeline, &[], &[], &rates), &utc_zone());
        assert_eq!(0, found.aggregate.block_count);
        assert_eq!(None, found.aggregate.average_block_seconds);
        assert_eq!(None, found.aggregate.context_switches_per_human_hour);
        assert!(found.days.is_empty());
        let mut blocks = Vec::new();
        focus_blocks(&mut blocks, &found, 30, true);
        assert!(matches!(blocks.last(), Some(Block::Paragraph(text)) if text.contains("No human")));
    }

    // ---- patterns ----

    #[test]
    fn the_matrix_splits_work_at_hour_boundaries_in_the_viewers_zone() {
        // 2026-08-03 is a Monday. 09:30–11:15 at +02:00 is 07:30–09:15 UTC.
        let human = vec![piece(utc(3, 7, 30), 105, "a", "work-block:0")];
        // Two agents overlapping from 10:00 to 10:40 (+02:00): one stretch.
        let ai = vec![
            agent(utc(3, 8, 0), 30, "s1"),
            agent(utc(3, 8, 10), 30, "s2"),
        ];
        let timeline = timeline(human, ai);
        let rates = RateOverrides::default();
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let found = patterns(&input(&timeline, &[], &[], &rates), &zone);
        assert_eq!(1800.0, found.human_seconds[0][9]);
        assert_eq!(3600.0, found.human_seconds[0][10]);
        assert_eq!(900.0, found.human_seconds[0][11]);
        let human_total: f64 = found.human_seconds.iter().flatten().sum();
        assert_eq!(6300.0, human_total);
        assert_eq!(2400.0, found.agent_wall_seconds[0][10]);
        let agent_total: f64 = found.agent_wall_seconds.iter().flatten().sum();
        assert_eq!(2400.0, agent_total);
    }

    /// America/New_York in 2026, spelled out: UTC-5 until 07:00 UTC on 8 March,
    /// UTC-4 until 06:00 UTC on 1 November. The suite cannot change the process
    /// timezone (that needs `unsafe`), so a real DST day is exercised through a
    /// zone of its own.
    #[derive(Clone, Copy, Debug)]
    struct NewYork;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Behind(i32);

    impl chrono::Offset for Behind {
        fn fix(&self) -> FixedOffset {
            FixedOffset::west_opt(self.0 * 3600).unwrap()
        }
    }

    impl std::fmt::Display for Behind {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "-{:02}:00", self.0)
        }
    }

    fn local(month: u32, day: u32, hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, month, day)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    impl TimeZone for NewYork {
        type Offset = Behind;

        fn from_offset(_: &Behind) -> Self {
            NewYork
        }

        fn offset_from_local_date(&self, date: &NaiveDate) -> LocalResult<Behind> {
            self.offset_from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, at: &NaiveDateTime) -> LocalResult<Behind> {
            if *at >= local(3, 8, 2) && *at < local(3, 8, 3) {
                return LocalResult::None;
            }
            if *at >= local(11, 1, 1) && *at < local(11, 1, 2) {
                return LocalResult::Ambiguous(Behind(4), Behind(5));
            }
            if *at >= local(3, 8, 3) && *at < local(11, 1, 1) {
                LocalResult::Single(Behind(4))
            } else {
                LocalResult::Single(Behind(5))
            }
        }

        fn offset_from_utc_date(&self, date: &NaiveDate) -> Behind {
            self.offset_from_utc_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, at: &NaiveDateTime) -> Behind {
            if *at >= local(3, 8, 7) && *at < local(11, 1, 6) {
                Behind(4)
            } else {
                Behind(5)
            }
        }
    }

    fn matrix_row(piece: Interval, weekday: usize) -> [f64; 24] {
        let timeline = timeline(vec![piece], Vec::new());
        let rates = RateOverrides::default();
        patterns(&input(&timeline, &[], &[], &rates), &NewYork).human_seconds[weekday]
    }

    #[test]
    fn spring_forward_gives_the_skipped_hour_nothing() {
        // 06:30–08:30 UTC on Sunday 8 March: 01:30 EST, the clocks jump at
        // 02:00 to 03:00 EDT, and it ends at 04:30 EDT.
        let start = Utc.with_ymd_and_hms(2026, 3, 8, 6, 30, 0).unwrap();
        let row = matrix_row(piece(start, 120, "a", "work-block:0"), 6);
        assert_eq!(1800.0, row[1]);
        assert_eq!(0.0, row[2]);
        assert_eq!(3600.0, row[3]);
        assert_eq!(1800.0, row[4]);
        assert_eq!(7200.0, row.iter().sum::<f64>());
    }

    #[test]
    fn fall_back_gives_the_repeated_hour_both_passes() {
        // 05:30–07:30 UTC on Sunday 1 November: 01:30 EDT, the clocks fall back
        // at 02:00 EDT to 01:00 EST, and it ends at 02:30 EST.
        let start = Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap();
        let row = matrix_row(piece(start, 120, "a", "work-block:0"), 6);
        assert_eq!(1800.0 + 3600.0, row[1]);
        assert_eq!(1800.0, row[2]);
        assert_eq!(7200.0, row.iter().sum::<f64>());
    }

    #[test]
    fn late_night_and_weekend_time_and_days_are_counted_by_local_clock() {
        // Friday 2026-08-14 21:30 to Saturday 00:30 (UTC): the night runs from
        // 22:00, so 2h30m of it, and the last half hour is also weekend time.
        let human = vec![
            piece(utc(14, 21, 30), 60, "a", "work-block:0"),
            piece(utc(14, 22, 30), 120, "a", "work-block:0"),
            // A Monday morning that is neither.
            piece(utc(17, 9, 0), 60, "a", "work-block:1"),
        ];
        let timeline = timeline(human, Vec::new());
        let rates = RateOverrides::default();
        let found = patterns(&input(&timeline, &[], &[], &rates), &utc_zone());
        assert_eq!(9000.0, found.night.human_seconds);
        assert_eq!(1, found.night.days, "one night, named by its evening");
        assert_eq!(1800.0, found.weekend.human_seconds);
        assert_eq!(1, found.weekend.days);
        assert_eq!(vec!["sat", "sun"], found.weekend.weekdays);
        assert_eq!(3, found.active_days);
        assert_eq!(Some(0.625), found.night.share_of_human);
        assert_eq!("22:00", found.night.from);
        assert_eq!("06:00", found.night.to);
    }

    #[test]
    fn night_and_weekend_follow_the_insights_config() {
        let config = serde_json::json!({"night": ["21:00", "07:00"], "weekend": ["fri"]});
        let settings = PatternSettings::from_config(Some(&config)).unwrap();
        assert_eq!(21 * 60, settings.night_from);
        assert_eq!(7 * 60, settings.night_to);
        assert_eq!(BTreeSet::from([4]), settings.weekend);
        let human = vec![piece(utc(14, 20, 30), 90, "a", "work-block:0")];
        let timeline = timeline(human, Vec::new());
        let rates = RateOverrides::default();
        let mut shaped = input(&timeline, &[], &[], &rates);
        shaped.patterns = settings;
        let found = patterns(&shaped, &utc_zone());
        // 20:30–22:00: an hour of it is night, all of it is Friday.
        assert_eq!(3600.0, found.night.human_seconds);
        assert_eq!(5400.0, found.weekend.human_seconds);
        assert_eq!(vec!["fri"], found.weekend.weekdays);
    }

    #[test]
    fn a_daytime_night_window_does_not_wrap() {
        let config = serde_json::json!({"night": ["01:00", "03:00"]});
        let settings = PatternSettings::from_config(Some(&config)).unwrap();
        assert!(!settings.wraps());
        assert!(settings.is_night(90));
        assert!(!settings.is_night(180));
        assert!(!settings.is_night(30));
    }

    #[test]
    fn a_bad_insights_config_is_refused_by_key() {
        let refused = |value: serde_json::Value| {
            PatternSettings::from_config(Some(&value))
                .unwrap_err()
                .to_string()
        };
        assert!(refused(serde_json::json!({"night": ["22:00"]})).contains("insights.night"));
        assert!(refused(serde_json::json!({"night": ["22:00", "25:00"]})).contains("25:00"));
        assert!(refused(serde_json::json!({"night": ["22:00", "22:00"]})).contains("different"));
        assert!(refused(serde_json::json!({"weekend": ["sat", "funday"]})).contains("funday"));
        assert!(refused(serde_json::json!({"holidays": []})).contains("holidays"));
        assert!(refused(serde_json::json!(["night"])).contains("object"));
        assert_eq!(
            PatternSettings::default(),
            PatternSettings::from_config(None).unwrap()
        );
    }

    // ---- leverage ----

    #[test]
    fn every_ratio_is_n_a_when_it_has_nothing_to_divide_by() {
        let timeline = timeline(Vec::new(), Vec::new());
        let rates = RateOverrides::default();
        let found = usage(&input(&timeline, &[], &[], &rates));
        let leverage = &found.leverage;
        assert_eq!(None, leverage.agent_wall_per_human_hour);
        assert_eq!(None, leverage.parallel_agent_per_human_hour);
        assert_eq!(None, leverage.tokens_per_commit);
        assert_eq!(None, leverage.tokens_per_100_lines);
        assert_eq!(None, leverage.tokens_per_human_hour);
        assert_eq!(None, leverage.list_value_per_commit);
        assert_eq!(None, leverage.list_value_per_100_lines);
        assert_eq!(None, leverage.list_value_per_human_hour);
        assert_eq!(None, leverage.sessions_without_commits_share);
        let mut blocks = Vec::new();
        leverage_blocks(&mut blocks, leverage);
        let Block::Facts(rows) = &blocks[1] else {
            panic!("facts");
        };
        let value = |label: &str| {
            rows.iter()
                .find(|(name, _)| name == label)
                .unwrap_or_else(|| panic!("{label}"))
                .1
                .clone()
        };
        assert_eq!("n/a", value("Agent wall per human hour"));
        assert_eq!("n/a", value("Tokens per commit"));
        assert_eq!("n/a", value("List value per human hour"));
        assert!(value("Sessions without commits").starts_with("n/a"));
    }

    #[test]
    fn ratios_with_a_zero_in_only_one_denominator_stay_defined_elsewhere() {
        // Human time and tokens but no commits: per-commit and per-line figures
        // are n/a, per-human-hour ones are not.
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a", start, 30, "claude-opus-5", 10_000)];
        let timeline = timeline(
            vec![piece(start, 60, "a", "work-block:0")],
            vec![agent(start, 30, "s1")],
        );
        let rates = RateOverrides::default();
        let found = usage(&input(&timeline, &sessions, &[], &rates)).leverage;
        assert_eq!(None, found.tokens_per_commit);
        assert_eq!(None, found.list_value_per_100_lines);
        assert_eq!(Some(18_000.0), found.tokens_per_human_hour);
        assert_eq!(Some(0.5), found.agent_wall_per_human_hour);
        assert!(found.list_value_per_human_hour.unwrap() > 0.0);
    }

    #[test]
    fn leverage_divides_tokens_and_list_value_by_commits_lines_and_hours() {
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a", start, 30, "claude-opus-5", 10_000)];
        let timeline = timeline(
            vec![piece(start, 120, "a", "work-block:0")],
            vec![
                agent(start, 30, "s1"),
                agent(start + Duration::minutes(10), 30, "s2"),
            ],
        );
        let commits = [
            commit("c1", "a", start + Duration::minutes(20), 150, 50),
            commit("c2", "a", start + Duration::minutes(40), 90, 10),
        ];
        let rates = RateOverrides::default();
        let found = usage(&input(&timeline, &sessions, &commits, &rates)).leverage;
        // 1000 + 10000 + 5000 + 2000 tokens.
        assert_eq!(18_000, found.tokens);
        assert_eq!(2, found.commits);
        assert_eq!(300, found.changed_lines);
        assert_eq!(Some(9_000.0), found.tokens_per_commit);
        assert_eq!(Some(6_000.0), found.tokens_per_100_lines);
        assert_eq!(Some(9_000.0), found.tokens_per_human_hour);
        let expected = pricing::list_value_usd(
            "claude-opus-5",
            &TokenUsage {
                input_tokens: 1_000,
                output_tokens: 10_000,
                cache_read_tokens: 5_000,
                cache_creation_tokens: 2_000,
            },
            &rates,
        )
        .unwrap();
        assert!((found.list_value_usd - expected).abs() < 1e-6);
        assert!((found.list_value_per_commit.unwrap() - expected / 2.0).abs() < 1e-6);
        assert!((found.list_value_per_100_lines.unwrap() - expected / 3.0).abs() < 1e-6);
        assert!((found.list_value_per_human_hour.unwrap() - expected / 2.0).abs() < 1e-6);
        // Two overlapping agents: 40 minutes of wall clock, 60 of parallel work,
        // against two human hours.
        assert_eq!(2400.0, found.agent_wall_seconds);
        assert_eq!(3600.0, found.parallel_agent_seconds);
        assert_eq!(Some(0.333), found.agent_wall_per_human_hour);
        assert_eq!(Some(0.5), found.parallel_agent_per_human_hour);
        // The session has a commit in its repo inside the idle window.
        assert_eq!(1, found.foreground_sessions_with_commits);
        assert_eq!(0, found.foreground_sessions_without_commits);
        assert_eq!(Some(0.0), found.sessions_without_commits_share);
    }

    #[test]
    fn sessions_without_commits_share_ignores_repos_that_never_committed() {
        let start = utc(3, 9, 0);
        let sessions = [
            session("with", "a", start, 30, "claude-opus-5", 1),
            session(
                "without",
                "a",
                start + Duration::hours(10),
                30,
                "claude-opus-5",
                1,
            ),
            // No commits in this repo at all: says nothing about output.
            session("elsewhere", "b", start, 30, "claude-opus-5", 1),
        ];
        let timeline = timeline(
            Vec::new(),
            vec![
                agent(start, 30, "with"),
                agent(start + Duration::hours(10), 30, "without"),
                agent(start, 30, "elsewhere"),
            ],
        );
        let commits = [commit("c1", "a", start + Duration::minutes(5), 1, 1)];
        let rates = RateOverrides::default();
        let found = usage(&input(&timeline, &sessions, &commits, &rates));
        assert_eq!(1, found.leverage.foreground_sessions_with_commits);
        assert_eq!(1, found.leverage.foreground_sessions_without_commits);
        assert_eq!(Some(0.5), found.leverage.sessions_without_commits_share);
        let linked: Vec<_> = found
            .models
            .models
            .iter()
            .map(|row| row.sessions_with_commits)
            .collect();
        assert_eq!(vec![1], linked);
        assert_eq!(3, found.models.models[0].sessions);
    }

    #[test]
    fn tokens_outside_the_window_are_not_counted() {
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a", start, 30, "claude-opus-5", 10_000)];
        let timeline = timeline(Vec::new(), Vec::new());
        let rates = RateOverrides::default();
        let mut shaped = input(&timeline, &sessions, &[], &rates);
        shaped.window = (Some(utc(4, 0, 0)), None);
        assert_eq!(0, usage(&shaped).leverage.tokens);
        shaped.window = (Some(utc(3, 0, 0)), Some(utc(4, 0, 0)));
        assert_eq!(18_000, usage(&shaped).leverage.tokens);
    }

    #[test]
    fn unpriced_models_are_named_and_left_out_of_list_value() {
        let start = utc(3, 9, 0);
        let sessions = [
            session("s1", "a", start, 30, "claude-opus-5", 10_000),
            session("s2", "a", start, 30, "mystery-model-9", 10_000),
        ];
        let timeline = timeline(Vec::new(), Vec::new());
        let rates = RateOverrides::default();
        let found = usage(&input(&timeline, &sessions, &[], &rates));
        assert_eq!(vec!["mystery-model-9"], found.unpriced);
        assert_eq!(36_000, found.leverage.tokens);
        let mystery = found
            .models
            .models
            .iter()
            .find(|row| row.model == "mystery-model-9")
            .unwrap();
        assert!(!mystery.priced);
        assert_eq!(None, mystery.list_value_usd);
        assert_eq!(18_000, mystery.tokens);
    }

    #[test]
    fn the_stale_rates_warning_comes_with_list_value_and_only_for_built_in_rates() {
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a", start, 30, "claude-opus-5", 10)];
        let timeline = timeline(Vec::new(), Vec::new());
        let rates = RateOverrides::default();
        let mut shaped = input(&timeline, &sessions, &[], &rates);
        let analysis = Analysis::from_input(&shaped, &utc_zone());
        assert!(
            analysis.warnings(true).is_empty(),
            "fresh rates do not warn"
        );
        shaped.today = NaiveDate::from_ymd_opt(2027, 6, 1).unwrap();
        let analysis = Analysis::from_input(&shaped, &utc_zone());
        let warnings = analysis.warnings(true);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("built-in list rates"))
        );
        assert!(
            analysis.warnings(false).is_empty(),
            "sections without a list value do not carry it"
        );
        // Priced entirely by the user's own rates: nothing built in to go stale.
        let config = serde_json::json!({"claude-opus-5": {
            "input": 1.0, "cache_write": 1.0, "cache_read": 1.0, "output": 1.0}});
        let own = RateOverrides::from_value(&config).unwrap();
        let mut shaped = input(&timeline, &sessions, &[], &own);
        shaped.today = NaiveDate::from_ymd_opt(2027, 6, 1).unwrap();
        let analysis = Analysis::from_input(&shaped, &utc_zone());
        assert!(analysis.warnings(true).is_empty());
        assert_eq!(vec!["claude-opus-5"], analysis.models.rate_overrides);
    }

    #[test]
    fn per_model_list_value_matches_what_allocate_computes_from_the_same_rows() {
        let start = utc(3, 9, 0);
        let sessions = vec![
            session("s1", "alpha", start, 30, "claude-opus-5", 10_000),
            session(
                "s2",
                "alpha",
                start + Duration::days(40),
                30,
                "claude-opus-5",
                4_000,
            ),
            session("s3", "beta", start, 30, "claude-sonnet-4-6", 25_000),
            session(
                "s4",
                "beta",
                start + Duration::days(2),
                30,
                "claude-opus-5",
                7_000,
            ),
        ];
        let dimensions: Vec<String> = ["repo", "provider", "model", "month"]
            .map(String::from)
            .to_vec();
        let built = build_report(
            &sessions,
            &[],
            &[],
            Duration::minutes(5),
            None,
            None,
            &dimensions,
            Duration::hours(1),
            Duration::minutes(30),
        );
        let allocation = allocate::build(
            &built.rows,
            &AllocationOptions {
                projects: vec!["alpha".to_string()],
                top: 0,
                subscriptions: BTreeMap::from([(
                    "claude".to_string(),
                    Plan {
                        count: 1,
                        price: 200.0,
                    },
                )]),
                vat_percent: 0.0,
                currency: "USD".to_string(),
                basis: Basis::Value,
                gap_policy: GapPolicy::Skip,
                rate_overrides: RateOverrides::default(),
                today: NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
            },
        );
        let rates = RateOverrides::default();
        let found = usage(&input(&built.timeline, &sessions, &[], &rates));
        assert_eq!(allocation.models.len(), found.models.models.len());
        for expected in &allocation.models {
            let row = found
                .models
                .models
                .iter()
                .find(|row| row.model == expected.model)
                .unwrap_or_else(|| panic!("{} missing", expected.model));
            assert!(
                (row.list_value_usd.unwrap() - expected.pool_list_value).abs() < 1e-6,
                "{}: {:?} against {}",
                expected.model,
                row.list_value_usd,
                expected.pool_list_value
            );
        }
        // The total is the sum of the models, and the provider row carries it.
        let total: f64 = allocation
            .models
            .iter()
            .map(|row| row.pool_list_value)
            .sum();
        assert!((found.leverage.list_value_usd - total).abs() < 1e-6);
        assert!((found.models.providers[0].list_value_usd.unwrap() - total).abs() < 1e-6);
    }

    // ---- digest ----

    #[test]
    fn rankings_order_by_human_time_and_say_what_the_limit_left_out() {
        let start = utc(3, 9, 0);
        let timeline = timeline(
            vec![
                piece(start, 30, "small", "work-block:0"),
                piece(start + Duration::hours(2), 90, "big", "work-block:1"),
                piece(start + Duration::hours(5), 60, "mid", "work-block:2"),
            ],
            vec![agent(start, 60, "s1")],
        );
        let commits = [commit("c1", "mid", start, 1, 1)];
        let refs: Vec<&GitCommit> = commits.iter().collect();
        let found = rankings(&timeline, &refs, 2);
        let names: Vec<_> = found
            .repos
            .rows
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(vec!["big", "mid"], names);
        // `small`, and `a`, which only an agent worked in, are the two left out.
        assert_eq!(2, found.repos.omitted);
        assert_eq!(1, found.repos.rows[1].commits);
        assert_eq!(Some(0.5), found.repos.rows[0].share_of_human);
        // No branch recorded and no rules: every feature is the placeholder.
        assert_eq!(1, found.features.rows.len() + found.features.omitted);
        assert_eq!("—", found.features.rows[0].name);
    }

    #[test]
    fn a_digest_has_every_section_and_the_goals_hook_only_when_goals_exist() {
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a", start, 30, "claude-opus-5", 10_000)];
        let timeline = timeline(
            vec![piece(start, 60, "a", "work-block:0")],
            vec![agent(start, 30, "s1")],
        );
        let rates = RateOverrides::default();
        let analysis = Analysis::from_input(&input(&timeline, &sessions, &[], &rates), &utc_zone());
        let window = WindowInfo::new((None, None), false);
        let ranked = rankings(&timeline, &[], 10);
        let sections = |goals: Option<&GoalReport>| -> Vec<String> {
            digest_document(
                &window,
                &None,
                &ranked,
                &analysis,
                goals,
                &Diagnostics::default(),
                &[],
                false,
            )
            .blocks
            .into_iter()
            .filter_map(|block| match block {
                Block::Section(title) => Some(title),
                _ => None,
            })
            .collect()
        };
        assert_eq!(
            vec!["Top repositories", "Top features", "Focus", "Leverage"],
            sections(None)
        );
        assert_eq!(
            vec![
                "Top repositories",
                "Top features",
                "Focus",
                "Leverage",
                "Goals"
            ],
            sections(Some(&GoalReport::default()))
        );
    }

    #[test]
    fn documents_render_to_text_markdown_and_html_without_script() {
        let start = utc(3, 9, 0);
        let sessions = [session("s1", "a|b", start, 30, "claude-opus-5", 10_000)];
        let timeline = timeline(
            vec![piece(start, 60, "a|b", "work-block:0")],
            vec![agent(start, 30, "s1")],
        );
        let rates = RateOverrides::default();
        let analysis = Analysis::from_input(&input(&timeline, &sessions, &[], &rates), &utc_zone());
        let window = WindowInfo::new((None, None), false);
        let document = insights_document(
            &window,
            &analysis,
            Sections::parse(&[]),
            30,
            &Diagnostics::default(),
            &["a <script> warning".to_string()],
            true,
        );
        let text = render_text(&document);
        assert!(text.starts_with("workstats insights"));
        assert!(text.contains("Longest block"));
        assert!(text.contains("█") || text.contains("░"));
        let markdown = render_markdown(&document);
        assert!(markdown.contains("a\\|b"));
        let html = render_html(&document);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn section_names_select_what_is_shown() {
        let only = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
            let sections = Sections::parse(&names);
            (
                sections.focus,
                sections.heatmap,
                sections.leverage,
                sections.models,
            )
        };
        assert_eq!((true, true, true, true), only(&[]));
        assert_eq!((true, false, false, true), only(&["focus", "models"]));
    }

    #[test]
    fn the_default_window_is_described_as_such() {
        let since = utc(1, 0, 0);
        let described = WindowInfo::new((Some(since), None), true);
        assert!(described.label.starts_with("the last 28 days"));
        assert!(described.defaulted);
        let explicit = WindowInfo::new((Some(since), None), false);
        assert!(explicit.label.starts_with("since "));
        assert_eq!("all history", WindowInfo::new((None, None), false).label);
    }
}
