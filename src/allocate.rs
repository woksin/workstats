//! Apportions flat-rate subscription spend to one project.
//!
//! The question this answers: you pay for N plans per vendor, you worked on
//! several projects, how much of that spend belongs to one of them?
//!
//! It is a post-processing pass over a report already grouped by
//! `repo,provider,model,month`. Nothing here re-reads transcripts or changes
//! how anything is measured.
//!
//! Three commitments shape the design:
//!
//! - **Apportion on measured quantities.** Tokens and wall clock are recorded
//!   by the providers. Human time is an estimate built from prompt counts and
//!   session edges, and it drifts with orchestration style — a run that fans
//!   out subagents books more estimated attention per real hour than a single
//!   long session does. It is offered as a cross-check, never as the default.
//! - **Weigh models by list rate.** A million Opus tokens and a million Haiku
//!   tokens are not equal claims on a plan.
//! - **Never invent coverage.** Pruned history is a hole, and a hole that
//!   reads as a zero silently under-bills. Gaps are named and their handling
//!   is chosen explicitly.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::NaiveDate;
use serde::Serialize;

use crate::model::ReportRow;
use crate::output::number;
use crate::pricing::{self, RATES_AS_OF, RateOverrides, RateSource};

/// Which measured quantity drives the split.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Tokens the models generated. The closest measured proxy for work done,
    /// and unlike total tokens it is not swamped by cache reads.
    Output,
    /// List-price value of every token class, weighted by model.
    Value,
    /// Wall-clock time with at least one agent active.
    Wall,
    /// Every token class summed. Dominated by cache reads, which are an
    /// artifact of context length and turn count rather than of work.
    Tokens,
    /// Estimated human involvement. Not provider-recorded; see the module note.
    Human,
}

impl Basis {
    pub fn label(self) -> &'static str {
        match self {
            Basis::Output => "output tokens",
            Basis::Value => "list-price value",
            Basis::Wall => "agent wall clock",
            Basis::Tokens => "total tokens",
            Basis::Human => "human time (est)",
        }
    }

    /// Whether the number is recorded by the provider rather than inferred.
    pub fn is_measured(self) -> bool {
        !matches!(self, Basis::Human)
    }

    pub const ALL: [Basis; 5] = [
        Basis::Output,
        Basis::Value,
        Basis::Wall,
        Basis::Tokens,
        Basis::Human,
    ];
}

/// What to do with a family-month that has no surviving history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapPolicy {
    /// Drop it from the claim *and* from documented spend. The share stays
    /// honest and the report says what it could not see.
    Skip,
    /// Claim nothing for it but still count the spend. Under-bills, and is
    /// offered only because it is the most conservative number available.
    Zero,
    /// Apply that family's mean share across the months it can see.
    Impute,
}

/// One vendor's plans: how many, and what one costs per month before tax.
///
/// Prices are per pool because a Norwegian buyer pays Anthropic in converted
/// dollars and OpenAI in a fixed krone price the vendor sets. A single global
/// price cannot express that, and doing the arithmetic by hand is exactly what
/// this command exists to avoid.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Plan {
    pub count: u32,
    /// Advertised, before tax, in the run's currency.
    pub price: f64,
}

impl Plan {
    /// What these plans cost for one month, tax included.
    fn spend(&self, vat_percent: f64) -> f64 {
        f64::from(self.count) * self.price * (1.0 + vat_percent / 100.0)
    }

    fn price_with_tax(&self, vat_percent: f64) -> f64 {
        self.price * (1.0 + vat_percent / 100.0)
    }
}

#[derive(Clone, Debug)]
pub struct AllocationOptions {
    /// Empty means no claim is being made: report how the whole spend divides
    /// across every project instead of what one of them is owed.
    pub projects: Vec<String>,
    /// Rows to show in that breakdown; 0 means all.
    pub top: usize,
    pub subscriptions: BTreeMap<String, Plan>,
    /// Consumption tax added at checkout, as a percentage. Vendors advertise
    /// ex-tax prices, so what left the account is usually more than `price`.
    pub vat_percent: f64,
    /// ISO code for `price`. Labelling only — no rate is applied and nothing is
    /// converted, because workstats makes no network calls and a hardcoded
    /// exchange rate would go stale without saying so.
    pub currency: String,
    pub basis: Basis,
    pub gap_policy: GapPolicy,
    /// User rates from `model_rates` in the config file. They outrank the
    /// built-in table, and the output says when any of them was applied.
    pub rate_overrides: RateOverrides,
    /// The current date, for judging how stale the built-in table is. Injected
    /// rather than read here so the threshold is testable and `build` stays a
    /// pure function of its inputs.
    pub today: NaiveDate,
}

/// Both sides of one split, in every measure at once, so the chosen basis and
/// the cross-check come from a single pass.
#[derive(Clone, Copy, Debug, Default)]
struct Measures {
    output: f64,
    value: f64,
    wall: f64,
    tokens: f64,
    human: f64,
}

impl Measures {
    fn get(&self, basis: Basis) -> f64 {
        match basis {
            Basis::Output => self.output,
            Basis::Value => self.value,
            Basis::Wall => self.wall,
            Basis::Tokens => self.tokens,
            Basis::Human => self.human,
        }
    }

    fn add(&mut self, other: &Measures) {
        self.output += other.output;
        self.value += other.value;
        self.wall += other.wall;
        self.tokens += other.tokens;
        self.human += other.human;
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Split {
    project: Measures,
    pool: Measures,
}

impl Split {
    fn add(&mut self, measures: &Measures, is_project: bool) {
        self.pool.add(measures);
        if is_project {
            self.project.add(measures);
        }
    }

    fn share(&self, basis: Basis) -> Option<f64> {
        let pool = self.pool.get(basis);
        (pool > 0.0).then(|| self.project.get(basis) / pool)
    }
}

/// One month of one vendor's subscriptions.
#[derive(Debug, Serialize)]
pub struct PeriodRow {
    pub month: String,
    pub family: String,
    pub subscriptions: u32,
    /// What one of these plans cost that month, tax included.
    pub plan_price: f64,
    pub project_metric: f64,
    pub pool_metric: f64,
    pub share: f64,
    pub amount: f64,
    /// Empty when the cell was measured; otherwise why it was not.
    pub note: &'static str,
}

/// Per-model usage, which is what makes a share auditable rather than asserted.
#[derive(Debug, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub family: String,
    pub project_tokens: u64,
    pub pool_tokens: u64,
    pub project_output_tokens: u64,
    pub share: f64,
    pub project_list_value: f64,
    pub pool_list_value: f64,
    pub priced: bool,
    /// `override`, `built-in`, or `none` when the model is unpriced.
    pub rate_source: &'static str,
}

/// One project's slice of the whole spend, for the no-claim breakdown.
#[derive(Debug, Serialize)]
pub struct ProjectRow {
    pub project: String,
    /// Amount drawn from each pool, keyed by pool name.
    pub amounts: BTreeMap<String, f64>,
    pub total: f64,
    pub share_of_billed: f64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct CrossCheckRow {
    pub basis: Basis,
    pub label: &'static str,
    pub measured: bool,
    pub share: f64,
    pub amount: f64,
}

#[derive(Debug, Serialize)]
pub struct Allocation {
    pub projects: Vec<String>,
    pub months: Vec<String>,
    pub basis: Basis,
    pub gap_policy: GapPolicy,
    pub vat_percent: f64,
    pub currency: String,
    pub subscriptions: BTreeMap<String, Plan>,
    /// Everything the plans cost over the window.
    pub billed: f64,
    /// The part of that spend a measured month stands behind.
    pub documented: f64,
    pub attributable: f64,
    /// `attributable / documented` — the share of *documented* spend, which is
    /// not the same as the share of everything billed when gaps were skipped.
    pub effective_share: f64,
    #[serde(skip)]
    pub top: usize,
    pub periods: Vec<PeriodRow>,
    /// Every project's slice of the spend. Present only when no project was
    /// named, because that is the question being asked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<ProjectRow>,
    pub models: Vec<ModelRow>,
    pub cross_check: Vec<CrossCheckRow>,
    pub rates_as_of: &'static str,
    /// The `model_rates` keys that priced or pooled a model in this run. Empty
    /// means every number came from the built-in table.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rate_overrides: Vec<String>,
    pub warnings: Vec<String>,
}

fn row_value(row: &ReportRow, model: &str, overrides: &RateOverrides) -> Measures {
    let value = overrides
        .resolve(model)
        .map(|resolved| {
            resolved.rate.value(
                row.input_tokens,
                row.cache_creation_tokens,
                row.cache_read_tokens,
                row.output_tokens,
            )
        })
        .unwrap_or(0.0);
    Measures {
        output: row.output_tokens as f64,
        value,
        wall: row.parallel_agent_seconds,
        tokens: row.total_tokens as f64,
        human: row.human_estimated_seconds,
    }
}

/// Takes rows rather than a whole `Report`: allocation is a pure function of
/// the grouped rows, and depending on nothing else keeps it directly testable.
/// Rows are expected to be grouped by `repo,provider,model,month`.
pub fn build(rows: &[ReportRow], options: &AllocationOptions) -> Allocation {
    let wanted: BTreeSet<String> = options
        .projects
        .iter()
        .map(|project| project.trim().to_ascii_lowercase())
        .collect();

    let mut cells: BTreeMap<(String, String), Split> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), Split> = BTreeMap::new();
    let mut model_tokens: BTreeMap<(String, String), (u64, u64, u64)> = BTreeMap::new();
    let mut months: BTreeSet<String> = BTreeSet::new();
    let mut unclassified: BTreeMap<String, u64> = BTreeMap::new();
    let mut unpriced: BTreeMap<String, u64> = BTreeMap::new();
    let mut seen_repos: BTreeSet<String> = BTreeSet::new();
    let mut by_repo: BTreeMap<String, BTreeMap<String, Measures>> = BTreeMap::new();
    let mut repo_tokens: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut overrides_used: BTreeSet<String> = BTreeSet::new();
    // Staleness only matters for numbers that came from the built-in table; a
    // run priced entirely by the user's own rates has nothing out of date.
    let mut built_in_used = false;

    for row in rows {
        let model = row.key.get("model").map(String::as_str).unwrap_or("");
        if model.is_empty() || model == "unknown" || model == "<synthetic>" {
            continue;
        }
        let provider = row.key.get("provider").map(String::as_str).unwrap_or("");
        let Some(pool) = options.rate_overrides.pool_for(provider, model) else {
            *unclassified.entry(model.to_string()).or_default() += row.total_tokens;
            continue;
        };
        // A pool nobody declared a plan for is not part of any split. Copilot
        // usage does not dilute the Codex pool it was never billed to.
        if !options.subscriptions.contains_key(&pool) {
            continue;
        }
        match options.rate_overrides.resolve(model) {
            None => *unpriced.entry(model.to_string()).or_default() += row.total_tokens,
            Some(resolved) => match resolved.source {
                RateSource::BuiltIn => built_in_used = true,
                RateSource::Override => {
                    overrides_used.extend(resolved.pattern.map(str::to_string));
                }
            },
        }

        // Rows are grouped with `month`, so a missing key means the report was
        // built for a single unnamed window; fold those into one bucket rather
        // than dropping them.
        let month = row
            .key
            .get("month")
            .cloned()
            .unwrap_or_else(|| "(window)".to_string());
        months.insert(month.clone());

        let repo = row
            .key
            .get("repo")
            .map(|repo| repo.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if !repo.is_empty() {
            seen_repos.insert(repo.clone());
        }
        let is_project = wanted.contains(&repo);
        let measures = row_value(row, model, &options.rate_overrides);

        let label = row
            .key
            .get("repo")
            .cloned()
            .unwrap_or_else(|| "(unattributed)".to_string());
        by_repo
            .entry(label.clone())
            .or_default()
            .entry(pool.clone())
            .or_default()
            .add(&measures);
        let counted = repo_tokens.entry(label).or_default();
        counted.0 += row.output_tokens;
        counted.1 += row.total_tokens;

        cells
            .entry((month, pool.clone()))
            .or_default()
            .add(&measures, is_project);
        models
            .entry((pool.clone(), model.to_string()))
            .or_default()
            .add(&measures, is_project);
        let entry = model_tokens.entry((pool, model.to_string())).or_default();
        entry.1 += row.total_tokens;
        if is_project {
            entry.0 += row.total_tokens;
            entry.2 += row.output_tokens;
        }
    }

    let months: Vec<String> = months.into_iter().collect();
    let chosen = apportion(&cells, &months, options, options.basis);

    // Every cross-check runs the *same* apportionment, only the basis changes.
    // Pooling all vendors together instead would ignore that the families hold
    // different numbers of plans, and the row marked as the chosen basis would
    // then disagree with the headline it is supposed to corroborate.
    let cross_check = Basis::ALL
        .iter()
        .map(|basis| {
            let outcome = apportion(&cells, &months, options, *basis);
            CrossCheckRow {
                basis: *basis,
                label: basis.label(),
                measured: basis.is_measured(),
                share: outcome.share(),
                amount: outcome.attributable,
            }
        })
        .collect();

    let Apportionment {
        periods,
        attributable,
        documented,
        billed,
        mut warnings,
    } = chosen;

    let mut model_rows: Vec<ModelRow> = models
        .iter()
        .map(|((pool, model), split)| {
            let (project_tokens, pool_tokens, project_output) = model_tokens
                .get(&(pool.clone(), model.clone()))
                .copied()
                .unwrap_or_default();
            ModelRow {
                model: model.clone(),
                family: pool.clone(),
                project_tokens,
                pool_tokens,
                project_output_tokens: project_output,
                share: split.share(options.basis).unwrap_or(0.0),
                project_list_value: split.project.value,
                pool_list_value: split.pool.value,
                priced: options.rate_overrides.resolve(model).is_some(),
                rate_source: options
                    .rate_overrides
                    .resolve(model)
                    .map_or("none", |resolved| resolved.source.as_str()),
            }
        })
        .collect();
    model_rows.sort_by(|left, right| {
        right
            .project_list_value
            .partial_cmp(&left.project_list_value)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.project_tokens.cmp(&left.project_tokens))
    });

    // A misspelled project matches nothing and apportions $0 — which reads as
    // "you cannot claim anything" rather than "that repository does not
    // exist". Same silent-zero shape as pruned history, so it gets named too.
    for project in &options.projects {
        let name = project.trim().to_ascii_lowercase();
        if seen_repos.contains(&name) {
            continue;
        }
        let mut nearest: Vec<&String> = seen_repos.iter().collect();
        nearest.sort_by_key(|candidate| distance(&name, candidate));
        let suggestion: Vec<&str> = nearest
            .iter()
            .take(3)
            .map(|candidate| candidate.as_str())
            .collect();
        warnings.push(if suggestion.is_empty() {
            format!("`{project}` matched no activity in this window")
        } else {
            format!(
                "`{project}` matched no activity in this window — did you mean {}?",
                suggestion.join(", ")
            )
        });
    }
    if cells.is_empty() {
        warnings.push(
            "no activity for any declared subscription in this window; every share is zero because nothing was measured, not because nothing was used"
                .to_string(),
        );
    }
    // Without a named project there is no claim to make, so the question
    // becomes how the whole spend divides. Same pool shares, applied to every
    // repository, which makes the rows sum to what was billed.
    let mut breakdown = Vec::new();
    if options.projects.is_empty() {
        let mut pool_totals: BTreeMap<&str, f64> = BTreeMap::new();
        let mut pool_spend: BTreeMap<&str, f64> = BTreeMap::new();
        for (pool, plan) in &options.subscriptions {
            let total: f64 = by_repo
                .values()
                .filter_map(|pools| pools.get(pool))
                .map(|measures| measures.get(options.basis))
                .sum();
            pool_totals.insert(pool.as_str(), total);
            pool_spend.insert(
                pool.as_str(),
                plan.spend(options.vat_percent) * months.len() as f64,
            );
        }
        for (project, pools) in &by_repo {
            let mut amounts = BTreeMap::new();
            let mut total = 0.0;
            for (pool, plan_total) in &pool_totals {
                let measure = pools
                    .get(*pool)
                    .map(|measures| measures.get(options.basis))
                    .unwrap_or(0.0);
                let amount = if *plan_total > 0.0 {
                    measure / plan_total * pool_spend[pool]
                } else {
                    0.0
                };
                amounts.insert((*pool).to_string(), amount);
                total += amount;
            }
            let (output_tokens, total_tokens) =
                repo_tokens.get(project).copied().unwrap_or_default();
            breakdown.push(ProjectRow {
                project: project.clone(),
                amounts,
                total,
                share_of_billed: if billed > 0.0 { total / billed } else { 0.0 },
                output_tokens,
                total_tokens,
            });
        }
        breakdown.sort_by(|left, right| {
            right
                .total
                .partial_cmp(&left.total)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    if !unpriced.is_empty() {
        warnings.push(format!(
            "no published rate for {} — counted in token and wall-clock bases, excluded from list-price value",
            name_list(&unpriced)
        ));
    }
    if built_in_used && let Some(warning) = pricing::stale_rates_warning(options.today) {
        warnings.push(warning);
    }
    if !unclassified.is_empty() {
        warnings.push(format!(
            "could not place {} in a subscription family — excluded entirely",
            name_list(&unclassified)
        ));
    }

    Allocation {
        projects: options.projects.clone(),
        months,
        basis: options.basis,
        gap_policy: options.gap_policy,
        vat_percent: options.vat_percent,
        currency: options.currency.clone(),
        subscriptions: options.subscriptions.clone(),
        billed,
        documented,
        attributable,
        effective_share: if documented > 0.0 {
            attributable / documented
        } else {
            0.0
        },
        top: options.top,
        periods,
        breakdown,
        models: model_rows,
        cross_check,
        rates_as_of: RATES_AS_OF,
        rate_overrides: overrides_used.into_iter().collect(),
        warnings,
    }
}

/// One basis apportioned across every month and family.
struct Apportionment {
    periods: Vec<PeriodRow>,
    attributable: f64,
    documented: f64,
    billed: f64,
    warnings: Vec<String>,
}

impl Apportionment {
    fn share(&self) -> f64 {
        if self.documented > 0.0 {
            self.attributable / self.documented
        } else {
            0.0
        }
    }
}

fn apportion(
    cells: &BTreeMap<(String, String), Split>,
    months: &[String],
    options: &AllocationOptions,
    basis: Basis,
) -> Apportionment {
    // A family's fallback share, for imputing months whose history is gone.
    let mut covered: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for ((_, pool), split) in cells {
        if let Some(share) = split.share(basis) {
            covered.entry(pool.as_str()).or_default().push(share);
        }
    }
    let imputed: BTreeMap<&str, f64> = covered
        .iter()
        .map(|(pool, shares)| (*pool, shares.iter().sum::<f64>() / shares.len() as f64))
        .collect();

    let mut outcome = Apportionment {
        periods: Vec::new(),
        attributable: 0.0,
        documented: 0.0,
        billed: 0.0,
        warnings: Vec::new(),
    };

    for month in months {
        for (pool, plan) in &options.subscriptions {
            let spend = plan.spend(options.vat_percent);
            outcome.billed += spend;
            let split = cells
                .get(&(month.clone(), pool.clone()))
                .copied()
                .unwrap_or_default();

            let (share, amount, note) = match split.share(basis) {
                Some(share) => {
                    outcome.documented += spend;
                    (share, share * spend, "")
                }
                None => {
                    let fallback = imputed.get(pool.as_str()).copied().unwrap_or(0.0);
                    let handling = match options.gap_policy {
                        GapPolicy::Impute => {
                            outcome.documented += spend;
                            (fallback, fallback * spend, "imputed")
                        }
                        GapPolicy::Zero => {
                            outcome.documented += spend;
                            (0.0, 0.0, "no data")
                        }
                        GapPolicy::Skip => (0.0, 0.0, "excluded"),
                    };
                    outcome.warnings.push(gap_warning(
                        month,
                        pool,
                        spend,
                        options.gap_policy,
                        fallback,
                        &options.currency,
                    ));
                    handling
                }
            };
            outcome.attributable += amount;
            outcome.periods.push(PeriodRow {
                month: month.clone(),
                family: pool.clone(),
                subscriptions: plan.count,
                plan_price: plan.price_with_tax(options.vat_percent),
                project_metric: split.project.get(basis),
                pool_metric: split.pool.get(basis),
                share,
                amount,
                note,
            });
        }
    }
    outcome
}

fn gap_warning(
    month: &str,
    pool: &str,
    spend: f64,
    policy: GapPolicy,
    fallback: f64,
    currency: &str,
) -> String {
    let handling = match policy {
        GapPolicy::Skip => format!(
            "{} excluded from both the claim and documented spend",
            money(spend, currency)
        ),
        GapPolicy::Zero => format!(
            "{} counted as spend but claims nothing — this under-bills",
            money(spend, currency)
        ),
        GapPolicy::Impute => format!(
            "claimed at {:.1}%, that family's mean over the months it can see = {}",
            fallback * 100.0,
            money(fallback * spend, currency)
        ),
    };
    format!("no {pool} history for {month} (history is pruned, not idle) — {handling}")
}

/// Levenshtein distance, for turning a mistyped project into a usable
/// suggestion rather than a bare zero.
fn distance(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0; right_chars.len() + 1];
    for (row, left_char) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right_chars.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != *right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_chars.len()]
}

fn name_list(counts: &BTreeMap<String, u64>) -> String {
    let mut entries: Vec<_> = counts.iter().collect();
    entries.sort_by(|left, right| right.1.cmp(left.1));
    let shown: Vec<String> = entries
        .iter()
        .take(4)
        .map(|(model, tokens)| format!("{model} ({})", tokens_short(**tokens)))
        .collect();
    if entries.len() > 4 {
        format!("{} and {} more", shown.join(", "), entries.len() - 4)
    } else {
        shown.join(", ")
    }
}

/// Renders an amount in `currency`. Symbols where they are unambiguous, the
/// ISO code otherwise — an invoice figure should never leave the reader
/// guessing which currency it is in.
fn money(value: f64, currency: &str) -> String {
    let amount = number(value.round() as i64);
    match currency.to_ascii_uppercase().as_str() {
        "USD" => format!("${amount}"),
        "EUR" => format!("\u{20ac}{amount}"),
        "GBP" => format!("\u{a3}{amount}"),
        "NOK" | "SEK" | "DKK" | "ISK" => format!("{amount} kr"),
        other => format!("{amount} {other}"),
    }
}

fn tokens_short(tokens: u64) -> String {
    let value = tokens as f64;
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}k", value / 1e3)
    } else {
        format!("{tokens}")
    }
}

/// Where the list rates came from, so a reader can tell which numbers are the
/// published table's and which are the user's.
fn rates_note(allocation: &Allocation) -> String {
    if allocation.rate_overrides.is_empty() {
        format!("rates as of {}", allocation.rates_as_of)
    } else {
        format!(
            "rates as of {}; overridden by model_rates: {}",
            allocation.rates_as_of,
            allocation.rate_overrides.join(", ")
        )
    }
}

fn metric_short(value: f64, basis: Basis, currency: &str) -> String {
    match basis {
        Basis::Output | Basis::Tokens => tokens_short(value as u64),
        Basis::Wall | Basis::Human => format!("{:.0}h", value / 3600.0),
        Basis::Value => money(value, currency),
    }
}

pub fn print_table(allocation: &Allocation) {
    let window = match allocation.months.as_slice() {
        [] => "no data".to_string(),
        [only] => only.clone(),
        [first, .., last] => format!("{first} → {last}"),
    };
    let total: u32 = allocation
        .subscriptions
        .values()
        .map(|plan| plan.count)
        .sum();
    println!();
    println!(
        "  ALLOCATION  {}",
        if allocation.projects.is_empty() {
            "every project".to_string()
        } else {
            allocation.projects.join(", ")
        }
    );
    let tax = if allocation.vat_percent > 0.0 {
        format!(" incl. {}% tax", trim_percent(allocation.vat_percent))
    } else {
        String::new()
    };
    println!(
        "  {window} · {} subscription{} · {} billed{tax} · basis: {}",
        total,
        if total == 1 { "" } else { "s" },
        money(allocation.billed, &allocation.currency),
        allocation.basis.label()
    );
    println!();

    if !allocation.breakdown.is_empty() {
        print_breakdown(allocation);
        return;
    }
    println!(
        "  {:<9} {:<8} {:>4} {:>10} {:>12} {:>12} {:>8} {:>11}",
        "month", "family", "subs", "plan/mo", "project", "pool", "share", "owed"
    );
    println!("  {}", "─".repeat(81));
    for period in &allocation.periods {
        let note = if period.note.is_empty() {
            String::new()
        } else {
            format!("  ← {}", period.note)
        };
        println!(
            "  {:<9} {:<8} {:>4} {:>10} {:>12} {:>12} {:>7.1}% {:>11}{note}",
            period.month,
            period.family,
            period.subscriptions,
            money(period.plan_price, &allocation.currency),
            metric_short(
                period.project_metric,
                allocation.basis,
                &allocation.currency
            ),
            metric_short(period.pool_metric, allocation.basis, &allocation.currency),
            period.share * 100.0,
            money(period.amount, &allocation.currency),
        );
    }
    println!("  {}", "─".repeat(81));
    println!(
        "  {:<60} {:>7.1}% {:>11}",
        "ATTRIBUTABLE",
        allocation.effective_share * 100.0,
        money(allocation.attributable, &allocation.currency)
    );
    if allocation.documented < allocation.billed {
        println!(
            "  of {} documented — {} of {} billed has no surviving history",
            money(allocation.documented, &allocation.currency),
            money(
                allocation.billed - allocation.documented,
                &allocation.currency
            ),
            money(allocation.billed, &allocation.currency)
        );
    }

    if !allocation.models.is_empty() {
        println!();
        println!("  MODELS  ({})", rates_note(allocation));
        println!(
            "  {:<26} {:<8} {:>11} {:>11} {:>8} {:>12}",
            "model", "family", "project", "pool", "share", "list value"
        );
        println!("  {}", "─".repeat(82));
        for model in &allocation.models {
            if model.project_tokens == 0 && model.pool_tokens == 0 {
                continue;
            }
            println!(
                "  {:<26} {:<8} {:>11} {:>11} {:>7.1}% {:>12}{}",
                truncate(&model.model, 26),
                model.family,
                tokens_short(model.project_tokens),
                tokens_short(model.pool_tokens),
                model.share * 100.0,
                money(model.project_list_value, &allocation.currency),
                match model.rate_source {
                    "override" => "  ← override",
                    "none" => "  ← unpriced",
                    _ => "",
                },
            );
        }
    }

    println!();
    println!("  CROSS-CHECK  same window, every basis");
    let mut ordered: Vec<&CrossCheckRow> = allocation.cross_check.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .share
            .partial_cmp(&left.share)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for row in ordered {
        println!(
            "   {} {:<20} {:>6.1}%   {:>11}{}",
            if row.measured { " " } else { "~" },
            row.label,
            row.share * 100.0,
            money(row.amount, &allocation.currency),
            if row.basis == allocation.basis {
                "  ←"
            } else {
                ""
            }
        );
    }
    println!("     ~ estimated, not provider-recorded");

    if !allocation.warnings.is_empty() {
        println!();
        println!("  WARNINGS");
        for warning in &allocation.warnings {
            println!("   ! {warning}");
        }
    }
    println!();
    println!(
        "  List value is the pay-per-token price of this usage, shown as a ceiling and as the"
    );
    println!("  weight between models. It is not an amount owed: a subscription is what you paid.");
    println!();
}

/// The no-claim view: how the whole spend divides across every project.
fn print_breakdown(allocation: &Allocation) {
    let pools: Vec<&String> = allocation.subscriptions.keys().collect();
    let mut header = format!("  {:<30}", "project");
    for pool in &pools {
        header.push_str(&format!(" {:>12}", pool));
    }
    header.push_str(&format!(
        " {:>12} {:>7} {:>10} {:>9}",
        "TOTAL", "%", "out", "tokens"
    ));
    let width = header.chars().count();
    println!("  PROJECTS");
    println!("{header}");
    println!("  {}", "─".repeat(width.saturating_sub(2)));

    let shown = if allocation.top == 0 {
        allocation.breakdown.len()
    } else {
        allocation.top.min(allocation.breakdown.len())
    };
    for project in &allocation.breakdown[..shown] {
        let mut line = format!("  {:<30}", truncate(&project.project, 30));
        for pool in &pools {
            let amount = project.amounts.get(*pool).copied().unwrap_or(0.0);
            line.push_str(&format!(" {:>12}", money(amount, &allocation.currency)));
        }
        line.push_str(&format!(
            " {:>12} {:>6.1}% {:>9} {:>8}",
            money(project.total, &allocation.currency),
            project.share_of_billed * 100.0,
            tokens_short(project.output_tokens),
            tokens_short(project.total_tokens),
        ));
        println!("{line}");
    }
    if shown < allocation.breakdown.len() {
        let rest = &allocation.breakdown[shown..];
        let total: f64 = rest.iter().map(|project| project.total).sum();
        println!(
            "  {:<30}{:>width$} {:>6.1}%",
            format!("({} smaller projects)", rest.len()),
            money(total, &allocation.currency),
            total / allocation.billed * 100.0,
            width = 13 * pools.len() + 13,
        );
    }
    println!("  {}", "─".repeat(width.saturating_sub(2)));
    let mut totals = format!("  {:<30}", "TOTAL");
    for pool in &pools {
        let sum: f64 = allocation
            .breakdown
            .iter()
            .map(|project| project.amounts.get(*pool).copied().unwrap_or(0.0))
            .sum();
        totals.push_str(&format!(" {:>12}", money(sum, &allocation.currency)));
    }
    totals.push_str(&format!(
        " {:>12} {:>6.1}%",
        money(allocation.billed, &allocation.currency),
        100.0
    ));
    println!("{totals}");
    // The models table is not shown here, so this is the only place the
    // breakdown says whose rates the list-price numbers rest on.
    if !allocation.rate_overrides.is_empty() {
        println!();
        println!("  Rates: {}", rates_note(allocation));
    }

    if !allocation.warnings.is_empty() {
        println!();
        println!("  WARNINGS");
        for warning in &allocation.warnings {
            println!("   ! {warning}");
        }
    }
    println!();
}

pub fn print_json(allocation: &Allocation) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(allocation)?);
    Ok(())
}

pub fn print_csv(allocation: &Allocation) -> Result<()> {
    // The per-month, per-family cells are the billable artifact; the model
    // breakdown is evidence for them and stays in table and JSON output.
    println!("month,family,subscriptions,project_metric,pool_metric,share,amount,note");
    for period in &allocation.periods {
        println!(
            "{},{},{},{},{},{:.6},{:.2},{}",
            period.month,
            period.family,
            period.subscriptions,
            period.project_metric,
            period.pool_metric,
            period.share,
            period.amount,
            period.note
        );
    }
    Ok(())
}

/// `25` rather than `25.0`, while `8.5` stays `8.5`.
fn trim_percent(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars()
            .take(width.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(repo: &str, model: &str, month: &str, output: u64, total: u64) -> ReportRow {
        let mut key = BTreeMap::new();
        key.insert("repo".to_string(), repo.to_string());
        key.insert("provider".to_string(), "claude".to_string());
        key.insert("model".to_string(), model.to_string());
        key.insert("month".to_string(), month.to_string());
        ReportRow {
            key,
            output_tokens: output,
            total_tokens: total,
            parallel_agent_seconds: output as f64,
            human_estimated_seconds: output as f64,
            ..ReportRow::default()
        }
    }

    fn options(basis: Basis, gap: GapPolicy) -> AllocationOptions {
        AllocationOptions {
            projects: vec!["Ada".to_string()],
            top: 0,
            subscriptions: BTreeMap::from([
                (
                    "claude".to_string(),
                    Plan {
                        count: 2,
                        price: 200.0,
                    },
                ),
                (
                    "openai".to_string(),
                    Plan {
                        count: 4,
                        price: 200.0,
                    },
                ),
            ]),
            vat_percent: 0.0,
            currency: "USD".to_string(),
            basis,
            gap_policy: gap,
            rate_overrides: RateOverrides::default(),
            today: NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
        }
    }

    #[test]
    fn share_is_the_projects_slice_of_its_own_family_pool() {
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Other", "claude-opus-5", "2026-08", 40, 40),
                row("Ada", "gpt-5.6-sol", "2026-08", 25, 25),
                row("Other", "gpt-5.6-sol", "2026-08", 75, 75),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        // Claude 60/100 of 2 subs, OpenAI 25/100 of 4 subs.
        let claude = allocation
            .periods
            .iter()
            .find(|period| period.family == "claude")
            .expect("claude row");
        let openai = allocation
            .periods
            .iter()
            .find(|period| period.family == "openai")
            .expect("openai row");
        assert!((claude.share - 0.6).abs() < 1e-9);
        assert!((claude.amount - 240.0).abs() < 1e-9);
        assert!((openai.share - 0.25).abs() < 1e-9);
        assert!((openai.amount - 200.0).abs() < 1e-9);
        assert!((allocation.attributable - 440.0).abs() < 1e-9);
    }

    #[test]
    fn a_pruned_family_month_is_never_silently_a_zero() {
        // The bug this guards: a month whose history is gone looks identical to
        // a month of genuinely no work, and reporting it as 0% quietly bills
        // the user's own project for the shortfall.
        let rows = [
            row("Ada", "claude-opus-5", "2026-07", 50, 50),
            row("Other", "claude-opus-5", "2026-07", 50, 50),
            row("Ada", "claude-opus-5", "2026-08", 50, 50),
            row("Other", "claude-opus-5", "2026-08", 50, 50),
            // No OpenAI rows at all: both months are holes for that family.
            row("Ada", "gpt-5.6-sol", "2026-08", 40, 40),
            row("Other", "gpt-5.6-sol", "2026-08", 60, 60),
        ];

        let skipped = build(&rows, &options(Basis::Output, GapPolicy::Skip));
        // July OpenAI is a hole: $800 leaves both the claim and the denominator.
        assert!(skipped.documented < skipped.billed);
        assert!((skipped.billed - skipped.documented - 800.0).abs() < 1e-9);
        assert!(!skipped.warnings.is_empty());

        let zeroed = build(&rows, &options(Basis::Output, GapPolicy::Zero));
        // Same claim, larger denominator — strictly the pessimistic reading.
        assert!((zeroed.attributable - skipped.attributable).abs() < 1e-9);
        assert!(zeroed.effective_share < skipped.effective_share);

        let imputed = build(&rows, &options(Basis::Output, GapPolicy::Impute));
        // August OpenAI ran at 40%, so July is claimed at 40% of $800.
        assert!((imputed.attributable - skipped.attributable - 320.0).abs() < 1e-9);
    }

    #[test]
    fn models_are_weighed_by_rate_not_by_raw_token_count() {
        // Equal token counts on a cheap and an expensive model must not imply
        // an equal claim on the plan.
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 1_000_000, 1_000_000),
                row("Other", "claude-haiku-4-5", "2026-08", 1_000_000, 1_000_000),
            ],
            &options(Basis::Value, GapPolicy::Skip),
        );
        let claude = allocation
            .periods
            .iter()
            .find(|period| period.family == "claude")
            .expect("claude row");
        // Opus output is $25/MTok against Haiku's $5 — 25/30 of the pool.
        assert!(
            (claude.share - 25.0 / 30.0).abs() < 1e-9,
            "got {}",
            claude.share
        );
    }

    fn override_options(json: &str, today: NaiveDate) -> AllocationOptions {
        let config = serde_json::from_str(json).unwrap();
        AllocationOptions {
            rate_overrides: RateOverrides::from_config(&config).unwrap(),
            today,
            ..options(Basis::Value, GapPolicy::Skip)
        }
    }

    fn stale_warning(allocation: &Allocation) -> Option<&String> {
        allocation
            .warnings
            .iter()
            .find(|warning| warning.contains("built-in list rates"))
    }

    #[test]
    fn stale_built_in_rates_warn_and_fresh_ones_do_not() {
        let rows = [row("Ada", "claude-opus-5", "2026-08", 10, 10)];
        let mut fresh = options(Basis::Value, GapPolicy::Skip);
        fresh.today = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        assert!(stale_warning(&build(&rows, &fresh)).is_none());

        let mut old = fresh.clone();
        old.today = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        let allocation = build(&rows, &old);
        let warning = stale_warning(&allocation).expect("stale warning");
        assert!(warning.contains(RATES_AS_OF), "{warning}");
        assert!(warning.contains("days old"), "{warning}");
        assert!(warning.contains("model_rates"), "{warning}");
    }

    #[test]
    fn a_run_priced_entirely_by_overrides_has_nothing_stale_to_warn_about() {
        let old = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        let json =
            r#"{"claude-opus-5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 1}}"#;
        let allocation = build(
            &[row("Ada", "claude-opus-5", "2026-08", 10, 10)],
            &override_options(json, old),
        );
        assert!(stale_warning(&allocation).is_none());
        assert_eq!(vec!["claude-opus-5".to_string()], allocation.rate_overrides);

        // One model still on the built-in table brings the warning back.
        let mixed = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 10, 10),
                row("Ada", "claude-haiku-4-5", "2026-08", 10, 10),
            ],
            &override_options(json, old),
        );
        assert!(stale_warning(&mixed).is_some());
    }

    #[test]
    fn overrides_reweigh_models_and_price_the_ones_the_table_lacks() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        // Built-in: Opus output $25 vs Haiku $5. The override makes them equal
        // and adds a model the table has never heard of.
        let json = r#"{
            "claude-opus-5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 5},
            "acme-coder": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 5,
                           "family": "claude"}
        }"#;
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 1_000_000, 1_000_000),
                row("Other", "claude-haiku-4-5", "2026-08", 1_000_000, 1_000_000),
                row("Other", "acme-coder-1", "2026-08", 1_000_000, 1_000_000),
            ],
            &override_options(json, today),
        );
        let claude = allocation
            .periods
            .iter()
            .find(|period| period.family == "claude")
            .expect("claude row");
        assert!(
            (claude.share - 1.0 / 3.0).abs() < 1e-9,
            "got {}",
            claude.share
        );
        let acme = allocation
            .models
            .iter()
            .find(|model| model.model == "acme-coder-1")
            .expect("acme row");
        assert!(acme.priced);
        assert_eq!("override", acme.rate_source);
        assert_eq!(
            "built-in",
            allocation
                .models
                .iter()
                .find(|model| model.model == "claude-haiku-4-5")
                .unwrap()
                .rate_source
        );
        assert!(
            !allocation
                .warnings
                .iter()
                .any(|warning| warning.contains("no published rate"))
        );
        assert_eq!(
            vec!["acme-coder".to_string(), "claude-opus-5".to_string()],
            allocation.rate_overrides
        );
    }

    #[test]
    fn cross_check_reports_every_basis_over_the_same_window() {
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 200),
                row("Other", "claude-opus-5", "2026-08", 40, 800),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        assert_eq!(Basis::ALL.len(), allocation.cross_check.len());
        let by_output = allocation
            .cross_check
            .iter()
            .find(|row| row.basis == Basis::Output)
            .expect("output basis");
        let by_tokens = allocation
            .cross_check
            .iter()
            .find(|row| row.basis == Basis::Tokens)
            .expect("tokens basis");
        assert!((by_output.share - 0.6).abs() < 1e-9);
        // Cache-heavy totals tell a different story, which is the entire point
        // of showing them side by side.
        assert!((by_tokens.share - 0.2).abs() < 1e-9);
    }

    #[test]
    fn the_cross_check_row_for_the_chosen_basis_equals_the_headline() {
        // Regression: the cross-check used to pool every vendor together while
        // the headline weighed each family by how many plans it holds, so the
        // row marked as the chosen basis contradicted the total it was meant
        // to corroborate. Unequal plan counts are what expose it.
        let rows = [
            row("Ada", "claude-opus-5", "2026-08", 60, 60),
            row("Other", "claude-opus-5", "2026-08", 40, 40),
            row("Ada", "gpt-5.6-sol", "2026-08", 25, 25),
            row("Other", "gpt-5.6-sol", "2026-08", 75, 75),
        ];
        for basis in Basis::ALL {
            let allocation = build(&rows, &options(basis, GapPolicy::Skip));
            let marked = allocation
                .cross_check
                .iter()
                .find(|row| row.basis == basis)
                .expect("chosen basis appears in its own cross-check");
            assert!(
                (marked.share - allocation.effective_share).abs() < 1e-9,
                "{basis:?}: cross-check {} vs headline {}",
                marked.share,
                allocation.effective_share
            );
            assert!(
                (marked.amount - allocation.attributable).abs() < 1e-9,
                "{basis:?}: cross-check ${} vs headline ${}",
                marked.amount,
                allocation.attributable
            );
        }
    }

    #[test]
    fn subscription_counts_weigh_the_families_against_each_other() {
        // Claude is 60% of its pool on 2 plans, OpenAI 25% on 4. Pooling the
        // tokens would say 42.5%; weighing by plans held says 36.7%.
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Other", "claude-opus-5", "2026-08", 40, 40),
                row("Ada", "gpt-5.6-sol", "2026-08", 25, 25),
                row("Other", "gpt-5.6-sol", "2026-08", 75, 75),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        // (0.6 * $400 + 0.25 * $800) / $1200
        assert!(
            (allocation.effective_share - 440.0 / 1200.0).abs() < 1e-9,
            "got {}",
            allocation.effective_share
        );
    }

    #[test]
    fn a_family_without_a_subscription_is_not_part_of_any_pool() {
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 50, 50),
                row("Other", "claude-opus-5", "2026-08", 50, 50),
                row("Ada", "gemini-3-pro", "2026-08", 999, 999),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        assert!(
            !allocation
                .models
                .iter()
                .any(|model| model.family == "google"),
            "Gemini has no plan declared and must not appear"
        );
        assert_eq!(2, allocation.periods.len());
    }

    #[test]
    fn a_project_that_matched_nothing_says_so_and_suggests_the_near_miss() {
        // A typo apportions $0, which reads as "nothing to claim" rather than
        // "no such repository" — the same silent zero as pruned history.
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.projects = vec!["Adaa".to_string()];
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Chronicle", "claude-opus-5", "2026-08", 40, 40),
            ],
            &options,
        );
        assert_eq!(0.0, allocation.attributable);
        let warning = allocation
            .warnings
            .iter()
            .find(|warning| warning.contains("Adaa"))
            .expect("an unmatched project must be named");
        assert!(
            warning.contains("did you mean ada"),
            "the nearest real repository should be offered, got {warning}"
        );
    }

    #[test]
    fn a_matched_project_is_never_reported_as_a_near_miss() {
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Chronicle", "claude-opus-5", "2026-08", 40, 40),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        assert!(
            !allocation
                .warnings
                .iter()
                .any(|warning| warning.contains("did you mean")),
            "got {:?}",
            allocation.warnings
        );
    }

    #[test]
    fn an_empty_window_says_nothing_was_measured_rather_than_implying_no_usage() {
        let allocation = build(&[], &options(Basis::Output, GapPolicy::Skip));
        assert_eq!(0.0, allocation.attributable);
        assert!(
            allocation
                .warnings
                .iter()
                .any(|warning| warning.contains("nothing was measured")),
            "got {:?}",
            allocation.warnings
        );
    }

    #[test]
    fn tax_is_apportioned_because_it_is_money_that_actually_left_the_account() {
        let rows = [
            row("Ada", "claude-opus-5", "2026-08", 60, 60),
            row("Other", "claude-opus-5", "2026-08", 40, 40),
        ];
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.subscriptions = BTreeMap::from([(
            "claude".to_string(),
            Plan {
                count: 2,
                price: 200.0,
            },
        )]);

        let untaxed = build(&rows, &options);
        options.vat_percent = 25.0;
        let taxed = build(&rows, &options);

        // The share is a usage ratio and cannot move when only the price does.
        assert_eq!(untaxed.effective_share, taxed.effective_share);
        // 2 plans at 200 + 25% = 500; 60% of that is 300.
        assert!((untaxed.attributable - 240.0).abs() < 1e-9);
        assert!((taxed.attributable - 300.0).abs() < 1e-9);
        assert!((taxed.billed - 500.0).abs() < 1e-9);
        // The tax-inclusive unit price rides on the row, so a mixed-vendor
        // run stays auditable line by line.
        assert!((taxed.periods[0].plan_price - 250.0).abs() < 1e-9);
    }

    #[test]
    fn each_vendor_is_priced_on_its_own_bill() {
        // Outside the US the two vendors rarely cost the same: one converts
        // dollars through your card, the other sets a local price. A single
        // global price forces that arithmetic back onto the user by hand.
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.subscriptions = BTreeMap::from([
            (
                "claude".to_string(),
                Plan {
                    count: 2,
                    price: 1866.0,
                },
            ),
            (
                "openai".to_string(),
                Plan {
                    count: 3,
                    price: 1992.0,
                },
            ),
        ]);
        options.vat_percent = 25.0;
        options.currency = "NOK".to_string();

        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Other", "claude-opus-5", "2026-08", 40, 40),
                row("Ada", "gpt-5.6-sol", "2026-08", 25, 25),
                row("Other", "gpt-5.6-sol", "2026-08", 75, 75),
            ],
            &options,
        );

        // 2 x 1866 x 1.25 = 4665, and 3 x 1992 x 1.25 = 7470.
        assert!((allocation.billed - 12_135.0).abs() < 1e-6);
        let claude = allocation
            .periods
            .iter()
            .find(|period| period.family == "claude")
            .expect("claude row");
        let openai = allocation
            .periods
            .iter()
            .find(|period| period.family == "openai")
            .expect("openai row");
        assert!((claude.plan_price - 2332.5).abs() < 1e-6);
        assert!((openai.plan_price - 2490.0).abs() < 1e-6);
        // 60% of 4665 plus 25% of 7470.
        assert!((allocation.attributable - (0.6 * 4665.0 + 0.25 * 7470.0)).abs() < 1e-6);
    }

    #[test]
    fn a_vendor_without_its_own_price_falls_back_to_the_run_default() {
        // Mixing `claude=2` and `codex=3@1992` must not leave the unpriced
        // vendor at zero.
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.subscriptions = BTreeMap::from([
            (
                "claude".to_string(),
                Plan {
                    count: 2,
                    price: 200.0,
                },
            ),
            (
                "openai".to_string(),
                Plan {
                    count: 3,
                    price: 1992.0,
                },
            ),
        ]);
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Other", "claude-opus-5", "2026-08", 40, 40),
            ],
            &options,
        );
        let claude = allocation
            .periods
            .iter()
            .find(|period| period.family == "claude")
            .expect("claude row");
        assert!((claude.plan_price - 200.0).abs() < 1e-9);
    }

    #[test]
    fn without_a_named_project_every_project_is_reported_and_the_rows_total_the_bill() {
        // No project named means no claim is being made, so the question is
        // how the whole spend divides. The rows must reconcile exactly to what
        // was billed, or the table is not a breakdown of anything.
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.projects = Vec::new();
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Chronicle", "claude-opus-5", "2026-08", 40, 40),
                row("Ada", "gpt-5.6-sol", "2026-08", 25, 25),
                row("Varde", "gpt-5.6-sol", "2026-08", 75, 75),
            ],
            &options,
        );

        assert_eq!(3, allocation.breakdown.len());
        let total: f64 = allocation.breakdown.iter().map(|row| row.total).sum();
        assert!(
            (total - allocation.billed).abs() < 1e-6,
            "breakdown {total} must reconcile to billed {}",
            allocation.billed
        );
        // Ranked by money, not by tokens: Varde holds 75% of the four-plan
        // OpenAI pool (600) and outranks Ada's 440 across both pools, even
        // though Ada produced more output overall.
        assert_eq!("Varde", allocation.breakdown[0].project);
        let ada = allocation
            .breakdown
            .iter()
            .find(|row| row.project == "Ada")
            .expect("Ada row");
        assert!((ada.total - (0.6 * 400.0 + 0.25 * 800.0)).abs() < 1e-6);
        // A project absent from a pool contributes nothing to it rather than
        // silently borrowing another project's share.
        let varde = allocation
            .breakdown
            .iter()
            .find(|row| row.project == "Varde")
            .expect("Varde row");
        assert_eq!(0.0, varde.amounts["claude"]);
        assert!((varde.amounts["openai"] - 0.75 * 800.0).abs() < 1e-6);
    }

    #[test]
    fn naming_a_project_reports_a_claim_rather_than_a_breakdown() {
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 60, 60),
                row("Chronicle", "claude-opus-5", "2026-08", 40, 40),
            ],
            &options(Basis::Output, GapPolicy::Skip),
        );
        assert!(allocation.breakdown.is_empty());
        assert!(allocation.attributable > 0.0);
    }

    #[test]
    fn amounts_carry_their_currency_rather_than_an_assumed_dollar() {
        // A figure destined for an invoice must never leave the reader
        // guessing which currency it is in.
        assert_eq!("$1,250", money(1250.0, "USD"));
        assert_eq!("1,250 kr", money(1250.0, "NOK"));
        assert_eq!("1,250 CHF", money(1250.0, "CHF"));
        assert_eq!("1,250 kr", money(1250.0, "nok"));
    }

    #[test]
    fn project_matching_ignores_case_and_padding() {
        let mut options = options(Basis::Output, GapPolicy::Skip);
        options.projects = vec!["  ada  ".to_string()];
        let allocation = build(
            &[
                row("Ada", "claude-opus-5", "2026-08", 70, 70),
                row("Other", "claude-opus-5", "2026-08", 30, 30),
            ],
            &options,
        );
        let claude = &allocation.periods[0];
        assert!((claude.share - 0.7).abs() < 1e-9);
    }
}
