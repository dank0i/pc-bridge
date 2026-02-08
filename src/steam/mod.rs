//! Steam game auto-discovery
//!
//! Parses Steam's VDF files to auto-discover installed games and their executables.
//! Uses memory-mapped I/O and cached indexing for minimal overhead.

mod appinfo;
mod discovery;
mod vdf;

pub use discovery::SteamGameDiscovery;
