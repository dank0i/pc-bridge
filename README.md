# PC Bridge

A lightweight cross-platform agent that bridges your PC with Home Assistant via MQTT.

## Features

- **Game Detection** - Monitors running processes and reports current game
- **Idle Tracking** - Reports last user input time
- **Power Events** - Detects sleep/wake and publishes state
- **Display Wake** - Wakes display after WoL, dismisses screensaver
- **Remote Commands** - Launch games, activate screensaver, shutdown, sleep
- **Hot-Reload** - Updates game mappings without restart

## Supported Platforms

| Platform | Status |
|----------|--------|
| Windows 10/11 | Full support |
| Linux (X11) | Full support |
| Linux (Wayland) | Partial (idle tracking requires qdbus) |
| macOS | Not supported |

## Quick Start

### Download

Download the latest release from [GitHub Releases](https://github.com/dank0i/pc-bridge/releases).

### Build from Source

```bash
# Install Rust (https://rustup.rs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/dank0i/pc-bridge.git
cd pc-bridge

# Windows (native)
cargo build --release
# Output: target/release/pc-bridge.exe

# Linux (native)
cargo build --release
# Output: target/release/pc-bridge

# Cross-compile Windows from Linux/macOS
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

### Setup

1. Copy the binary to your desired location
2. Run once - it creates a default `userConfig.json`
3. Edit the config with your MQTT settings
4. Run again

## Configuration

Edit `userConfig.json` next to the executable:

```json
{
  "device_name": "my-pc",
  "mqtt": {
    "broker": "tcp://homeassistant.local:1883",
    "user": "mqtt_user",
    "pass": "mqtt_pass"
  },
  "intervals": {
    "game_sensor": 5,
    "last_active": 10,
    "availability": 30
  },
  "games": {
    "bf6": "battlefield_6",
    "FortniteClient-Win64-Shipping": "fortnite",
    "MarvelRivals_Shipping": "marvel_rivals"
  }
}
```

### Games Configuration

The `games` object maps process names to game IDs:
- **Key**: Part of the process name to match (case-insensitive)
- **Value**: The game ID reported to Home Assistant

## MQTT Commands

Send commands via MQTT button topics:

| Button | Description |
|--------|-------------|
| `Screensaver` | Activate screensaver |
| `Wake` | Wake display, dismiss screensaver |
| `Shutdown` | Power off the PC |
| `sleep` | Put PC to sleep |

### Launch Payloads

The `Launch` button accepts special payloads:

| Payload | Description |
|---------|-------------|
| `steam:1234` | Launch Steam game by App ID |
| `epic:GameName` | Launch Epic game |
| `exe:C:\path\to.exe` | Run executable directly |
| `lnk:C:\path\to.lnk` | Run shortcut file |
| `close:processname` | Close process gracefully |

## Home Assistant Integration

PC Bridge auto-discovers via MQTT. After connecting, you'll get:

**Sensors:**
- `sensor.<device>_runninggames` - Current game (or "none")
- `sensor.<device>_sleep_state` - "awake" or "sleeping"
- `sensor.<device>_lastactive` - ISO timestamp of last input

**Buttons:**
- `button.<device>_screensaver`
- `button.<device>_wake`
- `button.<device>_shutdown`
- `button.<device>_sleep`
- `button.<device>_launch`

Where `<device>` is your configured `device_name` with dashes replaced by underscores.

## Linux Requirements

For full functionality on Linux, install these optional dependencies:

```bash
# Debian/Ubuntu
sudo apt install xdotool xprintidle xdg-utils

# Fedora
sudo dnf install xdotool xprintidle xdg-utils

# Arch
sudo pacman -S xdotool xprintidle xdg-utils
```

| Package | Purpose |
|---------|---------|
| `xdotool` | Screensaver/display wake |
| `xprintidle` | Idle time tracking (X11) |
| `xdg-utils` | Screensaver activation |

## Run as Service

### Windows

```powershell
# Create service
sc create PCBridge binPath= "C:\path\to\pc-bridge.exe"
sc config PCBridge start= auto
sc start PCBridge
```

### Linux (systemd)

Create `/etc/systemd/system/pc-bridge.service`:

```ini
[Unit]
Description=PC Bridge - Home Assistant Integration
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/pc-bridge
WorkingDirectory=/usr/local/bin
Restart=always
RestartSec=10
User=your-username

[Install]
WantedBy=multi-user.target
```

Then:

```bash
sudo systemctl daemon-reload
sudo systemctl enable pc-bridge
sudo systemctl start pc-bridge
```

## Performance

| Metric | Value |
|--------|-------|
| Binary size | ~3 MB |
| Memory usage | ~5 MB |
| CPU usage | < 1% |

## License

MIT
