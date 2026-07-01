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
use std::time::Duration;

use log::info;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::config::Config;
use crate::power::PowerEventListener;
use crate::sensors::{
    AudioDeviceSensor, CaptureSensor, CustomSensorManager, DiskSensor, GameSensor, GpuSensor,
    IdleSensor, NetworkSensor, NowPlayingSensor, SessionSensor, SteamSensor, SystemSensor,
    UptimeSensor, VolumeSensor,
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
    // the future; volume/audio_device/capture/idle poll via spawn_blocking. (Their
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
    TaskDef {
        name: "capture",
        enabled: |c| c.features.mic || c.features.webcam,
        spawn: |s, c| tokio::spawn(cancelable(CaptureSensor::new(s).run(), c.subscribe())),
    },
    // Thread-holding sensors: run() takes the per-task shutdown SENDER and uses it
    // (loop + OS threads) instead of state.shutdown_tx, so firing it stops them.
    TaskDef {
        name: "system",
        enabled: |c| c.features.cpu_sensor || c.features.memory_sensor || c.features.active_window,
        spawn: |s, c| tokio::spawn(SystemSensor::new(s).run(c)),
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

pub struct Supervisor {
    state: Arc<AppState>,
    /// task name -> (handle, cancel sender)
    running: HashMap<&'static str, (JoinHandle<()>, broadcast::Sender<()>)>,
}

impl Supervisor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            running: HashMap::new(),
        }
    }

    /// Start tasks that should be running and aren't, stop ones that shouldn't be.
    async fn reconcile(&mut self) {
        // Snapshot the desired state under a brief read lock.
        let wants: Vec<bool> = {
            let cfg = self.state.config.read().await;
            TASKS.iter().map(|t| (t.enabled)(&cfg)).collect()
        };

        for (def, want) in TASKS.iter().zip(wants) {
            // "In the map" is not "alive": a task that returned on its own (e.g.
            // an init failure early-return) leaves a finished handle. Treat that
            // as not-running so it can be respawned while still wanted.
            let have = match self.running.get(def.name) {
                Some((h, _)) if h.is_finished() => {
                    self.running.remove(def.name);
                    false
                }
                Some(_) => true,
                None => false,
            };
            match (want, have) {
                (true, false) => {
                    let (tx, _) = broadcast::channel(1);
                    let handle = (def.spawn)(Arc::clone(&self.state), tx.clone());
                    self.running.insert(def.name, (handle, tx));
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
    }

    pub async fn run(mut self) {
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut config_rx = self.state.config_generation.subscribe();

        // Initial start of everything currently enabled.
        self.reconcile().await;

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
}
