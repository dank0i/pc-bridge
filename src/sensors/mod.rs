//! Sensor modules for game detection, idle tracking, and system monitoring

#[cfg(windows)]
mod audio_device;
mod custom;
mod disk;
mod gpu;
mod network;
mod system;
mod uptime;

pub mod hwinfo;

#[cfg(windows)]
mod games;
#[cfg(windows)]
mod idle;
#[cfg(windows)]
mod process_watcher;
#[cfg(windows)]
mod session;
mod steam;

#[cfg(unix)]
mod games_linux;
#[cfg(unix)]
mod idle_linux;

#[cfg(windows)]
pub use audio_device::AudioDeviceSensor;
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
#[cfg(windows)]
pub use session::SessionSensor;
pub use steam::SteamSensor;

#[cfg(unix)]
pub use games_linux::GameSensor;
#[cfg(unix)]
pub(crate) use games_linux::current_process_names;
#[cfg(unix)]
pub use idle_linux::IdleSensor;
