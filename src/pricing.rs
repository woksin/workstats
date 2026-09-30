//! Published per-token list rates, and the provider family a model belongs to.
//!
//! These rates exist for one purpose: to weigh models against each other when
//! apportioning a flat-rate subscription. A month of Opus and a month of Haiku
//! are not the same claim on a plan, and token counts alone say they are.
//!
//! Nothing here is a bill. The rates are the public pay-per-token prices, which
//! is precisely what a subscription holder does *not* pay; `allocate` uses them
//! as relative weights and as a stated upper bound, never as an amount owed.
//!
//! Rates are the standard short-context tier: batch, fast-mode, long-context,
//! and data-residency multipliers are deliberately ignored, because a
//! subscription-backed CLI session does not use them.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::Deserialize;

/// When the rate table below was last checked against the vendors' price
/// pages. Surfaced in output so a stale table is visible rather than silent.
pub const RATES_AS_OF: &str = "2026-09-02";

/// How old the table may get before output warns about it. Vendors reprice and
/// release models every few months, and a value-weighted split computed from
/// last year's prices looks exactly as authoritative as one computed from
/// this month's; a quarter is long enough not to nag and short enough to catch
/// a table that has been missed by a release.
pub const STALE_AFTER_DAYS: i64 = 90;

/// A warning when the built-in table is older than [`STALE_AFTER_DAYS`] on
/// `today`, or `None` while it is fresh.
///
/// `today` is passed in rather than read here so the clock stays in one place
/// and the threshold can be tested on either side without waiting for it.
pub fn stale_rates_warning(today: NaiveDate) -> Option<String> {
    let as_of = NaiveDate::parse_from_str(RATES_AS_OF, "%Y-%m-%d").ok()?;
    let age = today.signed_duration_since(as_of).num_days();
    // A table dated in the future (a skewed clock) is not evidence of staleness.
    (age > STALE_AFTER_DAYS).then(|| {
        format!(
            "built-in list rates are dated {RATES_AS_OF} ({age} days old, past the {STALE_AFTER_DAYS}-day limit) and vendors may have repriced since; \
             set current prices under \"model_rates\" in the config file, or upgrade workstats for a refreshed table"
        )
    })
}

/// Which subscription pool a model's usage draws down.
///
/// Deliberately coarse: a plan is bought per vendor, not per model, so this is
/// the granularity at which money actually moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Family {
    Claude,
    OpenAI,
    Google,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Claude => "claude",
            Family::OpenAI => "openai",
            Family::Google => "google",
        }
    }

    /// Accepts the vendor name, the family name, and the CLI people actually
    /// say. `--sub codex=4` and `--sub openai=4` are the same subscription.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "claude" | "anthropic" | "claude-code" => Some(Family::Claude),
            "openai" | "codex" | "chatgpt" | "gpt" => Some(Family::OpenAI),
            "google" | "gemini" => Some(Family::Google),
            _ => None,
        }
    }

    pub const ALL: [Family; 3] = [Family::Claude, Family::OpenAI, Family::Google];
}

/// Clients that bill through their own subscription rather than drawing on the
/// model vendor's plan.
///
/// Copilot serves Anthropic and OpenAI models, but a Copilot seat is not a
/// Claude or ChatGPT plan. Folding it into the vendor pool inflates that pool's
/// denominator with usage no vendor plan paid for, and — worse — makes a month
/// whose vendor history has been pruned look covered, so the gap goes
/// unreported and unclaimed.
pub fn separate_plan(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "copilot" | "copilot-vscode" => Some("copilot"),
        _ => None,
    }
}

/// Every pool name `--sub` accepts, for validation and error messages.
pub const SEPARATE_PLANS: [&str; 1] = ["copilot"];

/// Which subscription pool a row draws on: the client's own plan when it bills
/// separately, otherwise the vendor plan for the model it ran.
pub fn pool_for(provider: &str, model: &str) -> Option<String> {
    if let Some(plan) = separate_plan(provider) {
        return Some(plan.to_string());
    }
    family_of(model).map(|family| family.as_str().to_string())
}

impl fmt::Display for Family {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// USD per million tokens, by token class.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl Rate {
    const fn new(input: f64, cache_write: f64, cache_read: f64, output: f64) -> Self {
        Self {
            input,
            cache_write,
            cache_read,
            output,
        }
    }

    /// List-price value of one row's usage, in USD.
    pub fn value(&self, input: u64, cache_write: u64, cache_read: u64, output: u64) -> f64 {
        let per_million = |tokens: u64, rate: f64| (tokens as f64) / 1_000_000.0 * rate;
        per_million(input, self.input)
            + per_million(cache_write, self.cache_write)
            + per_million(cache_read, self.cache_read)
            + per_million(output, self.output)
    }
}

/// Model-name prefix to rate. Matched longest-prefix-first, so a dated release
/// (`claude-haiku-4-5-20251001`) inherits its base model's rate instead of
/// silently falling through to "unpriced" every time a vendor stamps a date on
/// a snapshot.
const RATES: &[(&str, Family, Rate)] = &[
    // Anthropic — platform.claude.com/docs/en/about-claude/pricing
    // Cache writes are the 5-minute tier (1.25x input); reads are 0.1x input,
    // except Fable/Mythos 5.1 which are 0.025x.
    (
        "claude-fable-5-1",
        Family::Claude,
        Rate::new(10.0, 12.5, 0.25, 50.0),
    ),
    (
        "claude-mythos-5-1",
        Family::Claude,
        Rate::new(10.0, 12.5, 0.25, 50.0),
    ),
    (
        "claude-fable-5",
        Family::Claude,
        Rate::new(10.0, 12.5, 1.0, 50.0),
    ),
    (
        "claude-mythos-5",
        Family::Claude,
        Rate::new(10.0, 12.5, 1.0, 50.0),
    ),
    (
        "claude-opus-5",
        Family::Claude,
        Rate::new(5.0, 6.25, 0.5, 25.0),
    ),
    (
        "claude-opus-4-8",
        Family::Claude,
        Rate::new(5.0, 6.25, 0.5, 25.0),
    ),
    (
        "claude-opus-4-7",
        Family::Claude,
        Rate::new(5.0, 6.25, 0.5, 25.0),
    ),
    (
        "claude-opus-4-6",
        Family::Claude,
        Rate::new(5.0, 6.25, 0.5, 25.0),
    ),
    (
        "claude-opus-4-5",
        Family::Claude,
        Rate::new(5.0, 6.25, 0.5, 25.0),
    ),
    (
        "claude-opus-4-1",
        Family::Claude,
        Rate::new(15.0, 18.75, 1.5, 75.0),
    ),
    (
        "claude-opus-4",
        Family::Claude,
        Rate::new(15.0, 18.75, 1.5, 75.0),
    ),
    (
        "claude-sonnet-5",
        Family::Claude,
        Rate::new(2.0, 2.5, 0.2, 10.0),
    ),
    (
        "claude-sonnet-4-6",
        Family::Claude,
        Rate::new(3.0, 3.75, 0.3, 15.0),
    ),
    (
        "claude-sonnet-4-5",
        Family::Claude,
        Rate::new(3.0, 3.75, 0.3, 15.0),
    ),
    (
        "claude-sonnet-4",
        Family::Claude,
        Rate::new(3.0, 3.75, 0.3, 15.0),
    ),
    (
        "claude-haiku-4-5",
        Family::Claude,
        Rate::new(1.0, 1.25, 0.1, 5.0),
    ),
    (
        "claude-haiku-3-5",
        Family::Claude,
        Rate::new(0.8, 1.0, 0.08, 4.0),
    ),
    // OpenAI — developers.openai.com/api/docs/pricing
    (
        "gpt-5.6-sol",
        Family::OpenAI,
        Rate::new(4.0, 5.0, 0.4, 20.0),
    ),
    (
        "gpt-5.6-terra",
        Family::OpenAI,
        Rate::new(2.0, 2.5, 0.2, 12.0),
    ),
    (
        "gpt-5.6-luna",
        Family::OpenAI,
        Rate::new(0.2, 0.25, 0.02, 1.2),
    ),
    (
        "gpt-5.6-cyber",
        Family::OpenAI,
        Rate::new(12.5, 15.625, 1.25, 75.0),
    ),
    (
        "gpt-5.5-pro",
        Family::OpenAI,
        Rate::new(30.0, 37.5, 3.0, 180.0),
    ),
    ("gpt-5.5", Family::OpenAI, Rate::new(5.0, 6.25, 0.5, 30.0)),
    ("gpt-5.4", Family::OpenAI, Rate::new(2.5, 3.125, 0.25, 15.0)),
    (
        "gpt-5.3-codex-spark",
        Family::OpenAI,
        Rate::new(1.75, 2.1875, 0.175, 14.0),
    ),
    (
        "gpt-5.3-codex",
        Family::OpenAI,
        Rate::new(1.75, 2.1875, 0.175, 14.0),
    ),
    (
        "gpt-5-mini",
        Family::OpenAI,
        Rate::new(0.25, 0.3125, 0.025, 2.0),
    ),
    (
        "gpt-5-nano",
        Family::OpenAI,
        Rate::new(0.05, 0.0625, 0.005, 0.4),
    ),
    // Google — ai.google.dev/gemini-api/docs/pricing
    (
        "gemini-3-pro",
        Family::Google,
        Rate::new(2.0, 2.5, 0.2, 12.0),
    ),
    (
        "gemini-3-flash",
        Family::Google,
        Rate::new(0.3, 0.375, 0.03, 2.5),
    ),
    (
        "gemini-2.5-pro",
        Family::Google,
        Rate::new(1.25, 1.5625, 0.125, 10.0),
    ),
    (
        "gemini-2.5-flash",
        Family::Google,
        Rate::new(0.3, 0.375, 0.03, 2.5),
    ),
];

/// One user-supplied rate, as written in the config file.
///
/// Every field is optional here so that a missing one is reported by
/// [`RateOverrides::from_config`] *with the model key*; serde alone would say
/// "missing field" and discard the whole config.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRateConfig {
    /// USD per million tokens, like the built-in table.
    pub input: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
    /// Which subscription pool the model draws on (`claude`, `openai`,
    /// `google`, or an alias such as `codex`). Optional: it falls back to the
    /// built-in table, then to the vendor prefix.
    pub family: Option<String>,
}

/// Where a model's rate came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateSource {
    Override,
    BuiltIn,
}

impl RateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RateSource::Override => "override",
            RateSource::BuiltIn => "built-in",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedRate<'a> {
    pub rate: Rate,
    pub source: RateSource,
    /// The override key that matched, as the user wrote it.
    pub pattern: Option<&'a str>,
}

#[derive(Clone, Debug)]
struct RateOverride {
    pattern: String,
    prefix: String,
    family: Option<Family>,
    rate: Rate,
}

/// User rates that take precedence over the built-in table.
///
/// Matching is the built-in table's: the key is a model-name *prefix*,
/// compared after lowercasing and treating `.` as `-`, and the longest matching
/// key wins. There is no wildcard syntax because the prefix rule already
/// covers dated snapshots, and a second matching language would need its own
/// precedence rules. Any matching override beats any built-in entry, however
/// long the built-in prefix is: the user wrote it to be believed.
#[derive(Clone, Debug, Default)]
pub struct RateOverrides {
    entries: Vec<RateOverride>,
}

impl RateOverrides {
    /// Validates the `model_rates` config map. Errors name the offending key.
    pub fn from_config(config: &BTreeMap<String, ModelRateConfig>) -> Result<Self> {
        if config.len() > 256 {
            bail!("at most 256 model rates are supported");
        }
        let mut entries: Vec<RateOverride> = Vec::new();
        for (key, value) in config {
            let pattern = key.trim();
            if pattern.is_empty()
                || pattern.len() > 128
                || pattern.chars().any(|character| character.is_control())
            {
                bail!(
                    "model rate key {key:?} must be a model name of 1 to 128 printable characters"
                );
            }
            let prefix = canonical(pattern);
            if let Some(other) = entries.iter().find(|entry| entry.prefix == prefix) {
                bail!(
                    "model rates {:?} and {key:?} match the same models",
                    other.pattern
                );
            }
            let amount = |name: &str, value: Option<f64>| -> Result<f64> {
                let Some(value) = value else {
                    bail!("model rate {key:?} is missing \"{name}\"");
                };
                if !value.is_finite() || value < 0.0 {
                    bail!(
                        "model rate {key:?}: \"{name}\" must be a non-negative number of USD per million tokens, got {value}"
                    );
                }
                Ok(value)
            };
            let rate = Rate::new(
                amount("input", value.input)?,
                amount("cache_write", value.cache_write)?,
                amount("cache_read", value.cache_read)?,
                amount("output", value.output)?,
            );
            let family = match value.family.as_deref() {
                None => None,
                Some(text) => Some(Family::parse(text).with_context(|| {
                    format!(
                        "model rate {key:?} has unknown family {text:?}; expected one of {}",
                        Family::ALL.map(Family::as_str).join(", ")
                    )
                })?),
            };
            entries.push(RateOverride {
                pattern: pattern.to_string(),
                prefix,
                family,
                rate,
            });
        }
        Ok(Self { entries })
    }

    fn matching(&self, model: &str) -> Option<&RateOverride> {
        let name = canonical(model);
        self.entries
            .iter()
            .filter(|entry| name.starts_with(&entry.prefix))
            .max_by_key(|entry| entry.prefix.len())
    }

    /// The rate for a model: an override if one matches, else the built-in
    /// table, else `None` (unpriced).
    pub fn resolve(&self, model: &str) -> Option<ResolvedRate<'_>> {
        if let Some(entry) = self.matching(model) {
            return Some(ResolvedRate {
                rate: entry.rate,
                source: RateSource::Override,
                pattern: Some(entry.pattern.as_str()),
            });
        }
        rate_for(model).map(|rate| ResolvedRate {
            rate,
            source: RateSource::BuiltIn,
            pattern: None,
        })
    }

    /// Like [`pool_for`], but an override's declared family wins over the
    /// built-in table and the vendor-prefix guess. That is what lets a model
    /// the tool has never heard of land in a pool instead of being excluded.
    pub fn pool_for(&self, provider: &str, model: &str) -> Option<String> {
        if let Some(plan) = separate_plan(provider) {
            return Some(plan.to_string());
        }
        if let Some(family) = self.matching(model).and_then(|entry| entry.family) {
            return Some(family.as_str().to_string());
        }
        pool_for(provider, model)
    }
}

/// Normalises the punctuation drift between how a vendor names a model and how
/// each CLI records it (`claude-sonnet-4.6` vs `claude-sonnet-4-6`).
fn canonical(model: &str) -> String {
    model.trim().to_ascii_lowercase().replace('.', "-")
}

/// The published rate for a model, matched longest-prefix-first.
pub fn rate_for(model: &str) -> Option<Rate> {
    entry_for(model).map(|(_, _, rate)| rate)
}

/// The subscription pool a model draws on.
///
/// Falls back to the vendor prefix so an unpriced but clearly-Anthropic model
/// still lands in the Claude pool rather than vanishing from the report. A
/// model that is neither priced nor recognisable returns `None` and is
/// surfaced as a warning rather than silently dropped.
pub fn family_of(model: &str) -> Option<Family> {
    if let Some((_, family, _)) = entry_for(model) {
        return Some(family);
    }
    let name = canonical(model);
    if name.starts_with("claude") {
        Some(Family::Claude)
    } else if name.starts_with("gpt") || name.contains("codex") || name.starts_with("o1") {
        Some(Family::OpenAI)
    } else if name.starts_with("gemini") {
        Some(Family::Google)
    } else {
        None
    }
}

fn entry_for(model: &str) -> Option<(&'static str, Family, Rate)> {
    let name = canonical(model);
    RATES
        .iter()
        // Both sides must be canonicalised. The table is written the way the
        // vendors write it (`gpt-5.6-sol`), the CLIs record it either way, and
        // comparing a normalised name against a raw prefix silently unpriced
        // every dotted model in the table.
        .filter(|(prefix, _, _)| name.starts_with(&canonical(prefix)))
        .max_by_key(|(prefix, _, _)| prefix.len())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_snapshots_inherit_the_base_model_rate() {
        // The reason prefix matching exists: vendors stamp dates on snapshots
        // and every one of them would otherwise read as unpriced.
        assert_eq!(
            rate_for("claude-haiku-4-5"),
            rate_for("claude-haiku-4-5-20251001")
        );
    }

    #[test]
    fn longest_prefix_wins_so_variants_do_not_collapse_into_their_base() {
        // "gpt-5.5-pro" also starts with "gpt-5.5"; picking the shorter match
        // would price a $180/MTok model at $30.
        assert_eq!(180.0, rate_for("gpt-5.5-pro").unwrap().output);
        assert_eq!(30.0, rate_for("gpt-5.5").unwrap().output);
        // Same trap in the other vendor: -spark must not swallow plain codex.
        assert_eq!(
            rate_for("gpt-5.3-codex"),
            rate_for("gpt-5.3-codex-2026-01-01")
        );
    }

    #[test]
    fn punctuation_drift_between_clis_resolves_to_one_rate() {
        assert_eq!(rate_for("claude-sonnet-4-6"), rate_for("claude-sonnet-4.6"));
    }

    #[test]
    fn unpriced_but_recognisable_models_still_pick_a_pool() {
        // Forward compatibility: a model released after this table was written
        // must not silently leave its family's pool and skew every share.
        assert_eq!(None, rate_for("claude-opus-9"));
        assert_eq!(Some(Family::Claude), family_of("claude-opus-9"));
        assert_eq!(Some(Family::OpenAI), family_of("gpt-7-turbo"));
        assert_eq!(None, family_of("llama-4"));
    }

    #[test]
    fn a_client_with_its_own_plan_never_draws_on_the_vendor_pool() {
        // Copilot running an OpenAI model is a Copilot seat being spent, not a
        // ChatGPT plan. Counting it in the OpenAI pool overstates that pool and
        // hides months where the vendor's own history is missing.
        assert_eq!(
            Some("copilot".to_string()),
            pool_for("copilot", "gpt-5.3-codex")
        );
        assert_eq!(
            Some("copilot".to_string()),
            pool_for("copilot-vscode", "claude-sonnet-4.6")
        );
        assert_eq!(
            Some("openai".to_string()),
            pool_for("codex", "gpt-5.3-codex")
        );
        // pi authenticates with the user's own vendor subscriptions, so it does
        // draw on the vendor pool for whichever model it ran.
        assert_eq!(Some("claude".to_string()), pool_for("pi", "claude-opus-5"));
        assert_eq!(Some("openai".to_string()), pool_for("pi", "gpt-5.6-sol"));
    }

    #[test]
    fn family_aliases_match_what_people_type() {
        assert_eq!(Some(Family::OpenAI), Family::parse("codex"));
        assert_eq!(Some(Family::OpenAI), Family::parse("OpenAI"));
        assert_eq!(Some(Family::Claude), Family::parse("anthropic"));
        assert_eq!(None, Family::parse("bedrock"));
    }

    #[test]
    fn value_weighs_each_token_class_separately() {
        let rate = Rate::new(5.0, 6.25, 0.5, 25.0);
        // 1M input + 1M cache write + 1M cache read + 1M output.
        let total = rate.value(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((total - 36.75).abs() < 1e-9, "got {total}");
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn the_table_date_is_a_real_date() {
        // The staleness check silently passes if the constant stops parsing.
        assert!(NaiveDate::parse_from_str(RATES_AS_OF, "%Y-%m-%d").is_ok());
    }

    #[test]
    fn rates_go_stale_only_after_the_threshold() {
        let as_of = date(RATES_AS_OF);
        let on = |days: i64| as_of + chrono::Duration::days(days);
        assert_eq!(None, stale_rates_warning(on(0)));
        assert_eq!(None, stale_rates_warning(on(STALE_AFTER_DAYS)));
        let warning = stale_rates_warning(on(STALE_AFTER_DAYS + 1)).expect("stale");
        assert!(warning.contains(RATES_AS_OF), "{warning}");
        assert!(warning.contains("91 days old"), "{warning}");
        assert!(warning.contains("model_rates"), "{warning}");
        // A clock set before the table was written is not staleness.
        assert_eq!(None, stale_rates_warning(on(-400)));
    }

    fn overrides(json: &str) -> Result<RateOverrides> {
        let config: BTreeMap<String, ModelRateConfig> = serde_json::from_str(json).unwrap();
        RateOverrides::from_config(&config)
    }

    fn error(json: &str) -> String {
        format!("{:#}", overrides(json).unwrap_err())
    }

    #[test]
    fn an_override_beats_the_built_in_table_and_can_add_unknown_models() {
        let rates = overrides(
            r#"{
                "claude-opus-5": {"input": 1, "cache_write": 2, "cache_read": 3, "output": 4},
                "acme-coder": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 9,
                               "family": "codex"}
            }"#,
        )
        .unwrap();
        let opus = rates.resolve("claude-opus-5-20260101").unwrap();
        assert_eq!(RateSource::Override, opus.source);
        assert_eq!(Some("claude-opus-5"), opus.pattern);
        assert_eq!(4.0, opus.rate.output);
        // Not overridden: still the built-in rate.
        let haiku = rates.resolve("claude-haiku-4-5").unwrap();
        assert_eq!(RateSource::BuiltIn, haiku.source);
        assert_eq!(5.0, haiku.rate.output);
        // Unknown to the table, priced and pooled by the override alone.
        assert_eq!(None, rate_for("acme-coder-2"));
        assert_eq!(9.0, rates.resolve("acme-coder-2").unwrap().rate.output);
        assert_eq!(
            Some("openai".to_string()),
            rates.pool_for("pi", "acme-coder-2")
        );
        assert_eq!(None, rates.resolve("llama-4"));
    }

    #[test]
    fn overrides_match_like_the_table_longest_prefix_and_punctuation_blind() {
        let rates = overrides(
            r#"{
                "gpt-5.5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 10},
                "gpt-5.5-pro": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 99}
            }"#,
        )
        .unwrap();
        assert_eq!(99.0, rates.resolve("gpt-5-5-pro").unwrap().rate.output);
        assert_eq!(
            10.0,
            rates.resolve("GPT-5.5-2026-01-01").unwrap().rate.output
        );
        // A shorter override still beats a longer built-in prefix.
        assert_eq!(10.0, rates.resolve("gpt-5.5").unwrap().rate.output);
    }

    #[test]
    fn an_override_without_a_family_keeps_the_built_in_pool() {
        let rates = overrides(
            r#"{"claude-opus-5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 1}}"#,
        )
        .unwrap();
        assert_eq!(
            Some("claude".to_string()),
            rates.pool_for("pi", "claude-opus-5")
        );
        // A client with its own seat still bills separately.
        assert_eq!(
            Some("copilot".to_string()),
            rates.pool_for("copilot", "claude-opus-5")
        );
    }

    #[test]
    fn bad_override_entries_are_refused_naming_the_key() {
        let negative =
            error(r#"{"x-model": {"input": 1, "cache_write": 1, "cache_read": 1, "output": -2}}"#);
        assert!(
            negative.contains("x-model") && negative.contains("output"),
            "{negative}"
        );
        let family = error(
            r#"{"x-model": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 2, "family": "bedrock"}}"#,
        );
        assert!(
            family.contains("x-model") && family.contains("bedrock"),
            "{family}"
        );
        assert!(family.contains("claude, openai, google"), "{family}");
        let missing = error(r#"{"x-model": {"input": 1, "output": 2}}"#);
        assert!(
            missing.contains("x-model") && missing.contains("cache_write"),
            "{missing}"
        );
        let blank =
            error(r#"{"  ": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 2}}"#);
        assert!(blank.contains("model rate key"), "{blank}");
        let duplicate = error(
            r#"{"gpt-5.5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 2},
                "gpt-5-5": {"input": 1, "cache_write": 1, "cache_read": 1, "output": 2}}"#,
        );
        assert!(
            duplicate.contains("gpt-5.5") && duplicate.contains("gpt-5-5"),
            "{duplicate}"
        );
        // A typo'd field is refused by serde rather than silently ignored.
        assert!(
            serde_json::from_str::<BTreeMap<String, ModelRateConfig>>(r#"{"x": {"inptu": 1}}"#)
                .is_err()
        );
    }
}
