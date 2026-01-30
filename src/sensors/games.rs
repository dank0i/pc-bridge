//! Game detection sensor - monitors running processes to detect games
//!
//! Detection priority:
//! 1. Steam auto-discovery (if Steam installed) - uses process name → app_id lookup
//! 2. Manual config `games` map (pattern → game_id)

use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::Foundation::CloseHandle;

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
        let interval_secs = config.intervals.game_sensor.max(1); // Prevent panic on 0
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
        // Enumerate processes
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
            let base_name = proc_lower.trim_end_matches(".exe");

            for (pattern, game_config) in &games {
                let pattern_lower = pattern.to_lowercase();
                if proc_lower.starts_with(&pattern_lower) || base_name == pattern_lower {
                    return game_config.game_id().to_string();
                }
            }
        }

        "none".to_string()
    }

    fn get_process_names(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };

            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let name = String::from_utf16_lossy(&entry.szExeFile)
                        .trim_end_matches('\0')
                        .to_string();
                    
                    if !name.is_empty() {
                        names.push(name);
                    }

                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
        }

        Ok(names)
    }
}
