//! Updates from GitHub releases: check the latest release on launch, download
//! its zip (verified against the `.sha256` file the release workflow publishes
//! beside it), then swap the exe and restart.
//!
//! Windows lets a running exe be renamed but not overwritten, so the swap is:
//! running exe → `voicetype.old.exe`, new exe → the original path, start it,
//! exit. The next launch deletes the `.old.exe`.

use crate::engine;
use std::path::PathBuf;

const LATEST: &str = "https://api.github.com/repos/JammingDino/voicetype/releases/latest";
const ASSET: &str = "voicetype-windows-x64.zip";

#[derive(Clone)]
pub struct Release {
    pub version: String,
    zip_url: String,
    sha_url: String,
}

/// Updating replaces the exe, so it is off for `cargo run` builds unless a test asks for it.
pub fn enabled() -> bool {
    cfg!(windows) && (!cfg!(debug_assertions) || std::env::var_os("VOICETYPE_PRETEND_VERSION").is_some())
}

/// This build's version. `VOICETYPE_PRETEND_VERSION` overrides it, to test updating.
pub fn current() -> String {
    std::env::var("VOICETYPE_PRETEND_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parse(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn newer(candidate: &str, than: &str) -> bool {
    parse(candidate) > parse(than)
}

/// The latest release, if it is newer than this build and has a Windows zip.
pub fn check() -> Result<Option<Release>, String> {
    let body: serde_json::Value = reqwest::blocking::Client::new()
        .get(LATEST)
        .header("User-Agent", "voicetype-updater")
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("update check failed: {e}"))?;
    let tag = body["tag_name"].as_str().unwrap_or_default();
    if !newer(tag, &current()) {
        return Ok(None);
    }
    let url = |name: &str| {
        body["assets"].as_array()?.iter().find(|a| a["name"] == name)?["browser_download_url"].as_str().map(str::to_string)
    };
    match (url(ASSET), url(&format!("{ASSET}.sha256"))) {
        (Some(zip_url), Some(sha_url)) => Ok(Some(Release { version: tag.trim_start_matches('v').to_string(), zip_url, sha_url })),
        _ => Ok(None), // a release without a verifiable Windows build is not offered
    }
}

fn dir() -> PathBuf {
    engine::data_dir().join("update")
}

/// Download and unpack the release; returns the new exe, ready for `apply`.
pub fn download(r: &Release) -> Result<PathBuf, String> {
    let dir = dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let sha = reqwest::blocking::Client::new()
        .get(&r.sha_url)
        .header("User-Agent", "voicetype-updater")
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("update checksum download failed: {e}"))?;
    let sha = sha.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if sha.len() != 64 {
        return Err("the update's checksum file is malformed".into());
    }
    let zip = dir.join(ASSET);
    engine::exclusive(|| engine::download(&r.zip_url, &zip, &sha, 0))?;
    engine::extract(&zip, &dir)?;
    let exe = dir.join("voicetype").join("voicetype.exe");
    exe.is_file().then_some(exe).ok_or_else(|| "the update zip has no voicetype.exe".into())
}

/// Put `new_exe` where this exe is and start it with `args`. The caller exits right after.
pub fn apply(new_exe: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = me.with_extension("old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&me, &old).map_err(|e| format!("couldn't move the running exe aside: {e}"))?;
    if let Err(e) = std::fs::copy(new_exe, &me) {
        let _ = std::fs::rename(&old, &me); // put the working version back
        return Err(format!("couldn't install the update: {e}"));
    }
    // The test override must not follow us into the new exe, or it would update forever.
    std::process::Command::new(&me).args(args).env_remove("VOICETYPE_PRETEND_VERSION").spawn().map_err(|e| format!("couldn't start the updated app: {e}"))?;
    Ok(())
}

/// Remove what the last update left behind.
pub fn cleanup() {
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::remove_file(me.with_extension("old.exe"));
    }
    let _ = std::fs::remove_dir_all(dir());
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_compare_numerically() {
        assert!(super::newer("v0.1.10", "0.1.9"));
        assert!(super::newer("v0.2.0", "0.1.11"));
        assert!(!super::newer("v0.1.1", "0.1.1"));
        assert!(!super::newer("v0.1.0", "0.1.1"));
    }
}
