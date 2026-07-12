//! Runtime task supervisor: starts and stops sensor tasks as their feature flags
//! change, so enabling/disabling a feature takes effect live (no restart).
//!
//! Two kinds of supervised task:
//! - Pure-async polling sensors (gpu, network, disk, uptime, games, custom,
//!   steam, idle, volume, audio_device, capture) hold no per-task OS thread, so
//!   they're cancelled by dropping their future (`cancelable` selects the run()
//!   future against a per-task cancel) - zero changes to those sensors.
//! - Thread-holding sensors (system, session, now_playing, power) take the
//!   per-task shutdown SENDER into run() and use it (loop + their OS threads) in
//!   place of the global shutdown, so firing it stops them and their threads.
//!
//! The supervisor fires a task's sender on disable and on global shutdown. HWiNFO
//! (Windows-only) stays startup-gated in main.rs.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::info;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::config::Config;
use crate::power::PowerEventListener;
use crate::sensors::{
    ActiveWindowSensor, AudioDeviceSensor, CaptureSensor, CustomSensorManager, DiskSensor,
    GameSensor, GpuSensor, IdleSensor, NetworkSensor, NowPlayingSensor, SessionSensor, SteamSensor,
    SystemSensor, UptimeSensor, VolumeSensor,
};

/// Run `fut` until it finishes on its own (global shutdown, handled inside the
/// sensor via `state.shutdown_tx`) OR the supervisor cancels this task (feature
/// disabled). Dropping the future on cancel is safe because these sensors hold no
/// OS threads - only per-tick spawn_blocking work, which is allowed to finish.
async fn cancelable(fut: impl Future<Output = ()>, mut cancel: broadcast::Receiver<()>) {
    tokio::select! {
        () = fut => {}
        _ = cancel.recv() => {}
    }
}

/// One supervised task: a name, whether it should be running for a given config,
/// and how to spawn it given a per-task shutdown SENDER. Pure-async sensors wrap
/// their run() in `cancelable(.., sd.subscribe())` (cancel = drop the future).
/// Sensors that hold OS threads instead pass the sender into their run() and use
/// it (loop + threads) in place of the global shutdown, so firing it stops them
/// cleanly. The supervisor fires each task's sender on disable and on shutdown.
struct TaskDef {
    name: &'static str,
    enabled: fn(&Config) -> bool,
    spawn: fn(Arc<AppState>, broadcast::Sender<()>) -> JoinHandle<()>,
}

const TASKS: &[TaskDef] = &[
    TaskDef {
        name: "gpu",
        enabled: |c| c.features.gpu_sensor,
        spawn: |s, c| tokio::spawn(cancelable(GpuSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "network",
        enabled: |c| c.features.network_sensor,
        spawn: |s, c| tokio::spawn(cancelable(NetworkSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "disk",
        enabled: |c| c.features.disk_sensor,
        spawn: |s, c| tokio::spawn(cancelable(DiskSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "uptime",
        enabled: |c| c.features.uptime_sensor,
        spawn: |s, c| tokio::spawn(cancelable(UptimeSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "games",
        enabled: |c| c.features.running_game || c.features.game_catalog,
        spawn: |s, c| tokio::spawn(cancelable(GameSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "custom_sensors",
        enabled: |c| c.custom_sensors_enabled && !c.custom_sensors.is_empty(),
        spawn: |s, c| tokio::spawn(cancelable(CustomSensorManager::new(s).run(), c.subscribe())),
    },
    // These hold no per-task OS thread either: steam's fs-watcher is dropped with
    // the future; volume/audio_device/idle poll via spawn_blocking. (Their
    // process-wide COM listener / ext-idle-notify helper is idempotent and
    // harmless if it lingers while disabled - a later pass can tear those down.)
    TaskDef {
        name: "steam",
        enabled: |c| c.features.steam_updates,
        spawn: |s, c| tokio::spawn(cancelable(SteamSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "idle",
        enabled: |c| c.features.idle_tracking,
        spawn: |s, c| tokio::spawn(cancelable(IdleSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "volume",
        enabled: |c| c.features.volume,
        spawn: |s, c| tokio::spawn(cancelable(VolumeSensor::new(s).run(), c.subscribe())),
    },
    TaskDef {
        name: "audio_device",
        enabled: |c| c.features.audio_device,
        spawn: |s, c| tokio::spawn(cancelable(AudioDeviceSensor::new(s).run(), c.subscribe())),
    },
    // Thread-holding sensors: run() takes the per-task shutdown SENDER and uses it
    // (loop + OS threads) instead of state.shutdown_tx, so firing it stops them.
    // (capture holds a registry-notification thread on Windows.)
    TaskDef {
        name: "capture",
        enabled: |c| c.features.mic || c.features.webcam,
        spawn: |s, c| tokio::spawn(CaptureSensor::new(s).run(c)),
    },
    TaskDef {
        name: "system",
        enabled: |c| c.features.cpu_sensor || c.features.memory_sensor || c.features.active_window,
        spawn: |s, c| tokio::spawn(SystemSensor::new(s).run(c)),
    },
    TaskDef {
        name: "active_window",
        enabled: |c| c.features.active_window,
        spawn: |s, c| tokio::spawn(ActiveWindowSensor::new(s).run(c)),
    },
    TaskDef {
        name: "session",
        enabled: |c| c.features.session_state,
        spawn: |s, c| tokio::spawn(SessionSensor::new(s).run(c)),
    },
    TaskDef {
        name: "now_playing",
        enabled: |c| c.features.now_playing,
        spawn: |s, c| tokio::spawn(NowPlayingSensor::new(s).run(c)),
    },
    TaskDef {
        name: "power",
        enabled: |c| c.features.sleep_wake || c.features.display_state,
        spawn: |s, c| tokio::spawn(PowerEventListener::new(s).run(c)),
    },
];

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the supervisor re-checks task health independent of config changes,
/// so a sensor that died on its own is respawned within a bounded time.
const RECONCILE_INTERVAL: Duration = Duration::from_mins(1);
/// A task that finished within this long of being (re)spawned is failing fast.
const FLAP_WINDOW: Duration = Duration::from_secs(10);
/// How long to wait before respawning a flapping task, so a permanently-broken
/// sensor doesn't hot-loop spawn->die->spawn.
const FLAP_BACKOFF: Duration = Duration::from_mins(1);

/// Whether to (re)spawn a wanted-but-not-running task this pass, and whether to
/// open a flap-backoff window. Pure so the flap logic is unit-testable.
#[derive(Debug, PartialEq)]
struct RespawnPlan {
    spawn: bool,
    start_backoff: bool,
}

fn respawn_plan(
    was_finished: bool,
    since_spawn: Option<Duration>,
    backoff_active: bool,
) -> RespawnPlan {
    // A task that died within FLAP_WINDOW of spawning is failing fast: skip this
    // pass and open a backoff window.
    if was_finished && since_spawn.is_some_and(|d| d < FLAP_WINDOW) {
        return RespawnPlan {
            spawn: false,
            start_backoff: true,
        };
    }
    // Respect an active backoff window opened by an earlier flap.
    if backoff_active {
        return RespawnPlan {
            spawn: false,
            start_backoff: false,
        };
    }
    RespawnPlan {
        spawn: true,
        start_backoff: false,
    }
}

pub struct Supervisor {
    state: Arc<AppState>,
    /// task name -> (handle, cancel sender)
    running: HashMap<&'static str, (JoinHandle<()>, broadcast::Sender<()>)>,
    /// task name -> times respawned after finishing on its own (feeds the
    /// bridge_health attributes; a climbing count flags a flapping sensor).
    restarts: HashMap<&'static str, u32>,
    /// task name -> when it was last (re)spawned, for the flap-backoff check.
    spawned_at: HashMap<&'static str, Instant>,
    /// task name -> respawn is deferred until this instant (flap backoff).
    backoff_until: HashMap<&'static str, Instant>,
    /// Last serialized bridge_health attributes, to change-gate the retained
    /// attributes topic (task states + MB-quantized RSS change rarely).
    last_health_attrs: String,
}

impl Supervisor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            running: HashMap::new(),
            restarts: HashMap::new(),
            spawned_at: HashMap::new(),
            backoff_until: HashMap::new(),
            last_health_attrs: String::new(),
        }
    }

    /// Start tasks that should be running and aren't, stop ones that shouldn't be.
    async fn reconcile(&mut self) {
        // Snapshot the desired state under a brief read lock.
        let wants: Vec<bool> = {
            let cfg = self.state.config.read().await;
            TASKS.iter().map(|t| (t.enabled)(&cfg)).collect()
        };

        // Set when we start a task this pass; if so we yield at the end so every
        // freshly-spawned task gets polled (and thus subscribes its cancel) before
        // the next reconcile could stop it. Note the missed-cancel window is
        // already closed on this single-threaded runtime: reconcile reads CURRENT
        // config, so for a later reconcile to cancel a just-started task the config
        // must have changed, which requires the writer task to run, which requires
        // us to yield - and any yield also polls the new task. Pure-async sensors
        // additionally subscribe synchronously at spawn (the `c.subscribe()`
        // argument is evaluated before tokio::spawn). This yield is belt-and-braces.
        let mut spawned_any = false;

        for (def, want) in TASKS.iter().zip(wants) {
            // "In the map" is not "alive": a task that returned on its own (e.g.
            // an init failure early-return) leaves a finished handle. Treat that
            // as not-running so it can be respawned while still wanted.
            let mut was_finished = false;
            let have = match self.running.get(def.name) {
                Some((h, _)) if h.is_finished() => {
                    self.running.remove(def.name);
                    was_finished = true;
                    false
                }
                Some(_) => true,
                None => false,
            };
            match (want, have) {
                (true, false) => {
                    // Respawning a task that finished on its own is a restart, not
                    // an initial start - count it for the health attributes.
                    if was_finished {
                        *self.restarts.entry(def.name).or_insert(0) += 1;
                    }
                    let now = Instant::now();
                    let since_spawn = self
                        .spawned_at
                        .get(def.name)
                        .map(|t| now.duration_since(*t));
                    let backoff_active = self.backoff_until.get(def.name).is_some_and(|t| *t > now);
                    let plan = respawn_plan(was_finished, since_spawn, backoff_active);
                    if plan.start_backoff {
                        self.backoff_until.insert(def.name, now + FLAP_BACKOFF);
                        info!(
                            "Supervisor: {} died within {}s of spawn; backing off {}s",
                            def.name,
                            FLAP_WINDOW.as_secs(),
                            FLAP_BACKOFF.as_secs()
                        );
                    }
                    if !plan.spawn {
                        continue;
                    }
                    self.backoff_until.remove(def.name);
                    let (tx, _) = broadcast::channel(1);
                    let handle = (def.spawn)(Arc::clone(&self.state), tx.clone());
                    self.running.insert(def.name, (handle, tx));
                    self.spawned_at.insert(def.name, now);
                    spawned_any = true;
                    info!("Supervisor: started {}", def.name);
                }
                (false, true) => {
                    if let Some((handle, tx)) = self.running.remove(def.name) {
                        let _ = tx.send(()); // cancel -> stops the sensor
                        // Abort on timeout: dropping the handle would DETACH (leak)
                        // the task, not stop it, so a slow/stuck sensor would keep
                        // running and re-enable would spawn a duplicate.
                        let abort = handle.abort_handle();
                        if tokio::time::timeout(Duration::from_secs(2), handle)
                            .await
                            .is_err()
                        {
                            abort.abort();
                        }
                        info!("Supervisor: stopped {}", def.name);
                    }
                }
                _ => {}
            }
        }

        // Let freshly-spawned tasks run to their first await (subscribing their
        // cancel) before we could be asked to stop them.
        if spawned_any {
            tokio::task::yield_now().await;
        }
    }

    pub async fn run(mut self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Initial start of everything currently enabled.
        self.reconcile().await;

        // bridge_health is the HA launch gate's live-ping and must be published
        // regardless of which sensor features are on, so it lives here in the
        // always-on supervisor rather than the (feature-gated) system sensor.
        // Publish an immediate heartbeat, then every 60s.
        self.publish_bridge_health().await;
        let mut health_tick = tokio::time::interval(Duration::from_mins(1));
        health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        health_tick.tick().await; // consume the interval's immediate first tick

        // Periodic reconcile so a task that died on its own (or a flap-backoff
        // window that has elapsed) is respawned even without a config change.
        let mut reconcile_tick = tokio::time::interval(RECONCILE_INTERVAL);
        reconcile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile_tick.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => break,
                r = config_rx.recv() => {
                    // Re-evaluate on any config change (Lagged too - don't miss it).
                    if matches!(r, Ok(()) | Err(broadcast::error::RecvError::Lagged(_))) {
                        self.reconcile().await;
                    }
                }
                _ = reconcile_tick.tick() => {
                    self.reconcile().await;
                }
                _ = health_tick.tick() => {
                    self.publish_bridge_health().await;
                }
            }
        }

        // Global shutdown: fire EVERY task's shutdown first (so they stop in
        // parallel), then await them under a single 2s budget. Awaiting serially
        // with a per-task timeout could exceed main's 5s shutdown cap; firing all
        // then waiting once keeps the whole drain bounded to ~2s.
        let mut handles = Vec::new();
        for (_, (handle, tx)) in self.running.drain() {
            let _ = tx.send(());
            handles.push(handle);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
    }

    /// Publish the bridge_health heartbeat: uptime seconds as state (published
    /// every tick for liveness), and version + per-task states + agent RSS as
    /// change-gated attributes.
    async fn publish_bridge_health(&mut self) {
        let uptime_secs = self.state.start_time.elapsed().as_secs();

        let tasks: Vec<serde_json::Value> = TASKS
            .iter()
            .map(|t| {
                let running = self
                    .running
                    .get(t.name)
                    .is_some_and(|(h, _)| !h.is_finished());
                serde_json::json!({
                    "name": t.name,
                    "running": running,
                    "restarts": self.restarts.get(t.name).copied().unwrap_or(0),
                })
            })
            .collect();

        let attrs = build_health_attrs(&tasks, agent_rss_bytes());

        self.state
            .mqtt
            .publish_sensor("bridge_health", &uptime_secs.to_string())
            .await;

        let serialized = attrs.to_string();
        if serialized != self.last_health_attrs {
            self.state
                .mqtt
                .publish_sensor_attributes("bridge_health", &attrs)
                .await;
            self.last_health_attrs = serialized;
        }
    }
}

/// Assemble the bridge_health attributes. RSS is quantized to whole MB so a
/// few-KB jitter doesn't churn the retained attributes topic.
fn build_health_attrs(tasks: &[serde_json::Value], rss_bytes: Option<u64>) -> serde_json::Value {
    let mut attrs = serde_json::json!({
        "version": VERSION,
        "tasks": tasks,
    });
    if let Some(bytes) = rss_bytes {
        attrs["memory_mb"] = serde_json::json!(bytes / (1024 * 1024));
    }
    attrs
}

/// Resident set size of this process in bytes, or None where unsupported.
#[cfg(windows)]
fn agent_rss_bytes() -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: counters is a valid, correctly-sized out-buffer; the pseudo-handle
    // from GetCurrentProcess needs no close.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb).is_ok() };
    ok.then_some(counters.WorkingSetSize as u64)
}

/// Resident set size of this process in bytes, or None where unsupported.
#[cfg(target_os = "linux")]
fn agent_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    // VmRSS is reported in kB.
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

/// Resident set size of this process in bytes, or None where unsupported
/// (e.g. macOS dev builds - the agent ships only for Windows and Linux).
#[cfg(not(any(windows, target_os = "linux")))]
fn agent_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_respawn_plan() {
        // Fresh start (never spawned): spawn, no backoff.
        assert_eq!(
            respawn_plan(false, None, false),
            RespawnPlan {
                spawn: true,
                start_backoff: false
            }
        );
        // Finished long after spawn: normal restart, spawn.
        assert_eq!(
            respawn_plan(true, Some(Duration::from_secs(30)), false),
            RespawnPlan {
                spawn: true,
                start_backoff: false
            }
        );
        // Finished within the flap window: skip and open a backoff window.
        assert_eq!(
            respawn_plan(true, Some(Duration::from_secs(2)), false),
            RespawnPlan {
                spawn: false,
                start_backoff: true
            }
        );
        // Backoff still active (from an earlier flap): skip, don't re-open.
        assert_eq!(
            respawn_plan(false, Some(Duration::from_secs(2)), true),
            RespawnPlan {
                spawn: false,
                start_backoff: false
            }
        );
    }

    #[test]
    fn health_attrs_include_version_and_tasks() {
        let tasks = vec![serde_json::json!({
            "name": "gpu",
            "running": true,
            "restarts": 2,
        })];
        let attrs = build_health_attrs(&tasks, None);
        assert_eq!(attrs["version"], VERSION);
        assert_eq!(attrs["tasks"][0]["name"], "gpu");
        assert_eq!(attrs["tasks"][0]["restarts"], 2);
        // No RSS reading -> memory_mb omitted, not null.
        assert!(attrs.get("memory_mb").is_none());
    }

    #[test]
    fn health_attrs_quantize_rss_to_mb() {
        // 5 MB + 700 KB rounds down to 5 MB (whole-MB quantization).
        let bytes = 5 * 1024 * 1024 + 700 * 1024;
        let attrs = build_health_attrs(&[], Some(bytes));
        assert_eq!(attrs["memory_mb"], 5);
    }
}
