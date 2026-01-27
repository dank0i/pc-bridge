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
	gamesPath      string
	watcherDone    chan struct{} // Signal to stop file watcher
)

// Default games (fallback if no JSON file exists)
var defaultGames = map[string]string{
	"FortniteClient":     "fortnite",
	"r5apex":             "apex_legends",
	"bg3":                "baldur_s_gate_3",
	"Overwatch":          "overwatch_2",
	"MarvelRivals":       "marvel_rivals",
	"bf6":                "battlefield_6",
	"3DMark":             "3dmark",
	"Balatro":            "balatro",
	"Brawlhalla":         "brawlhalla",
	"cs2":                "counter_strike_2",
	"helldivers2":        "helldivers_2",
	"KovaaK":             "kovaak_s",
	"Lethal Company":     "lethal_company",
	"javaw":              "minecraft",
	"MonsterHunterWilds": "monster_hunter_wilds",
	"okami":              "okami_hd",
	"Phasmophobia":       "phasmophobia",
	"REPO":               "r_e_p_o",
	"RVThereYet":         "rv_there_yet",
	"Skate":              "skate",
}

// InitGameMap loads the game map from games.json and starts the file watcher
func InitGameMap() {
	// Find games.json next to executable
	exe, err := os.Executable()
	if err != nil {
		log.Printf("Warning: couldn't get executable path: %v", err)
		gameMap = defaultGames
		return
	}
	gamesPath = filepath.Join(filepath.Dir(exe), "games.json")

	// Initial load
	if !loadGamesFromFile() {
		log.Printf("Using default game map (%d games)", len(defaultGames))
		gameMap = defaultGames
		// Create the file with defaults so user can edit it
		saveDefaultGames()
	}

	// Start file watcher with shutdown channel
	watcherDone = make(chan struct{})
	go watchGamesFile()
}

// StopGameMapWatcher stops the file watcher goroutine
func StopGameMapWatcher() {
	if watcherDone != nil {
		close(watcherDone)
	}
}

// loadGamesFromFile reads and parses games.json, returns false if failed
func loadGamesFromFile() bool {
	data, err := os.ReadFile(gamesPath)
	if err != nil {
		if !os.IsNotExist(err) {
			log.Printf("Error reading games.json: %v", err)
		}
		return false
	}

	var newMap map[string]string
	if err := json.Unmarshal(data, &newMap); err != nil {
		log.Printf("Invalid games.json: %v", err)
		return false
	}

	gameMapMutex.Lock()
	oldCount := len(gameMap)
	gameMap = newMap
	gameMapVersion++
	gameMapMutex.Unlock()

	if oldCount > 0 {
		log.Printf("Reloaded games.json: %d games (was %d)", len(newMap), oldCount)
	} else {
		log.Printf("Loaded games.json: %d games", len(newMap))
	}
	return true
}

// saveDefaultGames creates games.json with the default games for easy editing
func saveDefaultGames() {
	data, err := json.MarshalIndent(defaultGames, "", "  ")
	if err != nil {
		log.Printf("Error marshaling default games: %v", err)
		return
	}

	if err := os.WriteFile(gamesPath, data, 0644); err != nil {
		log.Printf("Error writing default games.json: %v", err)
		return
	}

	log.Printf("Created games.json with %d default games", len(defaultGames))
}

// watchGamesFile monitors games.json for changes and reloads when modified
func watchGamesFile() {
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		log.Printf("Can't create file watcher for games.json: %v", err)
		return
	}
	defer watcher.Close()

	// Watch the directory (more reliable than watching the file directly)
	dir := filepath.Dir(gamesPath)
	if err := watcher.Add(dir); err != nil {
		log.Printf("Can't watch directory for games.json: %v", err)
		return
	}

	filename := filepath.Base(gamesPath)
	log.Printf("Watching for changes to %s", gamesPath)

	for {
		select {
		case <-watcherDone:
			log.Println("Game map watcher stopped")
			return
		case event, ok := <-watcher.Events:
			if !ok {
				return
			}
			// Check if it's our file
			if filepath.Base(event.Name) == filename {
				if event.Op&(fsnotify.Write|fsnotify.Create) != 0 {
					log.Println("games.json changed, reloading...")
					loadGamesFromFile()
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

// GetGameMap returns the current game map and its version (thread-safe)
// The version increments whenever the map is reloaded from disk.
// Callers can cache results and only rebuild when version changes.
func GetGameMap() (map[string]string, uint64) {
	gameMapMutex.RLock()
	defer gameMapMutex.RUnlock()
	return gameMap, gameMapVersion
}
