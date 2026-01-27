package power

import (
	"log"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Windows API constants for display control
const (
	HWND_BROADCAST   = 0xFFFF
	WM_SYSCOMMAND    = 0x0112
	SC_MONITORPOWER  = 0xF170
	MONITOR_ON       = 0xFFFFFFFFFFFFFFFF // -1 as unsigned
	MONITOR_STANDBY  = 1
	MONITOR_OFF      = 2

	// SetThreadExecutionState flags
	ES_CONTINUOUS       = 0x80000000
	ES_SYSTEM_REQUIRED  = 0x00000001
	ES_DISPLAY_REQUIRED = 0x00000002

	// Virtual key codes and flags
	VK_F15          = 0x7E // F15 - benign key, unlikely to trigger anything
	KEYEVENTF_KEYUP = 0x0002
)

var (
	kernel32                    = syscall.NewLazyDLL("kernel32.dll")
	procSetThreadExecutionState = kernel32.NewProc("SetThreadExecutionState")
	procSendMessageW            = user32.NewProc("SendMessageW")
	procKeybd_event             = user32.NewProc("keybd_event")

	// sleepPreventionActive tracks if sleep prevention is currently active
	// to avoid spawning multiple reset goroutines
	sleepPreventionActive atomic.Bool
)

// WakeDisplay attempts to wake the display using multiple methods.
// It's designed to handle WoL wake issues where the display doesn't turn on properly.
func WakeDisplay() {
	log.Println("WakeDisplay: Initiating display wake sequence")

	// Step 1: Turn on monitor via Windows API
	turnOnMonitor()

	// Step 2: Send a benign keypress to register user activity
	sendBenignKeypress()

	// Step 3: Temporarily prevent system from sleeping
	// This gives the user time to interact if they're there
	preventSleepTemporary(30 * time.Second)

	log.Println("WakeDisplay: Wake sequence completed")
}

// WakeDisplayWithRetry attempts to wake the display with retries.
// This is useful when called immediately after WoL wake, as the system
// may need time to fully initialize.
func WakeDisplayWithRetry(maxAttempts int, delayBetween time.Duration) {
	if maxAttempts <= 0 {
		maxAttempts = 1
	}
	log.Printf("WakeDisplay: Starting wake sequence with %d attempts", maxAttempts)

	for attempt := 1; attempt <= maxAttempts; attempt++ {
		// Turn on monitor
		turnOnMonitor()

		// Small delay to let the display respond
		time.Sleep(100 * time.Millisecond)

		// Send keypress
		sendBenignKeypress()

		if attempt < maxAttempts {
			time.Sleep(delayBetween)
		}
	}

	// After all attempts, prevent sleep temporarily
	preventSleepTemporary(30 * time.Second)

	log.Println("WakeDisplay: Wake sequence completed")
}

// turnOnMonitor sends the SC_MONITORPOWER message to turn on all monitors
func turnOnMonitor() {
	procSendMessageW.Call(
		HWND_BROADCAST,
		WM_SYSCOMMAND,
		SC_MONITORPOWER,
		uintptr(MONITOR_ON),
	)
}

// sendBenignKeypress sends F15 keypress to register user activity.
// F15 is chosen because:
// - It exists on Windows but is rarely used by applications
// - It won't trigger any visible action in most programs
// - It registers as user input, preventing immediate re-sleep
func sendBenignKeypress() {
	// Key down F15
	procKeybd_event.Call(uintptr(VK_F15), 0, 0, 0)
	time.Sleep(10 * time.Millisecond)
	// Key up F15
	procKeybd_event.Call(uintptr(VK_F15), 0, uintptr(KEYEVENTF_KEYUP), 0)
}

// preventSleepTemporary prevents the system from sleeping for the specified duration.
// This uses SetThreadExecutionState to tell Windows the system is in use.
// Only one prevention period can be active at a time; subsequent calls extend the duration.
func preventSleepTemporary(duration time.Duration) {
	// Only spawn a reset goroutine if one isn't already running
	if !sleepPreventionActive.CompareAndSwap(false, true) {
		return
	}

	go func() {
		// Lock to OS thread - SetThreadExecutionState is thread-local
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()

		// Set execution state to prevent sleep
		ret, _, _ := procSetThreadExecutionState.Call(
			uintptr(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED),
		)
		if ret == 0 {
			sleepPreventionActive.Store(false)
			return
		}

		time.Sleep(duration)
		procSetThreadExecutionState.Call(uintptr(ES_CONTINUOUS))
		sleepPreventionActive.Store(false)
		log.Println("WakeDisplay: Sleep prevention ended")
	}()
}

// DisplayWakeHandler manages automatic display wake on system resume.
// It's designed to be integrated with the PowerEventListener.
type DisplayWakeHandler struct {
	enabled      bool
	attempts     int
	attemptDelay time.Duration
	initialDelay time.Duration
	mu           sync.Mutex // protects against concurrent OnWake calls
}

// NewDisplayWakeHandler creates a new handler with configurable settings.
func NewDisplayWakeHandler(enabled bool, attempts int, attemptDelay, initialDelay time.Duration) *DisplayWakeHandler {
	if attempts <= 0 {
		attempts = 1
	}
	return &DisplayWakeHandler{
		enabled:      enabled,
		attempts:     attempts,
		attemptDelay: attemptDelay,
		initialDelay: initialDelay,
	}
}

// DefaultDisplayWakeHandler creates a handler with sensible defaults.
func DefaultDisplayWakeHandler() *DisplayWakeHandler {
	return &DisplayWakeHandler{
		enabled:      true,
		attempts:     3,
		attemptDelay: 500 * time.Millisecond,
		initialDelay: 1 * time.Second,
	}
}

// OnWake should be called when the system wakes from sleep.
// It will handle display wake asynchronously.
func (h *DisplayWakeHandler) OnWake() {
	if !h.enabled {
		return
	}

	// Use TryLock to prevent overlapping wake sequences
	if !h.mu.TryLock() {
		return
	}

	go func() {
		defer h.mu.Unlock()

		// Wait for system to stabilize after wake
		if h.initialDelay > 0 {
			time.Sleep(h.initialDelay)
		}

		WakeDisplayWithRetry(h.attempts, h.attemptDelay)
	}()
}
