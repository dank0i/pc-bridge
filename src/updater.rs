//! Auto-updater - checks GitHub releases on launch
//!
//! Uses the "rename trick" on Windows: a running exe can be renamed but not
//! deleted. So we rename ourselves to `.old`, move the update into place,
//! spawn the new exe, and exit. The new instance cleans up `.old` on startup.

use log::{info, warn};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const GITHUB_OWNER: &str = "dank0i";
const GITHUB_REPO: &str = "pc-bridge";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("pc-bridge/", env!("CARGO_PKG_VERSION"));

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// GitHub release info (minimal fields we need)
#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Create an HTTP agent configured to use native-tls (OS TLS stack).
///
/// ureq v3 defaults to Rustls even with the `native-tls` feature;
/// the provider must be set explicitly via [`TlsConfig`].
/// Root certs default to `WebPki` which requires `native-tls-webpki-roots`;
/// we use `PlatformVerifier` so Windows Schannel loads the OS cert store.
fn http_agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder().tls_config(tls).build();
    ureq::Agent::new_with_config(config)
}

/// Clean up leftover `.old` files from a previous update.
/// Called on startup before the update check.
pub fn cleanup_old_files() {
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let old_path = current_exe.with_extension("exe.old");
    if old_path.exists() {
        match std::fs::remove_file(&old_path) {
            Ok(()) => info!("Cleaned up old update file: {:?}", old_path),
            Err(e) => warn!("Failed to clean up {:?}: {}", old_path, e),
        }
    }
}

/// Check for updates and download if available
pub async fn check_for_updates() {
    match fetch_latest_release().await {
        Ok(Some(release)) => {
            let remote_version = release.tag_name.trim_start_matches('v');

            if is_newer_version(remote_version, CURRENT_VERSION) {
                info!(
                    "Update available: {} -> {}",
                    CURRENT_VERSION, remote_version
                );

                // Find the exe asset
                if let Some(asset) = release.assets.iter().find(|a| a.name.ends_with(".exe")) {
                    // Look for a .sha256 sidecar (e.g. "pc-bridge.exe.sha256")
                    let checksum_asset = release
                        .assets
                        .iter()
                        .find(|a| a.name == format!("{}.sha256", asset.name));

                    match download_update(
                        &asset.browser_download_url,
                        &asset.name,
                        checksum_asset.map(|a| a.browser_download_url.as_str()),
                    )
                    .await
                    {
                        Ok(update_path) => {
                            info!("Update downloaded, installing...");
                            install_and_restart(&update_path);
                        }
                        Err(e) => {
                            warn!("Failed to download update: {}", e);
                        }
                    }
                } else {
                    warn!("No exe asset found in release");
                }
            } else {
                info!("Already up to date (v{})", CURRENT_VERSION);
            }
        }
        Ok(None) => {
            info!("No releases found");
        }
        Err(e) => {
            warn!("Failed to check for updates: {}", e);
        }
    }
}

async fn fetch_latest_release() -> anyhow::Result<Option<GitHubRelease>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases",
        GITHUB_OWNER, GITHUB_REPO
    );

    let response = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let body = http_agent()
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .call()?
            .body_mut()
            .read_to_string()?;
        Ok(body)
    })
    .await??;

    let releases: Vec<GitHubRelease> = serde_json::from_str(&response)?;
    Ok(releases.into_iter().next())
}

async fn download_update(
    url: &str,
    filename: &str,
    checksum_url: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent dir"))?
        .to_path_buf();

    let update_path = exe_dir.join(format!("{}.update", filename));

    let url = url.to_string();
    let checksum_url = checksum_url.map(String::from);
    let path = update_path.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Download the binary
        let mut body = http_agent()
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .call()?
            .into_body();

        let mut file = std::fs::File::create(&path)?;
        std::io::copy(&mut body.as_reader(), &mut file)?;
        drop(file);

        // Verify SHA-256 if checksum asset is available
        if let Some(checksum_url) = checksum_url {
            verify_sha256(&path, &checksum_url)?;
        } else {
            warn!("No .sha256 checksum asset in release — skipping integrity check");
        }

        Ok(())
    })
    .await??;

    info!("Downloaded update to {:?}", update_path);
    Ok(update_path)
}

/// Download the `.sha256` sidecar and verify the file matches.
fn verify_sha256(file_path: &Path, checksum_url: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    // Fetch expected hash from the .sha256 file
    let checksum_body = http_agent()
        .get(checksum_url)
        .header("User-Agent", USER_AGENT)
        .call()?
        .body_mut()
        .read_to_string()?;

    // Format: "<hex_hash>  <filename>" or just "<hex_hash>"
    let expected_hex = checksum_body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Empty checksum file"))?
        .to_lowercase();

    // Compute actual hash
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(file_path)?;
    std::io::copy(&mut file, &mut hasher)?;
    let actual_hex = format!("{:x}", hasher.finalize());

    if actual_hex != expected_hex {
        // Remove the corrupt download
        let _ = std::fs::remove_file(file_path);
        anyhow::bail!(
            "SHA-256 mismatch: expected {}, got {} — download may be corrupted or tampered",
            expected_hex,
            actual_hex
        );
    }

    info!("SHA-256 verified: {}", actual_hex);
    Ok(())
}

fn is_newer_version(remote: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.trim_start_matches('v')
            .split(['.', '-'])
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let remote_parts = parse(remote);
    let current_parts = parse(current);

    for (r, c) in remote_parts.iter().zip(current_parts.iter()) {
        if r > c {
            return true;
        }
        if r < c {
            return false;
        }
    }

    remote_parts.len() > current_parts.len()
}

/// Install update and restart the application.
///
/// Uses the rename trick — no external processes (cmd.exe, PowerShell, bash).
/// A running exe on Windows can be renamed but not overwritten, so:
/// 1. Rename `pc-bridge.exe` → `pc-bridge.exe.old`
/// 2. Rename `pc-bridge.exe.update` → `pc-bridge.exe`
/// 3. Spawn the new `pc-bridge.exe`
/// 4. Exit — new instance cleans up `.old` on startup via `cleanup_old_files()`
#[cfg(windows)]
fn install_and_restart(update_path: &Path) {
    use std::process::Command;

    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to get current exe path: {}", e);
            return;
        }
    };

    let old_path = current_exe.with_extension("exe.old");

    // Remove any leftover .old file from a previous update
    if old_path.exists() {
        if let Err(e) = std::fs::remove_file(&old_path) {
            warn!("Failed to remove old update file: {}", e);
            // Non-fatal — try to rename anyway, it may work if the old .old is unlocked
        }
    }

    // Step 1: Rename running exe out of the way
    if let Err(e) = std::fs::rename(&current_exe, &old_path) {
        warn!("Failed to rename current exe to .old: {}", e);
        return;
    }

    // Step 2: Move update into place
    if let Err(e) = std::fs::rename(update_path, &current_exe) {
        warn!("Failed to move update into place: {}", e);
        // Try to restore the original
        if let Err(e2) = std::fs::rename(&old_path, &current_exe) {
            warn!(
                "Failed to restore original exe: {} — manual intervention needed",
                e2
            );
        }
        return;
    }

    info!("Update installed, starting new version...");

    // Step 3: Spawn the new exe
    let mut cmd = Command::new(&current_exe);
    cmd.creation_flags(CREATE_NO_WINDOW);

    if let Err(e) = cmd.spawn() {
        warn!("Failed to start updated exe: {}", e);
        return;
    }

    // Step 4: Exit so the new instance takes over
    std::process::exit(0);
}

/// Install update and restart (Unix).
///
/// On Unix, a running binary can be replaced directly (the OS keeps the old
/// inode open until the process exits). So this is even simpler than Windows.
#[cfg(unix)]
fn install_and_restart(update_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to get current exe path: {}", e);
            return;
        }
    };

    // Make update executable
    if let Err(e) = std::fs::set_permissions(update_path, std::fs::Permissions::from_mode(0o755)) {
        warn!("Failed to set permissions: {}", e);
    }

    // Replace in place — Unix allows this while running
    if let Err(e) = std::fs::rename(update_path, &current_exe) {
        warn!("Failed to replace binary: {}", e);
        return;
    }

    info!("Update installed, starting new version...");

    if let Err(e) = Command::new(&current_exe).spawn() {
        warn!("Failed to start updated binary: {}", e);
        return;
    }

    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version_major_bump() {
        assert!(is_newer_version("3.0.0", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_minor_bump() {
        assert!(is_newer_version("2.8.0", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_patch_bump() {
        assert!(is_newer_version("2.7.1", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_same() {
        assert!(!is_newer_version("2.7.0", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_older() {
        assert!(!is_newer_version("2.6.0", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_older_major() {
        assert!(!is_newer_version("1.9.9", "2.0.0"));
    }

    #[test]
    fn test_is_newer_version_with_v_prefix() {
        // "v" prefix is stripped before parsing
        assert!(is_newer_version("v3.0.0", "v2.7.0"));
        assert!(is_newer_version("v3.0.0", "2.7.0"));
        assert!(is_newer_version("3.0.0", "v2.7.0"));
        // Without "v" prefix still works
        assert!(is_newer_version("3.0.0", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_with_prerelease() {
        // "2.8.0-1" splits into [2,8,0,1] which is > [2,7,0]
        assert!(is_newer_version("2.8.0-1", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_extra_segments() {
        // [2,7,0,1] > [2,7,0] due to longer length after equal prefix
        assert!(is_newer_version("2.7.0.1", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_shorter_not_newer() {
        // [2,7] vs [2,7,0] — equal prefix but shorter, so not newer
        assert!(!is_newer_version("2.7", "2.7.0"));
    }
}
