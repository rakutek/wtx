//! Explicit release checks and a 24-hour cache used only by the interactive TUI.
//! Regular CLI commands do not call this module or access the network.

use crate::util::last_nonempty_line;
use anyhow::{anyhow, Context, Result};
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH, USER_AGENT};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/rakutek/wtx/releases/latest";
const GITHUB_API_VERSION: &str = "2022-11-28";
const HOMEBREW_FORMULA: &str = "rakutek/tap/wtx";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    pub schema_version: u32,
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateCache {
    schema_version: u32,
    checked_at: u64,
    etag: Option<String>,
    latest_version: String,
    release_url: String,
}

fn cache_path() -> PathBuf {
    crate::util::wtx_home().join("update-check.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const fn cache_is_fresh(cache: &UpdateCache, now: u64) -> bool {
    cache.schema_version == SCHEMA_VERSION && now.saturating_sub(cache.checked_at) < CACHE_TTL_SECS
}

fn read_cache() -> Result<UpdateCache> {
    let raw = std::fs::read(cache_path()).context("read update cache")?;
    let cache: UpdateCache = serde_json::from_slice(&raw).context("parse update cache")?;
    if cache.schema_version != SCHEMA_VERSION {
        return Err(anyhow!("unsupported update cache schema"));
    }
    Ok(cache)
}

fn write_cache(cache: &UpdateCache) -> Result<()> {
    let path = cache_path();
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let raw = serde_json::to_vec_pretty(cache)?;
    if let Err(e) = std::fs::write(&tmp, raw) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("write update cache");
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("commit update cache");
    }
    Ok(())
}

fn parse_version(raw: &str) -> Result<Version> {
    let normalized = raw
        .strip_prefix('v')
        .or_else(|| raw.strip_prefix('V'))
        .unwrap_or(raw);
    Version::parse(normalized).with_context(|| format!("invalid release version: {raw}"))
}

fn status(latest: &str, release_url: &str) -> Result<UpdateStatus> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest = parse_version(latest)?;
    Ok(UpdateStatus {
        schema_version: SCHEMA_VERSION,
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        update_available: latest > current,
        release_url: release_url.to_string(),
    })
}

fn status_from_cache(cache: &UpdateCache) -> Result<UpdateStatus> {
    status(&cache.latest_version, &cache.release_url)
}

fn tui_check_disabled() -> bool {
    std::env::var("WTX_NO_UPDATE_CHECK")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Return the release candidate shown at interactive TUI startup and whether a background
/// refresh is needed. This reads only the cache and does not access the network.
pub fn tui_state() -> (Option<UpdateStatus>, bool) {
    if tui_check_disabled() {
        return (None, false);
    }
    let Ok(cache) = read_cache() else {
        return (None, true);
    };
    if !cache_is_fresh(&cache, now_secs()) {
        return (None, true);
    }
    match status_from_cache(&cache) {
        Ok(s) => (s.update_available.then_some(s), false),
        Err(_) => (None, true),
    }
}

async fn fetch() -> Result<UpdateStatus> {
    let cached = read_cache().ok();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut request = client
        .get(LATEST_RELEASE_URL)
        .header(USER_AGENT, concat!("wtx/", env!("CARGO_PKG_VERSION")))
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
    if let Some(etag) = cached.as_ref().and_then(|c| c.etag.as_deref()) {
        request = request.header(IF_NONE_MATCH, etag);
    }

    let response = request.send().await.context("request GitHub Releases")?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        let mut cache = cached.ok_or_else(|| anyhow!("GitHub returned 304 without a cache"))?;
        cache.checked_at = now_secs();
        let result = status_from_cache(&cache)?;
        let _ = write_cache(&cache);
        return Ok(result);
    }
    let response = response
        .error_for_status()
        .context("GitHub Releases request failed")?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let release: GitHubRelease = response.json().await.context("parse GitHub release")?;
    let result = status(&release.tag_name, &release.html_url)?;
    let cache = UpdateCache {
        schema_version: SCHEMA_VERSION,
        checked_at: now_secs(),
        etag,
        latest_version: result.latest_version.clone(),
        release_url: result.release_url.clone(),
    };
    let _ = write_cache(&cache);
    Ok(result)
}

/// Synchronous boundary shared by the explicit CLI and the TUI background thread.
pub fn check() -> Result<UpdateStatus> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fetch())
}

pub fn check_and_print(json: bool) -> Result<()> {
    let result = check()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.update_available {
        println!(
            "wtx {} is available (current {})",
            result.latest_version, result.current_version
        );
        println!("Upgrade: wtx upgrade");
        println!("{}", result.release_url);
    } else {
        println!("wtx {} is up to date", result.current_version);
    }
    Ok(())
}

fn run_brew(args: &[&str]) -> Result<()> {
    let display = format!("brew {}", args.join(" "));
    let status = Command::new("brew")
        .args(args)
        .status()
        .with_context(|| format!("run `{display}`; is Homebrew installed?"))?;
    if !status.success() {
        return Err(anyhow!("`{display}` failed with {status}"));
    }
    Ok(())
}

fn run_brew_captured(args: &[&str]) -> Result<()> {
    let display = format!("brew {}", args.join(" "));
    let output = Command::new("brew")
        .args(args)
        .output()
        .with_context(|| format!("run `{display}`; is Homebrew installed?"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = last_nonempty_line(&stderr)
        .or_else(|| last_nonempty_line(&stdout))
        .unwrap_or("Homebrew command failed");
    Err(anyhow!("`{display}` failed: {detail}"))
}

fn parse_brew_versions(raw: &str) -> Result<Version> {
    raw.split_whitespace()
        .skip(1)
        .filter_map(|value| Version::parse(value.split('_').next().unwrap_or(value)).ok())
        .max()
        .ok_or_else(|| anyhow!("Homebrew did not report an installed wtx version"))
}

fn installed_homebrew_version() -> Result<Version> {
    let args = ["list", "--versions", "--formula", HOMEBREW_FORMULA];
    let display = format!("brew {}", args.join(" "));
    let output = Command::new("brew")
        .args(args)
        .output()
        .with_context(|| format!("run `{display}`; is Homebrew installed?"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = last_nonempty_line(&stderr).unwrap_or("Homebrew could not inspect wtx");
        return Err(anyhow!("`{display}` failed: {detail}"));
    }
    parse_brew_versions(&String::from_utf8_lossy(&output.stdout))
}

fn upgrade_with(mut run: impl FnMut(&[&str]) -> Result<()>) -> Result<()> {
    run(&["update", "--quiet"])?;
    run(&["upgrade", "--yes", "--formula", HOMEBREW_FORMULA])?;
    Ok(())
}

/// Update the formula explicitly before upgrading only wtx, independent of Homebrew's
/// automatic update interval.
pub fn upgrade() -> Result<()> {
    let result = upgrade_with(|args| {
        if args.first() == Some(&"update") {
            println!("Refreshing Homebrew metadata...");
        } else {
            println!("Upgrading wtx...");
        }
        run_brew(args)
    });
    if result.is_ok() {
        println!("wtx upgrade complete");
    }
    result
}

/// Capture Homebrew output to preserve the alternate screen, then verify that the notified
/// version was installed.
pub fn upgrade_captured(expected_version: &str) -> Result<()> {
    let expected = parse_version(expected_version)?;
    upgrade_with(run_brew_captured)?;
    let installed = installed_homebrew_version()?;
    if installed < expected {
        return Err(anyhow!(
            "Homebrew installed wtx {installed}, but {expected} is available; retry after the tap updates"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(checked_at: u64) -> UpdateCache {
        UpdateCache {
            schema_version: SCHEMA_VERSION,
            checked_at,
            etag: Some("etag".into()),
            latest_version: "0.8.0".into(),
            release_url: "https://example.test/release".into(),
        }
    }

    #[test]
    fn accepts_github_v_prefix() {
        assert_eq!(parse_version("v1.2.3").unwrap(), Version::new(1, 2, 3));
    }

    #[test]
    fn compares_versions_semantically() {
        let result = status("v999.0.0", "https://example.test/release").unwrap();
        assert!(result.update_available);
        assert_eq!(result.latest_version, "999.0.0");
    }

    #[test]
    fn cache_expires_after_twenty_four_hours() {
        assert!(cache_is_fresh(&cache(100), 100 + CACHE_TTL_SECS - 1));
        assert!(!cache_is_fresh(&cache(100), 100 + CACHE_TTL_SECS));
    }

    #[test]
    fn json_contract_is_versioned() {
        let value =
            serde_json::to_value(status("v1.2.3", "https://example.test").unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert!(value.get("current_version").is_some());
        assert_eq!(value["latest_version"], "1.2.3");
        assert!(value.get("update_available").is_some());
        assert_eq!(value["release_url"], "https://example.test");
    }

    #[test]
    fn upgrade_refreshes_metadata_before_upgrading_only_wtx() {
        let mut calls = Vec::new();
        upgrade_with(|args| {
            calls.push(args.iter().map(ToString::to_string).collect::<Vec<_>>());
            Ok(())
        })
        .unwrap();

        assert_eq!(
            calls,
            [
                vec!["update".to_string(), "--quiet".to_string()],
                vec![
                    "upgrade".to_string(),
                    "--yes".to_string(),
                    "--formula".to_string(),
                    HOMEBREW_FORMULA.to_string(),
                ],
            ]
        );
    }

    #[test]
    fn upgrade_stops_when_metadata_refresh_fails() {
        let mut calls = 0;
        let err = upgrade_with(|_| {
            calls += 1;
            Err(anyhow!("update failed"))
        })
        .unwrap_err();

        assert_eq!(calls, 1);
        assert_eq!(err.to_string(), "update failed");
    }

    #[test]
    fn parses_the_newest_installed_homebrew_version() {
        assert_eq!(
            parse_brew_versions("wtx 0.9.0 1.0.0_1\n").unwrap(),
            Version::new(1, 0, 0)
        );
        assert!(parse_brew_versions("wtx\n").is_err());
    }
}
