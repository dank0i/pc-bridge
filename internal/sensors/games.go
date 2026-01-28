package sensors

import (
	"pc-agent/internal/config"
	"pc-agent/internal/winapi"
	"strings"
	"sync"
	"syscall"
	"unsafe"
)

type gamePattern struct {
	patternLower string
	gameID       string
}

// Cached patterns with version tracking
var (
	patternCache   []gamePattern
	patternVersion uint64
	patternMu      sync.RWMutex
)

// getPatterns returns cached patterns, rebuilding only when game map version changes
func getPatterns() []gamePattern {
	gameMap, version := config.GetGameMap()

	// Fast path: check if cache is valid
	patternMu.RLock()
	if patternVersion == version && patternCache != nil {
		patterns := patternCache
		patternMu.RUnlock()
		return patterns
	}
	patternMu.RUnlock()

	// Slow path: rebuild patterns
	patternMu.Lock()
	defer patternMu.Unlock()

	// Double-check after acquiring write lock
	if patternVersion == version && patternCache != nil {
		return patternCache
	}

	patternCache = make([]gamePattern, 0, len(gameMap))
	for pattern, gameID := range gameMap {
		patternCache = append(patternCache, gamePattern{
			patternLower: strings.ToLower(pattern),
			gameID:       gameID,
		})
	}
	patternVersion = version
	return patternCache
}

// GetRunningGame checks for running game processes and returns the game identifier.
// Uses Windows API directly to avoid gopsutil allocations.
func GetRunningGame() string {
	// Get cached patterns (rebuilds only on game map change)
	gamePatterns := getPatterns()
	if len(gamePatterns) == 0 {
		return "none"
	}

	// Create snapshot of all processes
	handle, _, _ := winapi.CreateToolhelp32Snapshot.Call(winapi.TH32CS_SNAPPROCESS, 0)
	if handle == uintptr(syscall.InvalidHandle) {
		return "none"
	}
	defer syscall.CloseHandle(syscall.Handle(handle))

	var entry winapi.ProcessEntry32
	entry.Size = uint32(unsafe.Sizeof(entry))

	// Get first process
	ret, _, _ := winapi.Process32FirstW.Call(handle, uintptr(unsafe.Pointer(&entry)))
	if ret == 0 {
		return "none"
	}

	for {
		// Convert process name to lowercase string
		procName := strings.ToLower(syscall.UTF16ToString(entry.ExeFile[:]))
		baseNameLower := strings.TrimSuffix(procName, ".exe")

		// Check against patterns
		for _, gp := range gamePatterns {
			if strings.HasPrefix(procName, gp.patternLower) || baseNameLower == gp.patternLower {
				return gp.gameID
			}
		}

		// Get next process
		ret, _, _ = winapi.Process32NextW.Call(handle, uintptr(unsafe.Pointer(&entry)))
		if ret == 0 {
			break
		}
	}

	return "none"
}
