package main

import (
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
	"unsafe"

	"pc-agent/internal/commands"
	"pc-agent/internal/config"
	"pc-agent/internal/mqtt"
	"pc-agent/internal/power"
	"pc-agent/internal/sensors"

	"golang.org/x/sys/windows/svc"
)

const serviceName = "PCAgentService"

var (
	kernel32                  = syscall.NewLazyDLL("kernel32.dll")
	procCreateToolhelp32Snapshot = kernel32.NewProc("CreateToolhelp32Snapshot")
	procProcess32FirstW       = kernel32.NewProc("Process32FirstW")
	procProcess32NextW        = kernel32.NewProc("Process32NextW")
	procOpenProcess           = kernel32.NewProc("OpenProcess")
	procTerminateProcess      = kernel32.NewProc("TerminateProcess")
)

const (
	TH32CS_SNAPPROCESS = 0x00000002
	PROCESS_TERMINATE  = 0x0001
)

type processEntry32 struct {
	Size            uint32
	Usage           uint32
	ProcessID       uint32
	DefaultHeapID   uintptr
	ModuleID        uint32
	Threads         uint32
	ParentProcessID uint32
	PriClassBase    int32
	Flags           uint32
	ExeFile         [260]uint16
}

// killExistingInstances kills any other running pc-agent.exe processes using Windows API
func killExistingInstances() {
	myPID := uint32(os.Getpid())
	
	exe, err := os.Executable()
	if err != nil {
		return
	}
	exeName := strings.ToLower(filepath.Base(exe))
	
	// Create snapshot of all processes
	handle, _, _ := procCreateToolhelp32Snapshot.Call(TH32CS_SNAPPROCESS, 0)
	if handle == uintptr(syscall.InvalidHandle) {
		return
	}
	defer syscall.CloseHandle(syscall.Handle(handle))
	
	var entry processEntry32
	entry.Size = uint32(unsafe.Sizeof(entry))
	
	// Get first process
	ret, _, _ := procProcess32FirstW.Call(handle, uintptr(unsafe.Pointer(&entry)))
	if ret == 0 {
		return
	}
	
	for {
		procName := strings.ToLower(syscall.UTF16ToString(entry.ExeFile[:]))
		if procName == exeName && entry.ProcessID != myPID {
			// Open and terminate the process
			procHandle, _, _ := procOpenProcess.Call(PROCESS_TERMINATE, 0, uintptr(entry.ProcessID))
			if procHandle != 0 {
				log.Printf("Killing existing instance (PID %d)", entry.ProcessID)
				procTerminateProcess.Call(procHandle, 0)
				syscall.CloseHandle(syscall.Handle(procHandle))
			}
		}
		
		// Get next process
		ret, _, _ = procProcess32NextW.Call(handle, uintptr(unsafe.Pointer(&entry)))
		if ret == 0 {
			break
		}
	}
	
	time.Sleep(200 * time.Millisecond)
}

type pcAgentService struct {
	mqttClient         *mqtt.Client
	powerListener      *power.PowerEventListener
	displayWakeHandler *power.DisplayWakeHandler
	stopChan           chan struct{}
	wg                 sync.WaitGroup
	mu                 sync.Mutex // protects mqttClient access
}

func main() {
	// Kill any existing instances before starting
	killExistingInstances()

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

	// Load user config (creates from example and exits if not found)
	if err := config.LoadUserConfig(); err != nil {
		log.Fatalf("Failed to load config: %v", err)
	}

	// Start watching config file for game changes (hot-reload)
	config.InitGameMapWatcher()

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
