//! Power management module

// On non-Windows targets sync_mqtt functions are only exercised by tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod sync_mqtt;

#[cfg(windows)]
mod display;
#[cfg(windows)]
mod events;

#[cfg(unix)]
mod display_linux;
#[cfg(unix)]
mod events_linux;

#[cfg(windows)]
pub use display::wake_display;
#[cfg(windows)]
pub use events::PowerEventListener;

#[cfg(unix)]
pub use display_linux::wake_display;
#[cfg(unix)]
pub use events_linux::PowerEventListener;
