package mqtt

import (
	"encoding/json"
	"fmt"
	"log"
	"pc-agent/internal/config"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"
)

type Client struct {
	mqtt.Client
	onCommand func(command string, payload string)
}

// IsConnected returns true if the MQTT client is connected
func (c *Client) IsConnected() bool {
	return c.Client != nil && c.Client.IsConnected()
}

// HADiscoveryPayload for Home Assistant MQTT Discovery
type HADiscoveryPayload struct {
	Name              string            `json:"name"`
	UniqueID          string            `json:"unique_id"`
	StateTopic        string            `json:"state_topic,omitempty"`
	CommandTopic      string            `json:"command_topic,omitempty"`
	AvailabilityTopic string            `json:"availability_topic,omitempty"`
	Device            HADevice          `json:"device"`
	Icon              string            `json:"icon,omitempty"`
	DeviceClass       string            `json:"device_class,omitempty"`
	UnitOfMeasurement string            `json:"unit_of_measurement,omitempty"`
}

type HADevice struct {
	Identifiers  []string `json:"identifiers"`
	Name         string   `json:"name"`
	Model        string   `json:"model"`
	Manufacturer string   `json:"manufacturer"`
}

func NewClient(onCommand func(command string, payload string)) *Client {
	c := &Client{onCommand: onCommand}

	opts := mqtt.NewClientOptions().
		AddBroker(config.MQTTBroker).
		SetUsername(config.MQTTUser).
		SetPassword(config.MQTTPass).
		SetClientID(config.MQTTClientID).
		SetAutoReconnect(true).
		SetConnectRetry(true).
		SetConnectRetryInterval(5 * time.Second).
		SetMaxReconnectInterval(60 * time.Second).
		SetKeepAlive(30 * time.Second). // Detect dead connections faster
		SetPingTimeout(10 * time.Second).
		SetWriteTimeout(10 * time.Second). // Don't hang forever on writes
		SetCleanSession(false). // Preserve subscriptions across reconnect
		SetWill(availabilityTopic(), "offline", 1, true).
		SetOnConnectHandler(func(client mqtt.Client) {
			log.Println("MQTT connected")
			c.onConnect()
		}).
		SetConnectionLostHandler(func(client mqtt.Client, err error) {
			log.Printf("MQTT connection lost: %v (will auto-reconnect)", err)
		}).
		SetReconnectingHandler(func(client mqtt.Client, opts *mqtt.ClientOptions) {
			log.Println("MQTT reconnecting...")
		})

	c.Client = mqtt.NewClient(opts)
	return c
}

func (c *Client) Connect() error {
	if token := c.Client.Connect(); token.Wait() && token.Error() != nil {
		return token.Error()
	}
	return nil
}

func (c *Client) onConnect() {
	// Publish availability
	c.Publish(availabilityTopic(), 1, true, "online")

	// Register HA discovery
	c.registerDiscovery()

	// Subscribe to command topics
	c.subscribeCommands()
}

func (c *Client) registerDiscovery() {
	device := HADevice{
		Identifiers:  []string{config.DeviceID},
		Name:         config.DeviceName,
		Model:        "PC Agent Go",
		Manufacturer: "Custom",
	}

	// Running Games Sensor
	c.publishDiscovery("sensor", "runninggames", HADiscoveryPayload{
		Name:              "Runninggames",
		UniqueID:          config.DeviceID + "_runninggames",
		StateTopic:        sensorTopic("runninggames"),
		AvailabilityTopic: availabilityTopic(),
		Device:            device,
		Icon:              "mdi:gamepad-variant",
	})

	// Last Active Sensor
	c.publishDiscovery("sensor", "lastactive", HADiscoveryPayload{
		Name:              "Last Active",
		UniqueID:          config.DeviceID + "_lastactive",
		StateTopic:        sensorTopic("lastactive"),
		AvailabilityTopic: availabilityTopic(),
		Device:            device,
		Icon:              "mdi:clock-outline",
		DeviceClass:       "timestamp",
	})

	// Sleep State Sensor - no availability topic so it shows last value even when PC is asleep
	c.publishDiscovery("sensor", "sleep_state", HADiscoveryPayload{
		Name:       "Sleep State",
		UniqueID:   config.DeviceID + "_sleep_state",
		StateTopic: sensorTopic("sleep_state"),
		Device:     device,
		Icon:       "mdi:power-sleep",
	})

	// Command buttons
	commands := []struct {
		name string
		icon string
	}{
		{"SteamLaunch", "mdi:steam"},
		{"Screensaver", "mdi:monitor"},
		{"Wake", "mdi:monitor-eye"},
		{"Shutdown", "mdi:power"},
		{"sleep", "mdi:power-sleep"},
		{"discord_join", "mdi:discord"},
		{"discord_leave_channel", "mdi:phone-hangup"},
	}

	for _, cmd := range commands {
		c.publishDiscovery("button", cmd.name, HADiscoveryPayload{
			Name:              cmd.name,
			UniqueID:          config.DeviceID + "_" + cmd.name,
			CommandTopic:      commandTopic(cmd.name),
			AvailabilityTopic: availabilityTopic(),
			Device:            device,
			Icon:              cmd.icon,
		})
	}
}

func (c *Client) subscribeCommands() {
	// Subscribe to all command topics
	commands := []string{"SteamLaunch", "Screensaver", "Wake", "Shutdown", "sleep", "discord_join", "discord_leave_channel"}

	for _, cmd := range commands {
		topic := commandTopic(cmd)
		cmdName := cmd // capture for closure
		token := c.Subscribe(topic, 1, func(_ mqtt.Client, msg mqtt.Message) {
			payload := string(msg.Payload())
			log.Printf("Command received: %s = %s", cmdName, payload)
			if c.onCommand != nil {
				c.onCommand(cmdName, payload)
			}
		})
		if token.Wait() && token.Error() != nil {
			log.Printf("Failed to subscribe to %s: %v", topic, token.Error())
		}
	}

	// Subscribe to notifications
	notifyTopic := fmt.Sprintf("hass.agent/notifications/%s", config.DeviceName)
	token := c.Subscribe(notifyTopic, 1, func(_ mqtt.Client, msg mqtt.Message) {
		log.Printf("Notification received: %s", string(msg.Payload()))
		if c.onCommand != nil {
			c.onCommand("notification", string(msg.Payload()))
		}
	})
	if token.Wait() && token.Error() != nil {
		log.Printf("Failed to subscribe to %s: %v", notifyTopic, token.Error())
	}
}

func (c *Client) publishDiscovery(component, name string, payload HADiscoveryPayload) {
	topic := fmt.Sprintf("%s/%s/%s/%s/config", config.DiscoveryPrefix, component, config.DeviceName, name)
	data, _ := json.Marshal(payload)
	c.Publish(topic, 1, true, data)
}

func (c *Client) PublishSensor(name, value string) {
	token := c.Publish(sensorTopic(name), 1, false, value)
	// Don't wait for non-retained sensor data - fire and forget
	_ = token
}

func (c *Client) PublishSensorRetained(name, value string) {
	token := c.Publish(sensorTopic(name), 1, true, value)
	// Wait for retained messages to ensure delivery (e.g., sleep_state)
	if token.WaitTimeout(5 * time.Second) {
		if token.Error() != nil {
			log.Printf("Failed to publish %s: %v", name, token.Error())
		}
	} else {
		log.Printf("Publish %s timed out", name)
	}
}

// Pre-computed topic strings for efficiency
var (
	cachedAvailabilityTopic string
	cachedSensorTopics      = make(map[string]string)
	cachedCommandTopics     = make(map[string]string)
)

func init() {
	// Pre-compute topics at startup
	cachedAvailabilityTopic = fmt.Sprintf("%s/sensor/%s/availability", config.DiscoveryPrefix, config.DeviceName)

	// Pre-compute sensor topics
	for _, name := range []string{"runninggames", "lastactive", "sleep_state"} {
		cachedSensorTopics[name] = fmt.Sprintf("%s/sensor/%s/%s/state", config.DiscoveryPrefix, config.DeviceName, name)
	}

	// Pre-compute command topics
	for _, name := range []string{"SteamLaunch", "Screensaver", "Wake", "Shutdown", "sleep", "discord_join", "discord_leave_channel"} {
		cachedCommandTopics[name] = fmt.Sprintf("%s/button/%s/%s/action", config.DiscoveryPrefix, config.DeviceName, name)
	}
}

// Topic helpers - use cached values
func availabilityTopic() string {
	return cachedAvailabilityTopic
}

func sensorTopic(name string) string {
	if topic, ok := cachedSensorTopics[name]; ok {
		return topic
	}
	// Fallback for unknown sensors
	return fmt.Sprintf("%s/sensor/%s/%s/state", config.DiscoveryPrefix, config.DeviceName, name)
}

func commandTopic(name string) string {
	if topic, ok := cachedCommandTopics[name]; ok {
		return topic
	}
	// Fallback for unknown commands
	return fmt.Sprintf("%s/button/%s/%s/action", config.DiscoveryPrefix, config.DeviceName, name)
}
