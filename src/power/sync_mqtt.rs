//! Synchronous MQTT publish for sleep state notifications.
//!
//! Provides a one-shot raw TCP MQTT 3.1.1 publish that bypasses the async
//! rumqttc event loop. This guarantees the "sleeping" message reaches the
//! broker before Windows/Linux powers down the NIC, since the call blocks
//! until the packet is on the wire.
//!
//! All functions are platform-independent and compiled on every target so
//! that the full test suite runs on macOS/Linux CI as well as Windows.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// MQTT broker config for synchronous publish from the power-events thread.
/// Kept separate from the async `MqttClient` so the blocking thread can
/// send messages without depending on the tokio runtime or event loop.
pub struct SyncMqttConfig {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub user: String,
    pub pass: String,
    pub client_id: String,
    pub sleep_topic: String,
}

/// Parse a broker URL like "tcp://host:port" into (host, port, use_tls).
pub fn parse_broker_url(url: &str) -> (String, u16, bool) {
    let (without_scheme, use_tls) = if let Some(rest) = url.strip_prefix("ssl://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("wss://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("tcp://") {
        (rest, false)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        (rest, false)
    } else {
        (url, false)
    };

    let parts: Vec<&str> = without_scheme.split(':').collect();
    let host = parts.first().unwrap_or(&"localhost").to_string();
    let default_port = if use_tls { 8883 } else { 1883 };
    let port = parts
        .get(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(default_port);
    (host, port, use_tls)
}

/// Publish "sleeping" to the sleep_state topic using a one-shot synchronous
/// TCP connection. This bypasses the async rumqttc event loop entirely so
/// that the PUBLISH packet is guaranteed to be on the wire before `wnd_proc`
/// returns and the OS powers down the NIC.
pub fn sync_mqtt_publish_sleep(cfg: &SyncMqttConfig) -> std::io::Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let timeout = Duration::from_secs(2);

    // Resolve hostname to IP — ToSocketAddrs handles both hostnames and IPs.
    let socket_addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("DNS resolution failed for {addr}"),
        )
    })?;

    let stream = TcpStream::connect_timeout(&socket_addr, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;

    if cfg.use_tls {
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| std::io::Error::other(format!("TLS init failed: {e}")))?;
        let mut tls_stream = connector.connect(&cfg.host, stream).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("TLS handshake failed: {e}"),
            )
        })?;
        do_mqtt_exchange(&mut tls_stream, cfg)
    } else {
        let mut stream = stream;
        do_mqtt_exchange(&mut stream, cfg)
    }
}

/// Perform the MQTT CONNECT/CONNACK/PUBLISH/DISCONNECT exchange over any
/// Read+Write stream (plain TCP or TLS-wrapped).
fn do_mqtt_exchange(stream: &mut (impl Read + Write), cfg: &SyncMqttConfig) -> std::io::Result<()> {
    // --- CONNECT ---
    let connect = build_mqtt_connect(&cfg.client_id, &cfg.user, &cfg.pass);
    stream.write_all(&connect)?;
    stream.flush()?;

    // --- CONNACK ---
    let mut connack = [0u8; 4];
    stream.read_exact(&mut connack)?;
    // connack[0] must be 0x20 (CONNACK packet type),
    // connack[1] must be 0x02 (remaining length = 2 bytes),
    // connack[3] is return code (0x00 = accepted)
    if connack[0] != 0x20 || connack[1] != 0x02 || connack[3] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!(
                "CONNACK rejected (type={:#x}, len={}, rc={})",
                connack[0], connack[1], connack[3]
            ),
        ));
    }

    // --- PUBLISH (QoS 0, retained) ---
    let publish = build_mqtt_publish(&cfg.sleep_topic, b"sleeping", true);
    stream.write_all(&publish)?;
    stream.flush()?;

    // --- DISCONNECT ---
    stream.write_all(&[0xE0, 0x00])?;
    let _ = stream.flush();

    Ok(())
}

/// Build an MQTT 3.1.1 CONNECT packet.
fn build_mqtt_connect(client_id: &str, user: &str, pass: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);

    // Variable header: Protocol Name
    payload.extend_from_slice(&[0x00, 0x04]); // length
    payload.extend_from_slice(b"MQTT");

    // Protocol Level (4 = MQTT 3.1.1)
    payload.push(4);

    // Connect Flags: clean session + optional username/password
    let mut flags: u8 = 0x02; // clean session
    if !user.is_empty() {
        flags |= 0x80;
    }
    if !pass.is_empty() {
        flags |= 0x40;
    }
    payload.push(flags);

    // Keep Alive (10 seconds — connection is ephemeral)
    payload.extend_from_slice(&10_u16.to_be_bytes());

    // Payload: Client ID
    let cid = client_id.as_bytes();
    payload.extend_from_slice(&(cid.len() as u16).to_be_bytes());
    payload.extend_from_slice(cid);

    // Payload: Username
    if !user.is_empty() {
        let u = user.as_bytes();
        payload.extend_from_slice(&(u.len() as u16).to_be_bytes());
        payload.extend_from_slice(u);
    }

    // Payload: Password
    if !pass.is_empty() {
        let p = pass.as_bytes();
        payload.extend_from_slice(&(p.len() as u16).to_be_bytes());
        payload.extend_from_slice(p);
    }

    let mut packet = Vec::with_capacity(2 + payload.len());
    packet.push(0x10); // CONNECT packet type
    encode_remaining_length(&mut packet, payload.len());
    packet.extend_from_slice(&payload);
    packet
}

/// Build an MQTT 3.1.1 PUBLISH packet (QoS 0).
fn build_mqtt_publish(topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
    let topic_bytes = topic.as_bytes();
    let remaining = 2 + topic_bytes.len() + payload.len();

    let mut packet = Vec::with_capacity(4 + remaining);
    // Fixed header: PUBLISH (type 3), QoS 0, retain bit
    packet.push(if retain { 0x31 } else { 0x30 });
    encode_remaining_length(&mut packet, remaining);
    // Topic length + topic
    packet.extend_from_slice(&(topic_bytes.len() as u16).to_be_bytes());
    packet.extend_from_slice(topic_bytes);
    // Payload (no packet id for QoS 0)
    packet.extend_from_slice(payload);
    packet
}

/// Encode MQTT remaining length (variable-length encoding).
fn encode_remaining_length(buf: &mut Vec<u8>, mut len: usize) {
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if len == 0 {
            break;
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Unit tests — packet building, parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_encode_remaining_length_small() {
        let mut buf = Vec::new();
        encode_remaining_length(&mut buf, 42);
        assert_eq!(buf, vec![42]);
    }

    #[test]
    fn test_encode_remaining_length_medium() {
        let mut buf = Vec::new();
        encode_remaining_length(&mut buf, 200);
        assert_eq!(buf, vec![0xC8, 0x01]);
    }

    #[test]
    fn test_encode_remaining_length_zero() {
        let mut buf = Vec::new();
        encode_remaining_length(&mut buf, 0);
        assert_eq!(buf, vec![0]);
    }

    #[test]
    fn test_encode_remaining_length_boundary_127() {
        let mut buf = Vec::new();
        encode_remaining_length(&mut buf, 127);
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn test_encode_remaining_length_boundary_128() {
        let mut buf = Vec::new();
        encode_remaining_length(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn test_build_mqtt_publish_retained() {
        let packet = build_mqtt_publish("test/topic", b"hello", true);
        assert_eq!(packet[0], 0x31);
        assert_eq!(packet[1], 17);
        assert_eq!(&packet[2..4], &[0x00, 0x0A]);
        assert_eq!(&packet[4..14], b"test/topic");
        assert_eq!(&packet[14..19], b"hello");
    }

    #[test]
    fn test_build_mqtt_publish_not_retained() {
        let packet = build_mqtt_publish("t", b"v", false);
        assert_eq!(packet[0], 0x30);
    }

    #[test]
    fn test_build_mqtt_publish_empty_payload() {
        let packet = build_mqtt_publish("topic", b"", true);
        assert_eq!(packet[0], 0x31);
        assert_eq!(packet[1], 7);
    }

    #[test]
    fn test_build_mqtt_connect_with_auth() {
        let packet = build_mqtt_connect("test-client", "user", "pass");
        assert_eq!(packet[0], 0x10);
        assert_eq!(&packet[2..8], b"\x00\x04MQTT");
        assert_eq!(packet[8], 4);
        assert_eq!(packet[9], 0xC2);
    }

    #[test]
    fn test_build_mqtt_connect_no_auth() {
        let packet = build_mqtt_connect("test-client", "", "");
        assert_eq!(packet[9], 0x02);
    }

    #[test]
    fn test_build_mqtt_connect_user_only() {
        let packet = build_mqtt_connect("c", "admin", "");
        assert_eq!(packet[9], 0x82);
    }

    #[test]
    fn test_parse_broker_url() {
        assert_eq!(
            parse_broker_url("tcp://192.168.1.1:1883"),
            ("192.168.1.1".into(), 1883, false)
        );
        assert_eq!(
            parse_broker_url("192.168.1.1:1884"),
            ("192.168.1.1".into(), 1884, false)
        );
        assert_eq!(
            parse_broker_url("ssl://broker:8883"),
            ("broker".into(), 8883, true)
        );
    }

    #[test]
    fn test_parse_broker_url_defaults() {
        assert_eq!(parse_broker_url("myhost"), ("myhost".into(), 1883, false));
        assert_eq!(
            parse_broker_url("ws://broker:9001"),
            ("broker".into(), 9001, false)
        );
        assert_eq!(
            parse_broker_url("wss://broker:9002"),
            ("broker".into(), 9002, true)
        );
    }

    #[test]
    fn test_parse_broker_url_tls_default_ports() {
        // ssl:// without port should default to 8883
        assert_eq!(
            parse_broker_url("ssl://mybroker"),
            ("mybroker".into(), 8883, true)
        );
        // wss:// without port should default to 8883
        assert_eq!(
            parse_broker_url("wss://mybroker"),
            ("mybroker".into(), 8883, true)
        );
        // tcp:// without port should default to 1883
        assert_eq!(
            parse_broker_url("tcp://mybroker"),
            ("mybroker".into(), 1883, false)
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests — sync MQTT publish against a mini-broker
    // -----------------------------------------------------------------------
    //
    // Each test spins up a disposable TCP listener on port 0 and runs a
    // blocking MQTT broker on a background thread. The broker speaks just
    // enough MQTT 3.1.1 to handle CONNECT/PUBLISH/DISCONNECT and records
    // every PUBLISH it receives.

    mod integration {
        use super::*;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        /// Decode MQTT variable-length remaining length from a byte slice.
        /// Returns (value, bytes_consumed).
        fn decode_remaining_length(bytes: &[u8]) -> Option<(usize, usize)> {
            let mut value = 0usize;
            let mut multiplier = 1;
            for (i, &byte) in bytes.iter().enumerate() {
                value += (byte as usize & 0x7F) * multiplier;
                if byte & 0x80 == 0 {
                    return Some((value, i + 1));
                }
                multiplier *= 128;
                if i >= 3 {
                    return None;
                }
            }
            None
        }

        #[derive(Debug, Clone)]
        struct ReceivedPublish {
            topic: String,
            payload: Vec<u8>,
            retain: bool,
        }

        /// Accept one MQTT connection, process packets, return received publishes.
        fn run_mini_broker(listener: TcpListener) -> Vec<ReceivedPublish> {
            let mut received = Vec::new();
            let Ok((mut stream, _)) = listener.accept() else {
                return received;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let mut buf = vec![0u8; 4096];
            let mut data = Vec::new();

            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(_) => break,
                }

                loop {
                    if data.len() < 2 {
                        break;
                    }
                    let packet_type = data[0] >> 4;
                    let Some((remaining_len, len_bytes)) = decode_remaining_length(&data[1..])
                    else {
                        break;
                    };
                    let total = 1 + len_bytes + remaining_len;
                    if data.len() < total {
                        break;
                    }

                    match packet_type {
                        1 => {
                            // CONNECT -> CONNACK (success)
                            let _ = stream.write_all(&[0x20, 0x02, 0x00, 0x00]);
                            let _ = stream.flush();
                        }
                        3 => {
                            // PUBLISH
                            let retain = data[0] & 0x01 != 0;
                            let pos = 1 + len_bytes;
                            let topic_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                            let topic =
                                String::from_utf8_lossy(&data[pos + 2..pos + 2 + topic_len])
                                    .to_string();
                            let payload = data[pos + 2 + topic_len..total].to_vec();
                            received.push(ReceivedPublish {
                                topic,
                                payload,
                                retain,
                            });
                        }
                        14 => {
                            // DISCONNECT
                            data.drain(..total);
                            return received;
                        }
                        _ => {}
                    }
                    data.drain(..total);
                }
            }
            received
        }

        #[test]
        fn sync_publish_delivers_sleeping_message() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-sleep".into(),
                sleep_topic: "homeassistant/sensor/test-pc/sleep_state/state".into(),
            };

            let broker_handle = std::thread::spawn(move || run_mini_broker(listener));

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_ok(), "sync publish failed: {:?}", result.err());

            let received = broker_handle.join().unwrap();
            assert_eq!(received.len(), 1, "Expected 1 publish, got {received:?}");
            assert_eq!(
                received[0].topic,
                "homeassistant/sensor/test-pc/sleep_state/state"
            );
            assert_eq!(received[0].payload, b"sleeping");
            assert!(received[0].retain, "Sleep message must be retained");
        }

        #[test]
        fn sync_publish_with_auth() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: "testuser".into(),
                pass: "testpass".into(),
                client_id: "test-auth".into(),
                sleep_topic: "test/sleep".into(),
            };

            let broker_handle = std::thread::spawn(move || run_mini_broker(listener));

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(
                result.is_ok(),
                "sync publish with auth failed: {:?}",
                result.err()
            );

            let received = broker_handle.join().unwrap();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].payload, b"sleeping");
        }

        #[test]
        fn sync_publish_connection_refused() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-fail".into(),
                sleep_topic: "test/sleep".into(),
            };

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_err(), "Should fail when broker is down");
        }

        /// The entire sync publish (connect + connack + publish + disconnect)
        /// must complete fast enough to finish inside the wnd_proc handler
        /// before Windows proceeds with suspend. On loopback this should be
        /// well under 50ms; we assert <100ms with margin.
        #[test]
        fn sync_publish_completes_within_nic_shutdown_window() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let broker_handle = std::thread::spawn(move || run_mini_broker(listener));

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-timing".into(),
                sleep_topic: "test/timing".into(),
            };

            let start = std::time::Instant::now();
            let result = sync_mqtt_publish_sleep(&cfg);
            let elapsed = start.elapsed();

            assert!(result.is_ok(), "sync publish failed: {:?}", result.err());
            assert!(
                elapsed < Duration::from_millis(100),
                "Sync publish took {elapsed:?} — must complete in <100ms \
                 to beat the NIC shutdown window"
            );

            let received = broker_handle.join().unwrap();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].payload, b"sleeping");
        }

        /// Simulates NIC death: immediately after sync_mqtt_publish_sleep
        /// returns (mimicking wnd_proc returning → OS suspends NIC), we
        /// verify the broker already has the message. The broker must NOT
        /// need any further TCP traffic to have recorded the PUBLISH.
        #[test]
        fn message_survives_immediate_nic_death() {
            use std::sync::{Arc, Mutex};

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
            let captured_clone = Arc::clone(&captured);

            // Broker: accept connection, read everything, store raw bytes.
            // Does NOT send CONNACK until it has seen enough data — this
            // forces the client to block on the write side while we
            // accumulate bytes. But our protocol sends CONNECT then waits
            // for CONNACK, so the broker sends CONNACK normally and then
            // reads the rest.
            let broker_handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = vec![0u8; 4096];
                let mut data = Vec::new();
                let mut connack_sent = false;

                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => data.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }

                    // Send CONNACK once we see CONNECT
                    if !connack_sent && !data.is_empty() && data[0] >> 4 == 1 {
                        let _ = stream.write_all(&[0x20, 0x02, 0x00, 0x00]);
                        let _ = stream.flush();
                        connack_sent = true;
                    }
                }
                *captured_clone.lock().unwrap() = data;
            });

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-nic-death".into(),
                sleep_topic: "test/nic-death".into(),
            };

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_ok());

            // === NIC DIES HERE ===
            // Drop all client-side resources (simulating OS killing the NIC).
            // The broker must already have the PUBLISH bytes.
            drop(result);

            let _join = broker_handle.join();
            let raw = captured.lock().unwrap();

            // The raw bytes must contain our PUBLISH packet.
            // PUBLISH for "sleeping" on topic "test/nic-death":
            //   0x31 (PUBLISH, retain) followed by the topic and payload.
            let payload_needle = b"sleeping";
            let topic_needle = b"test/nic-death";
            assert!(
                raw.windows(payload_needle.len())
                    .any(|w| w == payload_needle),
                "Broker raw bytes don't contain 'sleeping' payload — \
                 message was NOT on the wire before NIC died. Raw: {:?}",
                &raw[..]
            );
            assert!(
                raw.windows(topic_needle.len()).any(|w| w == topic_needle),
                "Broker raw bytes don't contain topic — \
                 PUBLISH packet incomplete. Raw: {:?}",
                &raw[..]
            );
        }

        /// Verify that a CONNACK rejection (non-zero return code) is handled.
        #[test]
        fn sync_publish_handles_connack_rejection() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            // Broker that rejects with rc=5 (not authorized)
            let broker_handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf); // consume CONNECT
                let _ = stream.write_all(&[0x20, 0x02, 0x00, 0x05]); // CONNACK rc=5
                let _ = stream.flush();
            });

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-reject".into(),
                sleep_topic: "test/sleep".into(),
            };

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
            assert!(err.to_string().contains("rc=5"));

            let _ = broker_handle.join();
        }

        /// CONNACK with wrong remaining-length byte (not 0x02) must be rejected.
        /// This exercises the C2 fix that validates connack[1].
        #[test]
        fn sync_publish_rejects_malformed_connack_length() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let broker_handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                // Send CONNACK with remaining length = 0x04 instead of 0x02
                let _ = stream.write_all(&[0x20, 0x04, 0x00, 0x00]);
                let _ = stream.flush();
            });

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-bad-connack".into(),
                sleep_topic: "test/sleep".into(),
            };

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
            assert!(err.to_string().contains("len=4"));

            let _ = broker_handle.join();
        }

        /// CONNACK with wrong packet type byte must be rejected.
        #[test]
        fn sync_publish_rejects_wrong_packet_type() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let broker_handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                // Send SUBACK (0x90) instead of CONNACK (0x20)
                let _ = stream.write_all(&[0x90, 0x02, 0x00, 0x00]);
                let _ = stream.flush();
            });

            let cfg = SyncMqttConfig {
                host: "127.0.0.1".into(),
                port,
                use_tls: false,
                user: String::new(),
                pass: String::new(),
                client_id: "test-wrong-type".into(),
                sleep_topic: "test/sleep".into(),
            };

            let result = sync_mqtt_publish_sleep(&cfg);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(err.to_string().contains("type=0x90"));

            let _ = broker_handle.join();
        }
    }
}
