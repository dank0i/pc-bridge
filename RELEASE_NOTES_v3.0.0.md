# pc-bridge v3.0.0

Full Linux support, a native settings window, live sensor control, and a large round of reliability and security fixes.

## New features

### Linux

No external helper tools required anymore. The bridge talks to the desktop directly:

- **X11**: idle time, active window, display power (DPMS) query/control, and display wake are all done through a bundled pure-Rust X11 client. External tools are now only a fallback.
- **GNOME / KDE Wayland**: idle time via D-Bus (Mutter / KDE ScreenSaver `GetIdletime`).
- **wlroots Wayland** (Sway, Hyprland, gaming stacks): idle via `ext-idle-notify`, active window via `foreign-toplevel`, and monitor power via `output-power`.
- **Sleep/wake**: a systemd sleep delay-inhibitor lets the agent publish "sleeping" over a fresh connection before the NIC drops, so externally-initiated suspend (power button, lid, auto-sleep) still reports correctly.
- Linux sensor pass: disk uses available (non-root) space, CPU includes iowait/irq/softirq/steal, battery reports AC-connected to match Windows.

### Native settings window

A real settings app (run with `--ui`) instead of hand-editing JSON:

- Edit the broker, device name, games, feature toggles, and per-sensor poll intervals.
- First run opens this window automatically (the terminal wizard is now only a headless fallback).
- Group master switches are real bulk toggles; duplicate game rows are rejected on save; unsupported features on the current session are greyed out.

### Live sensor control (no restart)

A runtime supervisor starts and stops sensor tasks as you flip their feature flags in the UI. Enabling or disabling a sensor now takes effect immediately, and the matching Home Assistant entity is registered or torn down to match, no restart, no orphaned entities.

### Independent poll intervals

CPU, memory, GPU, network, and disk each have their own poll interval now, instead of sharing one. Existing configs migrate automatically.

### Command permissions

Two new Security toggles control how far launch/close commands can reach:

- **Allow launching any game** (default on): launch commands can start any owned Steam/Epic title. Off restricts them to your configured games.
- **Allow closing any process** (default off): close/kill commands only target your configured games unless you turn this on.

### Signed auto-updates

Update binaries are verified with a minisign signature before install, and a signed `{version, sha256}` manifest provides anti-rollback (a compromised release host can't push a downgrade or an unsigned build).

## Bug fixes

### Security

- Closed a scheme-parsing bypass where a Unicode look-alike character (U+212A KELVIN SIGN, which lowercases to `k`) could slip a `kill:` or `lnk:` payload past the command gates while still executing. The gates now normalize exactly like the resolver.
- Closed an earlier Launch-gate bypass via letter case, whitespace, and environment-variables. Arbitrary `exe:`/`lnk:`/`url:` launches are restricted to configured games unless raw commands are explicitly enabled.
- `close:`/`kill:` payloads now require the Close feature to be enabled, so they can't be smuggled in through another command's topic while Close is off.
- `DiscordJoin` payloads are guarded on both Windows and Linux so they can only carry a Discord deep link.

### Reliability and lifecycle

- Fixed an MQTT event-loop deadlock that could occur with many custom topics (reconnect resubscribe now runs off the poll loop), and a startup hang when the broker wasn't reachable yet at boot.
- Home Assistant discovery is re-published on reconnect so a broker restart can't orphan entities; entity teardown (at startup and when a feature is turned off) also clears retained state/attributes and unsubscribes removed command topics.
- The runtime supervisor aborts a stuck task on stop instead of leaking it, checks liveness, and drains all tasks in parallel on shutdown within the time budget.
- OS threads and child processes are now reaped on runtime disable, not just on exit: Linux `gdbus` and `xprop` children are killed and waited (no zombies), Windows event hooks are unhooked, and the settings-window / media-thread / power pumps stop cleanly even if their task is aborted.

### Sensors

- GPU usage on Windows reads the wildcard performance counter correctly (array API, summed per engine node then maxed across adapters), so a second adapter such as a Parsec virtual display no longer corrupts the number.
- HWiNFO: the mapped shared-memory view is bounded by the real region to prevent a crash on a corrupt header, goes offline when its data stops advancing (app closed), and recovers from a parse panic.
- Steam: full appinfo v29 support with correct string-table and nested-block handling, VDF path unescaping, and a fallback to manifest scanning.
- Windows default-audio-device changes are detected again (COM callback runs in the correct apartment), so volume/mute control follows a device switch.
- Sensors report "unavailable" on a read failure instead of a misleading 0 or 100.
- Blocking work (toast notifications, Sleep/Hibernate, Steam library discovery, HWiNFO parse, media keys, display wake) is offloaded off the single-threaded runtime.

### Config and persistence

- All config and credential writes are atomic (unique temp + fsync + rename), including the one-time inline-password migration.
- A Steam-library refresh re-loads from disk before merging, so it no longer clobbers a manual edit.
- Credential loading preserves passwords with legitimate leading/trailing spaces on Linux while still tolerating a trailing newline.

### Interface

- The version shown in the settings window is now read from the build instead of being hard-coded.
- Native (HACS) integration is shown as unsupported and cannot be selected (in the works).

## Dependencies

New dependencies added this release (all pure-Rust or already in the tree; the headless option does not initialize the UI ones):

- `minisign-verify` 0.2, update signature verification.
- `eframe` 0.29 and `rfd` 0.15, the native settings window and its file picker.
- `x11rb` 0.13, bundled X11 client (idle, active window, DPMS, XTEST).
- `zbus` 5, D-Bus client for Wayland idle on GNOME/KDE.
- `wayland-client` 0.31, `wayland-protocols-wlr` 0.3, `wayland-protocols` 0.32, wlroots active window, monitor power, and `ext-idle-notify` idle.
