//! HWiNFO64 → Home Assistant bridge sensor task.
//!
//! Lazily polls `Global\HWiNFO_SENS_SM2` every 500 ms by reading just the
//! `pollTime` field. When HWiNFO advances pollTime, we do a full parse and
//! republish the ~20 mapped sensor values (if changed beyond a threshold or
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

use crate::hwinfo::Reading;

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
    /// Accepted unit suffixes, compared on their ASCII tail. Empty means no
    /// constraint. A slice rather than a single value so a rule can accept more
    /// than one legitimate unit (temperatures are °C or °F depending on how the
    /// user configured HWiNFO).
    pub unit_suffix: &'static [&'static str],
}

/// True when `haystack` ends with `needle`, comparing only ASCII characters on
/// both sides.
///
/// Lets a unit compare equal whether its degree sign survived decoding or became
/// a replacement char (HWiNFO's szUnit is ANSI CP-1252, but it is read with
/// from_utf8_lossy). Allocation-free: this runs per candidate reading per rule,
/// and `match_reading` documents itself as zero-alloc on the hot path.
fn ascii_tail_ends_with(haystack: &str, needle: &str) -> bool {
    // Compare from the right, skipping non-ASCII on both sides. No allocation:
    // this runs per candidate reading per unit-pinned rule, and `match_reading`
    // documents itself as zero-alloc on the hot path.
    let mut h = haystack.chars().rev().filter(char::is_ascii);
    let mut n = needle.chars().rev().filter(char::is_ascii);
    let mut matched_any = false;
    loop {
        match (n.next(), h.next()) {
            (None, _) => return matched_any,
            (Some(_), None) => return false,
            (Some(nc), Some(hc)) if nc == hc => matched_any = true,
            (Some(_), Some(_)) => return false,
        }
    }
}

/// Hardcoded sensor → HWiNFO match table.
pub const MATCH_RULES: &[MatchRule] = &[
    MatchRule {
        key: "cpu_package_temp",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU (Tctl/Tdie)", "CPU Package"],
        label_excludes: &[],
        // MUST be pinned: "CPU Package" is also a substring of "CPU Package
        // Power" (watts), and the entity is declared device_class temperature.
        // "C" alone would make the rule vanish for users who configured HWiNFO
        // in Fahrenheit, so accept either degree unit.
        unit_suffix: &["C", "F"],
    },
    MatchRule {
        key: "cpu_package_power",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU Package Power", "CPU PPT"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "cpu_soc_power",
        sensor_substrings: &["9800x3d", "ryzen"],
        label_substrings: &["CPU SoC Power", "SoC Power"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "cpu_effective_clock",
        sensor_substrings: &["9800x3d", "ryzen"],
        // Verified against HWiNFO's live shared memory on a Ryzen 9800X3D: the
        // aggregate reading is literally "Average Effective Clock". The old
        // needles ("Core Effective Clock (avg)", "CPU Clock") match nothing HWiNFO
        // emits - the per-core readings are "Core N T0/T1 Effective Clock", and
        // matching one arbitrary core would be misleading, so target the average.
        label_substrings: &["Average Effective Clock"],
        label_excludes: &[],
        unit_suffix: &["MHz"],
    },
    MatchRule {
        // CPU Usage can come from any sensor (System / OS / per-CPU)
        key: "cpu_total_usage",
        sensor_substrings: &[],
        label_substrings: &["Total CPU Usage"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Temperature"],
        label_excludes: &["Hot Spot", "Memory"],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_hotspot_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Hot Spot Temperature"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_memory_temp",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Junction Temperature", "GPU Memory Temperature"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_power",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Power (Total)", "GPU Total Board Power", "GPU Power"],
        label_excludes: &[],
        // Pinned: the bare "GPU Power" needle is a substring of readings in other
        // units (percent-of-limit, for one), and the entity is declared watts.
        unit_suffix: &["W"],
    },
    MatchRule {
        key: "gpu_core_clock",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Clock"],
        label_excludes: &["Memory"],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_memory_clock",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Clock"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_core_load",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Core Load", "GPU Utilization"],
        label_excludes: &[],
        unit_suffix: &[],
    },
    MatchRule {
        key: "gpu_fan_rpm",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Fan"],
        label_excludes: &[],
        // HWiNFO emits both an RPM reading and a "%" duty-cycle sibling under
        // "GPU Fan"; without this the % could land in an RPM entity.
        unit_suffix: &["RPM"],
    },
    MatchRule {
        key: "gpu_vram_usage_pct",
        sensor_substrings: &["geforce", "rtx", "radeon", "gpu"],
        label_substrings: &["GPU Memory Usage"],
        label_excludes: &[],
        unit_suffix: &["%"],
    },
    MatchRule {
        key: "framerate",
        sensor_substrings: &["rivatuner", "rtss", "framerate", "presentmon"],
        label_substrings: &["Framerate"],
        label_excludes: &[],
        unit_suffix: &[],
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
        unit_suffix: &["RPM"],
    },
    MatchRule {
        key: "case_fan_cpu_opt",
        sensor_substrings: &[],
        label_substrings: &["CPU_OPT"],
        label_excludes: &[],
        unit_suffix: &["RPM"],
    },
    MatchRule {
        key: "case_fan_system_1",
        sensor_substrings: &[],
        label_substrings: &["System 1"],
        label_excludes: &[],
        unit_suffix: &["RPM"],
    },
    MatchRule {
        key: "case_fan_system_2",
        sensor_substrings: &[],
        label_substrings: &["System 2"],
        label_excludes: &[],
        unit_suffix: &["RPM"],
    },
    MatchRule {
        key: "vrm_temp",
        sensor_substrings: &[],
        label_substrings: &["VRM MOS", "VRM Temperature", "VRM Temp"],
        label_excludes: &[],
        unit_suffix: &["C", "F"],
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

/// The rule predicate, over plain string parts.
///
/// ONE implementation shared by the borrowing hot path
/// ([`rule_label_index`]) and the owned-slice reference version
/// ([`match_reading`], kept as the test oracle). Duplicating it is exactly the
/// single-source-of-truth failure this codebase has been bitten by before.
///
/// Returns which `label_substrings` index matched, so callers can preserve the
/// "lower index wins, then first reading wins" priority.
fn rule_label_index_parts(
    sensor_name: &str,
    label: &str,
    unit: &str,
    sensor_substrings: &[&str],
    label_substrings: &[&str],
    label_excludes: &[&str],
    unit_suffix: &[&str],
) -> Option<usize> {
    if label_excludes.iter().any(|e| contains_icase(label, e)) {
        return None;
    }
    if !sensor_substrings.is_empty()
        && !sensor_substrings
            .iter()
            .any(|sub| contains_icase(sensor_name, sub))
    {
        return None;
    }
    // Unit suffix compared on the ASCII tail only: HWiNFO's szUnit is ANSI
    // (CP-1252), so a degree sign arrives as U+FFFD after from_utf8_lossy and a
    // byte-exact ends_with("\u{00B0}C") could never match. That silently killed
    // the vrm_temp rule on every machine.
    if !unit_suffix.is_empty()
        && !unit_suffix
            .iter()
            .any(|sfx| ascii_tail_ends_with(unit, sfx))
    {
        return None;
    }
    label_substrings
        .iter()
        .position(|sub| contains_icase(label, sub))
}

/// Which `label_substrings` index this borrowed reading matches for `rule`, if any.
///
/// Same predicate as [`match_reading`], expressed for a single borrowed reading so
/// the whole rule set can be evaluated in ONE pass over the mapped view without
/// materializing ~363 owned `Reading`s. The returned index preserves
/// `match_reading`'s priority rule: a lower label-substring index always wins,
/// and within one index the first reading encountered wins.
fn rule_label_index(r: &crate::hwinfo::ReadingRef<'_>, rule: &MatchRule) -> Option<usize> {
    rule_label_index_parts(
        r.sensor_name,
        &r.label,
        &r.unit,
        rule.sensor_substrings,
        rule.label_substrings,
        rule.label_excludes,
        rule.unit_suffix,
    )
}

/// Matched readings paired with their rule key, plus the keys that matched nothing.
type MatchOutcome = (Vec<(&'static str, Reading)>, Vec<&'static str>);

/// Evaluate every `MATCH_RULES` entry in a single borrowing pass.
///
/// Returns the matched readings (owned, ~20 of them) and the keys that missed.
/// Equivalent to calling [`match_reading`] once per rule over an owned snapshot,
/// but without allocating the ~343 readings nothing ever reads.
fn match_all_borrowed(view: &[u8]) -> anyhow::Result<MatchOutcome> {
    let mut best: Vec<Option<(usize, Reading)>> = (0..MATCH_RULES.len()).map(|_| None).collect();

    crate::hwinfo::for_each_reading(view, |r| {
        for (i, rule) in MATCH_RULES.iter().enumerate() {
            let Some(li) = rule_label_index(r, rule) else {
                continue;
            };
            // Keep only a STRICTLY better (lower) label index, so the first
            // reading at the winning index wins, exactly as match_reading does.
            if best[i].as_ref().is_none_or(|(prev, _)| li < *prev) {
                best[i] = Some((li, r.to_owned_reading()));
            }
        }
    })?;

    let mut hits = Vec::with_capacity(MATCH_RULES.len());
    let mut misses = Vec::new();
    for (rule, slot) in MATCH_RULES.iter().zip(best) {
        match slot {
            Some((_, reading)) => hits.push((rule.key, reading)),
            None => misses.push(rule.key),
        }
    }
    Ok((hits, misses))
}

/// Find a `Reading` matching the given criteria. Substring matches are
/// case-insensitive (ASCII-fold). The first label-substring match (in declared
/// order) wins.
///
/// Zero-alloc on the hot path: needles and haystacks are compared via byte
/// windows; no temporary `String`s are produced.
///
/// * `sensor_substrings` empty means match any sensor name (used for `cpu_total_usage`)
/// * Substring priority is "first listed wins" for both sensor and label.
///
/// Kept as the TEST ORACLE for the borrowing hot path. Both it and
/// `rule_label_index` delegate to `rule_label_index_parts`, so a regression test
/// written against this also pins `match_all_borrowed`.
#[cfg(test)]
pub fn match_reading<'a>(
    readings: &'a [Reading],
    sensor_substrings: &[&str],
    label_substrings: &[&str],
    label_excludes: &[&str],
    unit_suffix: &[&str],
) -> Option<&'a Reading> {
    // Lowest matching label index wins; within one index, the first reading wins.
    let mut best: Option<(usize, &Reading)> = None;
    for reading in readings {
        let Some(li) = rule_label_index_parts(
            &reading.sensor_name,
            &reading.label,
            &reading.unit,
            sensor_substrings,
            label_substrings,
            label_excludes,
            unit_suffix,
        ) else {
            continue;
        };
        if best.as_ref().is_none_or(|(prev, _)| li < *prev) {
            best = Some((li, reading));
        }
    }
    best.map(|(_, r)| r)
}

// ---------------------------------------------------------------------------
// Windows-only task
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub use win::HwInfoSensor;

#[cfg(windows)]
mod win {
    use super::{MATCH_RULES, decimals_for, match_all_borrowed, threshold_for};
    use crate::AppState;
    use crate::hwinfo::{HwInfoClient, Reading};
    use log::{debug, info, warn};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::time::{Duration, MissedTickBehavior, interval};

    const HEARTBEAT_SECS: u64 = 30;
    /// If HWiNFO's pollTime stops advancing for this long, treat it as gone (the
    /// mapped section can outlive the app). HWiNFO's own poll is usually ~2s.
    const HWINFO_STALE_SECS: u64 = 15;
    /// Poll cadence once the shared memory is open.
    const OPEN_POLL_MS: u64 = 500;
    /// Slow backoff while closed: probing open() every 500ms with the feature on
    /// but HWiNFO not running is ~172k wakeups/day for nothing. The interval's
    /// first tick is still immediate, so startup detection stays fast.
    const CLOSED_POLL_MS: u64 = 10_000;

    pub struct HwInfoSensor {
        state: Arc<AppState>,
    }

    impl HwInfoSensor {
        pub fn new(state: Arc<AppState>) -> Self {
            Self { state }
        }

        // Long dispatch/event-loop body: splitting it would scatter tightly-coupled
        // state across helpers for no readability gain. Reviewed, allowed deliberately.
        #[allow(clippy::cognitive_complexity)]
        /// Takes the per-task shutdown SENDER (not the global one) so the
        /// supervisor can stop this sensor when its flag is turned off at
        /// runtime. Previously this read the flag once at entry and was absent
        /// from TASKS entirely, so disabling hwinfo_sensor cleared the HA
        /// entities while this loop kept polling and publishing to them.
        pub async fn run(self, shutdown: tokio::sync::broadcast::Sender<()>) {
            let config = self.state.config.read().await;
            if !config.features.hwinfo_sensor {
                return;
            }
            drop(config);

            // Start slow: the client is closed until the first probe opens it.
            let mut tick = interval(Duration::from_millis(CLOSED_POLL_MS));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut fast_poll = false;
            let mut shutdown_rx = shutdown.subscribe();
            let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();

            let mut client: Option<HwInfoClient> = None;
            let mut last_poll_time: Option<i64> = None;
            // Staleness watchdog: when HWiNFO exits gracefully its named section
            // stays mapped (our handle keeps it alive) with frozen contents, so
            // read_poll_time keeps returning the same value and we'd otherwise
            // stay "online" republishing stale numbers forever. Track when
            // pollTime last advanced; if it stops for HWINFO_STALE_SECS, mark
            // offline (and flip back online when it resumes, e.g. HWiNFO relaunch).
            let mut last_poll_change = Instant::now();
            let mut stale_offline = false;
            let mut last_published: HashMap<&'static str, (f64, Instant)> = HashMap::new();
            // Last (min, max, avg, unit) actually published for each sensor.
            // The state value changes every threshold-crossing, but attributes
            // (especially min/max/avg) are slow-moving - skip the attribute
            // publish when they're unchanged.  String unit comparison is cheap;
            // unit changes are essentially never in steady-state.
            let mut last_published_attrs: HashMap<&'static str, (f64, f64, f64, String)> =
                HashMap::new();

            info!(
                "HWiNFO sensor started (lazy poll: 10s closed / 500ms once open, {} mapped sensors)",
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
                    r = reconnect_rx.recv() => {
                        // Force republish on reconnect: clear thresholds and
                        // re-publish availability so HA can pick it up. Treat a
                        // Lagged receiver the same as a reconnect (a coincident
                        // lag must not silently drop the forced republish); only a
                        // Closed channel is ignored here.
                        if !matches!(r, Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) {
                            // Closed: `biased` re-polls this arm first every
                            // iteration and it is instantly Ready(Err(Closed)),
                            // so falling through spins at 100% CPU and the tick
                            // arm never runs again. Every other sensor exits
                            // here; this was the one that did not.
                            debug!("Reconnect channel closed; stopping hwinfo sensor");
                            break;
                        }
                        last_published.clear();
                        last_published_attrs.clear();
                        last_poll_time = None;
                        let online = client.is_some();
                        self.state.mqtt.publish_hwinfo_availability(online).await;
                    }
                    _ = tick.tick() => {
                        // Reconcile poll cadence to the client state: 500ms while
                        // open, 10s backoff while closed. Rebuilding here (rather than
                        // at each transition) keeps every open/loss path covered even
                        // through the `continue`s below.
                        if client.is_some() != fast_poll {
                            fast_poll = client.is_some();
                            let ms = if fast_poll { OPEN_POLL_MS } else { CLOSED_POLL_MS };
                            tick = interval(Duration::from_millis(ms));
                            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                        }

                        // Mid-session start: try to open if currently closed.
                        if client.is_none() {
                            if let Some(c) = HwInfoClient::open() {
                                info!("HWiNFO shared memory opened");
                                client = Some(c);
                                last_poll_change = Instant::now();
                                stale_offline = false;
                                self.state.mqtt.publish_hwinfo_availability(true).await;
                            } else {
                                // Still not open - skip this tick
                                // so HA can show the reason.
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
                            continue;
                        };

                        // Staleness watchdog: pollTime frozen too long means HWiNFO
                        // is gone even though the mapped section is still readable.
                        if last_poll_time == Some(pt) {
                            if !stale_offline
                                && last_poll_change.elapsed() >= Duration::from_secs(HWINFO_STALE_SECS)
                            {
                                warn!("HWiNFO pollTime stalled; marking offline");
                                stale_offline = true;
                                self.state.mqtt.publish_hwinfo_availability(false).await;
                            }
                        } else {
                            last_poll_change = Instant::now();
                            if stale_offline {
                                stale_offline = false;
                                self.state.mqtt.publish_hwinfo_availability(true).await;
                            }
                        }

                        // Open detection: did pollTime actually advance?
                        if last_poll_time != Some(pt) {
                            // Full parse off the single-threaded runtime (the
                            // rule x reading match can be multi-ms on big setups).
                            // Move the client in and back out of spawn_blocking.
                            let Some(taken) = client.take() else { continue };
                            // Parse AND match off the single-threaded runtime. The
                            // O(rules x readings) match scan used to run back on the
                            // async worker after the parse returned; doing it inside
                            // the blocking task keeps the runtime thread free. The
                            // closure returns the ~20 matched readings alongside the
                            // snapshot so the async side only publishes (no re-scan);
                            // the snapshot is discarded; only the matched readings are used.
                            let (taken, matched) = match tokio::task::spawn_blocking(move || {
                                // Single borrowing pass: evaluates every rule while
                                // walking the mapped view, materializing only the
                                // ~20 matched readings instead of all ~363. See
                                // hwinfo::ReadingRef.
                                let r = taken.with_view(match_all_borrowed);
                                (taken, r)
                            })
                            .await
                            {
                                Ok(pair) => pair,
                                Err(_) => {
                                    // The parse task panicked (e.g. the mapping went
                                    // invalid) and the client was dropped in the
                                    // unwind. Signal offline once (retained) so HA
                                    // doesn't keep showing stale "online" if HWiNFO
                                    // has actually gone; the next tick attempts a
                                    // reopen and flips back to online if it returns.
                                    warn!("HWiNFO snapshot task panicked; marking offline");
                                    self.state.mqtt.publish_hwinfo_availability(false).await;
                                    continue;
                                }
                            };
                            client = Some(taken);
                            match matched {
                                Ok((hits, misses)) => {
                                    last_poll_time = Some(pt);

                                    let now = Instant::now();
                                    // Matching already ran in the blocking task; here
                                    // we only publish the (few) matched readings.
                                    let mut matched: Vec<&'static str> =
                                        Vec::with_capacity(hits.len());
                                    for &(key, ref reading) in &hits {
                                        matched.push(key);
                                        if !self.should_publish(
                                            key,
                                            reading.value,
                                            now,
                                            &last_published,
                                        ) {
                                            continue;
                                        }
                                        self.publish_one(key, reading, &mut last_published_attrs)
                                            .await;
                                        last_published.insert(key, (reading.value, now));
                                    }
                                    for &key in &misses {
                                        debug!("HWiNFO: no match for sensor key '{}'", key);
                                    }

                                }
                                Err(e) => {
                                    let msg = format!("{:#}", e);
                                    warn!("HWiNFO snapshot parse failed: {}", msg);
                                }
                            }
                        }

                    }
                }
            }
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
            #[allow(clippy::float_cmp)]
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
        let r = match_reading(&readings, &["9800x3d"], &["cpu (tctl/tdie)"], &[], &[]);
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
            &[],
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
            &[],
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
        let r = match_reading(&readings, &["gpu"], &["GPU Memory Usage"], &[], &["%"]);
        assert!(r.is_some());
        assert!((r.unwrap().value - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_any_sensor_when_empty_filter() {
        // cpu_total_usage rule uses empty sensor_substrings - should match
        // any reading whose label matches.
        let readings = vec![mk_reading("OS", "Total CPU Usage", "%", 42.0)];
        let r = match_reading(&readings, &[], &["Total CPU Usage"], &[], &[]);
        assert!(r.is_some());
        assert!((r.unwrap().value - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_reading_returns_none_when_no_match() {
        let readings = vec![mk_reading("CPU", "Whatever", "°C", 50.0)];
        let r = match_reading(&readings, &["gpu"], &["GPU Temperature"], &[], &[]);
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

    // ── Unit-suffix regression tests ───────────────────────────────────────
    //
    // These pin the three bugs that made rules silently dead or wrong. All were
    // found by audit rather than by a failing test, which is why they exist now.

    /// HWiNFO's szUnit is ANSI CP-1252, so the degree sign is the single byte
    /// 0xB0. trim_cstr decodes with from_utf8_lossy, which turns that into
    /// U+FFFD - so a byte-exact `ends_with("°C")` could NEVER match and
    /// `vrm_temp` was dead on every machine. Comparing ASCII tails fixes it.
    #[test]
    fn unit_match_survives_cp1252_degree_sign() {
        let lossy = "\u{FFFD}C"; // what 0xB0 0x43 becomes via from_utf8_lossy
        let readings = vec![mk_reading("Board", "VRM MOS", lossy, 47.0)];
        let r = match_reading(&readings, &[], &["VRM MOS"], &[], &["C"]);
        assert!(
            r.is_some(),
            "CP-1252 degree sign must still match a C suffix"
        );

        // And the properly-encoded form must keep working.
        let readings = vec![mk_reading("Board", "VRM MOS", "\u{00B0}C", 47.0)];
        assert!(match_reading(&readings, &[], &["VRM MOS"], &[], &["C"]).is_some());
    }

    /// "CPU Package" is a substring of "CPU Package Power" (watts), and the
    /// entity is declared device_class temperature. Without a pinned unit the
    /// wattage reading won.
    #[test]
    fn cpu_package_temp_does_not_match_the_power_reading() {
        let readings = vec![
            mk_reading("AMD Ryzen 7 9800X3D", "CPU Package Power", "W", 88.0),
            mk_reading("AMD Ryzen 7 9800X3D", "CPU Package", "\u{00B0}C", 61.0),
        ];
        let r = match_reading(&readings, &["ryzen"], &["CPU Package"], &[], &["C", "F"])
            .expect("should match the temperature");
        assert!(
            (r.value - 61.0).abs() < f64::EPSILON,
            "must pick the temperature reading, not the watts one (got {})",
            r.value
        );
    }

    /// Temperature units follow HWiNFO's own configuration, so pinning only "C"
    /// would make the rule vanish entirely for Fahrenheit users rather than
    /// merely report an odd number.
    #[test]
    fn cpu_package_temp_still_matches_in_fahrenheit() {
        let readings = vec![mk_reading(
            "AMD Ryzen 7 9800X3D",
            "CPU Package",
            "\u{00B0}F",
            142.0,
        )];
        assert!(match_reading(&readings, &["ryzen"], &["CPU Package"], &[], &["C", "F"]).is_some());
    }

    /// HWiNFO reports both an RPM reading and a percent duty-cycle sibling under
    /// "GPU Fan"; first-in-order used to win.
    #[test]
    fn gpu_fan_rpm_does_not_match_the_percent_sibling() {
        let readings = vec![
            mk_reading("NVIDIA GeForce RTX 4090", "GPU Fan", "%", 38.0),
            mk_reading("NVIDIA GeForce RTX 4090", "GPU Fan", "RPM", 1450.0),
        ];
        let r = match_reading(&readings, &["rtx"], &["GPU Fan"], &[], &["RPM"])
            .expect("should match the RPM reading");
        assert!(
            (r.value - 1450.0).abs() < f64::EPSILON,
            "must pick the RPM reading, not the percent sibling (got {})",
            r.value
        );
    }

    /// Every rule that pins a unit must actually be reachable: an empty suffix
    /// list means "no constraint", which is different from a typo'd one.
    #[test]
    fn every_pinned_unit_suffix_is_ascii() {
        for rule in MATCH_RULES {
            for sfx in rule.unit_suffix {
                assert!(
                    sfx.is_ascii(),
                    "rule {} pins a non-ASCII unit {sfx:?}; ascii_tail_ends_with would strip it \
                     to nothing and match everything",
                    rule.key
                );
                assert!(
                    !sfx.is_empty(),
                    "rule {} pins an empty unit suffix",
                    rule.key
                );
            }
        }
    }

    // ── match_all_borrowed: the aggregation, which had NO test at all ─────────
    //
    // It was previously unreachable from tests because it took a live
    // `HwInfoClient`. Taking a `&[u8]` view makes the per-rule slot, the
    // tie-break and the hits/misses partition drivable from a fixture.

    /// Cross-check the single borrowing pass against the owned-slice oracle.
    #[test]
    fn match_all_borrowed_agrees_with_the_oracle() {
        let buf = crate::hwinfo::test_support::buffer_with(
            &["CPU [#0]: AMD Ryzen 7 9800X3D"],
            &[
                (0, "CPU Package Power", "W", 88.0),
                (0, "CPU Package", "\u{00B0}C", 61.0),
                (0, "Average Effective Clock", "MHz", 4200.0),
            ],
        );
        let (hits, misses) = match_all_borrowed(&buf).expect("aggregate");

        // Same inputs through the owned oracle, rule by rule.
        let mut owned = Vec::new();
        crate::hwinfo::for_each_reading(&buf, |r| owned.push(r.to_owned_reading()))
            .expect("collect");
        for rule in MATCH_RULES {
            let oracle = match_reading(
                &owned,
                rule.sensor_substrings,
                rule.label_substrings,
                rule.label_excludes,
                rule.unit_suffix,
            );
            let got = hits.iter().find(|(k, _)| *k == rule.key).map(|(_, r)| r);
            match (oracle, got) {
                (Some(a), Some(b)) => assert_eq!(a.label, b.label, "rule {}", rule.key),
                (None, None) => assert!(misses.contains(&rule.key), "rule {} missing", rule.key),
                (a, b) => panic!(
                    "rule {} disagreed: oracle={:?} borrowed={:?}",
                    rule.key, a, b
                ),
            }
        }
    }

    /// The unit pin must make cpu_package_temp pick the temperature, not the
    /// watts reading whose label it is a substring of.
    #[test]
    fn match_all_borrowed_honours_unit_pins() {
        let buf = crate::hwinfo::test_support::buffer_with(
            &["CPU [#0]: AMD Ryzen 7 9800X3D"],
            &[
                (0, "CPU Package Power", "W", 88.0),
                (0, "CPU Package", "\u{00B0}C", 61.0),
            ],
        );
        let (hits, _) = match_all_borrowed(&buf).expect("aggregate");
        let temp = hits
            .iter()
            .find(|(k, _)| *k == "cpu_package_temp")
            .map(|(_, r)| r)
            .expect("cpu_package_temp should match");
        assert!(
            (temp.value - 61.0).abs() < f64::EPSILON,
            "picked the watts reading: {}",
            temp.value
        );
    }

    /// Every rule that matches nothing must land in `misses`, exactly once, and
    /// hits + misses must together account for every rule.
    #[test]
    fn match_all_borrowed_partitions_every_rule() {
        let buf = crate::hwinfo::test_support::buffer_with(&["Nothing"], &[]);
        let (hits, misses) = match_all_borrowed(&buf).expect("aggregate");
        assert!(hits.is_empty(), "no reading should match anything");
        assert_eq!(
            misses.len(),
            MATCH_RULES.len(),
            "every rule must be reported as a miss"
        );
        let mut sorted = misses.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), misses.len(), "a rule was reported twice");
    }
}
