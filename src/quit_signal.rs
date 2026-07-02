//! Cross-process "quit the agent" signal.
//!
//! The settings window runs as a separate `--ui` process, so the sidebar Quit
//! button needs a way to stop the headless agent too. On Windows the agent waits
//! on a named event and fires its global shutdown when the window sets it; both
//! processes then exit cleanly. Off Windows the agent runs as a service, so this
//! is a no-op and the button just closes the settings window.

#[cfg(windows)]
const QUIT_EVENT: windows::core::PCWSTR = windows::core::w!("Local\\pc-bridge-agent-quit");

/// Agent side: watch for a quit request from the settings window and fire the
/// global shutdown. Runs on its own thread (the OS reclaims it on process exit),
/// so it never blocks the runtime's own shutdown.
#[cfg(windows)]
pub fn watch_for_quit(shutdown_tx: tokio::sync::broadcast::Sender<()>) {
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    std::thread::spawn(move || unsafe {
        // Manual-reset event so a single SetEvent reliably wakes the wait.
        let Ok(event) = CreateEventW(None, true, false, QUIT_EVENT) else {
            return;
        };
        // WAIT_OBJECT_0 == 0: the event was signaled.
        if WaitForSingleObject(event, INFINITE).0 == 0 {
            log::info!("Quit requested from settings window; shutting down");
            let _ = shutdown_tx.send(());
        }
    });
}

#[cfg(not(windows))]
pub fn watch_for_quit(_shutdown_tx: tokio::sync::broadcast::Sender<()>) {}

/// Settings-window side: ask the running agent to shut down. Returns true if an
/// agent was running (the event existed) and was signaled.
#[cfg(windows)]
pub fn request_agent_quit() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent};
    // EVENT_MODIFY_STATE (0x0002) is enough to SetEvent.
    let access = SYNCHRONIZATION_ACCESS_RIGHTS(0x0002);
    unsafe {
        match OpenEventW(access, false, QUIT_EVENT) {
            Ok(handle) => {
                let _ = SetEvent(handle);
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(windows))]
pub fn request_agent_quit() -> bool {
    false
}
