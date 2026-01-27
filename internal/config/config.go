package config

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"
)

// UserConfig represents the user configuration file structure
type UserConfig struct {
	DeviceName string `json:"device_name"`
	MQTT       struct {
		Broker   string `json:"broker"`
		User     string `json:"user"`
		Pass     string `json:"pass"`
		ClientID string `json:"client_id"`
	} `json:"mqtt"`
	Intervals struct {
		GameSensor   int `json:"game_sensor"`
		LastActive   int `json:"last_active"`
		Availability int `json:"availability"`
	} `json:"intervals"`
	Games map[string]string `json:"games"`
}

// Loaded configuration (populated by LoadUserConfig)
var (
	DeviceName   string
	DeviceID     string
	MQTTBroker   string
	MQTTUser     string
	MQTTPass     string
	MQTTClientID string

	GameSensorInterval   int
	LastActiveInterval   int
	AvailabilityInterval int

	configPath string // Path to userConfig.json for file watcher
)

// HA MQTT Discovery prefix (constant, not configurable)
const DiscoveryPrefix = "homeassistant"

// Commands with fixed commands (empty = dynamic from MQTT payload)
var Commands = map[string]string{
	"SteamLaunch":           "", // Dynamic - payload is the command
	"Screensaver":           `%windir%\System32\scrnsave.scr /s`,
	"Wake":                  `Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{F15}')`,
	"Shutdown":              "shutdown -s -t 0",
	"sleep":                 "Rundll32.exe powrprof.dll,SetSuspendState 0,1,0",
	"discord_join":          "", // Dynamic - payload is the command
	"discord_leave_channel": "", // Handled specially with keypress
}

// LoadUserConfig loads configuration from userConfig.json
func LoadUserConfig() error {
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("couldn't get executable path: %w", err)
	}
	exeDir := filepath.Dir(exe)

	configPath = filepath.Join(exeDir, "userConfig.json")

	// Check if userConfig.json exists
	if _, err := os.Stat(configPath); os.IsNotExist(err) {
		return fmt.Errorf("userConfig.json not found - run build.bat first to create it")
	}

	return loadConfigFromFile()
}

// loadConfigFromFile reads and parses userConfig.json
func loadConfigFromFile() error {
	data, err := os.ReadFile(configPath)
	if err != nil {
		return fmt.Errorf("couldn't read userConfig.json: %w", err)
	}

	var cfg UserConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return fmt.Errorf("couldn't parse userConfig.json: %w", err)
	}

	// Validate required fields
	if cfg.DeviceName == "" || cfg.DeviceName == "my-pc" {
		return fmt.Errorf("please set device_name in userConfig.json (currently: %q)", cfg.DeviceName)
	}

	// Populate global config vars
	DeviceName = cfg.DeviceName
	DeviceID = strings.ReplaceAll(cfg.DeviceName, "-", "_")

	MQTTBroker = cfg.MQTT.Broker
	if MQTTBroker == "" {
		MQTTBroker = "tcp://homeassistant.local:1883"
	}
	MQTTUser = cfg.MQTT.User
	MQTTPass = cfg.MQTT.Pass
	MQTTClientID = cfg.MQTT.ClientID
	if MQTTClientID == "" {
		MQTTClientID = "pc-agent-" + DeviceName
	}

	// Intervals with defaults
	GameSensorInterval = cfg.Intervals.GameSensor
	if GameSensorInterval == 0 {
		GameSensorInterval = 5
	}
	LastActiveInterval = cfg.Intervals.LastActive
	if LastActiveInterval == 0 {
		LastActiveInterval = 10
	}
	AvailabilityInterval = cfg.Intervals.Availability
	if AvailabilityInterval == 0 {
		AvailabilityInterval = 30
	}

	// Load games into the game map
	if len(cfg.Games) > 0 {
		setInitialGameMap(cfg.Games)
	}

	log.Printf("Loaded config for device: %s", DeviceName)
	if MQTTUser == "" {
		log.Println("Warning: MQTT user/pass not set - connecting without authentication")
	}

	return nil
}

// GetConfigPath returns the path to userConfig.json
func GetConfigPath() string {
	return configPath
}
