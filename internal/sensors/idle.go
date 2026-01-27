package sensors

import (
	"syscall"
	"time"
	"unsafe"
)

var (
	user32           = syscall.NewLazyDLL("user32.dll")
	getLastInputInfo = user32.NewProc("GetLastInputInfo")
	kernel32         = syscall.NewLazyDLL("kernel32.dll")
	getTickCount64   = kernel32.NewProc("GetTickCount64")
)

type lastInputInfo struct {
	cbSize uint32
	dwTime uint32
}

// GetLastActiveTime returns the timestamp of the last user input
func GetLastActiveTime() time.Time {
	var lii lastInputInfo
	lii.cbSize = uint32(unsafe.Sizeof(lii))

	ret, _, _ := getLastInputInfo.Call(uintptr(unsafe.Pointer(&lii)))
	if ret == 0 {
		return time.Now()
	}

	// Use GetTickCount64 to avoid overflow after 49 days
	// GetTickCount64 returns the value directly in r1
	tickCount, _, _ := getTickCount64.Call()

	// lii.dwTime is still 32-bit, but we handle the wrap correctly
	// by computing within the 32-bit space first, then extending
	currentTick32 := uint32(tickCount)
	idleMs := currentTick32 - lii.dwTime

	return time.Now().Add(-time.Duration(idleMs) * time.Millisecond)
}

// GetIdleSeconds returns how many seconds since last user input
func GetIdleSeconds() int {
	lastActive := GetLastActiveTime()
	return int(time.Since(lastActive).Seconds())
}
