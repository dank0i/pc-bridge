//! Sensor modules for game detection and idle tracking

mod memory;

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;

#[cfg(unix)]
mod games_linux;
#[cfg(unix)]
mod idle_linux;

pub use memory::MemorySensor;

#[cfg(windows)]
pub use games::GameSensor;
#[cfg(windows)]
pub use idle::IdleSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
