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

/// Minisign public key used to verify update binaries. The SHA-256 check only
/// proves the download matches the checksum the *same host* served, so a
/// compromised release host could swap both; a signature can't be forged
/// without the private key.
///
/// Setup (one time): generate a keypair with `minisign -G -p update.pub -s
/// update.sec` (keep `update.sec` offline / as a CI secret) and paste the base64
/// line from `update.pub` (the line after the comment) below.
///
/// Per platform the release ships THREE files: the binary, a `{version, sha256}`
/// `.manifest`, and the manifest's `.minisig`. The updater fetches + minisign-
/// verifies the manifest, trusts its SIGNED version for anti-rollback (a
/// compromised host can only replay an old manifest, whose version matches its
/// binary, so it can't serve an old validly-signed build under a newer tag), then
/// authenticates the downloaded binary by SHA-256 against the manifest hash. That
/// hash chain makes a separate per-binary signature (or a standalone `.sha256`)
/// redundant, so neither is shipped.
///
/// While this is empty, updates are refused (there is no signed manifest to trust).
/// Once set, a missing/invalid signature or manifest aborts the update.
const UPDATE_PUBLIC_KEY: &str = "RWSE5vU+bgt+LQ0szYtrcZL3qLst9oXVEdB/fptCnHUivRlWn1rgcw/E";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// GitHub release info (minimal fields we need). Only used by the beta
/// channel, which still queries the API; the stable channel resolves the
/// version from a CDN redirect and never deserializes this. Extra JSON fields
/// (assets, etc.) are ignored by serde.
#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    #[allow(dead_code)]
    prerelease: bool,
}

/// Create an HTTP agent configured to use native-tls (OS TLS stack).
///
/// ureq v3 defaults to Rustls even with the `native-tls` feature;
/// the provider must be set explicitly via [`TlsConfig`].
/// Root certs default to `WebPki` which requires `native-tls-webpki-roots`;
/// we use `PlatformVerifier` so Windows Schannel loads the OS cert store.
///
/// Returns a fresh agent each call - intentionally NOT static so the TLS
/// context and connection pool are freed after each update check, avoiding
/// ~500 KB+ of persistent Schannel/connection-pool memory on Windows.
fn http_agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder().tls_config(tls).build();
    ureq::Agent::new_with_config(config)
}

/// Like [`http_agent`] but does NOT follow redirects, so a 3xx response is
/// returned directly with its `Location` header intact. Used to resolve the
/// latest stable version from `github.com/.../releases/latest`, which 302s to
/// `/releases/tag/vX.Y.Z`. This path is served by GitHub's web CDN, not
/// `api.github.com`, so it is not subject to the 60-req/hour unauthenticated
/// API rate limit that blocks updates on shared-NAT networks.
fn http_agent_no_redirect() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .tls_config(tls)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Release asset filename for the current platform. These are fixed by the CI
/// release workflow, so the updater can construct download URLs directly
/// instead of enumerating assets from the API.
fn platform_asset_name() -> &'static str {
    if cfg!(windows) {
        "pc-bridge-windows.exe"
    } else {
        "pc-bridge-linux"
    }
}

/// Parse the version out of a GitHub release tag URL such as
/// `/dank0i/pc-bridge/releases/tag/v2.1.3` (the `Location` header value) or a
/// full `https://github.com/.../releases/tag/v2.1.3` URL.
///
/// Returns the version without the leading `v` (e.g. `"2.1.3"`), or `None` if
/// the final path segment isn't a `v`-prefixed numeric tag.
fn parse_version_from_release_url(url: &str) -> Option<String> {
    let tag = url.trim_end_matches('/').rsplit('/').next()?;
    let v = tag.strip_prefix('v')?;
    if v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        Some(v.to_string())
    } else {
        None
    }
}

/// Build the base download URL for a given version's release assets.
fn download_base_for(version: &str) -> String {
    format!(
        "https://github.com/{}/{}/releases/download/v{}",
        GITHUB_OWNER, GITHUB_REPO, version
    )
}

/// Clean up leftover files from a previous update.
/// Called on startup before the update check.
pub fn cleanup_old_files() {
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    // Windows: pc-bridge.exe.old
    let old_path = current_exe.with_extension("exe.old");
    if old_path.exists() {
        match std::fs::remove_file(&old_path) {
            Ok(()) => info!("Cleaned up old update file: {:?}", old_path),
            Err(e) => warn!("Failed to clean up {:?}: {}", old_path, e),
        }
    }

    // Leftover download from a failed install. Use the same name the download
    // path writes (<asset_name>.update), not current_exe.with_extension, which
    // produced a different name ("pc-bridge.update") that never matched.
    if let Some(exe_dir) = current_exe.parent() {
        let update_path = exe_dir.join(format!("{}.update", platform_asset_name()));
        if update_path.exists() {
            match std::fs::remove_file(&update_path) {
                Ok(()) => info!("Cleaned up leftover update file: {:?}", update_path),
                Err(e) => warn!("Failed to clean up {:?}: {}", update_path, e),
            }
        }
    }
}

/// Check for updates and download if available.
/// `update_channel`: "stable" (default), "beta", or "disabled".
pub async fn check_for_updates(update_channel: String) {
    if update_channel == "disabled" {
        info!("Update checking disabled by config");
        return;
    }

    // Stable resolves the version via the CDN-served redirect (no API limit).
    // Beta still needs the API, since there's no static URL for "latest
    // prerelease" - that path can hit the unauthenticated rate limit.
    let remote_version = if update_channel == "beta" {
        resolve_latest_via_api().await
    } else {
        resolve_latest_via_redirect().await
    };

    let remote_version = match remote_version {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to check for updates: {}", e);
            return;
        }
    };

    // Cheap unsigned pre-check (the tag is attacker-controllable, so this is only
    // a "maybe there's an update" gate, not the authoritative decision).
    if !is_newer_version(&remote_version, CURRENT_VERSION) {
        info!("Already up to date (v{})", CURRENT_VERSION);
        return;
    }

    info!(
        "Update available: {} -> {}",
        CURRENT_VERSION, remote_version
    );

    let asset_name = platform_asset_name();
    let base = download_base_for(&remote_version);
    let asset_url = format!("{}/{}", base, asset_name);

    // Anti-rollback: the AUTHORITATIVE version + binary hash come from the SIGNED
    // manifest, never the release tag. A compromised host can only replay a
    // previously-signed manifest, whose version matches its binary, so it can't
    // serve an old build under a newer tag.
    #[allow(clippy::const_is_empty)]
    let expected_sha256 = if UPDATE_PUBLIC_KEY.is_empty() {
        None
    } else {
        match tokio::task::spawn_blocking({
            let base = base.clone();
            move || fetch_verified_manifest(&base, asset_name)
        })
        .await
        {
            Ok(Ok(m)) => {
                if !is_newer_version(&m.version, CURRENT_VERSION) {
                    warn!(
                        "Signed manifest version {} is not newer than {} - refusing (possible rollback)",
                        m.version, CURRENT_VERSION
                    );
                    return;
                }
                Some(m.sha256)
            }
            Ok(Err(e)) => {
                warn!("Could not verify update manifest: {e} - refusing to install");
                return;
            }
            Err(e) => {
                warn!("Manifest verification task failed: {e}");
                return;
            }
        }
    };

    match download_update(&asset_url, asset_name, expected_sha256.as_deref()).await {
        Ok(update_path) => {
            info!("Update downloaded, installing...");
            install_and_restart(&update_path);
        }
        Err(e) => {
            warn!("Failed to download update: {}", e);
        }
    }
}

/// Resolve the latest STABLE version by following the `releases/latest`
/// redirect on the GitHub web host (CDN-served, no API rate limit).
async fn resolve_latest_via_redirect() -> anyhow::Result<String> {
    let url = format!(
        "https://github.com/{}/{}/releases/latest",
        GITHUB_OWNER, GITHUB_REPO
    );

    let location = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let response = http_agent_no_redirect()
            .head(&url)
            .header("User-Agent", USER_AGENT)
            .call()?;

        let status = response.status().as_u16();
        if !(300..400).contains(&status) {
            anyhow::bail!("expected redirect from releases/latest, got HTTP {status}");
        }

        response
            .headers()
            .get("location")
            .ok_or_else(|| anyhow::anyhow!("redirect had no Location header"))?
            .to_str()
            .map(str::to_string)
            .map_err(|e| anyhow::anyhow!("non-ASCII Location header: {e}"))
    })
    .await??;

    parse_version_from_release_url(&location)
        .ok_or_else(|| anyhow::anyhow!("could not parse version from redirect: {location}"))
}

/// Resolve the latest version (including prereleases) via the GitHub API.
/// Only used for the beta channel.
async fn resolve_latest_via_api() -> anyhow::Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases?per_page=1",
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
    let release = releases
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No releases found"))?;
    Ok(release.tag_name.trim_start_matches('v').to_string())
}

async fn download_update(
    url: &str,
    filename: &str,
    expected_sha256: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent dir"))?
        .to_path_buf();

    let update_path = exe_dir.join(format!("{}.update", filename));

    let url = url.to_string();
    let expected_sha256 = expected_sha256.map(String::from);
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

        // Authenticate the binary against the SIGNED manifest hash - refuse without
        // it. The manifest is minisign-signed (see fetch_verified_manifest), so a
        // SHA-256 match here transitively authenticates the binary: forging a
        // malicious binary with the same hash needs a SHA-256 collision. A separate
        // per-binary .minisig would add nothing over (signed manifest + hash), so we
        // don't ship or check one. With no embedded key, expected_sha256 is None and
        // we refuse (fail-closed) rather than install anything unverified.
        if let Some(hash) = expected_sha256 {
            verify_sha256_hex(&path, &hash)?;
        } else {
            let _ = std::fs::remove_file(&path);
            anyhow::bail!("No signed manifest hash - refusing to install unverified binary");
        }

        Ok(())
    })
    .await??;

    info!("Downloaded update to {:?}", update_path);
    Ok(update_path)
}

/// Verify a minisign signature (fetched from `sig_url`) over `bytes` using the
/// embedded [`UPDATE_PUBLIC_KEY`].
fn verify_signature_bytes(bytes: &[u8], sig_url: &str) -> anyhow::Result<()> {
    use minisign_verify::{PublicKey, Signature};

    let public_key = PublicKey::from_base64(UPDATE_PUBLIC_KEY)
        .map_err(|e| anyhow::anyhow!("invalid embedded update public key: {e}"))?;

    let sig_text = http_agent()
        .get(sig_url)
        .header("User-Agent", USER_AGENT)
        .call()?
        .body_mut()
        .read_to_string()?;
    let signature = Signature::decode(&sig_text)
        .map_err(|e| anyhow::anyhow!("malformed update signature: {e}"))?;

    public_key
        .verify(bytes, &signature, false)
        .map_err(|e| anyhow::anyhow!("update signature verification failed: {e}"))?;
    Ok(())
}

/// Signed release manifest binding a version to the binary's SHA-256. Signing
/// the (version, hash) pair - not just the binary - is what prevents rollback: a
/// compromised release host can only replay previously-signed manifests, whose
/// version genuinely matches their binary, so an old build can't be served under
/// a newer tag.
#[derive(serde::Deserialize)]
struct UpdateManifest {
    version: String,
    sha256: String,
}

/// Download + signature-verify the manifest for `asset_name` at `base`.
fn fetch_verified_manifest(base: &str, asset_name: &str) -> anyhow::Result<UpdateManifest> {
    let manifest_url = format!("{base}/{asset_name}.manifest");
    let body = http_agent()
        .get(&manifest_url)
        .header("User-Agent", USER_AGENT)
        .call()?
        .body_mut()
        .read_to_string()?;
    verify_signature_bytes(body.as_bytes(), &format!("{manifest_url}.minisig"))?;
    let manifest: UpdateManifest = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed update manifest: {e}"))?;
    Ok(manifest)
}

/// Download the `.sha256` sidecar and verify the file matches.
/// Verify `file_path`'s SHA-256 equals `expected_hex` (taken from the SIGNED
/// manifest, so this ties the downloaded binary to the signed version).
fn verify_sha256_hex(file_path: &Path, expected_hex: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let expected = expected_hex.trim().to_lowercase();
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(file_path)?;
    std::io::copy(&mut file, &mut hasher)?;
    let actual_hex = format!("{:x}", hasher.finalize());

    if actual_hex != expected {
        let _ = std::fs::remove_file(file_path);
        anyhow::bail!(
            "SHA-256 mismatch: expected {expected}, got {actual_hex} - download tampered or wrong build"
        );
    }

    info!("SHA-256 verified against signed manifest: {actual_hex}");
    Ok(())
}

fn is_newer_version(remote: &str, current: &str) -> bool {
    // Split "1.2.3-beta.4" into ([1,2,3], Some("beta.4")).
    fn parse(v: &str) -> (Vec<u32>, Option<String>) {
        let v = v.trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (v, None),
        };
        let nums = core.split('.').map_while(|s| s.parse().ok()).collect();
        (nums, pre)
    }

    let (r_nums, r_pre) = parse(remote);
    let (c_nums, c_pre) = parse(current);

    // Compare the numeric core (major.minor.patch), missing fields as 0.
    for i in 0..r_nums.len().max(c_nums.len()) {
        let r = r_nums.get(i).copied().unwrap_or(0);
        let c = c_nums.get(i).copied().unwrap_or(0);
        if r != c {
            return r > c;
        }
    }

    // Cores equal: semver precedence. A stable release outranks any pre-release
    // of the same core; two pre-releases compare by their dot identifiers.
    match (r_pre, c_pre) {
        (None, None) => false,
        (None, Some(_)) => true,  // remote stable > current pre-release
        (Some(_), None) => false, // remote pre-release < current stable
        (Some(r), Some(c)) => prerelease_gt(&r, &c),
    }
}

/// Semver pre-release precedence: numeric identifiers compare numerically and
/// rank below alphanumeric ones; more identifiers outrank a prefix.
fn prerelease_gt(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    for (ai, bi) in a.split('.').zip(b.split('.')) {
        let cmp = match (ai.parse::<u64>(), bi.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => ai.cmp(bi),
        };
        if cmp != Ordering::Equal {
            return cmp == Ordering::Greater;
        }
    }
    a.split('.').count() > b.split('.').count()
}

/// Install update and restart the application.
///
/// Uses the rename trick - no external processes (cmd.exe, PowerShell, bash).
/// A running exe on Windows can be renamed but not overwritten, so:
/// 1. Rename `pc-bridge.exe` → `pc-bridge.exe.old`
/// 2. Rename `pc-bridge.exe.update` → `pc-bridge.exe`
/// 3. Spawn the new `pc-bridge.exe`
/// 4. Exit - new instance cleans up `.old` on startup via `cleanup_old_files()`
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
    if old_path.exists()
        && let Err(e) = std::fs::remove_file(&old_path)
    {
        warn!("Failed to remove old update file: {}", e);
        // Non-fatal - try to rename anyway, it may work if the old .old is unlocked
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
                "Failed to restore original exe: {} - manual intervention needed",
                e2
            );
        }
        return;
    }

    info!("Update installed, starting new version...");

    // Step 3: Spawn the new exe, forwarding the CLI args we were started with
    // (e.g. --config-dir / service flags) so the restart preserves them.
    let mut cmd = Command::new(&current_exe);
    cmd.args(std::env::args_os().skip(1));
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

    // Replace in place - Unix allows this while running
    if let Err(e) = std::fs::rename(update_path, &current_exe) {
        warn!("Failed to replace binary: {}", e);
        return;
    }

    info!("Update installed, starting new version...");

    if let Err(e) = Command::new(&current_exe)
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        warn!("Failed to start updated binary: {}", e);
        return;
    }

    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_from_location_header() {
        // The bare path form GitHub returns in the Location header.
        assert_eq!(
            parse_version_from_release_url("/dank0i/pc-bridge/releases/tag/v2.1.3").as_deref(),
            Some("2.1.3")
        );
    }

    #[test]
    fn test_parse_version_from_full_url() {
        assert_eq!(
            parse_version_from_release_url(
                "https://github.com/dank0i/pc-bridge/releases/tag/v2.1.3"
            )
            .as_deref(),
            Some("2.1.3")
        );
    }

    #[test]
    fn test_parse_version_tolerates_trailing_slash() {
        assert_eq!(
            parse_version_from_release_url("https://github.com/x/y/releases/tag/v10.20.30/")
                .as_deref(),
            Some("10.20.30")
        );
    }

    #[test]
    fn test_parse_version_rejects_non_version_tags() {
        // No "v" prefix, or non-numeric after "v" → not a version we recognise.
        assert_eq!(parse_version_from_release_url("/x/y/releases/latest"), None);
        assert_eq!(
            parse_version_from_release_url("/x/y/releases/tag/nightly"),
            None
        );
        assert_eq!(
            parse_version_from_release_url("/x/y/releases/tag/vNext"),
            None
        );
    }

    #[test]
    fn test_download_base_for_constructs_expected_url() {
        assert_eq!(
            download_base_for("2.1.3"),
            "https://github.com/dank0i/pc-bridge/releases/download/v2.1.3"
        );
    }

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
    fn test_is_newer_version_prerelease_same_base_not_newer() {
        // Pre-release of the same base version must NOT trigger an update.
        // "4.0.3-beta" → [4,0,3] (stops at "beta"), equal to [4,0,3].
        assert!(!is_newer_version("4.0.3-beta", "4.0.3"));
        // "4.0.3-beta.1" → [4,0,3] (stops at "beta"), NOT [4,0,3,1].
        assert!(!is_newer_version("4.0.3-beta.1", "4.0.3"));
        assert!(!is_newer_version("4.0.3-rc1", "4.0.3"));
    }

    #[test]
    fn test_is_newer_version_prerelease_higher_base() {
        // Pre-release of a HIGHER base version is still newer.
        assert!(is_newer_version("4.1.0-beta", "4.0.3"));
        assert!(is_newer_version("5.0.0-alpha", "4.0.3"));
    }

    #[test]
    fn test_is_newer_version_extra_segments() {
        // [2,7,0,1] > [2,7,0] due to longer length after equal prefix
        assert!(is_newer_version("2.7.0.1", "2.7.0"));
    }

    #[test]
    fn test_is_newer_version_shorter_not_newer() {
        // [2,7] vs [2,7,0] - equal prefix but shorter, so not newer
        assert!(!is_newer_version("2.7", "2.7.0"));
    }

    #[test]
    fn test_embedded_public_key_verifies_known_signature() {
        use minisign_verify::{PublicKey, Signature};
        // A minisign signature over b"hello pc-bridge\n" produced by the matching
        // secret key. Guards that UPDATE_PUBLIC_KEY, the .minisig format, and the
        // verify path all agree - a mismatch would fail-closed and break real
        // updates. No-op if signing is disabled (empty key).
        #[allow(clippy::const_is_empty)]
        if UPDATE_PUBLIC_KEY.is_empty() {
            return;
        }
        let sig = "untrusted comment: signature from minisign secret key\n\
RUSE5vU+bgt+LV0n2mFj1Ju2MB8aM1pfZKkeVSL2vns/PAgF/zDxnyXkM07gHaiIOHZtV+XiOx3Fii6B7h7jXDksFFmNM29gYA4=\n\
trusted comment: timestamp:1782914726\tfile:sigtest.bin\thashed\n\
cS8yVMu/941m9iEXeSa/T6K+kerWtz+h/yjhdVGQMs4MccQmzxW1km/6/aOXbo4G0D5dvSk0AgzfVD8jlJc/AQ==\n";
        let pk = PublicKey::from_base64(UPDATE_PUBLIC_KEY).expect("valid embedded pubkey");
        let signature = Signature::decode(sig).expect("valid signature");
        pk.verify(b"hello pc-bridge\n", &signature, false)
            .expect("embedded key must verify a real minisign signature");
    }

    #[test]
    fn test_is_newer_version_prerelease_ordering() {
        // Same core: a later pre-release outranks an earlier one (beta channel).
        assert!(is_newer_version("2.8.0-beta.2", "2.8.0-beta.1"));
        assert!(!is_newer_version("2.8.0-beta.1", "2.8.0-beta.2"));
        // A stable release outranks any pre-release of the same core...
        assert!(is_newer_version("2.8.0", "2.8.0-beta.1"));
        // ...and a pre-release never outranks the matching stable (was the bug:
        // "2.8.0-1" used to parse as [2,8,0,1] and count as newer than 2.8.0).
        assert!(!is_newer_version("2.8.0-1", "2.8.0"));
    }

    // ===== Update channel logic =====

    // (test_current_version_not_empty removed - env!("CARGO_PKG_VERSION") is
    // a non-empty const, so the assertion was a tautology that clippy
    // correctly flagged. The semver shape test below already covers what
    // matters.)

    #[test]
    fn test_current_version_is_valid_semver() {
        let parts: Vec<&str> = CURRENT_VERSION.split('.').collect();
        assert!(parts.len() >= 3, "Version must be x.y.z format");
        for part in &parts {
            assert!(part.parse::<u64>().is_ok(), "Non-numeric segment: {part}");
        }
    }
}
