//! Power management module

#[cfg(windows)]
mod events;
#[cfg(windows)]
mod display;

#[cfg(unix)]
mod events_linux;
#[cfg(unix)]
mod display_linux;

#[cfg(windows)]
pub use events::PowerEventListener;
#[cfg(windows)]
pub use display::wake_display;

#[cfg(unix)]
pub use events_linux::PowerEventListener;
#[cfg(unix)]
pub use display_linux::wake_display;
