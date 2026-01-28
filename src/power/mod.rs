//! Power management module

mod events;
mod display;

pub use events::PowerEventListener;
pub use display::wake_display;
