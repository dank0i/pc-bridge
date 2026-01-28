//! Sensor modules for game detection and idle tracking

mod games;
mod idle;

pub use games::GameSensor;
pub use idle::IdleSensor;
