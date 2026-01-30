# PC Bridge

A lightweight cross-platform agent that bridges your PC with Home Assistant via MQTT.

## Features

- **Game Detection** - Monitors running processes and reports current game
- **Idle Tracking** - Reports last user input time
- **Power Events** - Detects sleep/wake and publishes state
- **Display Wake** - Wakes display after WoL, dismisses screensaver
- **Remote Commands** - Launch games, activate screensaver, shutdown, sleep
- **Notifications** - Native Windows toast notifications from Home Assistant
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
  },
  "show_tray_icon": true,
  "custom_sensors_enabled": false,
  "custom_commands_enabled": false,
  "custom_command_privileges_allowed": false,
  "custom_sensors": [],
  "custom_commands": []
}
```

> **Note:** Missing fields are automatically added when upgrading from older versions.

### Tray Icon

A system tray icon is shown by default (Windows only). Right-click for:
- **Open Config** - Opens `userConfig.json` in your default editor
- **Exit** - Gracefully shuts down PC Bridge

Set `"show_tray_icon": false` to disable.

### Games Configuration

The `games` object maps process names to game IDs:
- **Key**: Part of the process name to match (case-insensitive)
- **Value**: The game ID reported to Home Assistant

## Custom Sensors & Commands

You can define custom sensors and commands for PC-specific monitoring and control.

### Security Model

Custom features are **disabled by default** and require explicit opt-in:

| Setting | Purpose |
|---------|---------|
| `custom_sensors_enabled` | Enable custom sensor polling |
| `custom_commands_enabled` | Enable custom command execution |
| `custom_command_privileges_allowed` | Allow commands marked `admin: true` |

### Custom Sensors

Monitor anything - GPU temperature, service status, disk space:

```json
{
  "custom_sensors_enabled": true,
  "custom_sensors": [
    {
      "name": "gpu_temp",
      "type": "powershell",
      "script": "(Get-CimInstance -Namespace root/cimv2 -ClassName Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine | Where-Object Name -like '*engtype_3D*' | Select-Object -First 1).UtilizationPercentage",
      "unit": "°C",
      "icon": "mdi:thermometer",
      "interval_seconds": 30
    },
    {
      "name": "disk_free_c",
      "type": "powershell",
      "script": "[math]::Round((Get-PSDrive C).Free / 1GB, 1)",
      "unit": "GB",
      "icon": "mdi:harddisk",
      "interval_seconds": 300
    },
    {
      "name": "is_vpn_connected",
      "type": "process_exists",
      "process_name": "openvpn"
    },
    {
      "name": "hostname",
      "type": "registry",
      "registry_path": "HKLM\\SYSTEM\\CurrentControlSet\\Control\\ComputerName\\ComputerName",
      "registry_value": "ComputerName"
    }
  ]
}
```

**Sensor Types:**

| Type | Description | Required Fields |
|------|-------------|-----------------|
| `powershell` | Run PowerShell, use stdout as value | `script` |
| `process_exists` | Returns "true"/"false" | `process_name` |
| `file_contents` | Read file contents | `file_path` |
| `registry` | Read registry value (Windows) | `registry_path`, `registry_value` |

### Custom Commands

Execute custom actions from Home Assistant:

```json
{
  "custom_commands_enabled": true,
  "custom_command_privileges_allowed": true,
  "custom_commands": [
    {
      "name": "flush_dns",
      "type": "powershell",
      "script": "ipconfig /flushdns",
      "icon": "mdi:dns",
      "admin": true
    },
    {
      "name": "clear_temp",
      "type": "powershell",
      "script": "Remove-Item $env:TEMP\\* -Recurse -Force -ErrorAction SilentlyContinue",
      "icon": "mdi:broom"
    },
    {
      "name": "open_calculator",
      "type": "executable",
      "executable": "calc.exe"
    }
  ]
}
```

**Command Types:**

| Type | Description | Required Fields |
|------|-------------|-----------------|
| `powershell` | Run PowerShell script | `script` |
| `executable` | Run an executable | `executable`, optional `args` |
| `shell` | Run via cmd.exe | `shell_command` |

> **Running script files:** To run `.ps1` files, use the `powershell` type with `"script": "& 'C:\\path\\script.ps1'"`. The `executable` type works for `.bat`/`.cmd` files directly, but `.ps1` files require PowerShell's execution policy handling.

**Security:**
- Commands with `admin: true` require `custom_command_privileges_allowed: true`
- Admin commands run via `Start-Process -Verb RunAs` (UAC prompt may appear)
- Non-admin commands run in current user context

## Notifications

PC Bridge can display Windows toast notifications sent from Home Assistant. Uses native WinRT APIs for ~10ms latency (no PowerShell overhead).

### Using the Notify Service

After PC Bridge connects, a `notify.send_message` entity is auto-discovered:

```yaml
# In HA automations, scripts, etc.
action: notify.send_message
metadata: {}
data:
  message: Motion detected at front door!
  title: Security Alert
target:
  entity_id: notify.my_pc_notification
```

### Payload Format

Send JSON to the notify topic for full control:

```json
{"title": "Alert Title", "message": "Notification body text"}
```

Or just plain text (uses "Home Assistant" as default title):

```
Your plain text message here
```

### Direct MQTT Topic

You can also publish directly to the MQTT topic:

```
Topic: hass.agent/notifications/{device_name}
Payload: {"title": "My Title", "message": "My message"}
```

### Example Automations

**Doorbell notification:**
```yaml
automation:
  - alias: "Doorbell: Notify PC"
    trigger:
      - platform: state
        entity_id: binary_sensor.doorbell
        to: "on"
    action:
      - action: notify.send_message
        data:
          title: "🔔 Doorbell"
          message: "Someone is at the door"
        target:
          entity_id: notify.my_pc_notification
```

**Washer done notification:**
```yaml
automation:
  - alias: "Laundry: Notify when done"
    trigger:
      - platform: state
        entity_id: sensor.washer_status
        to: "complete"
    action:
      - action: notify.send_message
        data:
          title: "🧺 Laundry"
          message: "Washer cycle complete!"
        target:
          entity_id: notify.my_pc_notification
```

**Game suggestion notification:**
```yaml
script:
  suggest_game:
    sequence:
      - action: notify.send_message
        data:
          title: "🎮 Game Time?"
          message: "How about playing {{ states('sensor.suggested_game') }}?"
        target:
          entity_id: notify.my_pc_notification
```

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

> **Note:** The `Launch` button requires you to define actions in Home Assistant that send the appropriate payload. Unlike custom commands (which are self-contained), Launch is a generic endpoint that executes whatever payload you send it.

## Home Assistant Integration

PC Bridge auto-discovers via MQTT. After connecting, you'll get:

**Sensors:**
- `sensor.<device>_runninggames` - Current game (or "none")
- `sensor.<device>_sleep_state` - "awake" or "sleeping"
- `sensor.<device>_lastactive` - ISO timestamp of last input
- `sensor.<device>_<custom>` - Any custom sensors you define

**Buttons:**
- `button.<device>_screensaver`
- `button.<device>_wake`
- `button.<device>_shutdown`
- `button.<device>_sleep`
- `button.<device>_launch`
- `button.<device>_<custom>` - Any custom commands you define

**Notifications:**
- `notify.<device>_notification` - Send toast notifications to your PC

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
| Memory usage | ~5 MB base |
| CPU usage | < 1% |

**Custom Sensors Impact:**
- Each PowerShell sensor: ~10-50ms execution per poll
- Process check sensor: ~1ms (native API)
- Registry read: ~0.1ms (native API)
- Memory: +~100 bytes per sensor for state tracking
- Recommended: Keep intervals ≥30s for PowerShell sensors

## License

MIT
