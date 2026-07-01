//! Sensor modules for game detection, idle tracking, and system monitoring

mod audio_device;
mod capture;
mod custom;
mod disk;
mod gpu;
mod network;
mod now_playing;
mod system;
mod uptime;
mod volume;

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
#[cfg(unix)]
mod session_linux;

pub use audio_device::AudioDeviceSensor;
pub use capture::CaptureSensor;
pub use custom::CustomSensorManager;
pub use disk::DiskSensor;
pub use gpu::GpuSensor;
pub use network::NetworkSensor;
pub use now_playing::NowPlayingSensor;
pub use system::SystemSensor;
pub use uptime::UptimeSensor;
pub use volume::VolumeSensor;

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
#[cfg(unix)]
pub use session_linux::SessionSensor;
