package config

import (
	"log"
	"os"
)

// MQTT Configuration - credentials from environment variables
var (
	MQTTBroker   = getEnvOrDefault("MQTT_BROKER", "tcp://homeassistant.local:1883")
	MQTTUser     = getEnvOrDefault("MQTT_USER", "")
	MQTTPass     = getEnvOrDefault("MQTT_PASS", "")
	MQTTClientID = getEnvOrDefault("MQTT_CLIENT_ID", "pc-agent-dank0i-pc-2026")
)

// Device info for HA discovery
const (
	DeviceName = "dank0i-pc"
	DeviceID   = "dank0i_pc"

	// HA MQTT Discovery prefix
	DiscoveryPrefix = "homeassistant"
)

// Sensor update intervals (seconds)
const (
	GameSensorInterval     = 5
	LastActiveInterval     = 10
	AvailabilityInterval   = 30
)

// Note: Game process mappings moved to games.go (loaded from games.json)

// Commands with fixed commands (empty = dynamic from MQTT payload)
var Commands = map[string]string{
	"SteamLaunch":          "",  // Dynamic - payload is the command
	"Screensaver":          `%windir%\System32\scrnsave.scr /s`,
	"Wake":                 `Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{F15}')`,
	"Shutdown":             "shutdown -s -t 0",
	"sleep":                "Rundll32.exe powrprof.dll,SetSuspendState 0,1,0",
	"discord_join":         "",  // Dynamic - payload is the command
	"discord_leave_channel": "",  // Handled specially with keypress
}

func getEnvOrDefault(key, defaultVal string) string {
	if val := os.Getenv(key); val != "" {
		return val
	}
	return defaultVal
}

// ValidateConfig checks that required configuration is present
func ValidateConfig() {
	if MQTTUser == "" || MQTTPass == "" {
		log.Println("WARNING: MQTT_USER and/or MQTT_PASS environment variables not set")
		log.Println("Set environment variables: MQTT_BROKER, MQTT_USER, MQTT_PASS, MQTT_CLIENT_ID")
	}
}
