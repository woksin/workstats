use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::paths::home_dir;

pub const BUILTIN_PROVIDERS: &[&str] = &[
    "claude",
    "codex",
    "copilot",
    "copilot-vscode",
    "gemini",
    "opencode",
    "pi",
];

#[derive(Clone, Debug, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub format: String,
    pub path: String,
    pub detected: bool,
    pub support: String,
}

pub fn normalize_provider(value: &str) -> String {
    match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "claude-code" => "claude".to_string(),
        "openai-codex" => "codex".to_string(),
        "gemini-cli" | "google-gemini" => "gemini".to_string(),
        "copilot-cli" | "github-copilot" => "copilot".to_string(),
        // Named after the editor rather than after "chat": the CLI has a chat too, and
        // the surface a session came from is what the reader needs to know.
        "copilot-chat" | "vscode-copilot" | "vs-code-copilot" => "copilot-vscode".to_string(),
        "open-code" => "opencode".to_string(),
        "pi-agent" | "pi-coding-agent" | "pi-cli" => "pi".to_string(),
        other => other.to_string(),
    }
}

pub fn parse_history_overrides(values: &[String]) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut result: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for value in values {
        let Some((provider, path)) = value.split_once('=') else {
            bail!("--history must be PROVIDER=PATH");
        };
        let provider = normalize_provider(provider);
        if provider.is_empty() || provider == "all" || path.trim().is_empty() {
            bail!("--history must contain a provider name and a path");
        }
        if !BUILTIN_PROVIDERS.contains(&provider.as_str()) && provider != "events" {
            bail!(
                "unknown history adapter '{provider}'; use one of {} or --events for an open event log",
                BUILTIN_PROVIDERS.join(", ")
            );
        }
        result
            .entry(provider)
            .or_default()
            .push(expand_user_path(path));
    }
    Ok(result)
}

fn expand_user_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home_dir().join(rest);
    }
    PathBuf::from(value)
}

pub fn default_history_paths() -> BTreeMap<String, Vec<PathBuf>> {
    let home = home_dir();
    BTreeMap::from([
        ("claude".to_string(), vec![home.join(".claude/projects")]),
        ("codex".to_string(), vec![home.join(".codex/sessions")]),
        (
            "copilot".to_string(),
            vec![home.join(".copilot/session-state")],
        ),
        (
            "copilot-vscode".to_string(),
            copilot_vscode_history_paths(&home),
        ),
        ("gemini".to_string(), vec![home.join(".gemini/tmp")]),
        (
            "opencode".to_string(),
            vec![home.join(".local/share/opencode/opencode.db")],
        ),
        ("pi".to_string(), vec![pi_history_path(&home)]),
    ])
}

/// Where Pi keeps session transcripts.
///
/// Pi resolves this itself from `PI_CODING_AGENT_SESSION_DIR`, else
/// `PI_CODING_AGENT_DIR`, else `~/.pi/agent`, so the same variables are honoured here.
/// Reading them rather than assuming the default is what makes the tool find the history
/// of a Pi that stores it elsewhere — which is the normal case under a containerised or
/// XDG-relocated setup — instead of silently reporting no Pi activity.
fn pi_history_path(home: &Path) -> PathBuf {
    if let Some(path) = env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("PI_CODING_AGENT_DIR") {
        return PathBuf::from(path).join("sessions");
    }
    home.join(".pi/agent/sessions")
}

/// VS Code keeps chat transcripts per install, and several installs coexist happily.
///
/// Only roots that exist are returned, because a listed root that is missing reports
/// itself as missing on every single run — a warning about an editor the user never
/// installed is noise, not a diagnostic. When none of them exists the stable build's
/// root is named anyway, so `workstats sources` still documents where the tool looks. A
/// portable or relocated install is reachable through `--history copilot-vscode=PATH`.
fn copilot_vscode_history_paths(home: &Path) -> Vec<PathBuf> {
    let installed: Vec<PathBuf> = ["Code", "Code - Insiders", "VSCodium"]
        .into_iter()
        .map(|application| vscode_workspace_storage(home, application))
        .filter(|path| path.is_dir())
        .collect();
    if installed.is_empty() {
        return vec![vscode_workspace_storage(home, "Code")];
    }
    installed
}

fn vscode_workspace_storage(home: &Path, application: &str) -> PathBuf {
    #[cfg(windows)]
    let base = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"));
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = home.join(".config");
    base.join(application).join("User/workspaceStorage")
}

pub fn default_codex_database() -> PathBuf {
    home_dir().join(".codex/state_5.sqlite")
}

pub fn default_events_path() -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_EVENTS") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join("workstats/events.jsonl");
    }
    #[cfg(windows)]
    if let Some(path) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("workstats/events.jsonl");
    }
    #[cfg(target_os = "macos")]
    return home_dir().join("Library/Application Support/workstats/events.jsonl");
    #[cfg(not(target_os = "macos"))]
    home_dir().join(".local/share/workstats/events.jsonl")
}

pub fn resolve_opencode_database(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("opencode.db")
    } else {
        path.to_path_buf()
    }
}

pub fn source_inventory() -> Vec<SourceInfo> {
    let defaults = default_history_paths();
    let mut items = Vec::new();
    for (id, name, format, support) in [
        ("claude", "Claude Code", "JSONL transcripts", "built-in"),
        (
            "codex",
            "OpenAI Codex",
            "JSONL + optional SQLite",
            "built-in",
        ),
        (
            "copilot",
            "GitHub Copilot CLI",
            "events.jsonl",
            "best-effort",
        ),
        (
            "copilot-vscode",
            "GitHub Copilot Chat (VS Code)",
            "chatSessions JSON",
            "best-effort",
        ),
        (
            "gemini",
            "Google Gemini CLI",
            "session JSON/JSONL",
            "built-in",
        ),
        ("opencode", "OpenCode", "read-only SQLite", "best-effort"),
        ("pi", "Pi Coding Agent", "JSONL transcripts", "built-in"),
    ] {
        for path in defaults.get(id).into_iter().flatten() {
            let detected = if id == "opencode" {
                resolve_opencode_database(path).is_file()
            } else {
                path.is_dir()
            };
            items.push(SourceInfo {
                id: id.to_string(),
                name: name.to_string(),
                format: format.to_string(),
                path: path.to_string_lossy().into_owned(),
                detected,
                support: support.to_string(),
            });
        }
    }
    let events = default_events_path();
    items.push(SourceInfo {
        id: "events".to_string(),
        name: "Workstats Events".to_string(),
        format: "open JSONL".to_string(),
        path: events.to_string_lossy().into_owned(),
        detected: events.is_file(),
        support: "stable".to_string(),
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_aliases_and_history_overrides_are_portable() {
        assert_eq!("claude", normalize_provider("Claude_Code"));
        assert_eq!("copilot", normalize_provider("github-copilot"));
        // The editor's Copilot Chat is a separate surface from the CLI, so its aliases
        // must not collapse onto `copilot`.
        assert_eq!("copilot-vscode", normalize_provider("VSCode_Copilot"));
        assert_eq!("copilot-vscode", normalize_provider("Copilot-Chat"));
        assert_eq!("openai", normalize_provider("openai"));
        let parsed = parse_history_overrides(&[
            "gemini=/var/history/a".into(),
            "gemini=/var/history/b".into(),
            "copilot-chat=/var/history/c".into(),
        ])
        .unwrap();
        assert_eq!(2, parsed["gemini"].len());
        assert_eq!(1, parsed["copilot-vscode"].len());
        assert!(parse_history_overrides(&["cursor=/tmp/db".into()]).is_err());
    }

    /// A provider that `--history` accepts but `sources` never mentions is a source the
    /// user has no way to discover. The two lists are maintained by hand, so they are
    /// checked against each other rather than trusted to stay in step.
    #[test]
    fn every_builtin_adapter_appears_in_the_source_inventory() {
        let inventory = source_inventory();
        for provider in BUILTIN_PROVIDERS {
            assert!(
                inventory.iter().any(|item| item.id == *provider),
                "{provider} is a built-in adapter but is missing from `workstats sources`"
            );
        }
    }
}
