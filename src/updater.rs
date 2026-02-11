//! Auto-updater - checks GitHub releases on launch

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
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

/// Install update and restart the application
#[cfg(windows)]
fn install_and_restart(update_path: &PathBuf) {
    use std::process::Command;

    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to get current exe path: {}", e);
            return;
        }
    };

    let my_pid = std::process::id();

    // PowerShell script that:
    // 1. Waits for this process to exit
    // 2. Replaces the old exe with the new one
    // 3. Starts the new exe
    let ps_script = format!(
        r#"
        Start-Sleep -Milliseconds 500
        $maxWait = 30
        $waited = 0
        while ((Get-Process -Id {pid} -ErrorAction SilentlyContinue) -and ($waited -lt $maxWait)) {{
            Start-Sleep -Seconds 1
            $waited++
        }}
        Start-Sleep -Milliseconds 500
        Remove-Item -Path '{current}' -Force -ErrorAction SilentlyContinue
        Move-Item -Path '{update}' -Destination '{current}' -Force
        Start-Process -FilePath '{current}'
        "#,
        pid = my_pid,
        current = current_exe.display(),
        update = update_path.display()
    );

    info!("Launching update installer and exiting...");

    // Launch PowerShell in background to do the swap
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-WindowStyle",
        "Hidden",
        "-Command",
        &ps_script,
    ]);
    cmd.creation_flags(CREATE_NO_WINDOW);

    if let Err(e) = cmd.spawn() {
        warn!("Failed to spawn update installer: {}", e);
        return;
    }

    // Exit so the installer can replace us
    std::process::exit(0);
}

#[cfg(unix)]
fn install_and_restart(update_path: &PathBuf) {
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

    let my_pid = std::process::id();

    // Bash script to wait, replace, and restart
    let script = format!(
        r#"
        sleep 1
        while kill -0 {pid} 2>/dev/null; do sleep 1; done
        sleep 0.5
        mv -f '{update}' '{current}'
        '{current}' &
        "#,
        pid = my_pid,
        current = current_exe.display(),
        update = update_path.display()
    );

    info!("Launching update installer and exiting...");

    if let Err(e) = Command::new("bash").args(["-c", &script]).spawn() {
        warn!("Failed to spawn update installer: {}", e);
        return;
    }

    std::process::exit(0);
}
