# PC Agent (Rust)

A lightweight Windows agent that bridges your gaming PC with Home Assistant via MQTT.

## Features

- **Game Detection** - Monitors running processes and reports current game
- **Idle Tracking** - Reports last user input time
- **Power Events** - Detects sleep/wake and publishes state
- **Display Wake** - Fixes WoL display issues automatically
- **Remote Commands** - Launch games, screensaver, shutdown, Discord controls
- **Hot-Reload** - Updates game mappings without restart

## Requirements

- Windows 10/11
- [Rust](https://rustup.rs/) (for building)
- MQTT broker (e.g., Mosquitto on Home Assistant)

## Quick Start

**On your Windows PC:**

```powershell
# 1. Install Rust (if not installed)
# Download from https://rustup.rs/

# 2. Clone and build
git clone https://github.com/dank0i/pc-agent.git
cd pc-agent
git checkout rust
cargo build --release

# 3. Setup
copy target\release\pc-agent.exe .
copy userConfig.example.json userConfig.json
notepad userConfig.json   # Edit with your settings

# 4. Run
.\pc-agent.exe
```

Or just run `build.bat` after cloning.

## Configuration

Edit `userConfig.json`:

```json
{
  "device_name": "dank0i-pc",
  "mqtt": {
    "broker": "tcp://homeassistant.local:1883",
    "user": "mqtt_user",
    "pass": "mqtt_pass"
  },
  "intervals": {
    "game_sensor": 5,
    "last_active": 10
  },
  "games": {
    "bf6": "battlefield_6",
    "FortniteClient-Win64-Shipping": "fortnite",
    "MarvelRivals_Shipping": "marvel_rivals"
  }
}
```

## Commands

Send commands via MQTT (payload to `SteamLaunch`):

| Payload | Description |
|---------|-------------|
| `steam:1234` | Launch Steam game by App ID |
| `epic:Fortnite` | Launch Epic game |
| `exe:C:\path\to.exe` | Run executable |
| `lnk:C:\path\to.lnk` | Run shortcut |
| `close:processname` | Close process gracefully |

Other buttons: `Screensaver`, `Wake`, `Shutdown`, `sleep`

## Home Assistant

Auto-discovers via MQTT. You'll get sensors like:
- `sensor.dank0i_pc_runninggames`
- `sensor.dank0i_pc_sleep_state`
- `sensor.dank0i_pc_lastactive`

## Install as Windows Service

```powershell
sc create PCAgentService binPath= "C:\Scripts\pc-agent\pc-agent.exe"
sc start PCAgentService
```

## Why Rust?

- ~3MB binary (vs ~10MB Go)
- ~5MB RAM (vs ~15MB Go)  
- No garbage collector
- Compile-time safety

## License

MIT
