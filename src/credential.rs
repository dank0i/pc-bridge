//! Credential encryption using Windows DPAPI.
//!
//! On Windows, MQTT passwords are encrypted with `CryptProtectData` (tied to
//! the current Windows user) and stored in a separate `mqtt_credential` file
//! alongside `userConfig.json`.  The JSON config never contains the password.
//!
//! On other platforms, passwords are stored as plaintext in the credential
//! file with restrictive permissions (0600).
//!
//! Encrypted values use the format `"DPAPI:<base64>"`.  Plain strings
//! without the prefix are treated as unencrypted (first-run / manual edit)
//! and are automatically encrypted on the next save (Windows only).

/// Prefix that marks an encrypted credential in the credential file.
#[cfg(windows)]
const DPAPI_PREFIX: &str = "DPAPI:";

/// Encrypt a plaintext credential for storage.
/// On Windows, uses DPAPI.  On other platforms, returns the value as-is.
pub fn encrypt(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    #[cfg(windows)]
    {
        match dpapi_encrypt(plaintext) {
            Ok(encoded) => format!("{DPAPI_PREFIX}{encoded}"),
            Err(e) => {
                log::warn!("DPAPI encrypt failed, storing plaintext: {e}");
                plaintext.to_string()
            }
        }
    }
    #[cfg(not(windows))]
    {
        plaintext.to_string()
    }
}

/// Decrypt a credential read from the credential file.
///
/// - Values prefixed with `DPAPI:` are decrypted via DPAPI (Windows only).
/// - Plain values are returned as-is (first-run, manual edit, or non-Windows).
///
/// Returns `Ok(plaintext)` on success, or `Err` if DPAPI decryption fails
/// (e.g. config copied to another user/machine).
pub fn decrypt(stored: &str) -> Result<String, DecryptError> {
    if stored.is_empty() {
        return Ok(String::new());
    }

    #[cfg(windows)]
    if let Some(encoded) = stored.strip_prefix(DPAPI_PREFIX) {
        return dpapi_decrypt(encoded);
    }

    // Not encrypted (plaintext) — return as-is
    Ok(stored.to_string())
}

/// Error type for credential decryption failures.
#[derive(Debug)]
pub struct DecryptError {
    pub message: String,
}

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DecryptError {}

/// Typed error for `anyhow` downcasting when credential decryption fails.
/// Used to distinguish credential errors from other config-loading errors.
#[derive(Debug)]
pub struct CredentialDecryptFailed;

impl std::fmt::Display for CredentialDecryptFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MQTT credential decryption failed")
    }
}

impl std::error::Error for CredentialDecryptFailed {}

// ── Credential file I/O ─────────────────────────────────────────────────

/// Path to the credential file alongside `userConfig.json`.
pub fn credential_path() -> anyhow::Result<std::path::PathBuf> {
    let config_path = crate::config::Config::config_path()?;
    let dir = config_path
        .parent()
        .expect("config path always has a parent");
    Ok(dir.join("mqtt_credential"))
}

/// Encrypt a plaintext password and write it to the credential file.
pub fn save_to_file(plaintext: &str) -> anyhow::Result<()> {
    let path = credential_path()?;
    if plaintext.is_empty() {
        // Remove credential file when password is cleared
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encrypted = encrypt(plaintext);
    std::fs::write(&path, &encrypted)?;

    // Restrict permissions to owner-only on Unix (file may contain plaintext on non-Windows)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Read the credential file and decrypt its contents.
pub fn load_from_file() -> Result<String, DecryptError> {
    let path = credential_path().map_err(|e| DecryptError {
        message: e.to_string(),
    })?;
    if !path.exists() {
        return Ok(String::new());
    }
    let stored = std::fs::read_to_string(&path).map_err(|e| DecryptError {
        message: format!("Failed to read credential file: {e}"),
    })?;
    let stored = stored.trim();
    if stored.is_empty() {
        return Ok(String::new());
    }
    decrypt(stored)
}

// ── Windows DPAPI implementation ────────────────────────────────────────

#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

#[cfg(windows)]
fn dpapi_encrypt(plaintext: &str) -> Result<String, DecryptError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let data_bytes = plaintext.as_bytes();
    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: data_bytes.len() as u32,
        pbData: data_bytes.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: CryptProtectData is a well-defined Windows API.
    // input_blob points to valid data for the duration of the call.
    // output_blob is populated by Windows and freed below with LocalFree.
    let result = unsafe {
        CryptProtectData(
            &mut input_blob,
            None,                      // description (optional)
            None,                      // entropy (optional)
            None,                      // reserved
            None,                      // prompt (optional)
            CRYPTPROTECT_UI_FORBIDDEN, // no UI
            &mut output_blob,
        )
    };

    if result.is_err() {
        return Err(DecryptError {
            message: format!("CryptProtectData failed: {result:?}"),
        });
    }

    // SAFETY: CryptProtectData succeeded, output_blob.pbData is valid
    // for output_blob.cbData bytes.  We copy into a Vec immediately
    // and then free the Windows-allocated buffer.
    let encrypted = unsafe {
        let slice = std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize);
        let vec = slice.to_vec();
        windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
            output_blob.pbData.cast(),
        ));
        vec
    };

    Ok(BASE64.encode(&encrypted))
}

#[cfg(windows)]
fn dpapi_decrypt(encoded: &str) -> Result<String, DecryptError> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let encrypted = BASE64.decode(encoded).map_err(|e| DecryptError {
        message: format!("Invalid base64 in DPAPI credential: {e}"),
    })?;

    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: Same pattern as encrypt — CryptUnprotectData reads from
    // input_blob and allocates output_blob.  We copy and free below.
    let result = unsafe {
        CryptUnprotectData(
            &mut input_blob,
            None, // description out (optional)
            None, // entropy (must match encrypt)
            None, // reserved
            None, // prompt (optional)
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output_blob,
        )
    };

    if result.is_err() {
        return Err(DecryptError {
            message: "DPAPI decryption failed — password may have been encrypted by a different Windows user or machine. Run with --reset-password to re-enter.".to_string(),
        });
    }

    // SAFETY: CryptUnprotectData succeeded
    let decrypted = unsafe {
        let slice = std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize);
        let vec = slice.to_vec();
        windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
            output_blob.pbData.cast(),
        ));
        vec
    };

    String::from_utf8(decrypted).map_err(|e| DecryptError {
        message: format!("Decrypted credential is not valid UTF-8: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_roundtrip() {
        assert_eq!(encrypt(""), "");
        assert_eq!(decrypt("").unwrap(), "");
    }

    #[test]
    fn test_plaintext_passthrough() {
        // Non-DPAPI strings are returned as-is
        assert_eq!(decrypt("hello").unwrap(), "hello");
    }

    #[cfg(windows)]
    #[test]
    fn test_dpapi_roundtrip() {
        let secret = "test-value-for-dpapi";
        let encrypted = encrypt(secret);
        assert!(encrypted.starts_with("DPAPI:"));
        assert_ne!(encrypted, secret);

        let decrypted = decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[cfg(windows)]
    #[test]
    fn test_dpapi_tampered_fails() {
        let encrypted = encrypt("test_password");
        let tampered = format!("DPAPI:{}x", encrypted.strip_prefix("DPAPI:").unwrap());
        assert!(decrypt(&tampered).is_err());
    }
}
