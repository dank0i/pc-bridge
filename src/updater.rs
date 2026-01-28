//! Auto-updater - checks GitHub releases on launch

use std::path::PathBuf;
use tracing::{info, warn};

const GITHUB_OWNER: &str = "dank0i";
const GITHUB_REPO: &str = "pc-agent";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// Check for updates and download if available
pub async fn check_for_updates() {
    info!("Checking for updates (current: v{})", CURRENT_VERSION);
    
    match fetch_latest_release().await {
        Ok(Some(release)) => {
            let remote_version = release.tag_name.trim_start_matches('v');
            
            if is_newer_version(remote_version, CURRENT_VERSION) {
                info!("Update available: {} -> {}", CURRENT_VERSION, remote_version);
                
                // Find the exe asset
                if let Some(asset) = release.assets.iter().find(|a| a.name.ends_with(".exe")) {
                    match download_update(&asset.browser_download_url, &asset.name).await {
                        Ok(path) => {
                            show_update_message(&release.tag_name, &path);
                        }
                        Err(e) => {
                            warn!("Failed to download update: {}", e);
                        }
                    }
                } else {
                    warn!("No exe asset found in release");
                }
            } else {
                info!("Already up to date");
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
    
    let response = tokio::task::spawn_blocking(move || {
        fetch_url_blocking(&url)
    }).await??;
    
    let releases: Vec<GitHubRelease> = serde_json::from_str(&response)?;
    Ok(releases.into_iter().next())
}

fn fetch_url_blocking(url: &str) -> anyhow::Result<String> {
    use std::process::Command;
    
    // Use PowerShell to fetch URL (works on all Windows)
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                 (Invoke-WebRequest -Uri '{}' -UseBasicParsing -Headers @{{'User-Agent'='pc-agent'}}).Content",
                url
            ),
        ])
        .output()?;
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!("HTTP request failed: {}", String::from_utf8_lossy(&output.stderr))
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
        // Download using PowerShell
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                     Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing",
                    url, path.display()
                ),
            ])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("Download failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }).await??;
    
    info!("Downloaded update to {:?}", update_path);
    Ok(update_path)
}

fn is_newer_version(remote: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.split(|c| c == '.' || c == '-')
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    
    let remote_parts = parse(remote);
    let current_parts = parse(current);
    
    for (r, c) in remote_parts.iter().zip(current_parts.iter()) {
        if r > c { return true; }
        if r < c { return false; }
    }
    
    remote_parts.len() > current_parts.len()
}

fn show_update_message(version: &str, path: &PathBuf) {
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;
    
    let message = format!(
        "A new version ({}) has been downloaded!\n\n\
         The update is saved at:\n{}\n\n\
         To install:\n\
         1. Close this application\n\
         2. Rename the .update file to pc-agent.exe\n\
         3. Restart the application",
        version, path.display()
    );
    
    let wide_message: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR::from_raw(wide_message.as_ptr()),
            w!("PC Agent Update Available"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
