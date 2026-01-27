# PC Agent

A lightweight Windows service that bridges a gaming PC with Home Assistant via MQTT.

## Features

- **Game Detection**: Monitors running processes and reports the current game to Home Assistant
- **Power Events**: Detects sleep/wake events and publishes state to MQTT
- **Display Wake**: Automatically wakes displays after Wake-on-LAN (fixes BIOS/motherboard issues where WoL doesn't turn on monitors)
- **Remote Commands**: Receives commands via MQTT (launch Steam, screensaver, shutdown, sleep, Discord controls)
- **Hot-Reload Game Config**: Updates game mappings from `games.json` without restart

## Requirements

- Windows 10/11
- Go 1.21+
- MQTT broker (e.g., Mosquitto on Home Assistant)

## Installation

```cmd
go install github.com/tc-hib/go-winres@latest
build.bat
install.bat
```

## Configuration

Edit `config/config.go` for:
- MQTT broker address
- Device name
- Sensor polling intervals

Edit `games.json` for game process-to-name mappings (hot-reloaded on change).

## MQTT Topics

### Sensors (Published)
```
homeassistant/sensor/dank0i-pc/runninggames/state   # Current game name
homeassistant/sensor/dank0i-pc/lastactive/state     # Last input timestamp
homeassistant/sensor/dank0i-pc/sleep_state/state    # "sleeping" or "awake"
homeassistant/sensor/dank0i-pc/availability         # "online" or "offline" (LWT)
```

### Commands (Subscribed)
```
homeassistant/button/dank0i-pc/SteamLaunch/action
homeassistant/button/dank0i-pc/Screensaver/action
homeassistant/button/dank0i-pc/Wake/action
homeassistant/button/dank0i-pc/Shutdown/action
homeassistant/button/dank0i-pc/sleep/action
homeassistant/button/dank0i-pc/discord_join/action
homeassistant/button/dank0i-pc/discord_leave_channel/action
```

## Service Management

```cmd
# Install service
sc create PCAgentService binPath= "C:\Scripts\pc-agent\pc-agent.exe"
sc config PCAgentService start= auto
sc start PCAgentService

# Uninstall
sc stop PCAgentService
sc delete PCAgentService
```

## Development

Run in console mode for testing:
```cmd
pc-agent.exe
```

Logs go to Windows Event Log when running as a service.

## License

Private - personal use only.
