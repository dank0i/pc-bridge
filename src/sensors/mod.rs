//! Sensor modules for game detection, idle tracking, and system monitoring

mod memory;
mod custom;
mod system;

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;

#[cfg(unix)]
mod games_linux;
#[cfg(unix)]
mod idle_linux;

pub use custom::CustomSensorManager;
pub use system::SystemSensor;

#[cfg(windows)]
pub use games::GameSensor;
#[cfg(windows)]
pub use idle::IdleSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
