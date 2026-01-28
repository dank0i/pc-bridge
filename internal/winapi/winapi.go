// Package winapi provides centralized Windows API declarations.
// This avoids duplicate DLL loading across packages.
package winapi

import (
	"syscall"

	"golang.org/x/sys/windows"
)

// DLLs - loaded lazily on first use
var (
	Kernel32 = syscall.NewLazyDLL("kernel32.dll")
	User32   = windows.NewLazySystemDLL("user32.dll")
)

// Kernel32 procs
var (
	CreateToolhelp32Snapshot = Kernel32.NewProc("CreateToolhelp32Snapshot")
	Process32FirstW          = Kernel32.NewProc("Process32FirstW")
	Process32NextW           = Kernel32.NewProc("Process32NextW")
	OpenProcess              = Kernel32.NewProc("OpenProcess")
	TerminateProcess         = Kernel32.NewProc("TerminateProcess")
	SetConsoleCtrlHandler    = Kernel32.NewProc("SetConsoleCtrlHandler")
	GetTickCount64           = Kernel32.NewProc("GetTickCount64")
	SetThreadExecutionState  = Kernel32.NewProc("SetThreadExecutionState")
)

// User32 procs
var (
	GetLastInputInfo = User32.NewProc("GetLastInputInfo")
	KeybdEvent       = User32.NewProc("keybd_event")
	SendMessageW     = User32.NewProc("SendMessageW")
	CreateWindowExW  = User32.NewProc("CreateWindowExW")
	DefWindowProcW   = User32.NewProc("DefWindowProcW")
	RegisterClassExW = User32.NewProc("RegisterClassExW")
	GetMessageW      = User32.NewProc("GetMessageW")
	DispatchMessageW = User32.NewProc("DispatchMessageW")
	PostMessageW     = User32.NewProc("PostMessageW")
	DestroyWindow    = User32.NewProc("DestroyWindow")
)

// Constants
const (
	// Process snapshot
	TH32CS_SNAPPROCESS = 0x00000002
	PROCESS_TERMINATE  = 0x0001

	// Console control events
	CTRL_C_EVENT        = 0
	CTRL_BREAK_EVENT    = 1
	CTRL_CLOSE_EVENT    = 2
	CTRL_LOGOFF_EVENT   = 5
	CTRL_SHUTDOWN_EVENT = 6

	// Window messages
	WM_POWERBROADCAST = 0x218
	WM_QUIT           = 0x12
	WM_USER           = 0x0400
	WM_SYSCOMMAND     = 0x0112

	// Power broadcast events
	PBT_APMSUSPEND       = 4
	PBT_APMRESUMEAUTO    = 0x12
	PBT_APMRESUMESUSPEND = 7

	// Monitor power
	HWND_BROADCAST  = 0xFFFF
	SC_MONITORPOWER = 0xF170
	MONITOR_ON      = 0xFFFFFFFFFFFFFFFF // -1 as unsigned

	// SetThreadExecutionState flags
	ES_CONTINUOUS       = 0x80000000
	ES_SYSTEM_REQUIRED  = 0x00000001
	ES_DISPLAY_REQUIRED = 0x00000002

	// Virtual key codes
	VK_CONTROL      = 0x11
	VK_F6           = 0x75
	VK_F15          = 0x7E
	KEYEVENTF_KEYUP = 0x0002
)

// ProcessEntry32 for process enumeration
type ProcessEntry32 struct {
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

// LastInputInfo for idle detection
type LastInputInfo struct {
	CbSize uint32
	DwTime uint32
}
