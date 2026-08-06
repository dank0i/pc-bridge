//! Power management module

// On non-Windows targets sync_mqtt functions are only exercised by tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod sync_mqtt;

#[cfg(windows)]
mod display;
#[cfg(windows)]
mod display_attached;
#[cfg(windows)]
mod events;

#[cfg(unix)]
mod display_attached_linux;
#[cfg(unix)]
mod display_linux;
#[cfg(unix)]
mod events_linux;

#[cfg(windows)]
pub use display::{monitor_off, observed_display_state, query_display_state, wake_display};
#[cfg(windows)]
pub use display_attached::DisplayAttachedSensor;
#[cfg(windows)]
pub use events::PowerEventListener;

#[cfg(target_os = "linux")]
pub use display_attached_linux::DisplayAttachedSensor;
#[cfg(unix)]
pub use display_linux::{monitor_off, observed_display_state, query_display_state, wake_display};
#[cfg(unix)]
pub use events_linux::PowerEventListener;
