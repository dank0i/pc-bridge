//! Runtime task supervisor: starts and stops sensor tasks as their feature flags
//! change, so enabling/disabling a feature takes effect live (no restart).
//!
//! Scope: this supervises the pure-async polling sensors (gpu, network, disk,
//! uptime, games, custom) - tasks that loop on a timer + spawn_blocking and hold
//! NO persistent OS threads. Those can be cancelled cleanly by dropping their
//! future (via a select against a per-task cancel channel), with zero changes to
//! the sensors. Sensors that spawn long-lived threads (SystemSensor, session,
//! now_playing, the power listener, steam's fs-watcher, HWiNFO, the audio/media
//! COM sensors) are still spawned once at startup in main.rs; giving THEM live
//! start/stop needs a per-task shutdown wired through their threads, which is the
//! follow-up refactor.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use log::info;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::config::Config;
use crate::sensors::{
    CustomSensorManager, DiskSensor, GameSensor, GpuSensor, NetworkSensor, UptimeSensor,
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
/// and how to spawn it wrapped in the cancel guard.
struct TaskDef {
    name: &'static str,
    enabled: fn(&Config) -> bool,
    spawn: fn(Arc<AppState>, broadcast::Receiver<()>) -> JoinHandle<()>,
}

const TASKS: &[TaskDef] = &[
    TaskDef {
        name: "gpu",
        enabled: |c| c.features.gpu_sensor,
        spawn: |s, c| tokio::spawn(cancelable(GpuSensor::new(s).run(), c)),
    },
    TaskDef {
        name: "network",
        enabled: |c| c.features.network_sensor,
        spawn: |s, c| tokio::spawn(cancelable(NetworkSensor::new(s).run(), c)),
    },
    TaskDef {
        name: "disk",
        enabled: |c| c.features.disk_sensor,
        spawn: |s, c| tokio::spawn(cancelable(DiskSensor::new(s).run(), c)),
    },
    TaskDef {
        name: "uptime",
        enabled: |c| c.features.uptime_sensor,
        spawn: |s, c| tokio::spawn(cancelable(UptimeSensor::new(s).run(), c)),
    },
    TaskDef {
        name: "games",
        enabled: |c| c.features.running_game || c.features.game_catalog,
        spawn: |s, c| tokio::spawn(cancelable(GameSensor::new(s).run(), c)),
    },
    TaskDef {
        name: "custom_sensors",
        enabled: |c| c.custom_sensors_enabled && !c.custom_sensors.is_empty(),
        spawn: |s, c| tokio::spawn(cancelable(CustomSensorManager::new(s).run(), c)),
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
            let have = self.running.contains_key(def.name);
            match (want, have) {
                (true, false) => {
                    let (tx, rx) = broadcast::channel(1);
                    let handle = (def.spawn)(Arc::clone(&self.state), rx);
                    self.running.insert(def.name, (handle, tx));
                    info!("Supervisor: started {}", def.name);
                }
                (false, true) => {
                    if let Some((handle, tx)) = self.running.remove(def.name) {
                        let _ = tx.send(()); // cancel -> drops the sensor future
                        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
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

        // Global shutdown: each supervised task also observes state.shutdown_tx
        // and exits on its own, so we just wait briefly for them to finish.
        for (_, (handle, _)) in self.running.drain() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
    }
}
