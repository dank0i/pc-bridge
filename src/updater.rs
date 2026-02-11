//! Auto-updater - checks GitHub releases on launch
//!
//! Uses the "rename trick" on Windows: a running exe can be renamed but not
//! deleted. So we rename ourselves to `.old`, move the update into place,
//! spawn the new exe, and exit. The new instance cleans up `.old` on startup.

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const GITHUB_OWNER: &str = "dank0i";
const GITHUB_REPO: &str = "pc-bridge";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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
                    match download_update(&asset.browser_download_url, &asset.name).await {
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
    // Use /releases (not /releases/latest) since latest doesn't include pre-releases
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases",
        GITHUB_OWNER, GITHUB_REPO
    );

    let response = tokio::task::spawn_blocking(move || fetch_url_blocking(&url)).await??;

    let releases: Vec<GitHubRelease> = serde_json::from_str(&response)?;
    Ok(releases.into_iter().next())
}

fn fetch_url_blocking(url: &str) -> anyhow::Result<String> {
    use std::process::Command;

    // Use curl instead of PowerShell (~10ms vs ~200-500ms startup)
    #[allow(unused_mut)]
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-L", "-A", "pc-agent", url]);

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!(
            "HTTP request failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

async fn download_update(url: &str, filename: &str) -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent dir"))?
        .to_path_buf();

    let update_path = exe_dir.join(format!("{}.update", filename));

    let url = url.to_string();
    let path = update_path.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        #[allow(unused_mut)]
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-sS", "-L", "-A", "pc-agent", "-o"]);
        cmd.arg(path.as_os_str());
        cmd.arg(&url);

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let output = cmd.output()?;

        if !output.status.success() {
            anyhow::bail!(
                "Download failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    })
    .await??;

    info!("Downloaded update to {:?}", update_path);
    Ok(update_path)
}

fn is_newer_version(remote: &str, current: &str) -> bool {
    let parse =
        |v: &str| -> Vec<u32> { v.split(['.', '-']).filter_map(|s| s.parse().ok()).collect() };

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
        // "v" prefix causes first segment to fail parse, so "v3.0.0" → [0,0] vs "v2.7.0" → [0,0]
        // Both parse equally — this tests that the function doesn't panic on non-numeric input
        assert!(!is_newer_version("v3.0.0", "v2.7.0"));
        // Without "v" prefix, it works correctly
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
