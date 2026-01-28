//! Command execution module

#[cfg(windows)]
mod executor;
#[cfg(windows)]
mod launcher;

#[cfg(unix)]
mod executor_linux;

#[cfg(windows)]
pub use executor::CommandExecutor;

#[cfg(unix)]
pub use executor_linux::CommandExecutor;
