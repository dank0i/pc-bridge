package power

import (
	"log"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
)

const (
	WM_POWERBROADCAST    = 0x218
	WM_QUIT              = 0x12
	WM_USER              = 0x0400
	WM_APP_HEARTBEAT     = WM_USER + 1 // Custom message for heartbeat
	PBT_APMSUSPEND       = 4
	PBT_APMRESUMEAUTO    = 0x12
	PBT_APMRESUMESUSPEND = 7
)

var (
	user32               = windows.NewLazySystemDLL("user32.dll")
	procCreateWindowExW  = user32.NewProc("CreateWindowExW")
	procDefWindowProcW   = user32.NewProc("DefWindowProcW")
	procRegisterClassW   = user32.NewProc("RegisterClassExW")
	procGetMessageW      = user32.NewProc("GetMessageW")
	procDispatchMessageW = user32.NewProc("DispatchMessageW")
	procPostMessageW     = user32.NewProc("PostMessageW")
	procDestroyWindow    = user32.NewProc("DestroyWindow")
)

type PowerEventListener struct {
	OnSleep       func()
	OnWake        func()
	hwnd          uintptr
	wg            sync.WaitGroup
	stopped       atomic.Bool
	lastHeartbeat atomic.Int64 // Unix timestamp of last heartbeat response
	mu            sync.Mutex
}

func NewPowerEventListener(onSleep, onWake func()) *PowerEventListener {
	p := &PowerEventListener{
		OnSleep: onSleep,
		OnWake:  onWake,
	}
	p.lastHeartbeat.Store(time.Now().Unix())
	return p
}

func (p *PowerEventListener) Start() {
	p.wg.Add(1)
	go p.listen()

	// Start heartbeat monitor to detect if message pump died
	go p.monitorHeartbeat()
}

func (p *PowerEventListener) Stop() {
	p.stopped.Store(true)

	p.mu.Lock()
	hwnd := p.hwnd
	p.mu.Unlock()

	// Post WM_QUIT to unblock GetMessageW
	if hwnd != 0 {
		procPostMessageW.Call(hwnd, WM_QUIT, 0, 0)
	}

	p.wg.Wait()
}

// monitorHeartbeat periodically checks if the message pump is still responsive.
// If the message pump stops responding (e.g., after wake from sleep), it logs a warning.
func (p *PowerEventListener) monitorHeartbeat() {
	ticker := time.NewTicker(60 * time.Second)
	defer ticker.Stop()

	for {
		if p.stopped.Load() {
			return
		}

		<-ticker.C
		if p.stopped.Load() {
			return
		}

		// Send heartbeat message
		p.mu.Lock()
		hwnd := p.hwnd
		p.mu.Unlock()

		if hwnd != 0 {
			procPostMessageW.Call(hwnd, WM_APP_HEARTBEAT, 0, 0)
		}

		// Check if we got a response within 5 seconds
		time.Sleep(5 * time.Second)
		lastBeat := p.lastHeartbeat.Load()
		if time.Since(time.Unix(lastBeat, 0)) > 70*time.Second {
			log.Println("WARNING: Power event message pump may be unresponsive!")
		}
	}
}

func (p *PowerEventListener) listen() {
	defer p.wg.Done()

	// CRITICAL: Lock this goroutine to a single OS thread.
	// Windows message pumps are thread-affine - the window must be created
	// and its messages processed on the same thread.
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	defer func() {
		// Clean up the window on exit
		p.mu.Lock()
		if p.hwnd != 0 {
			procDestroyWindow.Call(p.hwnd)
			p.hwnd = 0
		}
		p.mu.Unlock()
	}()

	className, _ := syscall.UTF16PtrFromString("PCAgentPowerMonitor")
	windowName, _ := syscall.UTF16PtrFromString("")

	wndProc := syscall.NewCallback(func(hwnd, msg, wParam, lParam uintptr) uintptr {
		switch msg {
		case WM_POWERBROADCAST:
			switch wParam {
			case PBT_APMSUSPEND:
				log.Println("Power event: SLEEP (PBT_APMSUSPEND)")
				if p.OnSleep != nil {
					p.OnSleep()
				}
			case PBT_APMRESUMEAUTO:
				log.Println("Power event: WAKE (PBT_APMRESUMEAUTO)")
				if p.OnWake != nil {
					p.OnWake()
				}
			case PBT_APMRESUMESUSPEND:
				log.Println("Power event: WAKE (PBT_APMRESUMESUSPEND)")
				if p.OnWake != nil {
					p.OnWake()
				}
			default:
				log.Printf("Power event: unknown wParam=0x%X", wParam)
			}
		case WM_APP_HEARTBEAT:
			// Respond to heartbeat - proves message pump is alive
			p.lastHeartbeat.Store(time.Now().Unix())
		}
		ret, _, _ := procDefWindowProcW.Call(hwnd, msg, wParam, lParam)
		return ret
	})

	type WNDCLASSEXW struct {
		Size       uint32
		Style      uint32
		WndProc    uintptr
		ClsExtra   int32
		WndExtra   int32
		Instance   windows.Handle
		Icon       windows.Handle
		Cursor     windows.Handle
		Background windows.Handle
		MenuName   *uint16
		ClassName  *uint16
		IconSm     windows.Handle
	}

	var wc WNDCLASSEXW
	wc.Size = uint32(unsafe.Sizeof(wc))
	wc.WndProc = wndProc
	wc.ClassName = className

	procRegisterClassW.Call(uintptr(unsafe.Pointer(&wc)))

	hwnd, _, _ := procCreateWindowExW.Call(
		0, uintptr(unsafe.Pointer(className)), uintptr(unsafe.Pointer(windowName)),
		0, 0, 0, 0, 0, 0, 0, 0, 0,
	)

	// Store hwnd for Stop() to post quit message
	p.mu.Lock()
	p.hwnd = hwnd
	p.mu.Unlock()

	type MSG struct {
		Hwnd    uintptr
		Message uint32
		WParam  uintptr
		LParam  uintptr
		Time    uint32
		Pt      struct{ X, Y int32 }
	}

	var msg MSG
	// GetMessageW blocks until a message is available - no busy loop!
	for {
		ret, _, _ := procGetMessageW.Call(uintptr(unsafe.Pointer(&msg)), 0, 0, 0)
		if ret == 0 || ret == ^uintptr(0) { // WM_QUIT or error
			log.Println("Power event listener: message pump exiting")
			return
		}

		// Check if we should stop
		if p.stopped.Load() {
			return
		}

		procDispatchMessageW.Call(uintptr(unsafe.Pointer(&msg)))
	}
}
