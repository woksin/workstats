use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPO: &str = "woksin/workstats";
const CHECK_INTERVAL: chrono::Duration = chrono::Duration::hours(24);

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Default, Serialize, Deserialize)]
struct UpdateCheckCache {
    checked_at: Option<DateTime<Utc>>,
    latest_version: Option<String>,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn user_agent() -> String {
    format!("workstats/{}", current_version())
}

fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(user_agent())
        .build();
    config.into()
}

fn fetch_latest_release() -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = agent(Duration::from_secs(10))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("could not reach GitHub releases")?
        .body_mut()
        .read_to_string()
        .context("could not read the GitHub release response")?;
    serde_json::from_str(&body).context("could not parse the GitHub release response")
}

fn parse_version(value: &str) -> Option<[u64; 3]> {
    let value = value.trim_start_matches('v');
    let mut parts = value.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some([major, minor, patch])
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn read_cache(path: &Path) -> UpdateCheckCache {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_cache(path: &Path, cache: &UpdateCheckCache) {
    let Ok(encoded) = serde_json::to_vec(cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, encoded);
}

/// Best-effort, throttled update check for the passive dashboard footer. Never fails the
/// caller: network or cache errors just mean no footer is shown.
pub fn maybe_check_for_update(cache_path: &Path, opt_in: bool) -> Option<String> {
    if !opt_in {
        return None;
    }
    let mut cache = read_cache(cache_path);
    let stale = cache
        .checked_at
        .is_none_or(|checked_at| Utc::now() - checked_at >= CHECK_INTERVAL);
    if stale {
        cache.checked_at = Some(Utc::now());
        if let Ok(release) = fetch_latest_release() {
            cache.latest_version = Some(release.tag_name.trim_start_matches('v').to_string());
        }
        write_cache(cache_path, &cache);
    }
    let latest = cache.latest_version?;
    is_newer(&latest, current_version()).then_some(latest)
}

fn platform_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("workstats-macos-arm64.tar.gz"),
        ("macos", "x86_64") => Some("workstats-macos-x86_64.tar.gz"),
        ("linux", "x86_64") => Some("workstats-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("workstats-linux-arm64.tar.gz"),
        ("windows", "x86_64") => Some("workstats-windows-x86_64.exe"),
        ("windows", "x86") => Some("workstats-windows-x86.exe"),
        _ => None,
    }
}

fn download_text(url: &str) -> Result<String> {
    agent(Duration::from_secs(15))
        .get(url)
        .call()
        .with_context(|| format!("could not download {url}"))?
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .with_context(|| format!("could not read {url}"))
}

fn download_bytes(url: &str) -> Result<Vec<u8>> {
    agent(Duration::from_secs(120))
        .get(url)
        .call()
        .with_context(|| format!("could not download {url}"))?
        .body_mut()
        .with_config()
        .limit(200 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("could not read {url}"))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn checksum_for(checksums: &str, asset_name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset_name).then(|| digest.to_lowercase())
    })
}

fn extract_tar_gz_binary(bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .context("could not read release archive")?
    {
        let mut entry = entry.context("could not read release archive entry")?;
        let is_binary = entry
            .path()
            .ok()
            .is_some_and(|path| path.file_name().is_some_and(|name| name == "workstats"));
        if !is_binary {
            continue;
        }
        let mut buffer = Vec::new();
        entry
            .read_to_end(&mut buffer)
            .context("could not extract the workstats binary")?;
        return Ok(buffer);
    }
    bail!("release archive does not contain a workstats binary")
}

pub struct UpdateOutcome {
    pub current: String,
    pub latest: String,
    pub available: bool,
}

/// Always hits the network live (this is an explicit, user-triggered check) and refreshes
/// the throttled footer cache as a side effect.
pub fn check_now(cache_path: &Path) -> Result<UpdateOutcome> {
    let current = current_version().to_string();
    let release = fetch_latest_release()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    write_cache(
        cache_path,
        &UpdateCheckCache {
            checked_at: Some(Utc::now()),
            latest_version: Some(latest.clone()),
        },
    );
    let available = is_newer(&latest, &current);
    Ok(UpdateOutcome {
        current,
        latest,
        available,
    })
}

pub fn install_latest(cache_path: &Path) -> Result<UpdateOutcome> {
    let current = current_version().to_string();
    let release = fetch_latest_release()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    write_cache(
        cache_path,
        &UpdateCheckCache {
            checked_at: Some(Utc::now()),
            latest_version: Some(latest.clone()),
        },
    );
    if !is_newer(&latest, &current) {
        return Ok(UpdateOutcome {
            current,
            latest,
            available: false,
        });
    }
    let asset_name = platform_asset_name().with_context(|| {
        format!(
            "no prebuilt binary is published for this platform ({} {}); update manually",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let asset = release
        .assets
        .iter()
        .find(|item| item.name == asset_name)
        .with_context(|| {
            format!("release {latest} does not include an asset named {asset_name}")
        })?;
    let checksums_asset = release
        .assets
        .iter()
        .find(|item| item.name == "SHA256SUMS")
        .context("release does not include a SHA256SUMS file")?;

    let checksums = download_text(&checksums_asset.browser_download_url)?;
    let expected = checksum_for(&checksums, asset_name)
        .with_context(|| format!("SHA256SUMS does not list {asset_name}"))?;

    let downloaded = download_bytes(&asset.browser_download_url)?;
    let actual = hex_sha256(&downloaded);
    if actual != expected {
        bail!("checksum mismatch for {asset_name}: expected {expected}, got {actual}");
    }

    let binary = if asset_name.ends_with(".tar.gz") {
        extract_tar_gz_binary(&downloaded)?
    } else {
        downloaded
    };

    let current_exe = std::env::current_exe().context("could not locate the running executable")?;
    let temp_dir = current_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let temp_path = temp_dir.join(format!(".workstats-update-{}", std::process::id()));
    fs::write(&temp_path, &binary).context("could not write the downloaded binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))
            .context("could not mark the downloaded binary executable")?;
    }
    let replaced =
        self_replace::self_replace(&temp_path).context("could not replace the running binary");
    let _ = fs::remove_file(&temp_path);
    replaced?;

    Ok(UpdateOutcome {
        current,
        latest,
        available: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_orders_by_semver_triple() {
        assert!(is_newer("0.7.0", "0.6.1"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.6.1", "0.6.1"));
        assert!(!is_newer("0.6.0", "0.6.1"));
        assert!(!is_newer("not-a-version", "0.6.1"));
    }

    #[test]
    fn checksum_lookup_matches_the_named_asset() {
        let checksums =
            "abc123  workstats-linux-x86_64.tar.gz\ndef456  workstats-macos-arm64.tar.gz\n";
        assert_eq!(
            Some("abc123".to_string()),
            checksum_for(checksums, "workstats-linux-x86_64.tar.gz")
        );
        assert_eq!(None, checksum_for(checksums, "workstats-windows-x86.exe"));
    }

    #[test]
    fn stale_cache_without_opt_in_never_checks() {
        let directory = tempfile::tempdir().unwrap();
        let cache_path = directory.path().join("update-check.json");
        assert_eq!(None, maybe_check_for_update(&cache_path, false));
        assert!(!cache_path.exists());
    }
}
