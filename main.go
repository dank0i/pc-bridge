package main

import (
	"log"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"pc-agent/internal/commands"
	"pc-agent/internal/config"
	"pc-agent/internal/mqtt"
	"pc-agent/internal/power"
	"pc-agent/internal/sensors"

	"golang.org/x/sys/windows/svc"
)

const serviceName = "PCAgentService"

type pcAgentService struct {
	mqttClient         *mqtt.Client
	powerListener      *power.PowerEventListener
	displayWakeHandler *power.DisplayWakeHandler
	stopChan           chan struct{}
	wg                 sync.WaitGroup
	mu                 sync.Mutex // protects mqttClient access
}

func main() {
	isService, err := svc.IsWindowsService()
	if err != nil {
		log.Fatalf("Failed to detect service mode: %v", err)
	}

	if isService {
		svc.Run(serviceName, &pcAgentService{})
	} else {
		// Run in console mode for testing
		log.Println("Running in console mode (not as service)")
		log.Println("To install as service:")
		log.Println("  sc create PCAgentService binPath= \"C:\\path\\to\\pc-agent.exe\"")
		log.Println("  sc start PCAgentService")
		log.Println("")
		
		agent := &pcAgentService{stopChan: make(chan struct{})}
		agent.run()
		
		// Wait for Ctrl+C
		sigChan := make(chan os.Signal, 1)
		signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM)
		<-sigChan
		
		log.Println("Shutting down...")
		agent.stop()
	}
}

func (s *pcAgentService) Execute(args []string, r <-chan svc.ChangeRequest, changes chan<- svc.Status) (bool, uint32) {
	changes <- svc.Status{State: svc.StartPending}

	s.stopChan = make(chan struct{})
	s.run()

	changes <- svc.Status{State: svc.Running, Accepts: svc.AcceptStop | svc.AcceptShutdown}

	for {
		c := <-r
		switch c.Cmd {
		case svc.Stop, svc.Shutdown:
			changes <- svc.Status{State: svc.StopPending}
			s.stop()
			return false, 0
		}
	}
}

func (s *pcAgentService) run() {
	log.Println("PC Agent starting...")

	// Initialize game map from games.json (with hot-reload support)
	config.InitGameMap()

	// Create MQTT client with command handler
	s.mqttClient = mqtt.NewClient(func(command, payload string) {
		// Run command execution in goroutine to not block MQTT handler
		go func() {
			if err := commands.Execute(command, payload); err != nil {
				log.Printf("Command execution error: %v", err)
			}
		}()
	})

	// Connect to MQTT
	if err := s.mqttClient.Connect(); err != nil {
		log.Printf("MQTT connection failed: %v (will retry)", err)
		// Continue anyway - auto-reconnect will handle it
	}

	// Set up display wake handler to fix WoL display issues
	// This will automatically wake the display when the system resumes from sleep
	s.displayWakeHandler = power.DefaultDisplayWakeHandler()

	// Set up power event listener with thread-safe MQTT access
	s.powerListener = power.NewPowerEventListener(
		func() { // On Sleep
			s.mu.Lock()
			defer s.mu.Unlock()
			if s.mqttClient != nil && s.mqttClient.IsConnected() {
				s.mqttClient.PublishSensorRetained("sleep_state", "sleeping")
			}
		},
		func() { // On Wake
			// Trigger display wake sequence to fix WoL display issues
			if s.displayWakeHandler != nil {
				s.displayWakeHandler.OnWake()
			}

			// Give network time to come back up after wake
			// Try multiple times with increasing delays
			go func() {
				delays := []time.Duration{2 * time.Second, 5 * time.Second, 10 * time.Second}
				for _, delay := range delays {
					// Check if we're shutting down before sleeping
					select {
					case <-s.stopChan:
						return
					case <-time.After(delay):
					}

					s.mu.Lock()
					if s.mqttClient != nil && s.mqttClient.IsConnected() {
						s.mqttClient.PublishSensorRetained("sleep_state", "awake")
						s.mu.Unlock()
						log.Println("Published awake state after wake")
						return
					}
					s.mu.Unlock()
					log.Println("MQTT not connected after wake, will retry...")
				}
				log.Println("Failed to publish awake state after all retries")
			}()
		},
	)
	s.powerListener.Start()

	// Publish initial awake state
	if s.mqttClient.IsConnected() {
		s.mqttClient.PublishSensorRetained("sleep_state", "awake")
	}

	// Start sensor polling
	s.wg.Add(1)
	go s.pollSensors()
}

func (s *pcAgentService) stop() {
	// Signal stop
	close(s.stopChan)

	// Wait for pollSensors goroutine to finish
	s.wg.Wait()

	// Stop power listener
	if s.powerListener != nil {
		s.powerListener.Stop()
	}

	// Stop game map file watcher
	config.StopGameMapWatcher()

	// Disconnect MQTT (with lock to prevent race with power events)
	s.mu.Lock()
	if s.mqttClient != nil {
		s.mqttClient.Disconnect(500)
	}
	s.mu.Unlock()

	log.Println("PC Agent stopped")
}

func (s *pcAgentService) pollSensors() {
	defer s.wg.Done()

	gameTicker := time.NewTicker(time.Duration(config.GameSensorInterval) * time.Second)
	lastActiveTicker := time.NewTicker(time.Duration(config.LastActiveInterval) * time.Second)
	defer gameTicker.Stop()
	defer lastActiveTicker.Stop()

	// Initial publish
	s.publishGameSensor()
	s.publishLastActive()

	for {
		select {
		case <-s.stopChan:
			return
		case <-gameTicker.C:
			s.publishGameSensor()
		case <-lastActiveTicker.C:
			s.publishLastActive()
		}
	}
}

func (s *pcAgentService) publishGameSensor() {
	game := sensors.GetRunningGame()
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.mqttClient != nil && s.mqttClient.IsConnected() {
		s.mqttClient.PublishSensor("runninggames", game)
	}
}

func (s *pcAgentService) publishLastActive() {
	lastActive := sensors.GetLastActiveTime()
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.mqttClient != nil && s.mqttClient.IsConnected() {
		s.mqttClient.PublishSensor("lastactive", lastActive.Format(time.RFC3339))
	}
}
