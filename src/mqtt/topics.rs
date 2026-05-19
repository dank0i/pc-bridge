//! Topic-string construction.
//!
//! HA's MQTT discovery uses a `<prefix>/<component>/<device>/<entity>/<role>`
//! convention.  All of those strings are built here so the rest of `mqtt::*`
//! never has to think about the format.  Frequently-published sensors have
//! their state and attribute topics pre-cached at startup to avoid the
//! per-publish `format!()` cost.

use std::collections::HashMap;
use std::sync::Arc;

use super::{DISCOVERY_PREFIX, MqttClient};

/// Pre-computed topic strings for frequently published sensors.
pub(super) struct CachedTopics {
    pub(super) availability: Arc<str>,
    /// Sensor state topics: sensor_name -> topic
    pub(super) sensor_state: HashMap<&'static str, Arc<str>>,
    /// Sensor attribute topics: sensor_name -> topic
    pub(super) sensor_attrs: HashMap<&'static str, Arc<str>>,
}

impl CachedTopics {
    pub(super) fn new(device_name: &str) -> Self {
        let mut sensor_state = HashMap::new();
        let mut sensor_attrs = HashMap::new();

        // Pre-cache common sensor topics
        let sensors: &[&'static str] = &[
            "runninggames",
            "lastactive",
            "screensaver",
            "display",
            "volume_level",
            "cpu_usage",
            "memory_usage",
            "steam_updating",
            "bridge_health",
            "game_catalog",
            "active_window",
            "sleep_state",
            "battery_level",
            "battery_charging",
            "gpu_usage",
            "network_throughput",
            "disk_usage",
            "system_uptime",
            "bridge_info",
            "hwinfo_diagnostic",
        ];

        for name in sensors {
            sensor_state.insert(
                *name,
                Arc::from(format!(
                    "{}/sensor/{}/{}/state",
                    DISCOVERY_PREFIX, device_name, name
                )),
            );
            sensor_attrs.insert(
                *name,
                Arc::from(format!(
                    "{}/sensor/{}/{}/attributes",
                    DISCOVERY_PREFIX, device_name, name
                )),
            );
        }

        Self {
            availability: Arc::from(format!(
                "{}/sensor/{}/availability",
                DISCOVERY_PREFIX, device_name
            )),
            sensor_state,
            sensor_attrs,
        }
    }
}

// Topic helpers - split-impl block lives here so callers in mod.rs can
// continue to use `self.sensor_topic(...)` etc. but the format strings are
// no longer scattered.  All these return owned String because rumqttc's
// publish methods consume Into<String>.
impl MqttClient {
    pub(super) fn availability_topic(&self) -> String {
        self.cached_topics.availability.to_string()
    }

    pub(super) fn availability_topic_static(device_name: &str) -> String {
        format!("{}/sensor/{}/availability", DISCOVERY_PREFIX, device_name)
    }

    /// Topic published by the HWiNFO sensor task to indicate whether HWiNFO is
    /// currently running. Used by the multi-source `availability` list on each
    /// HWiNFO-backed sensor.
    pub fn hwinfo_availability_topic(&self) -> String {
        format!(
            "{}/sensor/{}/hwinfo_availability",
            DISCOVERY_PREFIX, self.device_name
        )
    }

    pub(super) fn sensor_topic(&self, name: &str) -> String {
        // Try cache first (Arc::clone is ~1 atomic op), fall back to format for custom sensors
        if let Some(cached) = self.cached_topics.sensor_state.get(name) {
            return cached.to_string();
        }
        format!(
            "{}/sensor/{}/{}/state",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    pub(super) fn sensor_attributes_topic(&self, name: &str) -> String {
        // Try cache first, fall back to format for custom sensors
        if let Some(cached) = self.cached_topics.sensor_attrs.get(name) {
            return cached.to_string();
        }
        format!(
            "{}/sensor/{}/{}/attributes",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    pub(super) fn command_topic(&self, name: &str) -> String {
        format!(
            "{}/button/{}/{}/action",
            DISCOVERY_PREFIX, self.device_name, name
        )
    }

    /// Discovery config topic.  Used at registration time for every entity.
    ///
    /// `component` is the HA MQTT discovery component (`sensor`, `button`,
    /// `notify`, etc.).  `name` is the entity's discovery name.
    pub(super) fn config_topic(&self, component: &str, name: &str) -> String {
        format!(
            "{}/{}/{}/{}/config",
            DISCOVERY_PREFIX, component, self.device_name, name
        )
    }
}
