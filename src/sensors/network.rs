//! Network throughput sensor
//!
//! Reports total bytes sent/received per second across all interfaces.
//! - Windows: GetIfTable2 (IP Helper API)
//! - Linux: /proc/net/dev

use log::{debug, info};
use std::sync::Arc;
use tokio::time::{Duration, interval};

use crate::AppState;

pub struct NetworkSensor {
    state: Arc<AppState>,
}

impl NetworkSensor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn run(self) {
        let config = self.state.config.read().await;
        if !config.features.network_sensor {
            return;
        }
        let interval_secs = config.intervals.system_sensors.max(1);
        drop(config);

        let mut tick = interval(Duration::from_secs(interval_secs));
        let mut shutdown_rx = self.state.shutdown_tx.subscribe();
        let mut reconnect_rx = self.state.mqtt.subscribe_reconnect();
        let mut prev_sample = get_network_totals();
        let mut prev_rx = String::new();
        let mut prev_tx = String::new();

        // Consume first tick
        tick.tick().await;

        info!("Network sensor started (polled every {}s)", interval_secs);

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    debug!("Network sensor shutting down");
                    break;
                }
                Ok(()) = reconnect_rx.recv() => {
                    prev_rx.clear();
                    prev_tx.clear();
                }
                _ = tick.tick() => {
                    let curr = get_network_totals();
                    let rx_per_sec = (curr.0.saturating_sub(prev_sample.0)) / interval_secs;
                    let tx_per_sec = (curr.1.saturating_sub(prev_sample.1)) / interval_secs;
                    prev_sample = curr;

                    let rx_str = format_bytes_per_sec(rx_per_sec);
                    let tx_str = format_bytes_per_sec(tx_per_sec);

                    if rx_str != prev_rx || tx_str != prev_tx {
                        let attrs = serde_json::json!({
                            "rx_bytes_per_sec": rx_per_sec,
                            "tx_bytes_per_sec": tx_per_sec,
                            "rx_formatted": &rx_str,
                            "tx_formatted": &tx_str,
                        });
                        // State is combined throughput in human-readable form
                        let state = format!("↓{} ↑{}", rx_str, tx_str);
                        self.state.mqtt.publish_sensor("network_throughput", &state).await;
                        self.state.mqtt.publish_sensor_attributes("network_throughput", &attrs).await;
                        prev_rx = rx_str;
                        prev_tx = tx_str;
                    }
                }
            }
        }
    }
}

/// Returns (total_rx_bytes, total_tx_bytes) across all interfaces
#[cfg(windows)]
fn get_network_totals() -> (u64, u64) {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2};

    // IF_TYPE_SOFTWARE_LOOPBACK = 24
    const LOOPBACK_TYPE: u32 = 24;

    unsafe {
        let mut table = std::ptr::null_mut();
        if GetIfTable2(&mut table) != 0 || table.is_null() {
            return (0, 0);
        }

        let num_entries = (*table).NumEntries as usize;
        let entries = std::slice::from_raw_parts((*table).Table.as_ptr(), num_entries);

        let mut rx: u64 = 0;
        let mut tx: u64 = 0;

        for entry in entries {
            if entry.Type == LOOPBACK_TYPE {
                continue;
            }
            rx = rx.saturating_add(entry.InOctets);
            tx = tx.saturating_add(entry.OutOctets);
        }

        FreeMibTable(table as *const _);
        (rx, tx)
    }
}

fn format_bytes_per_sec(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB/s", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB/s", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB/s", bytes as f64 / 1024.0)
    } else {
        format!("{} B/s", bytes)
    }
}

/// Parse `/proc/net/dev` content and return `(total_rx, total_tx)` bytes,
/// skipping the loopback interface.
#[cfg(unix)]
fn parse_proc_net_dev(content: &str) -> (u64, u64) {
    let mut rx: u64 = 0;
    let mut tx: u64 = 0;

    for line in content.lines().skip(2) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 10 {
            let iface = parts[0].trim_end_matches(':');
            if iface == "lo" {
                continue;
            }
            if let Ok(r) = parts[1].parse::<u64>() {
                rx = rx.saturating_add(r);
            }
            if let Ok(t) = parts[9].parse::<u64>() {
                tx = tx.saturating_add(t);
            }
        }
    }

    (rx, tx)
}

#[cfg(unix)]
fn get_network_totals() -> (u64, u64) {
    if let Ok(content) = std::fs::read_to_string("/proc/net/dev") {
        parse_proc_net_dev(&content)
    } else {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== format_bytes_per_sec =====

    #[test]
    fn test_format_zero() {
        assert_eq!(format_bytes_per_sec(0), "0 B/s");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes_per_sec(512), "512 B/s");
        assert_eq!(format_bytes_per_sec(1023), "1023 B/s");
    }

    #[test]
    fn test_format_kilobytes() {
        assert_eq!(format_bytes_per_sec(1024), "1.0 KB/s");
        assert_eq!(format_bytes_per_sec(1536), "1.5 KB/s");
        assert_eq!(format_bytes_per_sec(1_048_575), "1024.0 KB/s");
    }

    #[test]
    fn test_format_megabytes() {
        assert_eq!(format_bytes_per_sec(1_048_576), "1.0 MB/s");
        assert_eq!(format_bytes_per_sec(10_485_760), "10.0 MB/s");
        assert_eq!(format_bytes_per_sec(1_073_741_823), "1024.0 MB/s");
    }

    #[test]
    fn test_format_gigabytes() {
        assert_eq!(format_bytes_per_sec(1_073_741_824), "1.0 GB/s");
        assert_eq!(format_bytes_per_sec(10_737_418_240), "10.0 GB/s");
    }

    // ===== parse_proc_net_dev (Linux) =====

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_net_dev_typical() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000000  5000    0    0    0     0          0         0  1000000   5000    0    0    0     0       0          0
  eth0: 5000000  30000   0    0    0     0          0         0  2000000  20000    0    0    0     0       0          0
 wlan0: 3000000  15000   0    0    0     0          0         0  1000000  10000    0    0    0     0       0          0";

        let (rx, tx) = parse_proc_net_dev(content);
        // Should skip lo, sum eth0 + wlan0
        assert_eq!(rx, 8_000_000);
        assert_eq!(tx, 3_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_net_dev_loopback_only() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000000  5000    0    0    0     0          0         0  1000000   5000    0    0    0     0       0          0";

        let (rx, tx) = parse_proc_net_dev(content);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_proc_net_dev_empty() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed";

        let (rx, tx) = parse_proc_net_dev(content);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }
}
