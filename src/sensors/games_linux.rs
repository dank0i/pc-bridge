//! Game detection sensor for Linux - monitors running processes
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)

use std::sync::Arc;
use std::fs;
use tokio::time::{interval, Duration};
use tracing::{debug, error};

use crate::AppState;

pub struct GameSensor {
    state: Arc<AppState>,
}

impl GameSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        let interval_secs = config.intervals.game_sensor;
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();

        // Publish initial state
        let game = self.detect_game().await;
        self.state.mqtt.publish_sensor("runninggames", &game).await;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    debug!("Game sensor shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let game = self.detect_game().await;
                    self.state.mqtt.publish_sensor("runninggames", &game).await;
                }
            }
        }
    }

    async fn detect_game(&self) -> String {
        // Enumerate processes via /proc
        let processes = match self.get_process_names() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to enumerate processes: {}", e);
                return "none".to_string();
            }
        };

        // Check config games (includes Steam auto-discovered games)
        let config = self.state.config.read().await;
        let games = config.games.clone();
        drop(config);

        for proc_name in &processes {
            let proc_lower = proc_name.to_lowercase();

            for (pattern, game_config) in &games {
                let pattern_lower = pattern.to_lowercase();
                if proc_lower.contains(&pattern_lower) {
                    return game_config.game_id().to_string();
                }
            }
        }

        "none".to_string()
    }

    fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();

        // Read /proc to enumerate processes
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let path = entry.path();
            
            // Only process numeric directories (PIDs)
            if let Some(name) = path.file_name() {
                if let Some(name_str) = name.to_str() {
                    if name_str.chars().all(|c| c.is_ascii_digit()) {
                        // Read the process command line or comm
                        let comm_path = path.join("comm");
                        if let Ok(comm) = fs::read_to_string(&comm_path) {
                            let comm = comm.trim().to_string();
                            if !comm.is_empty() {
                                names.push(comm);
                            }
                        }
                    }
                }
            }
        }

        Ok(names)
    }
}
