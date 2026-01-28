package sensors

import (
	"pc-agent/internal/winapi"
	"time"
	"unsafe"
)

// GetLastActiveTime returns the timestamp of the last user input
func GetLastActiveTime() time.Time {
	var lii winapi.LastInputInfo
	lii.CbSize = uint32(unsafe.Sizeof(lii))

	ret, _, _ := winapi.GetLastInputInfo.Call(uintptr(unsafe.Pointer(&lii)))
	if ret == 0 {
		return time.Now()
	}

	// Use GetTickCount64 to avoid overflow after 49 days
	tickCount, _, _ := winapi.GetTickCount64.Call()

	// lii.DwTime is still 32-bit, but we handle the wrap correctly
	currentTick32 := uint32(tickCount)
	idleMs := currentTick32 - lii.DwTime

	return time.Now().Add(-time.Duration(idleMs) * time.Millisecond)
}

// GetIdleSeconds returns how many seconds since last user input
func GetIdleSeconds() int {
	lastActive := GetLastActiveTime()
	return int(time.Since(lastActive).Seconds())
}
