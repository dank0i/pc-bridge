//! Elevation helpers (Windows).
//!
//! When pc-bridge runs elevated (so it can launch HWiNFO's admin-only driver),
//! two things misbehave and must be routed back down to *medium* integrity:
//!   - game launches would inherit admin (breaks EAC-style anti-cheat, e.g. Fortnite)
//!   - WinRT toast notifications fail from an elevated, unpackaged process
//!
//! [`is_elevated`] gates that behavior — when false (the normal, non-elevated
//! case) callers take their existing code path unchanged. [`launch_deelevated`]
//! launches a child at the shell's integrity using Explorer's token, the
//! standard "run de-elevated from an elevated process" technique.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    TokenPrimary,
};
use windows::Win32::System::Threading::{
    CREATE_PROCESS_LOGON_FLAGS, CreateProcessWithTokenW, GetCurrentProcess, OpenProcess,
    OpenProcessToken, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{PCWSTR, PWSTR};

/// True if the current process is running with an elevated (high-integrity)
/// token. Read once at startup and branched on; cheap enough to call directly.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let res = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast::<core::ffi::c_void>()),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut ret_len,
        );
        let _ = CloseHandle(token);
        res.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Launch `program` (with `args`) at the desktop shell's integrity level, so a
/// child started by an elevated pc-bridge runs *de-elevated* (medium IL) — the
/// same level the user's own double-clicks run at.
///
/// Technique: grab Explorer's process token (it runs medium-IL), duplicate it to
/// a primary token, and `CreateProcessWithTokenW` with it. Requires
/// `SeImpersonatePrivilege`, which an elevated admin process has.
pub fn launch_deelevated(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    unsafe {
        let shell = GetShellWindow();
        if shell.0.is_null() {
            bail!("no shell window (Explorer not running); cannot de-elevate launch");
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&raw mut pid));
        if pid == 0 {
            bail!("could not resolve the shell process id");
        }

        let shell_proc = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid)
            .context("OpenProcess(explorer) failed")?;

        let mut shell_token = HANDLE::default();
        let open = OpenProcessToken(
            shell_proc,
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &raw mut shell_token,
        );
        if let Err(e) = open {
            let _ = CloseHandle(shell_proc);
            return Err(e).context("OpenProcessToken(explorer) failed");
        }

        let mut primary = HANDLE::default();
        let dup = DuplicateTokenEx(
            shell_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        );
        let _ = CloseHandle(shell_token);
        let _ = CloseHandle(shell_proc);
        dup.context("DuplicateTokenEx failed")?;

        // Build the command line: argv[0] is the program, then the args, each
        // quoted per the Windows CommandLineToArgvW rules. lpApplicationName is
        // left null so the system resolves `program` via the normal search path
        // (e.g. "powershell.exe").
        let mut cmdline: Vec<u16> = Vec::new();
        append_arg(&mut cmdline, program);
        for a in args {
            cmdline.push(u16::from(b' '));
            append_arg(&mut cmdline, a);
        }
        cmdline.push(0);

        let cwd_w: Option<Vec<u16>> = cwd.map(|p| {
            p.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        });

        let si = STARTUPINFOW {
            cb: core::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        let res = CreateProcessWithTokenW(
            primary,
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            PROCESS_CREATION_FLAGS(0),
            None,
            cwd_w
                .as_ref()
                .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            &raw const si,
            &raw mut pi,
        );
        let _ = CloseHandle(primary);
        res.context("CreateProcessWithTokenW failed")?;

        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
        Ok(())
    }
}

/// Append one argument to a UTF-16 command-line buffer, quoting/escaping per the
/// `CommandLineToArgvW` rules (backslashes doubled before a quote; the whole
/// token quoted when it contains whitespace, a quote, or is empty).
fn append_arg(cmd: &mut Vec<u16>, arg: &str) {
    let quote = arg.is_empty() || arg.bytes().any(|b| b == b' ' || b == b'\t' || b == b'"');
    if quote {
        cmd.push(u16::from(b'"'));
    }
    let mut backslashes: usize = 0;
    for x in arg.encode_utf16() {
        if x == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if x == u16::from(b'"') {
                // Escape the run of backslashes plus this literal quote.
                for _ in 0..=backslashes {
                    cmd.push(u16::from(b'\\'));
                }
            }
            backslashes = 0;
        }
        cmd.push(x);
    }
    if quote {
        // Double any trailing backslashes so they don't escape the closing quote.
        for _ in 0..backslashes {
            cmd.push(u16::from(b'\\'));
        }
        cmd.push(u16::from(b'"'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(arg: &str) -> String {
        let mut buf = Vec::new();
        append_arg(&mut buf, arg);
        String::from_utf16(&buf).unwrap()
    }

    #[test]
    fn plain_arg_unquoted() {
        assert_eq!(render("hello"), "hello");
    }

    #[test]
    fn arg_with_space_is_quoted() {
        assert_eq!(render("hello world"), "\"hello world\"");
    }

    #[test]
    fn empty_arg_is_quoted() {
        assert_eq!(render(""), "\"\"");
    }

    #[test]
    fn embedded_quote_is_escaped() {
        // a"b -> "a\"b"
        assert_eq!(render("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn trailing_backslashes_doubled_when_quoted() {
        // "a b\\" -> "a b\\\\"  (two backslashes doubled to four before closing quote)
        assert_eq!(render("a b\\\\"), "\"a b\\\\\\\\\"");
    }

    #[test]
    fn backslashes_without_quote_untouched() {
        assert_eq!(render("c:\\path\\file"), "c:\\path\\file");
    }
}
