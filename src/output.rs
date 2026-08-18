use std::collections::BTreeMap;
use std::io::{self, Write};

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::classify::Category;
use crate::model::{CompositionEntry, Diagnostics, Report, ReportRow};

pub fn print_json(report: &Report) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, report)?;
    writeln!(output)?;
    Ok(())
}

pub fn print_csv(report: &Report) -> Result<()> {
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
        ]
        .into_iter()
        .map(str::to_string),
    );
    for category in Category::ALL {
        for metric in ["files", "additions", "deletions"] {
            fields.push(format!("{}_{metric}", category.as_str()));
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
            println!("Parallel agent work by provider / model  (may overlap)");
            for (provider, seconds) in &summary.provider_seconds {
                println!("  {provider:<24} {:>10}", hours(*seconds));
            }
            let mut models: Vec<_> = summary.model_seconds.iter().collect();
            models.sort_by(|left, right| right.1.total_cmp(left.1));
            for (model, seconds) in models.into_iter().take(12) {
                println!("    {model:<34} {:>10}", hours(*seconds));
            }
            println!();
            if summary.total_tokens != 0 {
                println!("Tokens by provider / model");
                for (provider, tokens) in &summary.provider_tokens {
                    println!("  {provider:<24} {:>10}", compact_tokens(*tokens));
                }
                let mut model_tokens: Vec<_> = summary.model_tokens.iter().collect();
                model_tokens.sort_by(|left, right| right.1.cmp(left.1));
                for (model, tokens) in model_tokens.into_iter().take(12) {
                    println!("    {model:<34} {:>10}", compact_tokens(*tokens));
                }
                println!();
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
        let mut label = label(row, &report.group_by);
        if label.chars().count() > 38 {
            let suffix: String = label
                .chars()
                .rev()
                .take(37)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            label = format!("…{suffix}");
        }
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
        let mut totals: BTreeMap<String, f64> = BTreeMap::new();
        for row in &report.rows {
            if let Some(period) = row.key.get(calendar) {
                *totals.entry(period.clone()).or_default() += row.human_estimated_seconds;
            }
        }
        println!(
            "\n  Human-work trend  {}  (oldest → newest)",
            spark(&totals.into_values().collect::<Vec<_>>())
        );
    }
    println!();
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
        || !Category::ALL
            .into_iter()
            .any(|known| known.as_str() == category)
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

fn label(row: &ReportRow, dimensions: &[String]) -> String {
    dimensions
        .iter()
        .map(|name| {
            let value = row.key.get(name).cloned().unwrap_or_default();
            if name == "month" {
                named_month(&value).unwrap_or(value)
            } else {
                value
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
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

fn neutralize_formula(value: String) -> String {
    if value.starts_with(['=', '+', '-', '@']) {
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
    let source = touched(Category::Source.as_str());
    (source != 0).then(|| touched(Category::Test.as_str()) as f64 / source as f64)
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

    #[test]
    fn spreadsheet_formula_cells_are_neutralized() {
        assert_eq!("'=2+2", neutralize_formula("=2+2".into()));
        assert_eq!("safe", neutralize_formula("safe".into()));
    }

    #[test]
    fn calendar_months_have_readable_labels() {
        assert_eq!(Some("May 2026".into()), named_month("2026-05"));
        assert_eq!(None, named_month("not-a-month"));
    }
}
