package commands

import (
	"encoding/json"
	"log"
	"os"
	"os/exec"
	"pc-agent/internal/config"
	"pc-agent/internal/winapi"
	"strings"
	"syscall"
	"time"
)

// Command timeout - kills stuck processes after this duration
const commandTimeout = 5 * time.Minute

// Execute runs a command based on the command name and optional payload
func Execute(command, payload string) error {
	// Normalize payload - trim whitespace and handle HA button defaults
	payload = strings.TrimSpace(payload)
	if payload == "PRESS" || payload == "press" {
		payload = "" // HA buttons send "PRESS" by default
	}

	log.Printf("Executing command: %s (payload: %q)", command, payload)

	switch command {
	case "discord_leave_channel":
		return sendCtrlF6()
	case "Wake":
		return dismissScreensaver()
	case "notification":
		if payload == "" {
			log.Printf("Notification received with empty payload, ignoring")
			return nil
		}
		return showNotification(payload)
	default:
		return executeShellCommand(command, payload)
	}
}

func executeShellCommand(command, payload string) error {
	// Get the command string from config
	cmdStr := config.Commands[command]

	// If no predefined command, check if payload contains a command
	if cmdStr == "" && payload != "" {
		cmdStr = payload
	}

	if cmdStr == "" {
		log.Printf("No command configured for: %s (and no payload provided)", command)
		return nil
	}

	// Check for launcher shortcuts (steam:APPID, epic:GAME, exe:PATH)
	if launchCmd := expandLauncherShortcut(cmdStr); launchCmd != "" {
		cmdStr = launchCmd
	}

	// Expand environment variables (both %VAR% Windows style and $VAR Unix style)
	cmdStr = expandWindowsEnv(cmdStr)

	// Handle cmd.exe "start" syntax - convert to PowerShell Start-Process
	// Pattern: start "" "url" or start "" "path"
	if strings.HasPrefix(strings.ToLower(cmdStr), "start ") {
		// Extract the URL/path from: start "" "something"
		parts := strings.SplitN(cmdStr, `"`, 4)
		if len(parts) >= 4 {
			target := parts[3]
			// Remove trailing quote if present
			target = strings.TrimSuffix(target, `"`)
			cmdStr = `Start-Process "` + target + `"`
		}
	}

	log.Printf("Running: %s", cmdStr)

	// Use PowerShell to run the command
	// Add "& " prefix for executables (not needed for PowerShell cmdlets)
	psCommand := cmdStr
	psCmdlets := []string{"Start-Process", "Add-Type", "Get-Process", "Stop-Process", "Set-", "Get-", "New-", "Remove-", "Invoke-"}
	needsAmpersand := true
	for _, prefix := range psCmdlets {
		if strings.HasPrefix(cmdStr, prefix) {
			needsAmpersand = false
			break
		}
	}
	if needsAmpersand {
		psCommand = "& " + cmdStr
	}

	cmd := exec.Command("powershell", "-NoProfile", "-Command", psCommand)
	cmd.SysProcAttr = &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: 0x08000000, // CREATE_NO_WINDOW
	}

	if err := cmd.Start(); err != nil {
		return err
	}

	// Wait in goroutine with timeout to prevent zombie processes
	go func() {
		done := make(chan error, 1)
		go func() { done <- cmd.Wait() }()

		timer := time.NewTimer(commandTimeout)
		defer timer.Stop() // CRITICAL: Stop timer to prevent memory leak

		select {
		case err := <-done:
			if err != nil {
				log.Printf("Command finished with error: %v", err)
			}
		case <-timer.C:
			log.Printf("Command timed out after %v, killing process", commandTimeout)
			if cmd.Process != nil {
				cmd.Process.Kill()
			}
		}
	}()

	return nil
}

// expandWindowsEnv expands Windows-style %VAR% environment variables
func expandWindowsEnv(s string) string {
	result := s
	for {
		start := strings.Index(result, "%")
		if start == -1 {
			break
		}
		end := strings.Index(result[start+1:], "%")
		if end == -1 {
			break
		}
		end += start + 1
		varName := result[start+1 : end]
		varValue := os.Getenv(varName)
		result = result[:start] + varValue + result[end+1:]
	}
	return result
}

// dismissScreensaver kills any running screensaver process to dismiss it.
// Uses a single PowerShell command to kill all .scr processes at once.
func dismissScreensaver() error {
	log.Printf("Attempting to dismiss screensaver")
	
	// Kill all screensaver processes (.scr) with a single PowerShell command
	// This is more efficient than spawning multiple taskkill processes
	psCmd := `Get-Process | Where-Object { $_.Path -like '*.scr' } | Stop-Process -Force -ErrorAction SilentlyContinue`
	
	cmd := exec.Command("powershell", "-NoProfile", "-Command", psCmd)
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true}
	_ = cmd.Run() // Ignore errors - no screensaver might be running
	
	log.Printf("Screensaver dismiss attempted")
	return nil
}

// sendCtrlF6 sends Ctrl+F6 keypress (Discord leave channel hotkey)
func sendCtrlF6() error {
	// Key down Ctrl
	winapi.KeybdEvent.Call(uintptr(winapi.VK_CONTROL), 0, 0, 0)
	time.Sleep(10 * time.Millisecond)
	// Key down F6
	winapi.KeybdEvent.Call(uintptr(winapi.VK_F6), 0, 0, 0)
	time.Sleep(10 * time.Millisecond)
	// Key up F6
	winapi.KeybdEvent.Call(uintptr(winapi.VK_F6), 0, uintptr(winapi.KEYEVENTF_KEYUP), 0)
	time.Sleep(10 * time.Millisecond)
	// Key up Ctrl
	winapi.KeybdEvent.Call(uintptr(winapi.VK_CONTROL), 0, uintptr(winapi.KEYEVENTF_KEYUP), 0)
	return nil
}

// Notification structure for HA notifications
type Notification struct {
	Title   string `json:"title"`
	Message string `json:"message"`
	Data    struct {
		Image string `json:"image"`
	} `json:"data"`
}

// escapeXML escapes special XML characters and strips control characters.
// XML 1.0 prohibits control chars 0x00-0x08, 0x0B, 0x0C, 0x0E-0x1F (except tab, newline, CR).
// Uses strings.Builder for efficiency.
func escapeXML(s string) string {
	var b strings.Builder
	b.Grow(len(s) + 10) // Pre-allocate with some extra space for escapes
	for _, r := range s {
		// Skip XML 1.0 prohibited control characters
		if r < 0x20 && r != '\t' && r != '\n' && r != '\r' {
			continue // Strip prohibited control chars
		}
		switch r {
		case '&':
			b.WriteString("&amp;")
		case '<':
			b.WriteString("&lt;")
		case '>':
			b.WriteString("&gt;")
		case '\'':
			b.WriteString("&apos;")
		case '"':
			b.WriteString("&quot;")
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}

func showNotification(payload string) error {
	var notif Notification
	if err := json.Unmarshal([]byte(payload), &notif); err != nil {
		// Try simple message
		notif.Message = payload
	}

	title := notif.Title
	if title == "" {
		title = "Home Assistant"
	}
	message := notif.Message
	if message == "" {
		message = strings.TrimSpace(payload)
	}

	// Escape XML special characters to prevent injection
	title = escapeXML(title)
	message = escapeXML(message)

	// Use PowerShell to show toast notification
	psCmd := `
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$template = @"
<toast>
    <visual>
        <binding template="ToastText02">
            <text id="1">` + title + `</text>
            <text id="2">` + message + `</text>
        </binding>
    </visual>
</toast>
"@
$xml = New-Object Windows.Data.Xml.Dom.XmlDocument
$xml.LoadXml($template)
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($toast)
`

	cmd := exec.Command("powershell", "-NoProfile", "-Command", psCmd)
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true}

	if err := cmd.Start(); err != nil {
		return err
	}

	// Wait in goroutine with timeout - notifications should complete quickly
	go func() {
		done := make(chan error, 1)
		go func() { done <- cmd.Wait() }()

		timer := time.NewTimer(30 * time.Second)
		defer timer.Stop() // CRITICAL: Stop timer to prevent memory leak

		select {
		case err := <-done:
			if err != nil {
				log.Printf("Notification command error: %v", err)
			}
		case <-timer.C:
			log.Printf("Notification timed out, killing process")
			if cmd.Process != nil {
				cmd.Process.Kill()
			}
		}
	}()

	return nil
}

// expandLauncherShortcut converts launcher shortcuts to full commands.
// Supported formats:
//   - steam:APPID   → launches Steam game by App ID (e.g., steam:1517290)
//   - epic:GAME     → launches Epic game by name (e.g., epic:Fortnite)
//   - xbox:PKG      → launches Xbox/MS Store game (e.g., xbox:Microsoft.MinecraftUWP_8wekyb3d8bbwe!App)
//   - exe:PATH      → launches executable directly (e.g., exe:C:\Games\Game.exe)
//   - close:NAME    → gracefully closes process (e.g., close:bf6)
//
// Returns empty string if not a launcher shortcut.
func expandLauncherShortcut(cmd string) string {
	// Split on first colon to get launcher:argument
	parts := strings.SplitN(cmd, ":", 2)
	if len(parts) != 2 {
		return ""
	}

	launcher := strings.ToLower(strings.TrimSpace(parts[0]))
	arg := strings.TrimSpace(parts[1])

	if arg == "" {
		return ""
	}

	switch launcher {
	case "steam":
		// Steam App ID must be numeric
		if !isNumeric(arg) {
			log.Printf("Invalid Steam App ID (must be numeric): %s", arg)
			return ""
		}
		log.Printf("Launching Steam game: App ID %s", arg)
		return `Start-Process "steam://rungameid/` + arg + `"`

	case "epic":
		// Epic game names should be alphanumeric (with underscores/hyphens)
		if !isSafeIdentifier(arg) {
			log.Printf("Invalid Epic game name: %s", arg)
			return ""
		}
		log.Printf("Launching Epic game: %s", arg)
		return `Start-Process "com.epicgames.launcher://apps/` + arg + `?action=launch&silent=true"`

	case "xbox", "msstore":
		// Xbox package names contain alphanumeric, dots, underscores, exclamation
		if !isSafePackageName(arg) {
			log.Printf("Invalid Xbox/MS Store package name: %s", arg)
			return ""
		}
		log.Printf("Launching Xbox/MS Store game: %s", arg)
		// Use explorer.exe to launch - more reliable than Start-Process with shell:
		return `Start-Process explorer.exe -ArgumentList 'shell:AppsFolder\` + arg + `'`

	case "exe", "run":
		// Executable paths - allow file path characters but reject shell metacharacters
		if !isSafePath(arg) {
			log.Printf("Invalid executable path (contains shell metacharacters): %s", arg)
			return ""
		}
		log.Printf("Launching executable: %s", arg)
		// Split path and arguments if present (first space after .exe or no extension)
		var exePath, exeArgs string
		if idx := strings.Index(strings.ToLower(arg), ".exe "); idx != -1 {
			exePath = arg[:idx+4]
			exeArgs = strings.TrimSpace(arg[idx+5:])
		} else {
			exePath = arg
		}
		// Quote path if it contains spaces
		if strings.Contains(exePath, " ") && !strings.HasPrefix(exePath, `"`) {
			exePath = `"` + exePath + `"`
		}
		if exeArgs != "" {
			return `Start-Process ` + exePath + ` -ArgumentList '` + exeArgs + `'`
		}
		return `Start-Process ` + exePath

	case "close", "kill":
		// Process names should be simple identifiers
		processName := strings.TrimSuffix(arg, ".exe")
		if !isSafeIdentifier(processName) {
			log.Printf("Invalid process name: %s", arg)
			return ""
		}
		log.Printf("Closing process: %s", arg)
		return `Get-Process | Where-Object { $_.ProcessName -eq '` + processName + `' } | ForEach-Object { $_.CloseMainWindow() }`

	default:
		return ""
	}
}

// isNumeric returns true if s contains only digits
func isNumeric(s string) bool {
	for _, r := range s {
		if r < '0' || r > '9' {
			return false
		}
	}
	return len(s) > 0
}

// isSafeIdentifier returns true if s is alphanumeric with allowed punctuation (.-_)
func isSafeIdentifier(s string) bool {
	for _, r := range s {
		if !((r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') ||
			r == '.' || r == '-' || r == '_') {
			return false
		}
	}
	return len(s) > 0
}

// isSafePackageName returns true if s is valid for Xbox/MS Store package names
func isSafePackageName(s string) bool {
	for _, r := range s {
		if !((r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') ||
			r == '.' || r == '-' || r == '_' || r == '!') {
			return false
		}
	}
	return len(s) > 0
}

// isSafePath returns true if path doesn't contain PowerShell metacharacters
func isSafePath(s string) bool {
	if len(s) == 0 {
		return false
	}
	for _, r := range s {
		switch r {
		case ';', '|', '&', '$', '`', '"', '\'', '\n', '\r':
			return false
		}
	}
	return true
}
