//! Home Assistant MQTT discovery payload types.
//!
//! These types are serialized to JSON and published on `homeassistant/+/+/config`
//! topics so HA's MQTT integration auto-creates the entities.  Construction
//! happens in `discovery.rs`; this file only owns the wire format.

use serde::Serialize;
use std::sync::Arc;

/// Home Assistant MQTT Discovery payload
#[derive(Serialize)]
pub(super) struct HADiscoveryPayload {
    pub(super) name: String,
    pub(super) unique_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) command_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) availability_topic: Option<String>,
    /// Multi-source availability list (used instead of `availability_topic`
    /// when a sensor depends on more than one online signal - e.g. pc-bridge
    /// LWT AND HWiNFO running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) availability: Option<Vec<AvailabilityEntry>>,
    /// "all" (default in HA) or "any"; only meaningful with `availability`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) availability_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) json_attributes_topic: Option<String>,
    /// Shared device info - Arc avoids cloning per-entity.
    pub(super) device: Arc<HADevice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) device_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unit_of_measurement: Option<String>,
    /// Tells HA's recorder to populate long-term Statistics tables
    /// (kept forever as 5min/hour/day mean/min/max). Auto-derived from
    /// device_class + unit_of_measurement at registration time - see
    /// `derive_state_class`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state_class: Option<String>,
}

/// One entry in HA's multi-source `availability` list.
#[derive(Serialize)]
pub(super) struct AvailabilityEntry {
    pub(super) topic: String,
}

#[derive(Serialize, Clone)]
pub(super) struct HADevice {
    pub(super) identifiers: Vec<String>,
    pub(super) name: String,
    pub(super) model: String,
    pub(super) manufacturer: String,
    pub(super) sw_version: String,
}

/// Pick the right HA `state_class` for a numeric sensor so it ends up in the
/// long-term Statistics tables. `None` means "not a measurement" - string
/// enums, timestamps, and buttons skip this.
pub(super) fn derive_state_class(device_class: Option<&str>, unit: Option<&str>) -> Option<String> {
    // Timestamps and string-enum sensors don't aggregate as means/min/max.
    if matches!(device_class, Some("timestamp" | "enum")) {
        return None;
    }
    // Heuristic: anything with a unit is a numeric measurement.
    if unit.is_some() {
        return Some("measurement".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_state_class_numeric_sensors_get_measurement() {
        assert_eq!(
            derive_state_class(None, Some("%")),
            Some("measurement".to_string()),
        );
        assert_eq!(
            derive_state_class(Some("power"), Some("W")),
            Some("measurement".to_string()),
        );
        assert_eq!(
            derive_state_class(Some("temperature"), Some("°C")),
            Some("measurement".to_string()),
        );
    }

    #[test]
    fn derive_state_class_string_sensors_skip() {
        // Sleep state / display state / running games - no unit, no state_class
        assert_eq!(derive_state_class(None, None), None);
    }

    #[test]
    fn derive_state_class_timestamps_skip() {
        // lastactive uses device_class=timestamp; the value isn't a measurement
        assert_eq!(derive_state_class(Some("timestamp"), None), None);
    }
}
