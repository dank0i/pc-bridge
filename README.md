# PC Agent

A lightweight Windows agent that bridges a gaming PC with Home Assistant via MQTT.

## Features

- **Game Detection**: Monitors running processes and reports the current game to Home Assistant
- **Power Events**: Detects sleep/wake events and publishes state to MQTT
- **Display Wake**: Automatically wakes displays after Wake-on-LAN (fixes BIOS/motherboard issues where WoL doesn't turn on monitors)
- **Remote Commands**: Receives commands via MQTT (launch Steam, screensaver, shutdown, sleep, Discord controls)
- **Hot-Reload Config**: Updates game mappings from `userConfig.json` without restart

## Requirements

- Windows 10/11
- Go 1.21+ (for building)
- MQTT broker (e.g., Mosquitto on Home Assistant)

## Quick Start

1. Clone the repo
2. Install go-winres: `go install github.com/tc-hib/go-winres@latest`
3. Run `build.bat`
4. Run `pc-agent.exe` - it will create `userConfig.json` from the example
5. Edit `userConfig.json` with your settings
6. Run `pc-agent.exe` again

## Configuration

All settings are in `userConfig.json` (created on first run):

```json
{
  "device_name": "my-pc",
  "mqtt": {
    "broker": "tcp://homeassistant.local:1883",
    "user": "",
    "pass": "",
    "client_id": "pc-agent-my-pc"
  },
  "intervals": {
    "game_sensor": 5,
    "last_active": 10,
    "availability": 30
  },
  "games": {
    "FortniteClient": "fortnite",
    "r5apex": "apex_legends"
  }
}
```

- **device_name**: Your PC's name in Home Assistant (used in MQTT topics)
- **mqtt**: Broker connection settings
- **intervals**: Sensor update rates in seconds
- **games**: Map of process names to game IDs (hot-reloaded on save)

## MQTT Topics

### Sensors (Published)
```
homeassistant/sensor/{device_name}/runninggames/state   # Current game name
homeassistant/sensor/{device_name}/lastactive/state     # Last input timestamp
homeassistant/sensor/{device_name}/sleep_state/state    # "sleeping" or "awake"
homeassistant/sensor/{device_name}/availability         # "online" or "offline" (LWT)
```

### Commands (Subscribed)
```
homeassistant/button/{device_name}/SteamLaunch/action
homeassistant/button/{device_name}/Screensaver/action
homeassistant/button/{device_name}/Wake/action
homeassistant/button/{device_name}/Shutdown/action
homeassistant/button/{device_name}/sleep/action
homeassistant/button/{device_name}/discord_join/action
homeassistant/button/{device_name}/discord_leave_channel/action
```

## Running

### Console Mode (for testing)
```cmd
pc-agent.exe
```
Press Ctrl+C to stop.

### Auto-Start
Add a shortcut to `pc-agent.exe` in your Startup folder:
`shell:startup`

## Building

```cmd
build.bat
```

This pulls latest changes from git, generates Windows resources, and builds the exe.

## License

MIT
