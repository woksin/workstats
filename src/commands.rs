//! The small subcommands: `sources`, `classify`, `record` and `update`.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;

use crate::cli::{
    ClassifyArguments, EventKind, EventRole, OutputFormat, RecordArguments, SourcesArguments,
    UpdateArguments, valid_provider_identifier,
};
use crate::model::Diagnostics;
use crate::paths::{default_update_check_path, load_config};
use crate::sources::{default_events_path, normalize_provider, source_inventory};
use crate::timeutil::parse_timestamp;
use crate::update;

/// One path, the category it lands in, and why.
#[derive(Serialize)]
struct ClassifiedPath {
    path: String,
    category: String,
    rule: &'static str,
    pattern: String,
}

#[derive(Serialize)]
struct RecordedEvent {
    timestamp: String,
    provider: String,
    session_id: String,
    cwd: String,
    model: String,
    event: EventKind,
    role: EventRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
}

pub(crate) fn run_update_command(arguments: &UpdateArguments) -> Result<()> {
    let cache_path = default_update_check_path();
    println!("Current version: workstats {}", update::current_version());
    if arguments.check {
        let outcome = update::check_now(&cache_path)?;
        if outcome.available {
            println!(
                "A new version is available: workstats {}  (run `workstats update` to install it)",
                outcome.latest
            );
        } else {
            println!("workstats is up to date.");
        }
        return Ok(());
    }
    let outcome = update::install_latest(&cache_path)?;
    if outcome.available {
        println!(
            "Updated workstats {} → {}. Restart to use the new version.",
            outcome.current, outcome.latest
        );
    } else {
        println!("workstats is already up to date.");
    }
    Ok(())
}

pub(crate) fn print_sources(arguments: &SourcesArguments) -> Result<()> {
    let inventory = source_inventory();
    match arguments.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&inventory)?),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(io::stdout());
            for item in inventory {
                writer.serialize(item)?;
            }
            writer.flush()?;
        }
        OutputFormat::Markdown | OutputFormat::Html => {
            bail!(
                "--format {} is not available for `workstats sources`; use table, json, or csv",
                arguments.output_format.name()
            )
        }
        OutputFormat::Table => {
            println!("AI HISTORY SOURCES\n");
            // Widths hold the longest value each column can carry today —
            // `copilot-vscode` and `GitHub Copilot Chat (VS Code)` are the ones
            // setting them — so no row pushes the columns after it out of line.
            println!(
                "{:<4} {:<15} {:<30} {:<24} {:<12} PATH",
                "", "ID", "SOURCE", "FORMAT", "SUPPORT"
            );
            for item in inventory {
                println!(
                    "{:<4} {:<15} {:<30} {:<24} {:<12} {}",
                    if item.detected { "●" } else { "○" },
                    item.id,
                    item.name,
                    item.format,
                    item.support,
                    item.path
                );
            }
            println!(
                "\n● detected  ○ not found  · add any other tool with `workstats record` or `--events`"
            );
        }
    }
    Ok(())
}

/// Answers "why did this file land there?" against the configured registry,
/// which is the only way to debug a category rule without running a report.
pub(crate) fn classify_paths(
    arguments: &ClassifyArguments,
    fallback_config: Option<&Path>,
) -> Result<()> {
    let mut diagnostics = Diagnostics::default();
    let config = load_config(
        arguments.config.as_deref().or(fallback_config),
        &mut diagnostics,
    );
    let registry = config.category_registry()?;
    for message in &diagnostics.messages {
        eprintln!("workstats: {message}");
    }
    let classified: Vec<ClassifiedPath> = arguments
        .paths
        .iter()
        .map(|path| {
            let matched = registry.explain(path);
            ClassifiedPath {
                path: path.clone(),
                category: registry.name(matched.category).to_string(),
                rule: matched.rule.as_str(),
                pattern: matched.pattern,
            }
        })
        .collect();
    match arguments.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&classified)?),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(io::stdout());
            for item in &classified {
                writer.serialize(item)?;
            }
            writer.flush()?;
        }
        OutputFormat::Markdown | OutputFormat::Html => {
            bail!(
                "--format {} is not available for `workstats classify`; use table, json, or csv",
                arguments.output_format.name()
            )
        }
        OutputFormat::Table => {
            println!("{:<52} {:<10} {:<18} MATCHED", "PATH", "CATEGORY", "RULE");
            for item in &classified {
                let pattern = if item.pattern.is_empty() {
                    "—"
                } else {
                    item.pattern.as_str()
                };
                println!(
                    "{:<52} {:<10} {:<18} {pattern}",
                    item.path, item.category, item.rule
                );
            }
            println!(
                "\nCategories in match order: {}",
                registry.names().collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(())
}

pub(crate) fn record_event(arguments: &RecordArguments) -> Result<()> {
    let provider = normalize_provider(&arguments.provider);
    if !valid_provider_identifier(&provider, false) {
        bail!("--provider must be a short identifier using letters, numbers, '.', '/', or '-'");
    }
    if arguments.session.trim().is_empty()
        || arguments.session.len() > 256
        || arguments.session.chars().any(char::is_control)
    {
        bail!("--session must be a non-empty identifier of at most 256 bytes");
    }
    if let Some(model) = arguments.model.as_deref()
        && (!model.is_ascii()
            || model.is_empty()
            || model.len() > 128
            || !model.as_bytes()[0].is_ascii_alphanumeric()
            || !model.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b':' | b'/' | b'+' | b'<' | b'>' | b'-')
            }))
    {
        bail!("--model must be a short model identifier, not message content");
    }
    // A branch name goes into a log other tools read and into reports, so it is held to
    // the same bounds the readers apply: plain, at most 256 bytes, no control characters.
    if let Some(branch) = arguments.branch.as_deref()
        && crate::ai::safe_branch(branch).is_none()
    {
        bail!("--branch must be a branch name of at most 256 bytes without control characters");
    }
    // The value is echoed back because an RFC 3339 timestamp is usually wrong
    // in a way you can only see next to what you typed (AUDIT V).
    let timestamp = arguments
        .timestamp
        .as_deref()
        .map(|value| {
            parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --timestamp {value:?}"))
        })
        .transpose()?;
    let started_at = arguments
        .started_at
        .as_deref()
        .map(|value| {
            parse_timestamp(value).ok_or_else(|| anyhow::anyhow!("invalid --started-at {value:?}"))
        })
        .transpose()?;
    let completed_at = arguments
        .completed_at
        .as_deref()
        .map(|value| {
            parse_timestamp(value)
                .ok_or_else(|| anyhow::anyhow!("invalid --completed-at {value:?}"))
        })
        .transpose()?;
    if started_at
        .zip(completed_at)
        .is_some_and(|(start, end)| end <= start)
    {
        bail!("--completed-at must be later than --started-at");
    }
    let cwd = arguments
        .cwd
        .clone()
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let event = RecordedEvent {
        timestamp: timestamp
            .or(completed_at)
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
        provider,
        session_id: arguments.session.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: arguments
            .model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        event: arguments.kind,
        role: arguments.role,
        started_at: started_at.map(|value| value.to_rfc3339()),
        completed_at: completed_at.map(|value| value.to_rfc3339()),
        branch: arguments.branch.clone(),
    };
    let mut encoded = serde_json::to_vec(&event)?;
    encoded.push(b'\n');
    let output = arguments.output.clone().unwrap_or_else(default_events_path);
    if output.as_os_str() == "-" {
        io::stdout().write_all(&encoded)?;
        return Ok(());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create event directory {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output)
        .with_context(|| format!("cannot open event log {}", output.display()))?;
    file.write_all(&encoded)?;
    eprintln!("Recorded content-free event → {}", output.display());
    Ok(())
}
