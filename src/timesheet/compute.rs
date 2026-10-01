//! Turning the human timeline into suggested timesheet entries.
//!
//! The timeline is already a non-overlapping partition: every moment of human
//! time is one piece, labelled by the nearest signal, so hours per engagement
//! are sums of pieces and nothing is counted twice however many agents ran.
//! A split rule only changes who a work block's time is attributed to; the
//! block's total, and so the period's, never changes.
//!
//! All raw time is carried as integer microseconds until rounding, so the
//! three split rules can be compared exactly and the raw entries add up to the
//! report's human time.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Serialize;

use super::ledger;
use super::model::{
    Adjustment, CrossCheckRow, Detail, DroppedEntry, EntryStatus, Evidence, SplitRule, Timesheet,
    TimesheetEntry, TimesheetMethodology, TimesheetSettings, TimesheetWindow,
};
use super::round::{self, RawEntry, RoundParams};
use crate::aggregate::Timeline;
use crate::attribution::{self, Ctx};
use crate::cli::ReportWindow;
use crate::engagement::{Engagements, UNASSIGNED};
use crate::model::{HumanSignal, Interval};
use crate::timeutil::split_interval;

/// How far the raw entries may differ from the report's human time (it rounds
/// to a millisecond).
const RECONCILE_TOLERANCE_SECONDS: f64 = 0.001;

/// An engagement and, when `--detail` is given, what it is broken down by.
pub(crate) type Key = (String, Option<String>);

/// What `compute` needs. The engagements are passed in rather than read from
/// the process-wide registry so a test can compute against its own.
pub(crate) struct Input<'a> {
    pub(crate) timeline: &'a Timeline,
    pub(crate) engagements: &'a Engagements,
    pub(crate) settings: &'a TimesheetSettings,
    pub(crate) window: ReportWindow,
    /// `summary.human_estimated_seconds` of the same run.
    pub(crate) report_human_seconds: f64,
    /// Manual entries, overrides and locks to apply; `None` computes the bare
    /// estimate (the unit tests, which have no ledger).
    pub(crate) ledger: Option<&'a ledger::Context<'a>>,
}

/// Whether the entries add up to the report they came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Reconciliation {
    /// The sum of every raw entry, assigned or not.
    pub(crate) raw_seconds: f64,
    /// `summary.human_estimated_seconds`.
    pub(crate) report_seconds: f64,
    pub(crate) difference_seconds: f64,
    pub(crate) consistent: bool,
    /// The part of `raw_seconds` that matched no engagement.
    pub(crate) unassigned_seconds: f64,
}

pub(crate) struct Computation {
    pub(crate) timesheet: Timesheet,
    pub(crate) reconciliation: Reconciliation,
}

/// One day-sized piece of the human timeline.
struct Piece {
    block: String,
    date: NaiveDate,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    micros: u64,
    key: Key,
}

/// The part of one work block that falls on one local day. A block is split
/// by day first (a timesheet is per day) and shared out second.
struct BlockDay {
    block: String,
    date: NaiveDate,
    pieces: Vec<usize>,
    total: u64,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
}

#[derive(Default)]
struct Cell {
    micros: u64,
    blocks: BTreeSet<String>,
    first: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
}

impl Cell {
    fn add(&mut self, micros: u64, block: &str, start: DateTime<Utc>, end: DateTime<Utc>) {
        self.micros += micros;
        self.blocks.insert(block.to_string());
        self.first = Some(self.first.map_or(start, |first| first.min(start)));
        self.last = Some(self.last.map_or(end, |last| last.max(end)));
    }
}

struct Attributed {
    cells: BTreeMap<(NaiveDate, Key), Cell>,
    /// What each block-day came to after sharing, to check the invariant.
    block_day_totals: BTreeMap<(String, NaiveDate), u64>,
}

#[derive(Default)]
struct EvidenceBuilder {
    prompts: usize,
    commits: usize,
    sessions: HashSet<(String, String)>,
    repos: BTreeSet<String>,
    branches: BTreeSet<String>,
    issues: BTreeSet<String>,
}

pub(crate) fn compute(input: &Input<'_>) -> Result<Computation> {
    let settings = input.settings;
    let pieces = day_pieces(input)?;
    let block_days = block_days(&pieces);
    let locator = Locator::new(&pieces);
    let effective = effective_signals(&input.timeline.human_signals);
    let signal_weights = SignalWeights::new(input, &effective, &pieces, &locator);
    let agent = AgentIndex::new(input);

    // All three rules are always computed: the cross-check is the evidence
    // that the choice moves hours between engagements and nowhere else.
    let nearest = attribute(
        SplitRule::Nearest,
        &pieces,
        &block_days,
        &signal_weights,
        &agent,
    );
    let signals = attribute(
        SplitRule::Signals,
        &pieces,
        &block_days,
        &signal_weights,
        &agent,
    );
    let by_agent = attribute(
        SplitRule::Agent,
        &pieces,
        &block_days,
        &signal_weights,
        &agent,
    );

    let mut warnings = Vec::new();
    let planned: BTreeMap<(String, NaiveDate), u64> = block_days
        .iter()
        .map(|block_day| ((block_day.block.clone(), block_day.date), block_day.total))
        .collect();
    for (name, attributed) in [
        ("nearest", &nearest),
        ("signals", &signals),
        ("agent", &by_agent),
    ] {
        if attributed.block_day_totals != planned {
            warnings.push(format!(
                "internal check failed: the {name} split changed a work block's total; please report this"
            ));
        }
    }
    let cross_check = cross_check(&nearest, &signals, &by_agent);
    let chosen = match settings.split {
        SplitRule::Nearest => nearest,
        SplitRule::Signals => signals,
        SplitRule::Agent => by_agent,
    };

    let raw_micros: u64 = chosen.cells.values().map(|cell| cell.micros).sum();
    let unassigned_micros: u64 = chosen
        .cells
        .iter()
        .filter(|((_, key), _)| key.0 == UNASSIGNED)
        .map(|(_, cell)| cell.micros)
        .sum();
    let raw_seconds = raw_micros as f64 / 1e6;
    let difference = raw_seconds - input.report_human_seconds;
    let reconciliation = Reconciliation {
        raw_seconds,
        report_seconds: input.report_human_seconds,
        difference_seconds: difference,
        consistent: difference.abs() <= RECONCILE_TOLERANCE_SECONDS + 1e-9,
        unassigned_seconds: unassigned_micros as f64 / 1e6,
    };
    if !reconciliation.consistent {
        warnings.push(format!(
            "the entries add up to {raw_seconds:.3}s but the report's human time is {:.3}s; please report this",
            input.report_human_seconds
        ));
    }
    if input.engagements.is_empty() {
        warnings.push(
            "no engagements are configured, so all work is (unassigned); add an \"engagements\" block to the config (see docs/timesheet.md)"
                .to_string(),
        );
    }

    let evidence = gather_evidence(input, &effective, &chosen);
    let mut entries: Vec<TimesheetEntry> = chosen
        .cells
        .iter()
        .filter(|(_, cell)| cell.micros > 0)
        .map(|((date, key), cell)| {
            build_entry(
                input.engagements,
                *date,
                key,
                cell,
                evidence.get(&(*date, key.clone())),
            )
        })
        .collect();
    entries.sort_by(|left, right| entry_order(left).cmp(&entry_order(right)));

    let (entries, dropped, round_warnings) = round_entries(entries, settings);
    warnings.extend(round_warnings);

    let mut timesheet = Timesheet {
        window: TimesheetWindow {
            since: input.window.0,
            until: input.window.1,
        },
        settings: settings.clone(),
        entries,
        dropped,
        cross_check,
        warnings,
        methodology: TimesheetMethodology {
            status: "suggested",
            split_rule: split_rule_text(settings.split).to_string(),
            rounding: rounding_text(settings),
        },
        drift: Vec::new(),
        applied_locks: Vec::new(),
    };
    // The one point where manual entries, overrides and locks reach the
    // computed figures. Everything above is the estimate; everything below is
    // what is displayed and exported.
    if let Some(context) = input.ledger {
        ledger::apply(&mut timesheet, context).context("applying the timesheet ledger")?;
    }
    finalize(&mut timesheet.entries);
    Ok(Computation {
        timesheet,
        reconciliation,
    })
}

/// Final seconds and money, from whatever the entries now say. Run after the
/// ledger has had its say; locked entries keep the snapshot's figures.
pub(crate) fn finalize(entries: &mut [TimesheetEntry]) {
    for entry in entries {
        if entry.status == EntryStatus::Locked {
            continue;
        }
        entry.final_seconds =
            entry.override_seconds.unwrap_or(entry.estimated_seconds) + entry.manual_seconds;
        entry.amount = amount(entry);
    }
}

/// `round2(hours × rate)` for a billable entry with a rate; nothing otherwise.
pub(crate) fn amount(entry: &TimesheetEntry) -> Option<f64> {
    if !entry.billable {
        return None;
    }
    entry
        .rate
        .map(|rate| round_to_cents(entry.final_seconds as f64 / 3600.0 * rate))
}

pub(crate) fn round_to_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub(crate) fn split_rule_text(split: SplitRule) -> &'static str {
    match split {
        SplitRule::Nearest => {
            "each estimated moment is attributed once, to the engagement of the nearest prompt, commit or session edge; concurrent agents add no hours"
        }
        SplitRule::Signals => {
            "each work block is shared between engagements in proportion to the prompts, commits and session edges each had in it (a prompt or commit counts 1, a session edge 0.5); concurrent agents add no hours"
        }
        SplitRule::Agent => {
            "each work block is shared between engagements in proportion to the agent time each had inside it (a block with no agent time falls back to the nearest-signal rule); concurrent agents add no hours"
        }
    }
}

fn rounding_text(settings: &TimesheetSettings) -> String {
    let mut text = format!(
        "{} to {}",
        match settings.rounding {
            super::model::Rounding::Nearest => "nearest",
            super::model::Rounding::Up => "up",
            super::model::Rounding::Down => "down",
            super::model::Rounding::Balanced => "balanced",
        },
        span_text(settings.increment_seconds)
    );
    if settings.min_entry_seconds > 0 {
        text.push_str(&format!(
            ", minimum entry {}",
            span_text(settings.min_entry_seconds)
        ));
    }
    if settings.drop_below_seconds > 0 {
        text.push_str(&format!(
            ", dropping under {}",
            span_text(settings.drop_below_seconds)
        ));
    }
    if let Some(cap) = settings.daily_cap_seconds {
        text.push_str(&format!(", daily cap {}", span_text(cap)));
    }
    text
}

/// `15m`, `1h30m`, `45s`: the way a duration is typed.
pub(crate) fn span_text(seconds: u64) -> String {
    if seconds == 0 {
        return "0m".to_string();
    }
    let (hours, minutes, rest) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let mut text = String::new();
    if hours > 0 {
        text.push_str(&format!("{hours}h"));
    }
    if minutes > 0 {
        text.push_str(&format!("{minutes}m"));
    }
    if rest > 0 {
        text.push_str(&format!("{rest}s"));
    }
    text
}

pub(crate) fn entry_order(entry: &TimesheetEntry) -> (NaiveDate, bool, &str, &str) {
    (
        entry.date,
        entry.engagement == UNASSIGNED,
        entry.engagement.as_str(),
        entry.detail.as_deref().unwrap_or(""),
    )
}

fn key_for(input: &Input<'_>, repo_id: &str, cwd: &str, branch: Option<&str>, repo: &str) -> Key {
    entry_key(
        input.engagements,
        input.settings.detail,
        repo_id,
        cwd,
        branch,
        repo,
    )
}

/// The engagement (and `--detail` key) a signal's fields belong to. Public to
/// the crate so that anything that maps a commit or session back to a
/// timesheet entry, as descriptions do, uses the very rule the time used.
pub(crate) fn entry_key(
    engagements: &Engagements,
    detail: Option<Detail>,
    repo_id: &str,
    cwd: &str,
    branch: Option<&str>,
    repo: &str,
) -> Key {
    let engagement = engagements.label_for(&Ctx {
        repo_id,
        cwd,
        branch,
    });
    let detail = detail.map(|detail| match detail {
        Detail::Issue => attribution::issue_label(branch),
        Detail::Feature => attribution::feature_label(branch),
        Detail::Branch => attribution::branch_label(branch),
        Detail::Repo => repo.to_string(),
    });
    (engagement, detail)
}

fn micros_of(start: DateTime<Utc>, end: DateTime<Utc>) -> u64 {
    (end - start).num_microseconds().unwrap_or(0).max(0) as u64
}

fn local_date(timestamp: DateTime<Utc>) -> NaiveDate {
    timestamp.with_timezone(&Local).date_naive()
}

fn day_pieces(input: &Input<'_>) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    for interval in &input.timeline.human_intervals {
        for (day, piece) in split_interval(interval, "day") {
            let date = NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .with_context(|| format!("unreadable day {day:?} in the human timeline"))?;
            let micros = micros_of(piece.start, piece.end);
            if micros == 0 {
                continue;
            }
            let key = key_for(
                input,
                &piece.repo_id,
                &piece.cwd,
                piece.branch.as_deref(),
                &piece.repo,
            );
            pieces.push(Piece {
                block: piece.session_id,
                date,
                start: piece.start,
                end: piece.end,
                micros,
                key,
            });
        }
    }
    Ok(pieces)
}

fn block_days(pieces: &[Piece]) -> Vec<BlockDay> {
    let mut grouped: BTreeMap<(NaiveDate, String), BlockDay> = BTreeMap::new();
    for (index, piece) in pieces.iter().enumerate() {
        let entry = grouped
            .entry((piece.date, piece.block.clone()))
            .or_insert_with(|| BlockDay {
                block: piece.block.clone(),
                date: piece.date,
                pieces: Vec::new(),
                total: 0,
                first: piece.start,
                last: piece.end,
            });
        entry.pieces.push(index);
        entry.total += piece.micros;
        entry.first = entry.first.min(piece.start);
        entry.last = entry.last.max(piece.end);
    }
    grouped.into_values().collect()
}

/// Finds the piece a moment falls in, hence the work block it belongs to.
struct Locator {
    /// `(start, end, piece index)`, by start. Pieces never overlap.
    spans: Vec<(DateTime<Utc>, DateTime<Utc>, usize)>,
}

impl Locator {
    fn new(pieces: &[Piece]) -> Self {
        let mut spans: Vec<_> = pieces
            .iter()
            .enumerate()
            .map(|(index, piece)| (piece.start, piece.end, index))
            .collect();
        spans.sort();
        Self { spans }
    }

    /// The last signal of a block can sit exactly on the block's end (when no
    /// review credit is given), so the end is inclusive.
    fn find(&self, moment: DateTime<Utc>) -> Option<usize> {
        let after = self.spans.partition_point(|(start, _, _)| *start <= moment);
        let (_, end, index) = self.spans.get(after.checked_sub(1)?)?;
        (moment <= *end).then_some(*index)
    }
}

/// One signal per timestamp, the strongest kind winning (a prompt over a
/// commit over a session edge, the first listed on a tie): the same rule the
/// human-time calculation applies before it builds blocks.
fn effective_signals(signals: &[HumanSignal]) -> Vec<&HumanSignal> {
    let mut by_timestamp: BTreeMap<DateTime<Utc>, &HumanSignal> = BTreeMap::new();
    for signal in signals {
        by_timestamp
            .entry(signal.timestamp)
            .and_modify(|kept| {
                if priority(signal) > priority(kept) {
                    *kept = signal;
                }
            })
            .or_insert(signal);
    }
    by_timestamp.into_values().collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SignalKind {
    Prompt,
    Commit,
    Edge,
}

fn kind_of(signal: &HumanSignal) -> SignalKind {
    if signal.kind.ends_with("_prompt") {
        SignalKind::Prompt
    } else if signal.kind == "commit" {
        SignalKind::Commit
    } else {
        SignalKind::Edge
    }
}

fn priority(signal: &HumanSignal) -> u8 {
    match kind_of(signal) {
        SignalKind::Prompt => 3,
        SignalKind::Commit => 2,
        SignalKind::Edge => 1,
    }
}

/// Prompts and commits weigh 2, a session edge 1: the spec's 1 and 0.5, doubled
/// to stay in integers.
fn weight(signal: &HumanSignal) -> u64 {
    match kind_of(signal) {
        SignalKind::Prompt | SignalKind::Commit => 2,
        SignalKind::Edge => 1,
    }
}

struct SignalWeights {
    by_block_day: BTreeMap<(String, NaiveDate), BTreeMap<Key, u64>>,
    by_block: BTreeMap<String, BTreeMap<Key, u64>>,
}

impl SignalWeights {
    fn new(
        input: &Input<'_>,
        effective: &[&HumanSignal],
        pieces: &[Piece],
        locator: &Locator,
    ) -> Self {
        let mut by_block_day: BTreeMap<(String, NaiveDate), BTreeMap<Key, u64>> = BTreeMap::new();
        let mut by_block: BTreeMap<String, BTreeMap<Key, u64>> = BTreeMap::new();
        for signal in effective {
            let Some(index) = locator.find(signal.timestamp) else {
                continue;
            };
            let block = &pieces[index].block;
            let key = key_for(
                input,
                &signal.repo_id,
                &signal.cwd,
                signal.branch.as_deref(),
                &signal.repo,
            );
            // The day is the piece's, not the signal's own: they differ only
            // for a signal on the very end of a piece at midnight.
            let date = pieces[index].date;
            *by_block_day
                .entry((block.clone(), date))
                .or_default()
                .entry(key.clone())
                .or_default() += weight(signal);
            *by_block
                .entry(block.clone())
                .or_default()
                .entry(key)
                .or_default() += weight(signal);
        }
        Self {
            by_block_day,
            by_block,
        }
    }

    /// The signals on that day of the block; failing that, anywhere in it.
    fn for_block_day(&self, block: &str, date: NaiveDate) -> Option<Vec<(Key, u64)>> {
        let weights = self
            .by_block_day
            .get(&(block.to_string(), date))
            .or_else(|| self.by_block.get(block))?;
        Some(
            weights
                .iter()
                .map(|(key, weight)| (key.clone(), *weight))
                .collect(),
        )
    }
}

type Span = (DateTime<Utc>, DateTime<Utc>);

/// Agent intervals by start, with the running maximum of their ends, so the
/// ones overlapping a span are found without a scan of all of them.
struct AgentIndex {
    intervals: Vec<(DateTime<Utc>, DateTime<Utc>, Key)>,
    prefix_max_end: Vec<DateTime<Utc>>,
}

impl AgentIndex {
    fn new(input: &Input<'_>) -> Self {
        let mut intervals: Vec<_> = input
            .timeline
            .ai_intervals
            .iter()
            .filter(|interval: &&Interval| interval.end > interval.start)
            .map(|interval| {
                (
                    interval.start,
                    interval.end,
                    key_for(
                        input,
                        &interval.repo_id,
                        &interval.cwd,
                        interval.branch.as_deref(),
                        &interval.repo,
                    ),
                )
            })
            .collect();
        intervals.sort_by_key(|(start, end, _)| (*start, *end));
        let mut prefix_max_end = Vec::with_capacity(intervals.len());
        let mut running: Option<DateTime<Utc>> = None;
        for (_, end, _) in &intervals {
            let value = running.map_or(*end, |current| current.max(*end));
            running = Some(value);
            prefix_max_end.push(value);
        }
        Self {
            intervals,
            prefix_max_end,
        }
    }

    /// Each key's agent time inside `[from, to]`, overlaps within a key merged.
    fn weights(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<Vec<(Key, u64)>> {
        let mut upper = self.intervals.partition_point(|(start, _, _)| *start < to);
        let mut clipped: BTreeMap<&Key, Vec<Span>> = BTreeMap::new();
        while upper > 0 {
            upper -= 1;
            if self.prefix_max_end[upper] <= from {
                break;
            }
            let (start, end, key) = &self.intervals[upper];
            let (start, end) = ((*start).max(from), (*end).min(to));
            if end > start {
                clipped.entry(key).or_default().push((start, end));
            }
        }
        let weights: Vec<(Key, u64)> = clipped
            .into_iter()
            .map(|(key, mut spans)| {
                spans.sort();
                let mut total = 0;
                let mut current: Option<Span> = None;
                for (start, end) in spans {
                    match current {
                        Some((first, last)) if start <= last => {
                            current = Some((first, last.max(end)))
                        }
                        Some((first, last)) => {
                            total += micros_of(first, last);
                            current = Some((start, end));
                        }
                        None => current = Some((start, end)),
                    }
                }
                if let Some((first, last)) = current {
                    total += micros_of(first, last);
                }
                (key.clone(), total)
            })
            .filter(|(_, total)| *total > 0)
            .collect();
        (!weights.is_empty()).then_some(weights)
    }
}

/// Shares `total` between the weights in proportion, exactly: the shares are
/// integers that add up to `total`, the largest remainders getting the extra
/// microseconds (ties: the larger weight, then the earlier key).
pub(crate) fn apportion<K: Clone>(total: u64, weights: &[(K, u64)]) -> Vec<(K, u64)> {
    let sum: u128 = weights.iter().map(|(_, weight)| *weight as u128).sum();
    if sum == 0 {
        return Vec::new();
    }
    let mut shares: Vec<(u64, u128)> = weights
        .iter()
        .map(|(_, weight)| {
            let product = total as u128 * *weight as u128;
            ((product / sum) as u64, product % sum)
        })
        .collect();
    let assigned: u64 = shares.iter().map(|(share, _)| *share).sum();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&left, &right| {
        shares[right]
            .1
            .cmp(&shares[left].1)
            .then(weights[right].1.cmp(&weights[left].1))
            .then(left.cmp(&right))
    });
    for &index in order.iter().take((total - assigned) as usize) {
        shares[index].0 += 1;
    }
    weights
        .iter()
        .zip(shares)
        .map(|((key, _), (share, _))| (key.clone(), share))
        .collect()
}

fn attribute(
    split: SplitRule,
    pieces: &[Piece],
    block_days: &[BlockDay],
    signals: &SignalWeights,
    agent: &AgentIndex,
) -> Attributed {
    let mut cells: BTreeMap<(NaiveDate, Key), Cell> = BTreeMap::new();
    let mut block_day_totals = BTreeMap::new();
    for block_day in block_days {
        let weights = match split {
            SplitRule::Nearest => None,
            SplitRule::Signals => signals.for_block_day(&block_day.block, block_day.date),
            SplitRule::Agent => agent.weights(block_day.first, block_day.last),
        };
        let mut attributed = 0;
        match weights {
            Some(weights) => {
                for (key, micros) in apportion(block_day.total, &weights) {
                    if micros > 0 {
                        attributed += micros;
                        cells.entry((block_day.date, key)).or_default().add(
                            micros,
                            &block_day.block,
                            block_day.first,
                            block_day.last,
                        );
                    }
                }
            }
            None => {
                for &index in &block_day.pieces {
                    let piece = &pieces[index];
                    attributed += piece.micros;
                    cells
                        .entry((piece.date, piece.key.clone()))
                        .or_default()
                        .add(piece.micros, &piece.block, piece.start, piece.end);
                }
            }
        }
        block_day_totals.insert((block_day.block.clone(), block_day.date), attributed);
    }
    Attributed {
        cells,
        block_day_totals,
    }
}

fn cross_check(
    nearest: &Attributed,
    signals: &Attributed,
    agent: &Attributed,
) -> Vec<CrossCheckRow> {
    let mut rows: BTreeMap<String, [u64; 3]> = BTreeMap::new();
    for (index, attributed) in [nearest, signals, agent].into_iter().enumerate() {
        for ((_, key), cell) in &attributed.cells {
            rows.entry(key.0.clone()).or_default()[index] += cell.micros;
        }
    }
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_by_key(|(engagement, _)| (engagement == UNASSIGNED, engagement.clone()));
    rows.into_iter()
        .map(|(engagement, micros)| CrossCheckRow {
            engagement,
            nearest_seconds: micros[0] as f64 / 1e6,
            signals_seconds: micros[1] as f64 / 1e6,
            agent_seconds: micros[2] as f64 / 1e6,
        })
        .collect()
}

/// Evidence per entry, from the effective signals mapped the same way the
/// time was: a signal supports the entry of its own day and engagement. The
/// block count and the first and last moment come from the time itself.
fn gather_evidence(
    input: &Input<'_>,
    effective: &[&HumanSignal],
    chosen: &Attributed,
) -> BTreeMap<(NaiveDate, Key), Evidence> {
    let mut builders: BTreeMap<(NaiveDate, Key), EvidenceBuilder> = chosen
        .cells
        .iter()
        .filter(|(_, cell)| cell.micros > 0)
        .map(|(cell_key, _)| (cell_key.clone(), EvidenceBuilder::default()))
        .collect();
    for signal in effective {
        let key = key_for(
            input,
            &signal.repo_id,
            &signal.cwd,
            signal.branch.as_deref(),
            &signal.repo,
        );
        let Some(builder) = builders.get_mut(&(local_date(signal.timestamp), key)) else {
            continue;
        };
        match kind_of(signal) {
            SignalKind::Prompt => builder.prompts += 1,
            SignalKind::Commit => builder.commits += 1,
            SignalKind::Edge => {}
        }
        if kind_of(signal) != SignalKind::Commit {
            builder
                .sessions
                .insert((signal.provider.clone(), signal.session_id.clone()));
        }
        builder.repos.insert(signal.repo.clone());
        if let Some(branch) = &signal.branch {
            builder.branches.insert(branch.clone());
        }
        let issue = attribution::issue_label(signal.branch.as_deref());
        if issue != attribution::UNKNOWN {
            builder.issues.insert(issue);
        }
    }
    builders
        .into_iter()
        .map(|(cell_key, builder)| {
            let blocks = chosen.cells[&cell_key].blocks.len();
            (
                cell_key,
                Evidence {
                    prompts: builder.prompts,
                    commits: builder.commits,
                    sessions: builder.sessions.len(),
                    blocks,
                    repos: builder.repos.into_iter().collect(),
                    branches: builder.branches.into_iter().collect(),
                    issues: builder.issues.into_iter().collect(),
                },
            )
        })
        .collect()
}

fn build_entry(
    engagements: &Engagements,
    date: NaiveDate,
    key: &Key,
    cell: &Cell,
    evidence: Option<&Evidence>,
) -> TimesheetEntry {
    let configured = engagements.get(&key.0);
    let billable = configured.is_some_and(|engagement| engagement.billable);
    TimesheetEntry {
        date,
        engagement: key.0.clone(),
        detail: key.1.clone(),
        label: configured.map_or_else(|| key.0.clone(), |engagement| engagement.label.clone()),
        client: configured.and_then(|engagement| engagement.client.clone()),
        billable,
        raw_seconds: cell.micros as f64 / 1e6,
        estimated_seconds: 0,
        manual_seconds: 0,
        override_seconds: None,
        final_seconds: 0,
        first_start: cell.first,
        last_end: cell.last,
        evidence: evidence.cloned().unwrap_or_default(),
        rate: configured
            .filter(|_| billable)
            .and_then(|engagement| engagement.rate),
        currency: configured
            .filter(|_| billable)
            .and_then(|engagement| engagement.currency.clone()),
        amount: None,
        notes: Vec::new(),
        description: None,
        status: EntryStatus::Suggested,
        adjustments: Vec::new(),
        lock_drift_seconds: None,
    }
}

/// Rounds each day, applies the minimum, the drop threshold and the cap, and
/// hands back what is kept, what was removed (so nothing disappears
/// silently) and any warnings.
fn round_entries(
    entries: Vec<TimesheetEntry>,
    settings: &TimesheetSettings,
) -> (Vec<TimesheetEntry>, Vec<DroppedEntry>, Vec<String>) {
    let params = RoundParams {
        increment_seconds: settings.increment_seconds,
        rounding: settings.rounding,
        min_entry_seconds: settings.min_entry_seconds,
        drop_below_seconds: settings.drop_below_seconds,
    };
    let mut kept: Vec<TimesheetEntry> = Vec::new();
    let mut dropped = Vec::new();
    let mut warnings = Vec::new();
    let mut days: BTreeMap<NaiveDate, Vec<TimesheetEntry>> = BTreeMap::new();
    for entry in entries {
        days.entry(entry.date).or_default().push(entry);
    }
    for (_, day) in days {
        let raw: Vec<RawEntry> = day
            .iter()
            .map(|entry| RawEntry {
                raw_micros: (entry.raw_seconds * 1e6).round() as u64,
                key: format!(
                    "{}\u{0}{}",
                    entry.engagement,
                    entry.detail.as_deref().unwrap_or("")
                ),
            })
            .collect();
        let outcome = round::round_day(&raw, &params);
        let mut day_kept: Vec<TimesheetEntry> = Vec::new();
        for (mut entry, result) in day.into_iter().zip(outcome) {
            if result.dropped {
                dropped.push(dropped_entry(&entry));
                continue;
            }
            entry.estimated_seconds = result.units * settings.increment_seconds;
            if result.raised {
                entry.adjustments.push(Adjustment::RaisedToMinimum);
            }
            if result.balanced {
                entry.adjustments.push(Adjustment::Balanced);
            }
            day_kept.push(entry);
        }
        if let Some(cap) = settings.daily_cap_seconds
            && let Some(warning) =
                round::enforce_daily_cap(&mut day_kept, settings.increment_seconds, cap)
        {
            warnings.push(warning);
        }
        // An estimate the cap took down to nothing is removed like any other.
        let (alive, gone): (Vec<_>, Vec<_>) = day_kept
            .into_iter()
            .partition(|entry| entry.estimated_seconds > 0);
        dropped.extend(gone.iter().map(dropped_entry));
        kept.extend(alive);
    }
    (kept, dropped, warnings)
}

fn dropped_entry(entry: &TimesheetEntry) -> DroppedEntry {
    DroppedEntry {
        date: entry.date,
        engagement: entry.engagement.clone(),
        detail: entry.detail.clone(),
        raw_seconds: entry.raw_seconds,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::timesheet::model::{Rounding, UnassignedMode};

    /// A local moment, so the day it falls on does not depend on the timezone
    /// the suite runs in.
    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn engagements(config: serde_json::Value) -> Engagements {
        Engagements::compile(
            Some(&config),
            &std::collections::BTreeMap::new(),
            Path::new("/"),
        )
        .unwrap()
    }

    /// Two clients told apart by branch, so a test places time with a branch.
    fn two_clients() -> Engagements {
        engagements(json!({
            "acme": {"branches": ["acme/*"], "rate": 1000, "currency": "NOK", "label": "ACME"},
            "beta": {"branches": ["beta/*"], "billable": false},
        }))
    }

    fn signal(timestamp: DateTime<Utc>, kind: &str, branch: &str, session: &str) -> HumanSignal {
        HumanSignal {
            timestamp,
            provider: if kind == "commit" { "git" } else { "claude" }.to_string(),
            session_id: session.to_string(),
            cwd: "/work/repo".to_string(),
            repo: "repo".to_string(),
            repo_id: "local:repo".to_string(),
            root: "/work".to_string(),
            kind: kind.to_string(),
            model: "m".to_string(),
            branch: Some(branch.to_string()),
        }
    }

    fn piece(start: DateTime<Utc>, end: DateTime<Utc>, block: usize, branch: &str) -> Interval {
        Interval {
            start,
            end,
            provider: "claude".to_string(),
            model: "m".to_string(),
            session_id: format!("work-block:{block}"),
            cwd: "/work/repo".to_string(),
            repo: "repo".to_string(),
            repo_id: "local:repo".to_string(),
            root: "/work".to_string(),
            branch: Some(branch.to_string()),
        }
    }

    fn agent(start: DateTime<Utc>, end: DateTime<Utc>, branch: &str, session: &str) -> Interval {
        Interval {
            session_id: session.to_string(),
            ..piece(start, end, 0, branch)
        }
    }

    fn settings(split: SplitRule, rounding: Rounding) -> TimesheetSettings {
        TimesheetSettings {
            rounding,
            split,
            unassigned: UnassignedMode::Show,
            ..TimesheetSettings::default()
        }
    }

    fn run(
        timeline: &Timeline,
        engagements: &Engagements,
        settings: &TimesheetSettings,
    ) -> Computation {
        let seconds: f64 = timeline.human_intervals.iter().map(Interval::seconds).sum();
        compute(&Input {
            timeline,
            engagements,
            settings,
            window: (None, None),
            report_human_seconds: seconds,
            ledger: None,
        })
        .unwrap()
    }

    /// One work block on day 12 of 09:00-10:30, midpoint-cut between a prompt
    /// for acme at 09:30 and one for beta at 10:00, with agents on both.
    fn interleaved() -> Timeline {
        let (a, b) = ("acme/x", "beta/y");
        Timeline {
            human_intervals: vec![
                piece(at(12, 9, 0), at(12, 9, 45), 0, a),
                piece(at(12, 9, 45), at(12, 10, 30), 0, b),
            ],
            human_signals: vec![
                signal(at(12, 9, 30), "claude_prompt", a, "s1"),
                signal(at(12, 9, 40), "claude_prompt", a, "s1"),
                signal(at(12, 10, 0), "claude_prompt", b, "s2"),
            ],
            ai_intervals: vec![
                agent(at(12, 9, 10), at(12, 9, 20), a, "s1"),
                agent(at(12, 9, 10), at(12, 9, 20), a, "s3"),
                agent(at(12, 9, 50), at(12, 10, 20), b, "s2"),
            ],
        }
    }

    fn raw_by_engagement(computation: &Computation) -> BTreeMap<String, f64> {
        let mut totals = BTreeMap::new();
        for row in &computation.timesheet.cross_check {
            totals.insert(row.engagement.clone(), row.nearest_seconds);
        }
        totals
    }

    #[test]
    fn the_nearest_rule_keeps_the_timeline_pieces() {
        let timeline = interleaved();
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Nearest, Rounding::Nearest),
        );
        let totals = raw_by_engagement(&computation);
        assert_eq!(2700.0, totals["acme"]);
        assert_eq!(2700.0, totals["beta"]);
        assert!(computation.reconciliation.consistent);
        assert_eq!(5400.0, computation.reconciliation.raw_seconds);
        assert_eq!(0.0, computation.reconciliation.unassigned_seconds);
    }

    #[test]
    fn every_split_rule_keeps_every_block_total_and_only_moves_time_between_engagements() {
        let timeline = interleaved();
        for split in [SplitRule::Nearest, SplitRule::Signals, SplitRule::Agent] {
            let computation = run(
                &timeline,
                &two_clients(),
                &settings(split, Rounding::Nearest),
            );
            assert!(
                computation.timesheet.warnings.is_empty(),
                "{split:?}: {:?}",
                computation.timesheet.warnings
            );
            let sums: Vec<f64> = computation
                .timesheet
                .cross_check
                .iter()
                .fold([0.0; 3], |mut totals, row| {
                    totals[0] += row.nearest_seconds;
                    totals[1] += row.signals_seconds;
                    totals[2] += row.agent_seconds;
                    totals
                })
                .to_vec();
            assert_eq!(vec![5400.0; 3], sums, "{split:?}");
        }
    }

    #[test]
    fn the_signals_rule_shares_a_block_by_prompts() {
        let timeline = interleaved();
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Signals, Rounding::Nearest),
        );
        let row = |engagement: &str| {
            computation
                .timesheet
                .cross_check
                .iter()
                .find(|row| row.engagement == engagement)
                .unwrap()
                .clone()
        };
        // Two prompts for acme, one for beta: 2/3 and 1/3 of 90 minutes.
        assert_eq!(3600.0, row("acme").signals_seconds);
        assert_eq!(1800.0, row("beta").signals_seconds);
        // The nearest column is untouched by the choice.
        assert_eq!(2700.0, row("acme").nearest_seconds);
    }

    #[test]
    fn the_agent_rule_shares_a_block_by_agent_time_with_overlaps_counted_once() {
        let timeline = interleaved();
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Agent, Rounding::Nearest),
        );
        let agent_of = |engagement: &str| {
            computation
                .timesheet
                .cross_check
                .iter()
                .find(|row| row.engagement == engagement)
                .unwrap()
                .agent_seconds
        };
        // acme's two agents ran the same 10 minutes: 10m, not 20m. beta: 30m.
        assert_eq!(1350.0, agent_of("acme"));
        assert_eq!(4050.0, agent_of("beta"));
    }

    #[test]
    fn a_block_without_agent_time_falls_back_to_the_nearest_rule() {
        let mut timeline = interleaved();
        timeline.ai_intervals.clear();
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Agent, Rounding::Nearest),
        );
        for row in &computation.timesheet.cross_check {
            assert_eq!(row.nearest_seconds, row.agent_seconds);
        }
    }

    #[test]
    fn interleaved_sessions_for_two_engagements_are_not_double_counted() {
        // Two sessions alternate prompts for different clients inside one
        // block; the pieces partition the block, so the entries add up to it.
        let (a, b) = ("acme/x", "beta/y");
        let mut human = Vec::new();
        let mut signals = Vec::new();
        for step in 0..6u32 {
            let branch = if step % 2 == 0 { a } else { b };
            let start = at(12, 9, 0) + Duration::minutes(i64::from(step) * 10);
            human.push(piece(start, start + Duration::minutes(10), 0, branch));
            signals.push(signal(
                start + Duration::minutes(5),
                "claude_prompt",
                branch,
                if step % 2 == 0 { "s1" } else { "s2" },
            ));
        }
        let timeline = Timeline {
            human_intervals: human,
            human_signals: signals,
            ai_intervals: Vec::new(),
        };
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Nearest, Rounding::Nearest),
        );
        assert_eq!(3600.0, computation.reconciliation.raw_seconds);
        let total: f64 = computation
            .timesheet
            .entries
            .iter()
            .map(|entry| entry.raw_seconds)
            .sum();
        assert_eq!(3600.0, total);
    }

    #[test]
    fn a_work_block_across_midnight_is_split_by_day() {
        let timeline = Timeline {
            human_intervals: vec![piece(at(12, 23, 30), at(13, 0, 30), 0, "acme/x")],
            human_signals: vec![
                signal(at(12, 23, 45), "claude_prompt", "acme/x", "s1"),
                signal(at(13, 0, 15), "claude_prompt", "acme/x", "s1"),
            ],
            ai_intervals: Vec::new(),
        };
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Signals, Rounding::Nearest),
        );
        let days: Vec<(String, f64)> = computation
            .timesheet
            .entries
            .iter()
            .map(|entry| (entry.date.to_string(), entry.raw_seconds))
            .collect();
        assert_eq!(
            vec![
                ("2026-08-12".to_string(), 1800.0),
                ("2026-08-13".to_string(), 1800.0)
            ],
            days
        );
        assert_eq!(2, computation.timesheet.entries.len());
    }

    #[test]
    fn unassigned_work_is_kept_and_reconciles() {
        let timeline = interleaved();
        let computation = run(
            &timeline,
            &engagements(json!({"acme": {"branches": ["acme/*"]}})),
            &settings(SplitRule::Nearest, Rounding::Nearest),
        );
        let unassigned: Vec<_> = computation
            .timesheet
            .entries
            .iter()
            .filter(|entry| entry.engagement == UNASSIGNED)
            .collect();
        assert_eq!(1, unassigned.len());
        assert!(!unassigned[0].billable);
        assert_eq!(2700.0, computation.reconciliation.unassigned_seconds);
        assert!(computation.reconciliation.consistent);
        // Unassigned sorts after the real engagements on its day.
        assert_eq!(
            UNASSIGNED,
            computation.timesheet.entries.last().unwrap().engagement
        );
    }

    #[test]
    fn a_mismatch_with_the_report_is_a_warning() {
        let timeline = interleaved();
        let computation = compute(&Input {
            timeline: &timeline,
            engagements: &two_clients(),
            settings: &settings(SplitRule::Nearest, Rounding::Nearest),
            window: (None, None),
            report_human_seconds: 5000.0,
            ledger: None,
        })
        .unwrap();
        assert!(!computation.reconciliation.consistent);
        assert!(
            computation
                .timesheet
                .warnings
                .iter()
                .any(|w| w.contains("report's human time"))
        );
    }

    #[test]
    fn evidence_is_counted_from_the_signals_of_each_entry() {
        let mut timeline = interleaved();
        timeline
            .human_signals
            .push(signal(at(12, 9, 50), "commit", "beta/y", "repo:abc"));
        timeline.human_signals.push(signal(
            at(12, 10, 25),
            "claude_session_edge",
            "beta/y",
            "s2",
        ));
        // Same timestamp as a prompt: the prompt wins, the edge is dropped.
        timeline
            .human_signals
            .push(signal(at(12, 9, 30), "claude_session_edge", "acme/x", "s9"));
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Nearest, Rounding::Nearest),
        );
        let find = |engagement: &str| {
            computation
                .timesheet
                .entries
                .iter()
                .find(|entry| entry.engagement == engagement)
                .unwrap()
        };
        let acme = &find("acme").evidence;
        assert_eq!(
            (2, 0, 1, 1),
            (acme.prompts, acme.commits, acme.sessions, acme.blocks)
        );
        assert_eq!(vec!["acme/x"], acme.branches);
        let beta = &find("beta").evidence;
        // One prompt, one commit, and sessions s2 only (the commit is no session).
        assert_eq!((1, 1, 1), (beta.prompts, beta.commits, beta.sessions));
        assert_eq!(vec!["repo"], beta.repos);
        assert!(find("acme").first_start.unwrap() < find("beta").first_start.unwrap());
    }

    #[test]
    fn detail_breaks_an_engagement_down_without_changing_its_total() {
        let timeline = interleaved();
        let mut with_detail = settings(SplitRule::Nearest, Rounding::Nearest);
        with_detail.detail = Some(Detail::Branch);
        let detailed = run(&timeline, &two_clients(), &with_detail);
        assert_eq!(
            Some("acme/x"),
            detailed.timesheet.entries[0].detail.as_deref()
        );
        assert_eq!(5400.0, detailed.reconciliation.raw_seconds);
    }

    #[test]
    fn money_is_per_entry_from_the_final_hours() {
        let timeline = interleaved();
        let computation = run(
            &timeline,
            &two_clients(),
            &settings(SplitRule::Nearest, Rounding::Nearest),
        );
        let acme = computation
            .timesheet
            .entries
            .iter()
            .find(|entry| entry.engagement == "acme")
            .unwrap();
        // 45 minutes at 1000/h.
        assert_eq!(2700, acme.final_seconds);
        assert_eq!(Some(750.0), acme.amount);
        assert_eq!(Some("NOK"), acme.currency.as_deref());
        let beta = computation
            .timesheet
            .entries
            .iter()
            .find(|entry| entry.engagement == "beta")
            .unwrap();
        assert_eq!(None, beta.amount, "not billable");
    }

    #[test]
    fn apportioning_is_exact_and_deterministic() {
        let shares = apportion(10, &[("a", 1), ("b", 1), ("c", 1)]);
        assert_eq!(10, shares.iter().map(|(_, share)| share).sum::<u64>());
        assert_eq!(
            vec![4, 3, 3],
            shares.iter().map(|(_, share)| *share).collect::<Vec<_>>()
        );
        let mut state = 5u64;
        for _ in 0..500 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let total = (state >> 20) % 1_000_000_007;
            let weights: Vec<(usize, u64)> = (0..1 + (state % 5) as usize)
                .map(|index| (index, 1 + (state >> (index * 7)) % 9))
                .collect();
            let shares = apportion(total, &weights);
            assert_eq!(total, shares.iter().map(|(_, share)| share).sum::<u64>());
        }
        assert!(apportion(10, &[("a", 0)]).is_empty());
    }

    #[test]
    fn span_text_reads_the_way_a_duration_is_typed() {
        assert_eq!("15m", span_text(900));
        assert_eq!("1h30m", span_text(5400));
        assert_eq!("2h", span_text(7200));
        assert_eq!("0m", span_text(0));
    }

    #[test]
    fn the_cap_and_minimum_apply_per_day_and_report_what_they_removed() {
        let timeline = interleaved();
        let mut capped = settings(SplitRule::Nearest, Rounding::Nearest);
        capped.daily_cap_seconds = Some(3600);
        let computation = run(&timeline, &two_clients(), &capped);
        let total: u64 = computation
            .timesheet
            .entries
            .iter()
            .map(|entry| entry.final_seconds)
            .sum();
        assert_eq!(3600, total);
        assert!(
            computation
                .timesheet
                .entries
                .iter()
                .any(|entry| entry.adjustments.contains(&Adjustment::Capped))
        );

        let mut strict = settings(SplitRule::Nearest, Rounding::Nearest);
        strict.drop_below_seconds = 3000;
        let computation = run(&timeline, &two_clients(), &strict);
        assert!(computation.timesheet.entries.is_empty());
        assert_eq!(2, computation.timesheet.dropped.len());
    }
}
