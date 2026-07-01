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
/// On Windows, uses DPAPI. On other platforms, returns the value as-is.
///
/// On Windows a DPAPI failure is fatal (returns `Err`) rather than silently
/// downgrading the password to plaintext on disk.
pub fn encrypt(plaintext: &str) -> anyhow::Result<String> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    #[cfg(windows)]
    {
        let encoded = dpapi_encrypt(plaintext).map_err(|e| {
            anyhow::anyhow!("DPAPI encrypt failed (refusing to store plaintext): {e}")
        })?;
        Ok(format!("{DPAPI_PREFIX}{encoded}"))
    }
    #[cfg(not(windows))]
    {
        Ok(plaintext.to_string())
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

    // Not encrypted (plaintext) - return as-is
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
    let encrypted = encrypt(plaintext)?;
    // Atomic write with owner-only perms set before any bytes hit disk (the file
    // may hold plaintext on non-Windows).
    crate::fsutil::write_atomic(&path, encrypted.as_bytes(), Some(0o600))?;
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
    // Windows stores "DPAPI:<base64>" - whitespace-insensitive, so a full trim is
    // safe and salvages a manually-edited file with stray spaces/tabs. On other
    // platforms the stored value is the plaintext password, so strip ONLY a trailing
    // newline (a manual-edit artifact): a full trim would corrupt a password with
    // legitimate leading/trailing spaces.
    #[cfg(windows)]
    let stored = stored.trim();
    #[cfg(not(windows))]
    let stored = stored.trim_end_matches(['\n', '\r']);
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
    // SAFETY (cast_mut): CryptProtectData's pbData is typed *mut u8 in the
    // Win32 ABI but the function does NOT mutate the input buffer - it
    // reads cbData bytes for encryption.  Casting our &[u8] through *const
    // → *mut is required by the FFI signature only.  data_bytes outlives
    // the call (it borrows from `plaintext`, which lives for the function).
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: data_bytes.len() as u32,
        pbData: data_bytes.as_ptr().cast_mut(),
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
            &raw const input_blob,
            None,                      // description (optional)
            None,                      // entropy (optional)
            None,                      // reserved
            None,                      // prompt (optional)
            CRYPTPROTECT_UI_FORBIDDEN, // no UI
            &raw mut output_blob,
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

    // SAFETY (cast_mut): Same FFI ABI quirk as encrypt - CryptUnprotectData
    // does not mutate input, the *mut is only required by the signature.
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr().cast_mut(),
    };
    let mut output_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: Same pattern as encrypt - CryptUnprotectData reads from
    // input_blob and allocates output_blob.  We copy and free below.
    let result = unsafe {
        CryptUnprotectData(
            &raw const input_blob,
            None, // description out (optional)
            None, // entropy (must match encrypt)
            None, // reserved
            None, // prompt (optional)
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output_blob,
        )
    };

    if result.is_err() {
        return Err(DecryptError {
            message: "DPAPI decryption failed - password may have been encrypted by a different Windows user or machine. Run with --reset-password to re-enter.".to_string(),
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
        assert_eq!(encrypt("").unwrap(), "");
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
        let encrypted = encrypt(secret).unwrap();
        assert!(encrypted.starts_with("DPAPI:"));
        assert_ne!(encrypted, secret);

        let decrypted = decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[cfg(windows)]
    #[test]
    fn test_dpapi_tampered_fails() {
        let encrypted = encrypt("test_password").unwrap();
        let tampered = format!("DPAPI:{}x", encrypted.strip_prefix("DPAPI:").unwrap());
        assert!(decrypt(&tampered).is_err());
    }
}
