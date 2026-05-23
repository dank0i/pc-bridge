//! HWiNFO64 → Home Assistant bridge sensor task.
//!
//! Lazily polls `Global\HWiNFO_SENS_SM2` every 500 ms by reading just the
//! `pollTime` field. When HWiNFO advances pollTime, we do a full parse and
//! republish the 15 mapped sensor values (if changed beyond a threshold or
//! 30 s stale).
//!
//! Mid-session HWiNFO start/stop is auto-detected: we try `HwInfoClient::open()`
//! every tick while the client is `None`, and we drop the client (publishing
//! `hwinfo_availability=offline`) if `read_poll_time` returns `None`.
//!
//! The substring-matching logic lives in pure functions (`match_reading`,
//! `MATCH_RULES`) so it can be unit-tested without Win32.

// The Windows-only `HwInfoSensor` is the sole consumer of `MATCH_RULES`,
// `match_reading`, `threshold_for`, and `decimals_for`. On non-Windows the
// binary path never reaches them, but the tests still do - so suppress the
// dead-code lint on those targets without masking issues on Windows.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::hwinfo::{Reading, Snapshot};

/// Maximum number of sensor names and labels to include in the diagnostic
/// payload. Bounded so a machine with hundreds of sensors doesn't produce a
/// 100 KB MQTT message every 5 seconds.
const DIAGNOSTIC_SAMPLE_CAP: usize = 16;

/// The result of a single snapshot attempt, as exposed to the diagnostic
/// builder. `Ok` carries a borrowed `Snapshot`; `Err` carries the human
/// formatted error string.
pub enum DiagnosticInput<'a> {
    Ok(&'a Snapshot),
    Err(&'a str),
    /// No client open yet (HWiNFO not running, or never successfully opened).
    NotOpen,
}

/// Diagnostic payload built from a snapshot attempt + match results.
///
/// `state` is the short summary that goes into the MQTT state topic.
/// `attributes` is the JSON object delivered to HA via `json_attributes_topic`.
pub struct DiagnosticPayload {
    pub state: String,
    pub attributes: serde_json::Value,
}

/// Build the diagnostic payload published to
/// `homeassistant/sensor/<device>/hwinfo_diagnostic/state` (and matching
/// attributes topic). Pure function - exercised by unit tests below.
///
/// `matched_keys`/`unmatched_keys` describe how the `MATCH_RULES` table mapped
/// against the snapshot's readings (or the last-known-good snapshot when this
/// call is for an error/not-open state - in which case both should be empty).
pub fn build_diagnostic_payload(
    input: &DiagnosticInput<'_>,
    view_size_bytes: usize,
    matched_keys: &[&str],
    unmatched_keys: &[&str],
) -> DiagnosticPayload {
    let total_rules = MATCH_RULES.len();
    match input {
        DiagnosticInput::Ok(snap) => {
            let sensor_names = sample_sensor_names(&snap.readings, DIAGNOSTIC_SAMPLE_CAP);
            let labels = sample_labels(&snap.readings, DIAGNOSTIC_SAMPLE_CAP);
            let sensors_count = unique_sensor_count(&snap.readings);
            let readings_count = snap.readings.len();
            let attributes = serde_json::json!({
                "snapshot_ok": true,
                "error": serde_json::Value::Null,
                "sensors_count": sensors_count,
                "readings_count": readings_count,
                "view_size_bytes": view_size_bytes,
                "first_sensor_names": sensor_names,
                "sample_labels": labels,
                "matched_count": matched_keys.len(),
                "unmatched_keys": unmatched_keys,
            });
            let state = format!("ok: {}/{} matched", matched_keys.len(), total_rules);
            DiagnosticPayload { state, attributes }
        }
        DiagnosticInput::Err(msg) => {
            let attributes = serde_json::json!({
                "snapshot_ok": false,
                "error": msg,
                "sensors_count": 0,
                "readings_count": 0,
                "view_size_bytes": view_size_bytes,
                "first_sensor_names": Vec::<String>::new(),
                "sample_labels": Vec::<String>::new(),
                "matched_count": 0,
                "unmatched_keys": all_rule_keys(),
            });
            // Truncate the error to keep state value under HA's 255-char cap.
            let trimmed: String = msg.chars().take(200).collect();
            let state = format!("err: {}", trimmed);
            DiagnosticPayload { state, attributes }
        }
        DiagnosticInput::NotOpen => {
            let attributes = serde_json::json!({
                "snapshot_ok": false,
                "error": "hwinfo shared memory not open",
                "sensors_count": 0,
                "readings_count": 0,
                "view_size_bytes": view_size_bytes,
                "first_sensor_names": Vec::<String>::new(),
                "sample_labels": Vec::<String>::new(),
                "matched_count": 0,
                "unmatched_keys": all_rule_keys(),
            });
            DiagnosticPayload {
                state: "err: shared memory not open".to_string(),
                attributes,
            }
        }
    }
}

/// First `cap` distinct sensor names encountered while scanning readings.
fn sample_sensor_names(readings: &[Reading], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in readings {
        if out.iter().any(|n| n == &r.sensor_name) {
            continue;
        }
        out.push(r.sensor_name.clone());
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// First `cap` labels (in reading order, not deduplicated - labels reveal what
/// HWiNFO is reporting, and duplicates are informative).
fn sample_labels(readings: &[Reading], cap: usize) -> Vec<String> {
    readings.iter().take(cap).map(|r| r.label.clone()).collect()
}

/// Count of distinct sensor names in the snapshot.
fn unique_sensor_count(readings: &[Reading]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for r in readings {
        if !seen.iter().any(|n| *n == r.sensor_name) {
            seen.push(&r.sensor_name);
        }
    }
    seen.len()
}

/// All keys from `MATCH_RULES`, in declaration order.
fn all_rule_keys() -> Vec<&'static str> {
    MATCH_RULES.iter().map(|r| r.key).collect()
}

/// MQTT discovery key for the diagnostic sensor. Kept here so both the
/// publisher (in the `win` module) and the discovery registration in
/// `crate::mqtt` reference the same identifier.
pub const DIAGNOSTIC_KEY: &str = "hwinfo_diagnostic";

/// Per-key thresholds for change-based publishing.
///
/// Returns the absolute delta below which we suppress publishing (unless the
/// 30-second heartbeat fires). Derived from the suffix heuristic in the spec.
pub fn threshold_for(key: &str) -> f64 {
    if key == "framerate" {
        return 5.0;
    }
    if key.ends_with("_temp") {
        return 0.5;
    }
    if key.ends_with("_power") {
        return 5.0;
    }
    if key.ends_with("_clock") {
        return 50.0;
    }
    if key.ends_with("_load") || key.ends_with("_usage") || key.ends_with("_usage_pct") {
        return 2.0;
    }
    if key.ends_with("_rpm") || key.starts_with("case_fan_") {
        return 100.0;
    }
    // Default: any non-trivial change.
    0.1
}

/// Decimal-places spec per-key (used when formatting state strings).
pub fn decimals_for(key: &str) -> usize {
    let is_integer_like = key.ends_with("_clock")
        || key.ends_with("_rpm")
        || key.starts_with("case_fan_")
        || key == "framerate";
    usize::from(!is_integer_like)
}

/// One rule in the match table.
#[derive(Debug, Clone, Copy)]
pub struct MatchRule {
    pub key: &'static str,
    /// Sensor-name substrings to match (case-insensitive). First match wins.
    /// Empty slice = match any sensor (used for `cpu_total_usage`).
    pub sensor_substrings: &'static [&'static str],
    /// Label substrings to match (case-insensitive). First match wins.
    pub label_substrings: &'static [&'static str],
    /// Label substrings to EXCLUDE (case-insensitive). Useful when one label
    /// is a prefix of another (e.g. "GPU Clock" vs "GPU Memory Clock").
    pub label_excludes: &'static [&'static str],
    /// If set, the reading's szUnit must end with this string. Used to pick the
    /// percentage variant of `GPU Memory Usage` over the MB variant.
    pub unit_suffix: Option<&'static str>,
}

/// Hardcoded sensor → HWiNFO match table.
pub const MATCH_RULES: &[MatchRule] = &[
    MatchRule {
        key: "cpu_package_temp",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU (Tctl/Tdie)", "CPU Package"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "cpu_package_power",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU Package Power", "CPU PPT"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "cpu_soc_power",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU SoC Power", "SoC Power"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "cpu_effective_clock",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["Core Effective Clock (avg)", "CPU Clock"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        // CPU Usage can come from any sensor (System / OS / per-CPU)
        key: "cpu_total_usage",
        sensor_substrings: &[],
        label_substrings: &["Total CPU Usage"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Temperature"],
        label_excludes: &["Hot Spot", "Memory"],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_hotspot_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Hot Spot Temperature"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_memory_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Junction Temperature", "GPU Memory Temperature"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_power",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Power (Total)", "GPU Total Board Power", "GPU Power"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_core_clock",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Clock"],
        label_excludes: &["Memory"],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_memory_clock",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Clock"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_core_load",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Core Load", "GPU Utilization"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_fan_rpm",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Fan"],
        label_excludes: &[],
        unit_suffix: None,
    },
    MatchRule {
        key: "gpu_vram_usage_pct",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Usage"],
        label_excludes: &[],
        unit_suffix: Some("%"),
    },
    MatchRule {
        key: "framerate",
        sensor_substrings: &["rivatuner", "rtss", "framerate", "presentmon"],
        label_substrings: &["Framerate"],
        label_excludes: &[],
        unit_suffix: None,
    },
    // Motherboard SuperIO sensors. Sensor name varies wildly across boards
    // (ITE IT8689E, Nuvoton NCT6798D, etc.), so these rules use empty sensor
    // filter and lean on label uniqueness + unit_suffix for disambiguation.
    // The "CPU" / "System 1" labels also appear as temperatures elsewhere in
    // HWiNFO; the RPM unit_suffix scopes them to the fan readings.
    MatchRule {
        key: "case_fan_cpu",
        sensor_substrings: &[],
        label_substrings: &["CPU"],
        label_excludes: &["CPU_OPT"],
        unit_suffix: Some("RPM"),
    },
    MatchRule {
        key: "case_fan_cpu_opt",
        sensor_substrings: &[],
        label_substrings: &["CPU_OPT"],
        label_excludes: &[],
        unit_suffix: Some("RPM"),
    },
    MatchRule {
        key: "case_fan_system_1",
        sensor_substrings: &[],
        label_substrings: &["System 1"],
        label_excludes: &[],
        unit_suffix: Some("RPM"),
    },
    MatchRule {
        key: "case_fan_system_2",
        sensor_substrings: &[],
        label_substrings: &["System 2"],
        label_excludes: &[],
        unit_suffix: Some("RPM"),
    },
    MatchRule {
        key: "vrm_temp",
        sensor_substrings: &[],
        label_substrings: &["VRM MOS", "VRM Temperature", "VRM Temp"],
        label_excludes: &[],
        unit_suffix: Some("°C"),
    },
];

/// Zero-allocation case-insensitive substring search for ASCII needles.
///
/// HWiNFO labels are ASCII (the only non-ASCII bytes appear in `szUnit`, e.g.
/// "°C", and we don't match those substring-wise). For our needles (`gpu`,
/// `cpu`, `9800x3d`, etc.) ASCII case folding via `eq_ignore_ascii_case` is
/// exact. Multi-byte UTF-8 bytes in haystacks safely fail the byte-window
/// comparison; we never split a codepoint or claim a false match.
fn contains_icase(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Find a `Reading` matching the given criteria. Substring matches are
/// case-insensitive (ASCII-fold). The first label-substring match (in declared
/// order) wins.
///
/// Zero-alloc on the hot path: needles and haystacks are compared via byte
/// windows; no temporary `String`s are produced.
///
/// * `sensor_substrings` empty → match any sensor name (used for `cpu_total_usage`)
/// * Substring priority is "first listed wins" for both sensor and label.
pub fn match_reading<'a>(
    readings: &'a [Reading],
    sensor_substrings: &[&str],
    label_substrings: &[&str],
    label_excludes: &[&str],
    unit_suffix: Option<&str>,
) -> Option<&'a Reading> {
    for label_sub in label_substrings {
        for reading in readings {
            if !contains_icase(&reading.label, label_sub) {
                continue;
            }

            // Exclude list
            if label_excludes
                .iter()
                .any(|e| contains_icase(&reading.label, e))
            {
                continue;
            }

            // Sensor filter
            if !sensor_substrings.is_empty()
                && !sensor_substrings
                    .iter()
                    .any(|s| contains_icase(&reading.sensor_name, s))
            {
                continue;
            }

            // Unit suffix filter (exact, ASCII end-with - units include "°C"
            // which is non-ASCII but `str::ends_with` is byte-correct on the
            // exact `Some("%")` cases we use today).
            if let Some(suffix) = unit_suffix
                && !reading.unit.ends_with(suffix)
            {
                continue;
            }

            return Some(reading);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Windows-only task
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub use win::HwInfoSensor;

#[cfg(windows)]
mod win {
    use super::{
        DIAGNOSTIC_KEY, DiagnosticInput, DiagnosticPayload, MATCH_RULES, build_diagnostic_payload,
        decimals_for, match_reading, threshold_for,
    };
    use crate::AppState;
    use crate::hwinfo::{HwInfoClient, Reading, Snapshot};
    use log::{debug, info, warn};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::time::{Duration, MissedTickBehavior, interval};

    const HEARTBEAT_SECS: u64 = 30;
    /// Diagnostic publish interval. We rebuild the payload on every snapshot
    /// but only flush to MQTT this often, to keep the broker traffic boring.
    const DIAGNOSTIC_INTERVAL_SECS: u64 = 5;

    pub struct HwInfoSensor {
        state: Arc<AppState>,
    }

    impl HwInfoSensor {
        pub fn new(state: Arc<AppState>) -> Self {
            Self { state }
        }

        pub async fn run(self) {
            let config = self.state.config.read().await;
            if !config.features.hwinfo_sensor {
                return;
            }
            drop(config);

            let mut tick = interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut shutdown_rx = self.state.shutdown_tx.subscribe();
            let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

            let mut client: Option<HwInfoClient> = None;
            let mut last_poll_time: Option<i64> = None;
            let mut last_published: HashMap<&'static str, (f64, Instant)> = HashMap::new();
            // Last (min, max, avg, unit) actually published for each sensor.
            // The state value changes every threshold-crossing, but attributes
            // (especially min/max/avg) are slow-moving - skip the attribute
            // publish when they're unchanged.  String unit comparison is cheap;
            // unit changes are essentially never in steady-state.
            let mut last_published_attrs: HashMap<&'static str, (f64, f64, f64, String)> =
                HashMap::new();

            // Diagnostic state. We capture the latest snapshot outcome on
            // every tick that actually parsed something (or hit an error /
            // saw the client closed), then flush it to MQTT at most every
            // DIAGNOSTIC_INTERVAL_SECS to keep broker traffic boring.
            let mut latest_snapshot: Option<Snapshot> = None;
            let mut latest_view_size: usize = 0;
            let mut latest_error: Option<String> = None;
            let mut latest_matched: Vec<&'static str> = Vec::new();
            let mut latest_unmatched: Vec<&'static str> = Vec::new();
            let mut last_diagnostic_at: Option<Instant> = None;

            info!(
                "HWiNFO sensor started (lazy poll @ 500 ms, {} mapped sensors)",
                MATCH_RULES.len()
            );

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        debug!("HWiNFO sensor shutting down");
                        if client.is_some() {
                            // Last gasp: mark HWiNFO availability offline.
                            self.state.mqtt.publish_hwinfo_availability(false).await;
                        }
                        break;
                    }
                    Ok(()) = reconnect_rx.recv() => {
                        // Force republish on reconnect: clear thresholds and
                        // re-publish availability so HA can pick it up.
                        last_published.clear();
                        last_published_attrs.clear();
                        last_poll_time = None;
                        last_diagnostic_at = None;
                        let online = client.is_some();
                        self.state.mqtt.publish_hwinfo_availability(online).await;
                    }
                    _ = tick.tick() => {
                        // Mid-session start: try to open if currently closed.
                        if client.is_none() {
                            if let Some(c) = HwInfoClient::open() {
                                info!("HWiNFO shared memory opened");
                                client = Some(c);
                                self.state.mqtt.publish_hwinfo_availability(true).await;
                            } else {
                                // Still not open - publish a "not open" diagnostic
                                // so HA can show the reason.
                                latest_snapshot = None;
                                latest_view_size = 0;
                                latest_error = None;
                                latest_matched.clear();
                                latest_unmatched = MATCH_RULES.iter().map(|r| r.key).collect();
                                self
                                    .maybe_publish_diagnostic(
                                        DiagnosticInput::NotOpen,
                                        latest_view_size,
                                        &latest_matched,
                                        &latest_unmatched,
                                        &mut last_diagnostic_at,
                                    )
                                    .await;
                                continue;
                            }
                        }

                        // We have a client; probe pollTime cheaply.
                        let Some(c) = client.as_ref() else { continue };
                        let Some(pt) = c.read_poll_time() else {
                            warn!("HWiNFO view became invalid; dropping client");
                            client = None;
                            last_poll_time = None;
                            self.state.mqtt.publish_hwinfo_availability(false).await;
                            latest_snapshot = None;
                            latest_view_size = 0;
                            latest_error = Some("HWiNFO view became invalid".to_string());
                            latest_matched.clear();
                            latest_unmatched = MATCH_RULES.iter().map(|r| r.key).collect();
                            self
                                .maybe_publish_diagnostic(
                                    DiagnosticInput::Err("HWiNFO view became invalid"),
                                    0,
                                    &latest_matched,
                                    &latest_unmatched,
                                    &mut last_diagnostic_at,
                                )
                                .await;
                            continue;
                        };

                        // Open detection: did pollTime actually advance?
                        if last_poll_time != Some(pt) {
                            // Full parse + publish on new pollTime.
                            match c.snapshot() {
                                Ok(s) => {
                                    last_poll_time = Some(pt);
                                    latest_view_size = c.view_size_bytes();
                                    latest_error = None;

                                    let now = Instant::now();
                                    let mut matched: Vec<&'static str> = Vec::new();
                                    let mut unmatched: Vec<&'static str> = Vec::new();
                                    for rule in MATCH_RULES {
                                        let Some(reading) = match_reading(
                                            &s.readings,
                                            rule.sensor_substrings,
                                            rule.label_substrings,
                                            rule.label_excludes,
                                            rule.unit_suffix,
                                        ) else {
                                            debug!(
                                                "HWiNFO: no match for sensor key '{}'",
                                                rule.key
                                            );
                                            unmatched.push(rule.key);
                                            continue;
                                        };
                                        matched.push(rule.key);

                                        if !self.should_publish(
                                            rule.key,
                                            reading.value,
                                            now,
                                            &last_published,
                                        ) {
                                            continue;
                                        }
                                        self.publish_one(
                                            rule.key,
                                            reading,
                                            &mut last_published_attrs,
                                        )
                                        .await;
                                        last_published.insert(rule.key, (reading.value, now));
                                    }

                                    latest_matched = matched;
                                    latest_unmatched = unmatched;
                                    latest_snapshot = Some(s);
                                }
                                Err(e) => {
                                    let msg = format!("{:#}", e);
                                    warn!("HWiNFO snapshot parse failed: {}", msg);
                                    latest_view_size = c.view_size_bytes();
                                    latest_error = Some(msg);
                                    latest_snapshot = None;
                                    latest_matched.clear();
                                    latest_unmatched =
                                        MATCH_RULES.iter().map(|r| r.key).collect();
                                }
                            }
                        }

                        // Publish diagnostic at most every DIAGNOSTIC_INTERVAL_SECS.
                        let input = match (latest_snapshot.as_ref(), latest_error.as_deref()) {
                            (Some(s), _) => DiagnosticInput::Ok(s),
                            (None, Some(msg)) => DiagnosticInput::Err(msg),
                            (None, None) => DiagnosticInput::NotOpen,
                        };
                        self
                            .maybe_publish_diagnostic(
                                input,
                                latest_view_size,
                                &latest_matched,
                                &latest_unmatched,
                                &mut last_diagnostic_at,
                            )
                            .await;
                    }
                }
            }
        }

        /// Publish the diagnostic payload to MQTT if at least
        /// `DIAGNOSTIC_INTERVAL_SECS` has elapsed since the previous publish
        /// (or this is the first publish). Updates `last_diagnostic_at` on
        /// successful flush.
        async fn maybe_publish_diagnostic(
            &self,
            input: DiagnosticInput<'_>,
            view_size_bytes: usize,
            matched: &[&str],
            unmatched: &[&str],
            last_diagnostic_at: &mut Option<Instant>,
        ) {
            let now = Instant::now();
            let due = match *last_diagnostic_at {
                None => true,
                Some(when) => now.duration_since(when).as_secs() >= DIAGNOSTIC_INTERVAL_SECS,
            };
            if !due {
                return;
            }
            let DiagnosticPayload { state, attributes } =
                build_diagnostic_payload(&input, view_size_bytes, matched, unmatched);
            self.state.mqtt.publish_sensor(DIAGNOSTIC_KEY, &state).await;
            self.state
                .mqtt
                .publish_sensor_attributes(DIAGNOSTIC_KEY, &attributes)
                .await;
            *last_diagnostic_at = Some(now);
        }

        /// True if this value differs by ≥ threshold from the last-published
        /// value for `key`, or if the last publish was ≥ 30s ago, or if we've
        /// never published this key before.
        fn should_publish(
            &self,
            key: &'static str,
            value: f64,
            now: Instant,
            last: &HashMap<&'static str, (f64, Instant)>,
        ) -> bool {
            match last.get(key) {
                None => true,
                Some(&(prev_value, when)) => {
                    if now.duration_since(when).as_secs() >= HEARTBEAT_SECS {
                        return true;
                    }
                    (value - prev_value).abs() >= threshold_for(key)
                }
            }
        }

        async fn publish_one(
            &self,
            key: &'static str,
            reading: &Reading,
            last_attrs: &mut HashMap<&'static str, (f64, f64, f64, String)>,
        ) {
            let decimals = decimals_for(key);
            let value_str = format!("{:.*}", decimals, reading.value);
            self.state.mqtt.publish_sensor(key, &value_str).await;

            // Skip the attribute publish when min/max/avg/unit haven't moved.
            // f64 exact comparison is fine here: HWiNFO returns the same
            // bit-pattern when the underlying value didn't change.
            if let Some((min, max, avg, unit)) = last_attrs.get(key)
                && *min == reading.min
                && *max == reading.max
                && *avg == reading.avg
                && unit == &reading.unit
            {
                return;
            }

            let attributes = serde_json::json!({
                "min": reading.min,
                "max": reading.max,
                "avg": reading.avg,
                "unit": reading.unit,
            });
            self.state
                .mqtt
                .publish_sensor_attributes(key, &attributes)
                .await;
            last_attrs.insert(
                key,
                (reading.min, reading.max, reading.avg, reading.unit.clone()),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (cross-platform - match_reading is pure)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_reading(sensor: &str, label: &str, unit: &str, value: f64) -> Reading {
        Reading {
            sensor_name: sensor.to_string(),
            label: label.to_string(),
            unit: unit.to_string(),
            value,
            min: value,
            max: value,
            avg: value,
            reading_type: 0,
        }
    }

    #[test]
    fn test_match_reading_finds_label_case_insensitive() {
        let readings = vec![mk_reading(
            "CPU [#0]: AMD Ryzen 9 9800X3D",
            "CPU (Tctl/Tdie)",
            "°C",
            65.0,
        )];
        let r = match_reading(&readings, &["9800x3d"], &["cpu (tctl/tdie)"], &[], None);
        assert!(r.is_some());
        assert!((r.unwrap().value - 65.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_respects_excludes() {
        let readings = vec![
            mk_reading(
                "GPU [#0]: NVIDIA GeForce RTX 4090",
                "GPU Temperature",
                "°C",
                55.0,
            ),
            mk_reading(
                "GPU [#0]: NVIDIA GeForce RTX 4090",
                "GPU Hot Spot Temperature",
                "°C",
                70.0,
            ),
            mk_reading(
                "GPU [#0]: NVIDIA GeForce RTX 4090",
                "GPU Memory Temperature",
                "°C",
                60.0,
            ),
        ];

        let r = match_reading(
            &readings,
            &["geforce"],
            &["GPU Temperature"],
            &["Hot Spot", "Memory"],
            None,
        );
        // Should match the plain "GPU Temperature" only
        assert!(r.is_some());
        assert_eq!(r.unwrap().label, "GPU Temperature");
        assert!((r.unwrap().value - 55.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_priority_first_wins() {
        let readings = vec![
            mk_reading("CPU", "CPU PPT", "W", 95.0),
            mk_reading("CPU", "CPU Package Power", "W", 100.0),
        ];
        // "CPU Package Power" listed first → it wins even though "CPU PPT" comes first in the slice
        let r = match_reading(
            &readings,
            &["cpu"],
            &["CPU Package Power", "CPU PPT"],
            &[],
            None,
        );
        assert!(r.is_some());
        assert_eq!(r.unwrap().label, "CPU Package Power");
    }

    #[test]
    fn test_match_reading_unit_suffix_filter() {
        let readings = vec![
            mk_reading("GPU", "GPU Memory Usage", "MB", 12000.0),
            mk_reading("GPU", "GPU Memory Usage", "%", 50.0),
        ];
        let r = match_reading(&readings, &["gpu"], &["GPU Memory Usage"], &[], Some("%"));
        assert!(r.is_some());
        assert!((r.unwrap().value - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_any_sensor_when_empty_filter() {
        // cpu_total_usage rule uses empty sensor_substrings - should match
        // any reading whose label matches.
        let readings = vec![mk_reading("OS", "Total CPU Usage", "%", 42.0)];
        let r = match_reading(&readings, &[], &["Total CPU Usage"], &[], None);
        assert!(r.is_some());
        assert!((r.unwrap().value - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_returns_none_when_no_match() {
        let readings = vec![mk_reading("CPU", "Whatever", "°C", 50.0)];
        let r = match_reading(&readings, &["gpu"], &["GPU Temperature"], &[], None);
        assert!(r.is_none());
    }

    #[test]
    fn test_threshold_temp() {
        assert!((threshold_for("cpu_package_temp") - 0.5).abs() < f64::EPSILON);
        assert!((threshold_for("gpu_hotspot_temp") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_threshold_power_and_clock() {
        assert!((threshold_for("gpu_power") - 5.0).abs() < f64::EPSILON);
        assert!((threshold_for("gpu_memory_clock") - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_threshold_load_and_rpm() {
        assert!((threshold_for("gpu_core_load") - 2.0).abs() < f64::EPSILON);
        assert!((threshold_for("cpu_total_usage") - 2.0).abs() < f64::EPSILON);
        assert!((threshold_for("gpu_vram_usage_pct") - 2.0).abs() < f64::EPSILON);
        assert!((threshold_for("gpu_fan_rpm") - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_threshold_framerate() {
        assert!((threshold_for("framerate") - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_decimals_per_key() {
        // Clocks and RPM use 0 decimals; temps/power/percentages use 1.
        assert_eq!(decimals_for("cpu_package_temp"), 1);
        assert_eq!(decimals_for("cpu_effective_clock"), 0);
        assert_eq!(decimals_for("gpu_core_clock"), 0);
        assert_eq!(decimals_for("gpu_fan_rpm"), 0);
        assert_eq!(decimals_for("framerate"), 0);
        assert_eq!(decimals_for("gpu_core_load"), 1);
        assert_eq!(decimals_for("cpu_package_power"), 1);
    }

    #[test]
    fn test_match_rules_cover_expected_keys() {
        // 15 GPU/CPU sensors plus 5 motherboard sensors (4 fan RPMs + VRM temp).
        assert_eq!(MATCH_RULES.len(), 20);

        let keys: Vec<&str> = MATCH_RULES.iter().map(|r| r.key).collect();
        for required in [
            "cpu_package_temp",
            "cpu_package_power",
            "cpu_soc_power",
            "cpu_effective_clock",
            "cpu_total_usage",
            "gpu_temp",
            "gpu_hotspot_temp",
            "gpu_memory_temp",
            "gpu_power",
            "gpu_core_clock",
            "gpu_memory_clock",
            "gpu_core_load",
            "gpu_fan_rpm",
            "gpu_vram_usage_pct",
            "framerate",
            "case_fan_cpu",
            "case_fan_cpu_opt",
            "case_fan_system_1",
            "case_fan_system_2",
            "vrm_temp",
        ] {
            assert!(keys.contains(&required), "missing key: {}", required);
        }
    }

    #[test]
    fn test_case_fan_threshold_and_decimals() {
        // Case fan keys should get the same RPM threshold (100) as gpu_fan_rpm
        // and zero decimals (integer-like display).
        assert!((threshold_for("case_fan_cpu") - 100.0).abs() < f64::EPSILON);
        assert!((threshold_for("case_fan_system_1") - 100.0).abs() < f64::EPSILON);
        assert_eq!(decimals_for("case_fan_cpu"), 0);
        assert_eq!(decimals_for("case_fan_system_2"), 0);
    }

    #[test]
    fn test_case_fan_cpu_excludes_cpu_opt() {
        // The "CPU" label substring would otherwise also match "CPU_OPT".
        let readings = vec![
            mk_reading("Mobo", "CPU_OPT", "RPM", 1227.0),
            mk_reading("Mobo", "CPU", "RPM", 2280.0),
        ];
        let rule = MATCH_RULES
            .iter()
            .find(|r| r.key == "case_fan_cpu")
            .unwrap();
        let r = match_reading(
            &readings,
            rule.sensor_substrings,
            rule.label_substrings,
            rule.label_excludes,
            rule.unit_suffix,
        );
        assert!(r.is_some());
        assert_eq!(r.unwrap().label, "CPU");
        assert!((r.unwrap().value - 2280.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_gpu_core_clock_excludes_memory_clock() {
        let readings = vec![
            mk_reading("RTX 4090", "GPU Memory Clock", "MHz", 10500.0),
            mk_reading("RTX 4090", "GPU Clock", "MHz", 2800.0),
        ];

        // Get the gpu_core_clock rule
        let rule = MATCH_RULES
            .iter()
            .find(|r| r.key == "gpu_core_clock")
            .unwrap();
        let r = match_reading(
            &readings,
            rule.sensor_substrings,
            rule.label_substrings,
            rule.label_excludes,
            rule.unit_suffix,
        );
        assert!(r.is_some());
        assert_eq!(r.unwrap().label, "GPU Clock");
    }

    #[test]
    fn test_contains_icase_basic() {
        assert!(contains_icase("CPU (Tctl/Tdie)", "cpu"));
        assert!(contains_icase("AMD Ryzen 9 9800X3D", "9800x3d"));
        assert!(contains_icase("GPU Hot Spot Temperature", "hot spot"));
        assert!(!contains_icase("GPU Temperature", "Memory"));
    }

    #[test]
    fn test_contains_icase_handles_empty_and_edges() {
        assert!(contains_icase("anything", ""));
        assert!(!contains_icase("ab", "abc"));
        assert!(contains_icase("ABC", "abc"));
        assert!(contains_icase("abc", "ABC"));
    }

    #[test]
    fn test_contains_icase_safe_with_multibyte_haystack() {
        // Multi-byte UTF-8 ("°C") in haystack must not produce false matches
        // or panic. The needle is plain ASCII; byte-window comparison can't
        // split a codepoint into a positive match.
        assert!(!contains_icase("Temperature °C reading", "xyz"));
        assert!(contains_icase("Temperature °C reading", "temperature"));
    }

    #[test]
    fn test_gpu_temp_excludes_hotspot_and_memory() {
        let readings = vec![
            mk_reading("RTX 4090", "GPU Hot Spot Temperature", "°C", 75.0),
            mk_reading("RTX 4090", "GPU Memory Temperature", "°C", 70.0),
            mk_reading("RTX 4090", "GPU Temperature", "°C", 60.0),
        ];
        let rule = MATCH_RULES.iter().find(|r| r.key == "gpu_temp").unwrap();
        let r = match_reading(
            &readings,
            rule.sensor_substrings,
            rule.label_substrings,
            rule.label_excludes,
            rule.unit_suffix,
        );
        assert!(r.is_some());
        assert_eq!(r.unwrap().label, "GPU Temperature");
        assert!((r.unwrap().value - 60.0).abs() < f64::EPSILON);
    }

    // ===== Diagnostic payload tests =====

    fn mk_snapshot(readings: Vec<Reading>) -> Snapshot {
        Snapshot {
            poll_time: 0,
            readings,
        }
    }

    #[test]
    fn test_diagnostic_payload_ok_state_format() {
        let snap = mk_snapshot(vec![mk_reading(
            "CPU [#0]: AMD Ryzen 7 9800X3D",
            "CPU (Tctl/Tdie)",
            "°C",
            65.0,
        )]);
        let matched: Vec<&str> = vec!["cpu_package_temp", "gpu_temp"];
        let unmatched: Vec<&str> = vec!["framerate"];
        let payload =
            build_diagnostic_payload(&DiagnosticInput::Ok(&snap), 12345, &matched, &unmatched);
        assert_eq!(
            payload.state,
            format!("ok: 2/{} matched", MATCH_RULES.len())
        );
        assert_eq!(payload.attributes["snapshot_ok"], serde_json::json!(true));
        assert_eq!(payload.attributes["error"], serde_json::Value::Null);
        assert_eq!(
            payload.attributes["view_size_bytes"],
            serde_json::json!(12345)
        );
        assert_eq!(payload.attributes["matched_count"], serde_json::json!(2));
        assert_eq!(
            payload.attributes["unmatched_keys"],
            serde_json::json!(["framerate"])
        );
        assert_eq!(payload.attributes["sensors_count"], serde_json::json!(1));
        assert_eq!(payload.attributes["readings_count"], serde_json::json!(1));
        let names = payload.attributes["first_sensor_names"].as_array().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "CPU [#0]: AMD Ryzen 7 9800X3D");
        let labels = payload.attributes["sample_labels"].as_array().unwrap();
        assert_eq!(labels[0], "CPU (Tctl/Tdie)");
    }

    #[test]
    fn test_diagnostic_payload_err_state_format() {
        let payload = build_diagnostic_payload(
            &DiagnosticInput::Err("parse failed: bad version"),
            48,
            &[],
            &[],
        );
        assert!(payload.state.starts_with("err: "));
        assert!(payload.state.contains("bad version"));
        assert_eq!(payload.attributes["snapshot_ok"], serde_json::json!(false));
        assert_eq!(
            payload.attributes["error"],
            serde_json::json!("parse failed: bad version")
        );
        assert_eq!(payload.attributes["sensors_count"], serde_json::json!(0));
        assert_eq!(payload.attributes["readings_count"], serde_json::json!(0));
        assert_eq!(payload.attributes["view_size_bytes"], serde_json::json!(48));
        // Errors list every rule key as unmatched so HA shows the full set.
        let unmatched = payload.attributes["unmatched_keys"].as_array().unwrap();
        assert_eq!(unmatched.len(), MATCH_RULES.len());
    }

    #[test]
    fn test_diagnostic_payload_not_open() {
        let payload = build_diagnostic_payload(&DiagnosticInput::NotOpen, 0, &[], &[]);
        assert_eq!(payload.state, "err: shared memory not open");
        assert_eq!(payload.attributes["snapshot_ok"], serde_json::json!(false));
        assert_eq!(
            payload.attributes["error"],
            serde_json::json!("hwinfo shared memory not open")
        );
        assert_eq!(payload.attributes["view_size_bytes"], serde_json::json!(0));
    }

    #[test]
    fn test_diagnostic_samples_are_capped() {
        // Build a snapshot with way more sensors than the cap; sample list
        // must be bounded.
        let mut readings = Vec::new();
        for i in 0..50 {
            readings.push(mk_reading(
                &format!("Sensor #{i}"),
                &format!("Label {i}"),
                "",
                0.0,
            ));
        }
        let snap = mk_snapshot(readings);
        let payload = build_diagnostic_payload(&DiagnosticInput::Ok(&snap), 0, &[], &[]);
        let names = payload.attributes["first_sensor_names"].as_array().unwrap();
        let labels = payload.attributes["sample_labels"].as_array().unwrap();
        assert_eq!(names.len(), DIAGNOSTIC_SAMPLE_CAP);
        assert_eq!(labels.len(), DIAGNOSTIC_SAMPLE_CAP);
        // sensors_count must reflect the *full* distinct count, not the cap.
        assert_eq!(payload.attributes["sensors_count"], serde_json::json!(50));
        assert_eq!(payload.attributes["readings_count"], serde_json::json!(50));
    }

    #[test]
    fn test_diagnostic_state_truncates_long_error() {
        let huge = "x".repeat(500);
        let payload = build_diagnostic_payload(&DiagnosticInput::Err(&huge), 0, &[], &[]);
        // "err: " prefix (5) + at most 200 chars of message = 205.
        assert!(payload.state.len() <= 205);
        assert!(payload.state.starts_with("err: "));
    }

    #[test]
    fn test_diagnostic_unique_sensor_count_dedups() {
        // Two sensors, three readings each → 2 distinct sensor names.
        let readings = vec![
            mk_reading("Sensor A", "L1", "", 0.0),
            mk_reading("Sensor A", "L2", "", 0.0),
            mk_reading("Sensor B", "L3", "", 0.0),
            mk_reading("Sensor B", "L4", "", 0.0),
        ];
        let snap = mk_snapshot(readings);
        let payload = build_diagnostic_payload(&DiagnosticInput::Ok(&snap), 0, &[], &[]);
        assert_eq!(payload.attributes["sensors_count"], serde_json::json!(2));
        assert_eq!(payload.attributes["readings_count"], serde_json::json!(4));
        let names = payload.attributes["first_sensor_names"].as_array().unwrap();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_diagnostic_key_constant() {
        assert_eq!(DIAGNOSTIC_KEY, "hwinfo_diagnostic");
    }
}
