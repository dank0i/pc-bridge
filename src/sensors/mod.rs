//! Sensor modules for game detection and idle tracking

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;

#[cfg(unix)]
mod games_linux;
#[cfg(unix)]
mod idle_linux;

#[cfg(windows)]
pub use games::GameSensor;
#[cfg(windows)]
pub use idle::IdleSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
