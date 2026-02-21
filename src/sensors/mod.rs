//! Sensor modules for game detection, idle tracking, and system monitoring

mod custom;
mod system;

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;
#[cfg(windows)]
mod process_watcher;
#[cfg(windows)]
mod steam;

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
#[cfg(windows)]
pub use process_watcher::ProcessWatcher;
#[cfg(windows)]
pub use steam::SteamSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
