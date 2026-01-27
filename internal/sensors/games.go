package sensors

import (
	"log"
	"pc-agent/internal/config"
	"strings"
	"sync"

	"github.com/shirou/gopsutil/v3/process"
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

// GetRunningGame checks for running game processes and returns the game identifier
func GetRunningGame() string {
	processes, err := process.Processes()
	if err != nil {
		log.Printf("Error getting processes: %v", err)
		return "none"
	}

	// Get cached patterns (rebuilds only on game map change)
	gamePatterns := getPatterns()

	for _, p := range processes {
		name, err := p.Name()
		if err != nil {
			continue
		}

		// Lowercase once per process
		nameLower := strings.ToLower(name)
		baseNameLower := strings.TrimSuffix(nameLower, ".exe")

		// Check against patterns
		for _, gp := range gamePatterns {
			if strings.HasPrefix(nameLower, gp.patternLower) || baseNameLower == gp.patternLower {
				return gp.gameID
			}
		}
	}

	return "none"
}
