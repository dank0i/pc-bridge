//! Steam game auto-discovery
//! 
//! Parses Steam's VDF files to auto-discover installed games and their executables.
//! Uses memory-mapped I/O and cached indexing for minimal overhead.

mod vdf;
mod appinfo;
mod discovery;

pub use discovery::SteamGameDiscovery;
