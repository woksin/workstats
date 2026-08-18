use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::ops::AddAssign;

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::classify::active_registry;
use crate::model::{CompositionEntry, Diagnostics, MAX_STORED_MESSAGES, Report, ReportRow};

pub fn print_json(report: &Report) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, report)?;
    writeln!(output)?;
    Ok(())
}

/// CSV field names. The per-area columns follow the configured category
/// registry, so a custom category adds columns and a replaced one renames them.
fn csv_fields(report: &Report) -> Vec<String> {
    let mut fields = report.group_by.clone();
    fields.extend(
        [
            "human_estimated_seconds",
            "human_active_days",
            "average_human_seconds_per_active_day",
            "work_block_count",
            "human_signal_count",
            "ai_wall_seconds",
            "parallel_agent_seconds",
            "active_seconds",
            "foreground_session_count",
            "subagent_session_count",
            "session_count",
            "commit_count",
            "file_count",
            "additions",
            "deletions",
            "ignored_additions",
            "ignored_deletions",
            "net_lines",
            // Beside the churn columns they qualify, never inside them: a
            // consumer summing `additions` gets the configured author's own
            // lines, and has to ask for the agent's by name.
            "agent_commit_count",
            "agent_additions",
            "agent_deletions",
            "ai_assisted_commit_count",
            "autofix_assisted_commit_count",
        ]
        .into_iter()
        .map(str::to_string),
    );
    for category in active_registry().names() {
        for metric in ["files", "additions", "deletions"] {
            fields.push(format!("{category}_{metric}"));
        }
    }
    fields.extend(
        [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "total_tokens",
            "active_days",
            "calendar_days",
            "average_active_seconds_per_active_day",
            "average_active_seconds_per_calendar_day",
            "first_seen",
            "last_seen",
        ]
        .into_iter()
        .map(str::to_string),
    );
    fields
}

pub fn print_csv(report: &Report) -> Result<()> {
    let fields = csv_fields(report);
    let mut writer = csv::Writer::from_writer(io::stdout().lock());
    writer.write_record(&fields)?;
    for row in &report.rows {
        let values: Vec<_> = fields
            .iter()
            .map(|field| {
                let value = if let Some(value) = row.key.get(field) {
                    value.clone()
                } else {
                    row_field(row, field)
                };
                neutralize_formula(value)
            })
            .collect();
        writer.write_record(values)?;
    }
    writer.flush()?;
    Ok(())
}

/// Enough diagnostic messages to see what went wrong without burying the
/// report; the rest stay in `--format json`.
const MAX_PRINTED_MESSAGES: usize = 5;

/// Enough of the model list to see what did the work; the rest stay in
/// `--format json`.
const MAX_PRINTED_MODELS: usize = 12;

/// A quoted path is not this tool's own text, so it is clipped before it is
/// printed.
const MAX_MESSAGE_CHARACTERS: usize = 200;

/// One user-facing spelling for "no model named". The pipeline carries three:
/// `unknown` where a transcript never named one, `<synthetic>` where a provider
/// writes its own placeholder, and `—` for a Git commit, which has no model at
/// all. The distinction is internal, so it does not reach the table or `--raw`.
const NO_MODEL: &str = "(no model)";

pub fn print_table(report: &Report, diagnostics: &Diagnostics, top: usize, raw: bool) {
    let summary = &report.summary;
    println!("WORKSTATS  human involvement across local projects");
    println!("{}", "═".repeat(94));
    println!(
        "  Estimated human work  {}",
        hours(summary.human_estimated_seconds)
    );
    println!(
        "  Active work days      {}",
        number(summary.human_active_days)
    );
    println!(
        "  Average / active day  {}",
        hours(summary.average_human_seconds_per_active_day)
    );
    println!(
        "  Work blocks          {}  ({} foreground session edges + {} prompts + {} commits)",
        number(summary.work_block_count),
        number(summary.foreground_session_edge_signal_count),
        number(summary.prompt_signal_count),
        number(summary.commit_signal_count)
    );
    println!("  Git commits             {}", number(summary.commit_count));
    println!(
        "  Git lines               +{} / -{}",
        number(summary.additions),
        number(summary.deletions)
    );
    if summary.ignored_additions != 0 || summary.ignored_deletions != 0 {
        println!(
            "  Ignored Git lines       +{} / -{}",
            number(summary.ignored_additions),
            number(summary.ignored_deletions)
        );
    }
    // Its own line, below the two it is deliberately not part of. The Git
    // figures above are the configured author's, and code an agent pushed is
    // landed output rather than evidence anybody was at the keyboard — so it is
    // shown, and shown as output, in the same breath.
    if summary.agent_commit_count != 0 {
        println!(
            "  Agent-authored          {} commits  +{} / -{}  (output only — no human time)",
            number(summary.agent_commit_count),
            number(summary.agent_additions),
            number(summary.agent_deletions)
        );
    }
    // The opposite case, and it must not read like the line above: these are
    // commits already counted in "Git commits", described by how they were
    // written. "of the N above" is the whole claim — a share, not an addition.
    if summary.ai_assisted_commit_count != 0 || summary.autofix_assisted_commit_count != 0 {
        let autofix = if summary.autofix_assisted_commit_count == 0 {
            String::new()
        } else {
            format!(
                "  (+ {} Copilot Autofix)",
                number(summary.autofix_assisted_commit_count)
            )
        };
        println!(
            "  Co-authored by AI       {} of the {} commits above{autofix}",
            number(summary.ai_assisted_commit_count),
            number(summary.commit_count)
        );
    }
    if let Some(first) = &report.observed.first_seen {
        let last = report.observed.last_seen.as_deref().unwrap_or(first);
        println!(
            "  Observed                {} → {}",
            local_date(first),
            local_date(last)
        );
    }
    println!();

    if summary.session_count != 0 {
        let concurrency = if summary.agent_wall_seconds == 0.0 {
            0.0
        } else {
            summary.parallel_agent_seconds / summary.agent_wall_seconds
        };
        println!("AI activity  (context only — these are not human hours)");
        println!(
            "  Agent wall clock      {}  (any agent active, overlap removed)",
            hours(summary.agent_wall_seconds)
        );
        println!(
            "  Parallel agent work   {}  ({concurrency:.1}× concurrency)",
            hours(summary.parallel_agent_seconds)
        );
        println!(
            "  Sessions              {}  ({} foreground, {} subagents)",
            number(summary.session_count),
            number(summary.foreground_session_count),
            number(summary.subagent_session_count)
        );
        if summary.total_tokens != 0 {
            println!(
                "  Tokens                {}  ({} in, {} out, {} cached)",
                compact_tokens(summary.total_tokens),
                compact_tokens(summary.input_tokens),
                compact_tokens(summary.output_tokens),
                compact_tokens(summary.cache_read_tokens + summary.cache_creation_tokens)
            );
        }
        let comparable_sessions =
            summary.foreground_sessions_with_commits + summary.foreground_sessions_without_commits;
        if comparable_sessions != 0 {
            println!(
                "  Committed output      {} of {} foreground sessions in repos with visible commits",
                number(summary.foreground_sessions_with_commits),
                number(comparable_sessions)
            );
            if summary.foreground_sessions_without_commits != 0 {
                println!(
                    "                        {} left no commit — reading, review, or uncommitted work",
                    number(summary.foreground_sessions_without_commits)
                );
            }
        }
        println!();
        if raw {
            // The model totals are global: the summary never records which
            // provider served a model, so indenting them under the provider
            // list would draw a nesting that does not exist.
            if !summary.provider_seconds.is_empty() {
                println!("Parallel agent work by provider  (may overlap)");
                for (provider, seconds) in &summary.provider_seconds {
                    println!("  {provider:<24} {:>10}", hours(*seconds));
                }
                println!();
            }
            if !summary.model_seconds.is_empty() {
                println!("Parallel agent work by model  (all providers together)");
                let (models, omitted) = ranked_models(&summary.model_seconds, f64::total_cmp);
                for (model, seconds) in models {
                    println!("  {model:<34} {:>10}", hours(seconds));
                }
                print_omitted_models(omitted);
                println!();
            }
            if summary.total_tokens != 0 {
                if !summary.provider_tokens.is_empty() {
                    println!("Tokens by provider");
                    for (provider, tokens) in &summary.provider_tokens {
                        println!("  {provider:<24} {:>10}", compact_tokens(*tokens));
                    }
                    println!();
                }
                if !summary.model_tokens.is_empty() {
                    println!("Tokens by model  (all providers together)");
                    let (model_tokens, omitted) = ranked_models(&summary.model_tokens, u64::cmp);
                    for (model, tokens) in model_tokens {
                        println!("  {model:<34} {:>10}", compact_tokens(tokens));
                    }
                    print_omitted_models(omitted);
                    println!();
                }
            }
        }
    }

    if !summary.composition.is_empty() {
        println!("Work composition  (changed Git lines by file area)");
        println!(
            "  {:<10} {:>7} {:>11} {:>11} {:>7}",
            "Area", "Files", "Added", "Removed", "Share"
        );
        println!("  {}", "─".repeat(50));
        for entry in &summary.composition {
            println!(
                "  {:<10} {:>7} {:>11} {:>11} {:>7}",
                entry.category,
                number(entry.files),
                format!("+{}", number(entry.additions)),
                format!("-{}", number(entry.deletions)),
                percent(entry.share_of_changed_lines)
            );
        }
        if let Some(ratio) = test_to_source_ratio(&summary.composition) {
            println!("  Test lines per source line  {ratio:.2}");
        }
        println!();
    }

    if !summary.change_shapes.is_empty() {
        println!("Change shapes  (from diff composition only — commit messages are never read)");
        println!("  {:<10} {:>9} {:>7}", "Shape", "Commits", "Share");
        println!("  {}", "─".repeat(28));
        for entry in &summary.change_shapes {
            println!(
                "  {:<10} {:>9} {:>7}",
                entry.shape,
                number(entry.commits),
                percent(entry.share_of_classified_commits)
            );
        }
        println!();
    }

    let show_tokens = report.rows.iter().any(|row| row.total_tokens != 0);
    let title = report.group_by.join(" × ");
    println!("By {title}  (human involvement first; AI wall clock shown as context)");
    if show_tokens {
        println!(
            "  {:<38} {:>9} {:>5} {:>9} {:>8} {:>9} {:>10} {:>9}",
            "Work area", "Human", "Days", "Avg/day", "Commits", "AI wall", "Agent work", "Tokens"
        );
        println!("  {}", "─".repeat(106));
    } else {
        println!(
            "  {:<38} {:>9} {:>5} {:>9} {:>8} {:>9} {:>10}",
            "Work area", "Human", "Days", "Avg/day", "Commits", "AI wall", "Agent work"
        );
        println!("  {}", "─".repeat(96));
    }
    let rows = if top == 0 {
        &report.rows[..]
    } else {
        &report.rows[..report.rows.len().min(top)]
    };
    for row in rows {
        let label = clipped_label(row, &report.group_by);
        if show_tokens {
            println!(
                "  {label:<38} {:>9} {:>5} {:>9} {:>8} {:>9} {:>10} {:>9}",
                hours(row.human_estimated_seconds),
                number(row.human_active_days),
                hours(row.average_human_seconds_per_active_day),
                number(row.commit_count),
                hours(row.ai_wall_seconds),
                hours(row.parallel_agent_seconds),
                compact_tokens(row.total_tokens),
            );
        } else {
            println!(
                "  {label:<38} {:>9} {:>5} {:>9} {:>8} {:>9} {:>10}",
                hours(row.human_estimated_seconds),
                number(row.human_active_days),
                hours(row.average_human_seconds_per_active_day),
                number(row.commit_count),
                hours(row.ai_wall_seconds),
                hours(row.parallel_agent_seconds),
            );
        }
    }
    if top != 0 && report.rows.len() > top {
        println!("  … {} more rows; use --top 0", report.rows.len() - top);
    }
    if let Some(calendar) = ["day", "month"]
        .into_iter()
        .find(|name| report.group_by.iter().any(|dimension| dimension == name))
        && !rows.is_empty()
    {
        // The bars cover exactly the rows above them. Drawing the hidden rows
        // too gave `--top 1` a chart whose tallest bar belonged to a row the
        // reader could not see.
        let scope = if rows.len() < report.rows.len() {
            format!("the {} rows above only, oldest → newest", rows.len())
        } else {
            "oldest → newest".to_string()
        };
        println!(
            "\n  Human-work trend  {}  ({scope}; the table lists rows newest first)",
            spark(&trend_totals(rows, calendar))
        );
    }
    println!();

    // A table of its own rather than two more columns above. Every column in
    // that table is either human involvement or agent *runtime*, and a commit
    // count set among them — next to a column already called "Agent work",
    // which is hours — would be read as work somebody did. Here the heading
    // carries the claim, once, in the place the numbers are.
    let agent_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.agent_commit_count != 0)
        .collect();
    if !agent_rows.is_empty() {
        println!(
            "Agent-authored Git output  (landed code you did not type — no human time, no work blocks)"
        );
        println!(
            "  {:<38} {:>9} {:>11} {:>11}",
            "Work area", "Commits", "Added", "Removed"
        );
        println!("  {}", "─".repeat(72));
        for row in &agent_rows {
            println!(
                "  {:<38} {:>9} {:>11} {:>11}",
                clipped_label(row, &report.group_by),
                number(row.agent_commit_count),
                format!("+{}", number(row.agent_additions)),
                format!("-{}", number(row.agent_deletions))
            );
        }
        // `--top` can hide a row that is *only* agent output — with no human
        // time it sorts last — so the section says what it left out rather than
        // quietly disagreeing with the total at the top of the report.
        let shown: usize = agent_rows.iter().map(|row| row.agent_commit_count).sum();
        if let Some(hidden) = summary
            .agent_commit_count
            .checked_sub(shown)
            .filter(|hidden| *hidden != 0)
        {
            println!("  … {hidden} more in rows not shown; use --top 0");
        }
        println!();
    }
    println!(
        "Human estimate: prompts + foreground session edges + commits; {}m idle ends a block; each block receives {}m setup/review credit.",
        compact_number(report.methodology.human_idle_threshold_seconds / 60.0),
        compact_number(report.methodology.review_credit_seconds / 60.0)
    );
    println!(
        "Session edges add bounded setup/review time; autonomous AI output does not imply continuous human presence."
    );
    println!(
        "AI wall removes overlap within each row; rows can overlap each other. Agent work sums parallel sessions."
    );
    // Printed whenever the run asked for either pass, even if neither found
    // anything: a reader who turned the flag on needs to know what the report
    // would have done with a match, and a zero is an answer to that question.
    if summary.agent_commit_count != 0
        || summary.ai_assisted_commit_count != 0
        || summary.autofix_assisted_commit_count != 0
        || !report.inputs.agent_authors.is_empty()
        || report.inputs.co_authors
    {
        println!(
            "Agent-authored commits are output, never attendance: they add no human time, no work blocks, and no active work days. A Co-authored-by trailer describes a commit you already wrote — it never adds one."
        );
    }
    if report.inputs.repo_filter.is_some() || report.inputs.repo_exact_filter.is_some() {
        println!(
            "Scope note: work blocks are recomputed from the selected repositories, so filtered totals can differ from an all-repo row."
        );
    }
    println!(
        "Local retained history only. Missing/pruned transcripts and work on other machines are not visible."
    );
    if diagnostics.malformed_lines != 0
        || diagnostics.unreadable_files != 0
        || diagnostics.git_errors != 0
        || diagnostics.approximate_cwds != 0
    {
        println!(
            "Diagnostics: {} malformed lines, {} unreadable files, {} Git errors, {} approximate working directories.",
            diagnostics.malformed_lines,
            diagnostics.unreadable_files,
            diagnostics.git_errors,
            diagnostics.approximate_cwds
        );
    }
    if diagnostics.content_rejections != 0 {
        println!(
            "Privacy: {} record(s) carrying prompt or response text were skipped, as designed.",
            diagnostics.content_rejections
        );
    }
    // Without these a mistyped --history or --events path produces a clean
    // looking report with silently missing data.
    for message in diagnostics.messages.iter().take(MAX_PRINTED_MESSAGES) {
        println!("Warning: {}", safe_message(message));
    }
    if let Some(note) = hidden_messages_note(diagnostics.warning_count) {
        println!("{note}");
    }
}

/// The footer for the warnings that were not printed. `raised` counts every
/// warning, including the ones `Diagnostics` stopped storing, so the number is
/// exact; past the storage cap it is `--format json` that cannot show them all,
/// and the note says which ones it does carry.
fn hidden_messages_note(raised: u64) -> Option<String> {
    let hidden = raised
        .checked_sub(MAX_PRINTED_MESSAGES as u64)
        .filter(|hidden| *hidden != 0)?;
    Some(if raised > MAX_STORED_MESSAGES as u64 {
        format!(
            "Warning: … {hidden} more; only the first {MAX_STORED_MESSAGES} warnings were kept, and --format json carries those."
        )
    } else {
        format!("Warning: … {hidden} more; use --format json for all of them.")
    })
}

/// Human seconds per calendar period, oldest first, from the rows the table
/// printed rather than from every row in the report.
fn trend_totals(rows: &[ReportRow], calendar: &str) -> Vec<f64> {
    let mut totals: BTreeMap<&str, f64> = BTreeMap::new();
    for row in rows {
        if let Some(period) = row.key.get(calendar) {
            *totals.entry(period.as_str()).or_default() += row.human_estimated_seconds;
        }
    }
    totals.into_values().collect()
}

/// The largest models first, plus how many the cap left out so the list can say
/// what it hid. Models that render to the same name are summed, because the
/// spellings they came from are an internal distinction.
fn ranked_models<T>(
    totals: &BTreeMap<String, T>,
    compare: impl Fn(&T, &T) -> Ordering,
) -> (Vec<(String, T)>, usize)
where
    T: Copy + Default + AddAssign,
{
    let mut merged: BTreeMap<String, T> = BTreeMap::new();
    for (model, total) in totals {
        *merged.entry(display_model(model)).or_default() += *total;
    }
    let mut ranked: Vec<_> = merged.into_iter().collect();
    ranked.sort_by(|left, right| compare(&right.1, &left.1).then_with(|| left.0.cmp(&right.0)));
    let omitted = ranked.len().saturating_sub(MAX_PRINTED_MODELS);
    ranked.truncate(MAX_PRINTED_MODELS);
    (ranked, omitted)
}

fn print_omitted_models(omitted: usize) {
    if omitted != 0 {
        println!("  … {omitted} more models; use --format json for all of them.");
    }
}

/// A message quotes a path the tool did not choose, so it can carry control
/// characters — and characters that reorder the line around them — into a
/// terminal.
fn safe_message(value: &str) -> String {
    let mut safe: String = value
        .chars()
        .take(MAX_MESSAGE_CHARACTERS)
        .map(|character| {
            if character.is_control() || is_direction_override(character) {
                '·'
            } else {
                character
            }
        })
        .collect();
    if value.chars().nth(MAX_MESSAGE_CHARACTERS).is_some() {
        safe.push('…');
    }
    safe
}

/// The bidirectional embeddings and overrides (U+202A–202E) and isolates
/// (U+2066–2069), shared with the diff viewer. Unicode does not classify them
/// as control characters, so `char::is_control` lets them through, yet each
/// opens a directional scope that runs until its terminator or the end of the
/// line — which is what lets a quoted path or a line of a file read as
/// something it is not. That scope is the whole of Trojan Source
/// (CVE-2021-42574). Anything here that reaches a terminal is replaced rather
/// than printed.
///
/// LRM (U+200E) and RLM (U+200F) are deliberately **not** here, and must not be
/// added. UAX #9 calls them implicit directional marks: invisible characters
/// with a strong bidi class that never touch the directional status stack of
/// rules X1–X8, so they cannot open a scope and cannot reverse a letter. They
/// only tilt how the neutrals immediately beside them resolve — precisely what
/// any visible Hebrew or Arabic letter already does, so excluding them forfeits
/// no protection. What they are is ordinary content in Hebrew and Arabic file
/// and directory names, where replacing them corrupts a real path, and via
/// `safe_value` corrupts it in JSON and CSV too. rustc's own
/// `text_direction_codepoint_in_literal` draws the line in the same place.
pub fn is_direction_override(character: char) -> bool {
    matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn row_field(row: &ReportRow, name: &str) -> String {
    match name {
        "human_estimated_seconds" => value(row.human_estimated_seconds),
        "human_active_days" => row.human_active_days.to_string(),
        "average_human_seconds_per_active_day" => value(row.average_human_seconds_per_active_day),
        "work_block_count" => row.work_block_count.to_string(),
        "human_signal_count" => row.human_signal_count.to_string(),
        "ai_wall_seconds" => value(row.ai_wall_seconds),
        "parallel_agent_seconds" => value(row.parallel_agent_seconds),
        "active_seconds" => value(row.active_seconds),
        "foreground_session_count" => row.foreground_session_count.to_string(),
        "subagent_session_count" => row.subagent_session_count.to_string(),
        "session_count" => row.session_count.to_string(),
        "commit_count" => row.commit_count.to_string(),
        "file_count" => row.file_count.to_string(),
        "additions" => row.additions.to_string(),
        "deletions" => row.deletions.to_string(),
        "ignored_additions" => row.ignored_additions.to_string(),
        "ignored_deletions" => row.ignored_deletions.to_string(),
        "net_lines" => row.net_lines.to_string(),
        "agent_commit_count" => row.agent_commit_count.to_string(),
        "agent_additions" => row.agent_additions.to_string(),
        "agent_deletions" => row.agent_deletions.to_string(),
        "ai_assisted_commit_count" => row.ai_assisted_commit_count.to_string(),
        "autofix_assisted_commit_count" => row.autofix_assisted_commit_count.to_string(),
        "input_tokens" => row.input_tokens.to_string(),
        "output_tokens" => row.output_tokens.to_string(),
        "cache_read_tokens" => row.cache_read_tokens.to_string(),
        "cache_creation_tokens" => row.cache_creation_tokens.to_string(),
        "total_tokens" => row.total_tokens.to_string(),
        "active_days" => row.active_days.to_string(),
        "calendar_days" => row.calendar_days.to_string(),
        "average_active_seconds_per_active_day" => value(row.average_active_seconds_per_active_day),
        "average_active_seconds_per_calendar_day" => {
            value(row.average_active_seconds_per_calendar_day)
        }
        "first_seen" => row.first_seen.clone().unwrap_or_default(),
        "last_seen" => row.last_seen.clone().unwrap_or_default(),
        other => composition_field(row, other).unwrap_or_default(),
    }
}

/// Resolves the `{category}_{files,additions,deletions}` columns. A category
/// the row never touched reports zero rather than an empty cell, so the
/// numeric columns stay numeric for every row.
fn composition_field(row: &ReportRow, name: &str) -> Option<String> {
    let (category, metric) = name.rsplit_once('_')?;
    if !matches!(metric, "files" | "additions" | "deletions")
        || active_registry().index_of(category).is_none()
    {
        return None;
    }
    let Some(entry) = row
        .composition
        .iter()
        .find(|entry| entry.category == category)
    else {
        return Some("0".to_string());
    };
    Some(match metric {
        "files" => entry.files.to_string(),
        "additions" => entry.additions.to_string(),
        _ => entry.deletions.to_string(),
    })
}

/// The first column of every row table.
const LABEL_WIDTH: usize = 38;

/// A row's label, clipped to `LABEL_WIDTH` from the left. The tail is what is
/// kept because the distinguishing part of a long path is its end.
fn clipped_label(row: &ReportRow, dimensions: &[String]) -> String {
    let label = label(row, dimensions);
    if label.chars().count() <= LABEL_WIDTH {
        return label;
    }
    let suffix: String = label
        .chars()
        .rev()
        .take(LABEL_WIDTH - 1)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{suffix}")
}

fn label(row: &ReportRow, dimensions: &[String]) -> String {
    dimensions
        .iter()
        .map(|name| {
            let value = row.key.get(name).cloned().unwrap_or_default();
            match name.as_str() {
                "month" => named_month(&value).unwrap_or(value),
                "model" => display_model(&value),
                _ => value,
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `--format json` and `--format csv` keep the raw spelling, which is a
/// consumer contract; only what a person reads is normalised.
fn display_model(value: &str) -> String {
    if matches!(value, "" | "unknown" | "<synthetic>" | "—") {
        NO_MODEL.to_string()
    } else {
        value.to_string()
    }
}

fn named_month(value: &str) -> Option<String> {
    let (year, month) = value.split_once('-')?;
    let month: usize = month.parse().ok()?;
    let names = [
        "",
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    names.get(month).map(|name| format!("{name} {year}"))
}

/// Spreadsheets read a leading `= + - @` as a formula. A negative number is not
/// a formula though, and this output is made for pipes, so a cell that parses
/// as a number is left exactly as it is.
fn neutralize_formula(value: String) -> String {
    if value.starts_with(['=', '+', '-', '@']) && value.parse::<f64>().is_err() {
        format!("'{value}")
    } else {
        value
    }
}

fn local_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Local).date_naive().to_string())
        .unwrap_or_else(|_| value.to_string())
}

fn hours(seconds: f64) -> String {
    let rounded = seconds.round().max(0.0) as u64;
    format!("{}h {:02}m", rounded / 3600, rounded % 3600 / 60)
}

/// A present-but-tiny share reads as `<1%` rather than rounding away to `0%`.
fn percent(share: f64) -> String {
    if share > 0.0 && share < 0.005 {
        "<1%".to_string()
    } else {
        format!("{:.0}%", share * 100.0)
    }
}

/// Changed test lines for every changed source line. `None` when no source
/// line changed, because the ratio is undefined rather than zero there.
fn test_to_source_ratio(composition: &[CompositionEntry]) -> Option<f64> {
    let touched = |name: &str| {
        composition
            .iter()
            .find(|entry| entry.category == name)
            .map_or(0, |entry| entry.additions + entry.deletions)
    };
    let source = touched("source");
    (source != 0).then(|| touched("test") as f64 / source as f64)
}

fn compact_tokens(value: u64) -> String {
    let value = value as f64;
    if value < 1000.0 {
        format!("{value:.0}")
    } else if value < 1_000_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value < 1_000_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else {
        format!("{:.1}B", value / 1_000_000_000.0)
    }
}

fn number(value: impl ToString) -> String {
    let value = value.to_string();
    let mut output = String::new();
    for (index, character) in value.chars().rev().enumerate() {
        if index != 0 && index % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }
    output.chars().rev().collect()
}

fn value(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

fn compact_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn spark(values: &[f64]) -> String {
    let glyphs = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let maximum = values.iter().copied().fold(0.0_f64, f64::max);
    values
        .iter()
        .map(|value| {
            if maximum == 0.0 {
                glyphs[0]
            } else {
                glyphs[((value / maximum * 7.0) as usize).min(7)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Inputs, Methodology, Observed, Summary};

    #[test]
    fn spreadsheet_formula_cells_are_neutralized() {
        assert_eq!("'=2+2", neutralize_formula("=2+2".into()));
        assert_eq!("safe", neutralize_formula("safe".into()));
        assert_eq!("'@sum", neutralize_formula("@sum".into()));
        assert_eq!("'-lookup", neutralize_formula("-lookup".into()));
    }

    #[test]
    fn negative_numbers_stay_numbers() {
        // `net_lines` is routinely negative and the CSV is made for pipes.
        assert_eq!("-1", neutralize_formula("-1".into()));
        assert_eq!("-1234.5", neutralize_formula("-1234.5".into()));
        assert_eq!("+42", neutralize_formula("+42".into()));
        assert_eq!("-1e3", neutralize_formula("-1e3".into()));
    }

    #[test]
    fn csv_columns_follow_the_category_registry() {
        let fields = csv_fields(&report_with(vec!["repo".to_string()]));
        let registry = active_registry();
        for category in registry.names() {
            assert!(
                fields.contains(&format!("{category}_files")),
                "missing {category}_files in {fields:?}"
            );
        }
        // Group-by columns come first and the areas keep registry order.
        assert_eq!("repo", fields[0]);
        let position = |name: &str| fields.iter().position(|field| field == name);
        assert!(position("test_files") < position("source_files"));
        assert!(position("net_lines") < position("test_files"));
    }

    /// The split the feature exists for, at the machine-readable boundary. A
    /// consumer summing `commit_count`, `additions` or `deletions` gets the
    /// configured author's own work and nothing else; the agent's output has to
    /// be asked for by name. Nothing here can be totalled into human time,
    /// because none of these columns is time.
    #[test]
    fn the_csv_reports_agent_output_beside_your_own_and_never_inside_it() {
        let fields = csv_fields(&report_with(vec!["repo".to_string()]));
        let position = |name: &str| {
            fields
                .iter()
                .position(|field| field == name)
                .unwrap_or_else(|| panic!("missing column {name} in {fields:?}"))
        };
        // Beside the churn columns they qualify, and still ahead of the
        // registry-driven area columns.
        assert!(position("net_lines") < position("agent_commit_count"));
        assert!(position("autofix_assisted_commit_count") < position("test_files"));

        let mut row = row_with(Vec::new());
        row.commit_count = 3;
        row.additions = 30;
        row.deletions = 4;
        row.agent_commit_count = 7;
        row.agent_additions = 700;
        row.agent_deletions = 90;
        row.ai_assisted_commit_count = 2;
        row.autofix_assisted_commit_count = 1;
        assert_eq!("3", row_field(&row, "commit_count"));
        assert_eq!("30", row_field(&row, "additions"));
        assert_eq!("4", row_field(&row, "deletions"));
        assert_eq!("7", row_field(&row, "agent_commit_count"));
        assert_eq!("700", row_field(&row, "agent_additions"));
        assert_eq!("90", row_field(&row, "agent_deletions"));
        assert_eq!("2", row_field(&row, "ai_assisted_commit_count"));
        assert_eq!("1", row_field(&row, "autofix_assisted_commit_count"));
        // Seven agent commits carrying 790 changed lines, and not one second of
        // human work follows from them.
        assert_eq!("0.0", row_field(&row, "human_estimated_seconds"));
        assert_eq!("0", row_field(&row, "work_block_count"));
        assert_eq!("0", row_field(&row, "human_active_days"));
    }

    /// The tail is what identifies a long path, so it is the end that survives.
    #[test]
    fn a_row_label_is_clipped_from_the_left() {
        let dimensions = vec!["repo".to_string()];
        let labelled = |value: &str| {
            let mut row = row_with(Vec::new());
            row.key.insert("repo".to_string(), value.to_string());
            clipped_label(&row, &dimensions)
        };
        assert_eq!("widget", labelled("widget"));
        let long = format!("{}/widget", "deep/".repeat(20));
        let clipped = labelled(&long);
        assert_eq!(LABEL_WIDTH, clipped.chars().count());
        assert!(clipped.starts_with('…'), "{clipped}");
        assert!(clipped.ends_with("/widget"), "{clipped}");
    }

    #[test]
    fn composition_columns_report_zero_for_an_untouched_area() {
        let row = row_with(Vec::new());
        assert_eq!(
            Some("0".to_string()),
            composition_field(&row, "assets_files")
        );
        assert_eq!(None, composition_field(&row, "assets_pixels"));
        assert_eq!(None, composition_field(&row, "nonsense_files"));
    }

    fn row_with(composition: Vec<CompositionEntry>) -> ReportRow {
        ReportRow {
            key: BTreeMap::new(),
            active_seconds: 0.0,
            parallel_agent_seconds: 0.0,
            ai_wall_seconds: 0.0,
            human_estimated_seconds: 0.0,
            human_signal_count: 0,
            work_block_count: 0,
            session_count: 0,
            commit_count: 0,
            foreground_session_count: 0,
            subagent_session_count: 0,
            file_count: 0,
            additions: 0,
            deletions: 0,
            ignored_additions: 0,
            ignored_deletions: 0,
            net_lines: 0,
            agent_commit_count: 0,
            agent_additions: 0,
            agent_deletions: 0,
            ai_assisted_commit_count: 0,
            autofix_assisted_commit_count: 0,
            composition,
            change_shapes: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_tokens: 0,
            active_days: 0,
            human_active_days: 0,
            calendar_days: 0,
            average_human_seconds_per_active_day: 0.0,
            average_active_seconds_per_active_day: 0.0,
            average_active_seconds_per_calendar_day: 0.0,
            first_seen: None,
            last_seen: None,
            providers: Vec::new(),
            models: Vec::new(),
        }
    }

    fn report_with(group_by: Vec<String>) -> Report {
        Report {
            methodology: Methodology {
                human_work: "",
                human_idle_threshold_seconds: 0.0,
                review_credit_seconds: 0.0,
                human_estimate_caveat: "",
                ai_time: "",
                deduplication: "",
                gap_cap_seconds: 0.0,
                composition: String::new(),
                change_shapes: "",
                agent_output: "",
                scope: "",
            },
            observed: Observed {
                first_seen: None,
                last_seen: None,
            },
            summary: Summary {
                human_estimated_seconds: 0.0,
                human_active_days: 0,
                average_human_seconds_per_active_day: 0.0,
                work_block_count: 0,
                human_signal_count: 0,
                prompt_signal_count: 0,
                foreground_session_edge_signal_count: 0,
                commit_signal_count: 0,
                deduplicated_active_seconds: 0.0,
                attributed_active_seconds: 0.0,
                agent_wall_seconds: 0.0,
                parallel_agent_seconds: 0.0,
                session_count: 0,
                foreground_session_count: 0,
                subagent_session_count: 0,
                foreground_sessions_with_commits: 0,
                foreground_sessions_without_commits: 0,
                commit_count: 0,
                additions: 0,
                deletions: 0,
                ignored_additions: 0,
                ignored_deletions: 0,
                agent_commit_count: 0,
                agent_additions: 0,
                agent_deletions: 0,
                ai_assisted_commit_count: 0,
                autofix_assisted_commit_count: 0,
                composition: Vec::new(),
                change_shapes: Vec::new(),
                active_days: 0,
                provider_seconds: BTreeMap::new(),
                model_seconds: BTreeMap::new(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                total_tokens: 0,
                provider_tokens: BTreeMap::new(),
                model_tokens: BTreeMap::new(),
            },
            group_by,
            rows: Vec::new(),
            diagnostics: Diagnostics::default(),
            inputs: Inputs {
                git_root: String::new(),
                git_scan_roots: Vec::new(),
                history_sources: BTreeMap::new(),
                included_providers: Vec::new(),
                excluded_providers: Vec::new(),
                author: String::new(),
                agent_authors: Vec::new(),
                co_authors: false,
                repo_filter: None,
                repo_exact_filter: None,
                human_idle: String::new(),
                review_credit: String::new(),
                cache: None,
            },
        }
    }

    #[test]
    fn calendar_months_have_readable_labels() {
        assert_eq!(Some("May 2026".into()), named_month("2026-05"));
        assert_eq!(None, named_month("not-a-month"));
    }

    #[test]
    fn the_model_list_says_how_many_models_it_left_out() {
        let totals: BTreeMap<String, f64> = (0..15)
            .map(|index| (format!("model-{index:02}"), index as f64))
            .collect();
        let (ranked, omitted) = ranked_models(&totals, f64::total_cmp);
        assert_eq!(MAX_PRINTED_MODELS, ranked.len());
        assert_eq!(3, omitted);
        assert_eq!("model-14", ranked[0].0);
        assert_eq!("model-03", ranked[MAX_PRINTED_MODELS - 1].0);
    }

    #[test]
    fn a_short_model_list_is_not_reported_as_truncated() {
        let totals: BTreeMap<String, u64> =
            BTreeMap::from([("gpt".to_string(), 5), ("sonnet".to_string(), 9)]);
        let (ranked, omitted) = ranked_models(&totals, u64::cmp);
        assert_eq!(0, omitted);
        assert_eq!(
            vec![("sonnet".to_string(), 9), ("gpt".to_string(), 5)],
            ranked
        );
    }

    #[test]
    fn the_internal_no_model_spellings_render_as_one_entry() {
        let totals: BTreeMap<String, f64> = BTreeMap::from([
            ("unknown".to_string(), 10.0),
            ("<synthetic>".to_string(), 5.0),
            ("—".to_string(), 1.0),
            ("sonnet".to_string(), 4.0),
        ]);
        let (ranked, _) = ranked_models(&totals, f64::total_cmp);
        assert_eq!(
            vec![(NO_MODEL.to_string(), 16.0), ("sonnet".to_string(), 4.0)],
            ranked
        );
    }

    #[test]
    fn model_labels_use_one_spelling_for_no_model() {
        let dimensions = vec!["model".to_string()];
        for spelling in ["unknown", "<synthetic>", "—", ""] {
            let mut row = row_with(Vec::new());
            row.key.insert("model".to_string(), spelling.to_string());
            assert_eq!(NO_MODEL, label(&row, &dimensions), "for {spelling:?}");
        }
        let mut row = row_with(Vec::new());
        row.key
            .insert("model".to_string(), "claude-opus-4".to_string());
        assert_eq!("claude-opus-4", label(&row, &dimensions));
    }

    #[test]
    fn the_hidden_warning_count_stays_exact_past_the_storage_cap() {
        assert_eq!(None, hidden_messages_note(0));
        assert_eq!(None, hidden_messages_note(MAX_PRINTED_MESSAGES as u64));
        let few = hidden_messages_note(MAX_PRINTED_MESSAGES as u64 + 2).unwrap();
        assert!(few.contains("… 2 more"), "{few}");
        assert!(!few.contains("were kept"), "{few}");
        // 130 warnings with 100 of them stored: the count is the number that
        // happened, and only the reach of --format json is qualified.
        let capped = hidden_messages_note(130).unwrap();
        assert!(capped.contains("… 125 more"), "{capped}");
        assert!(capped.contains("first 100 warnings were kept"), "{capped}");
    }

    #[test]
    fn a_warning_cannot_reorder_itself() {
        // U+202E would print the rest of the line right-to-left.
        assert_eq!("path·to·file", safe_message("path\u{202e}to\u{2066}file"));
        assert_eq!("a·b", safe_message("a\tb"));
        // U+200F opens no directional scope and is ordinary content in an RTL
        // path, so a Hebrew directory a warning quotes survives verbatim.
        // Escaped rather than written literally so that this file carries no
        // invisible characters of its own.
        let hebrew = "~/\u{5de}\u{5e1}\u{5de}\u{5db}\u{5d9}\u{5dd}\u{200f}/log";
        assert_eq!(hebrew, safe_message(hebrew));
        let long = "x".repeat(MAX_MESSAGE_CHARACTERS + 10);
        let clipped = safe_message(&long);
        assert_eq!(MAX_MESSAGE_CHARACTERS + 1, clipped.chars().count());
        assert!(clipped.ends_with('…'));
        assert!(!safe_message("x").ends_with('…'));
    }

    #[test]
    fn the_trend_covers_only_the_rows_the_table_printed() {
        let rows = [
            calendar_row("2026-05", 60.0),
            calendar_row("2026-04", 30.0),
            calendar_row("2026-04", 10.0),
            calendar_row("2026-03", 3600.0),
        ];
        // Oldest first, and the two April rows are one bar.
        assert_eq!(vec![3600.0, 40.0, 60.0], trend_totals(&rows, "month"));
        // `--top 2` hides the tall March row, so its bar goes too.
        assert_eq!(vec![30.0, 60.0], trend_totals(&rows[..2], "month"));
        assert_eq!("▄█", spark(&trend_totals(&rows[..2], "month")));
    }

    fn calendar_row(month: &str, human_estimated_seconds: f64) -> ReportRow {
        let mut row = row_with(Vec::new());
        row.key.insert("month".to_string(), month.to_string());
        row.human_estimated_seconds = human_estimated_seconds;
        row
    }
}
