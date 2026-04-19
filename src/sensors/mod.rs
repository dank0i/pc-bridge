//! Sensor modules for game detection, idle tracking, and system monitoring

mod custom;
mod disk;
mod gpu;
mod network;
mod system;
mod uptime;

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;
#[cfg(windows)]
mod process_watcher;
mod steam;

#[cfg(unix)]
mod games_linux;
#[cfg(unix)]
mod idle_linux;

pub use custom::CustomSensorManager;
pub use disk::DiskSensor;
pub use gpu::GpuSensor;
pub use network::NetworkSensor;
pub use system::SystemSensor;
pub use uptime::UptimeSensor;

#[cfg(windows)]
pub use games::GameSensor;
#[cfg(windows)]
pub use idle::IdleSensor;
#[cfg(windows)]
pub use process_watcher::ProcessWatcher;
pub use steam::SteamSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
