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
//! Every sensor task is supervised, including HWiNFO (Windows-only),
//! which used to be startup-gated in main.rs.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::info;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::config::Config;
use crate::modules::ModuleManager;
#[cfg(any(windows, target_os = "linux"))]
use crate::power::DisplayAttachedSensor;
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
        // The path list is part of the gate, not just the flag. With the flag on
        // and no paths, run() returned immediately, the supervisor saw a finished
        // handle, and respawned forever - climbing the restart counter that
        // bridge_health reports on an otherwise healthy agent. `disk_sensor_paths`
        // defaults to empty and nothing in the wizard or settings UI fills it, so
        // this was reachable just by ticking "Disk Usage". Same shape as
        // custom_sensors below.
        enabled: |c| c.features.disk_sensor && !c.disk_sensor_paths.is_empty(),
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
    // Thread-holding in effect: this task owns child PROCESSES, so it takes the
    // sender rather than a receiver. Cancelling it must also stop what it
    // spawned, or disabling the feature would leave orphans running.
    // Same flag-AND-non-empty gate as `disk` above, for the same reason.
    TaskDef {
        name: "modules",
        enabled: |c| c.modules_enabled && !c.modules.is_empty(),
        spawn: |s, c| tokio::spawn(ModuleManager::new(s).run(c)),
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
    // Holds a file mapping into HWiNFO's shared memory, so run() takes the
    // sender. Supervised (it was startup-gated and unsupervised) so toggling
    // hwinfo_sensor at runtime actually starts and stops the producer.
    #[cfg(windows)]
    TaskDef {
        name: "hwinfo",
        enabled: |c| c.features.hwinfo_sensor,
        spawn: |s, c| tokio::spawn(crate::sensors::hwinfo::HwInfoSensor::new(s).run(c)),
    },
    // Windows holds a CfgMgr32 notification registration and Linux polls DRM
    // connectors; both take the sender directly so they can tear down their own
    // registration / republish on reconnect, rather than being wrapped in
    // cancelable().
    //
    // Gated to match the discovery registration exactly. macOS has no producer,
    // and a task publishing to an entity that was never registered would leave a
    // retained orphan on the broker.
    #[cfg(any(windows, target_os = "linux"))]
    TaskDef {
        name: "display_attached",
        enabled: |c| c.features.display_attached,
        spawn: |s, c| tokio::spawn(DisplayAttachedSensor::new(s).run(c)),
    },
];

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the supervisor re-checks task health independent of config changes,
/// so a sensor that died on its own is respawned within a bounded time.
const RECONCILE_INTERVAL: Duration = Duration::from_mins(1);
/// Upper bound on the escalating respawn backoff, so a permanently-broken sensor
/// is retried occasionally without hammering (respawn + log line) every reconcile.
const BACKOFF_CAP: Duration = Duration::from_mins(5);

/// Whether to (re)spawn a wanted-but-not-running task this pass, and how long to
/// defer the NEXT respawn if it keeps dying. Detection is reconcile-paced (a dead
/// task is only noticed on the periodic tick), so flapping is measured by how many
/// consecutive reconciles have found the task dead, not by wall-clock-since-spawn.
/// Pure so the logic is unit-testable.
#[derive(Debug, PartialEq)]
struct RespawnPlan {
    spawn: bool,
    /// When `Some`, open a backoff window of this length before the next respawn.
    backoff: Option<Duration>,
}

fn respawn_plan(consecutive_fails: u32, backoff_active: bool) -> RespawnPlan {
    // A backoff window opened by an earlier repeated failure hasn't elapsed yet.
    if backoff_active {
        return RespawnPlan {
            spawn: false,
            backoff: None,
        };
    }
    // Respawn now. A task that has already died repeatedly opens an escalating
    // (capped) backoff so its next death waits longer instead of retrying every
    // reconcile forever; a one-off death (fails <= 1) respawns with no backoff.
    let backoff =
        (consecutive_fails >= 2).then(|| (RECONCILE_INTERVAL * consecutive_fails).min(BACKOFF_CAP));
    RespawnPlan {
        spawn: true,
        backoff,
    }
}

pub struct Supervisor {
    state: Arc<AppState>,
    /// task name -> (handle, cancel sender)
    running: HashMap<&'static str, (JoinHandle<()>, broadcast::Sender<()>)>,
    /// task name -> times respawned after finishing on its own (feeds the
    /// bridge_health attributes; a climbing count flags a flapping sensor).
    restarts: HashMap<&'static str, u32>,
    /// task name -> reconciles in a row that found it dead, reset when it's seen
    /// alive again. Drives the escalating respawn backoff.
    consecutive_fails: HashMap<&'static str, u32>,
    /// task name -> respawn is deferred until this instant (escalating backoff).
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
            consecutive_fails: HashMap::new(),
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
                    // an initial start - count it for the health attributes and the
                    // escalating backoff.
                    if was_finished {
                        *self.restarts.entry(def.name).or_insert(0) += 1;
                        *self.consecutive_fails.entry(def.name).or_insert(0) += 1;
                    }
                    let now = Instant::now();
                    let backoff_active = self.backoff_until.get(def.name).is_some_and(|t| *t > now);
                    let fails = self.consecutive_fails.get(def.name).copied().unwrap_or(0);
                    let plan = respawn_plan(fails, backoff_active);
                    if !plan.spawn {
                        continue;
                    }
                    if let Some(d) = plan.backoff {
                        self.backoff_until.insert(def.name, now + d);
                        info!(
                            "Supervisor: {} has died {} times in a row; backing off {}s before the next respawn",
                            def.name,
                            fails,
                            d.as_secs()
                        );
                    } else {
                        self.backoff_until.remove(def.name);
                    }
                    let (tx, _) = broadcast::channel(1);
                    let handle = (def.spawn)(Arc::clone(&self.state), tx.clone());
                    self.running.insert(def.name, (handle, tx));
                    spawned_any = true;
                    info!("Supervisor: started {}", def.name);
                }
                (true, true) => {
                    // Alive and wanted: it survived, so clear any failure/backoff
                    // state accumulated by earlier deaths.
                    self.consecutive_fails.remove(def.name);
                    self.backoff_until.remove(def.name);
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
        // Healthy first start (no prior failures): spawn, no backoff.
        assert_eq!(
            respawn_plan(0, false),
            RespawnPlan {
                spawn: true,
                backoff: None
            }
        );
        // First death: respawn immediately, still no backoff.
        assert_eq!(
            respawn_plan(1, false),
            RespawnPlan {
                spawn: true,
                backoff: None
            }
        );
        // Repeated deaths: respawn but open an escalating backoff for the next one.
        assert_eq!(
            respawn_plan(2, false),
            RespawnPlan {
                spawn: true,
                backoff: Some(RECONCILE_INTERVAL * 2)
            }
        );
        // Backoff window still active: skip this pass, don't respawn.
        assert_eq!(
            respawn_plan(3, true),
            RespawnPlan {
                spawn: false,
                backoff: None
            }
        );
        // Escalation is capped so a permanently-broken task is still retried.
        assert_eq!(
            respawn_plan(100, false),
            RespawnPlan {
                spawn: true,
                backoff: Some(BACKOFF_CAP)
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
