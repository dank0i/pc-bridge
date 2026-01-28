package config

import (
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"sync"

	"github.com/fsnotify/fsnotify"
)

var (
	gameMapMutex   sync.RWMutex
	gameMap        map[string]string
	gameMapVersion uint64 // Incremented on each reload
	watcherDone    chan struct{} // Signal to stop file watcher
)

// setInitialGameMap sets the game map from config (called by LoadUserConfig)
func setInitialGameMap(games map[string]string) {
	gameMapMutex.Lock()
	gameMap = games
	gameMapVersion++
	gameMapMutex.Unlock()
	log.Printf("Loaded %d games from config", len(games))
}

// InitGameMapWatcher starts watching userConfig.json for game changes
func InitGameMapWatcher() {
	if configPath == "" {
		log.Println("Warning: config path not set, can't watch for game changes")
		return
	}

	watcherDone = make(chan struct{})
	go watchConfigFile()
}

// StopGameMapWatcher stops the file watcher goroutine
func StopGameMapWatcher() {
	if watcherDone != nil {
		close(watcherDone)
	}
}

// reloadGamesFromConfig reloads just the games section from userConfig.json
func reloadGamesFromConfig() {
	data, err := os.ReadFile(configPath)
	if err != nil {
		log.Printf("Error reading userConfig.json: %v", err)
		return
	}

	var cfg UserConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		log.Printf("Invalid userConfig.json: %v", err)
		return
	}

	if len(cfg.Games) == 0 {
		log.Println("Warning: no games found in config")
		return
	}

	gameMapMutex.Lock()
	oldCount := len(gameMap)
	gameMap = cfg.Games
	gameMapVersion++
	gameMapMutex.Unlock()

	log.Printf("Reloaded games from config: %d games (was %d)", len(cfg.Games), oldCount)
}

// watchConfigFile monitors userConfig.json for changes and reloads games when modified
func watchConfigFile() {
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		log.Printf("Can't create file watcher: %v", err)
		return
	}
	defer watcher.Close()

	// Watch the directory (more reliable than watching the file directly)
	dir := filepath.Dir(configPath)
	if err := watcher.Add(dir); err != nil {
		log.Printf("Can't watch directory: %v", err)
		return
	}

	filename := filepath.Base(configPath)
	log.Printf("Watching for changes to %s", configPath)

	for {
		select {
		case <-watcherDone:
			log.Println("Config watcher stopped")
			return
		case event, ok := <-watcher.Events:
			if !ok {
				return
			}
			// Check if it's our file
			// Include Rename for editors that use atomic save (write temp -> rename)
			if filepath.Base(event.Name) == filename {
				if event.Op&(fsnotify.Write|fsnotify.Create|fsnotify.Rename) != 0 {
					log.Println("userConfig.json changed, reloading games...")
					reloadGamesFromConfig()
				}
			}
		case err, ok := <-watcher.Errors:
			if !ok {
				return
			}
			log.Printf("File watcher error: %v", err)
		}
	}
}

// GetGameMap returns the current game map and its version (thread-safe).
// The version increments whenever the map is reloaded from disk.
// Callers can cache results and only rebuild when version changes.
// WARNING: The returned map is a direct reference - do not modify it.
func GetGameMap() (map[string]string, uint64) {
	gameMapMutex.RLock()
	defer gameMapMutex.RUnlock()
	return gameMap, gameMapVersion
}
