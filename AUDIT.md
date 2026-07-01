# PC Bridge — Consolidated Review & Action Plan

Single source of truth for the two review passes (5 guided agents + 4 cold agents),
what's already been fixed, what's outstanding, what's deferred, and the README plan.
Tags: **[mine]** = introduced/touched in recent work · **[pre-existing]** = older code.
Status: ☑ fixed · ☐ outstanding · ⏸ deferred.

---

## Verdict (all 9 reviewers agreed)

Genuinely engineered codebase (~80–85%): secure-by-default flags, thorough injection
validators with tests, TLS with real cert verification, DPAPI credentials, a
bounds-checked HWiNFO parser, and a real integration test kit. No critical/RCE issue.
Debt concentrates in three seams:

1. **Drifted Windows/Linux pairs** (one side edited without syncing the other).
2. **Stale comments** describing designs the code abandoned.
3. **The same fact re-encoded in 5–6 places** (commands/sensors/features).

---

## ☑ Already fixed this session

- Granular power flags + `Logoff`/`MonitorOff`/`MonitorOn`/`CloseGame` commands.
- HA entity teardown (disabling a feature removes its entity).
- Library + custom sensor/command UI bound to real config.
- 5 cross-platform sensors: `session`, `audio_device`, `mic`, `webcam`, `now_playing`.
- Windows `is_safe_path` hardened to block `( ) < > { } * ? [ ] ~` (injection fix).
- Blocking reads moved off the single-threaded runtime: `gpu`, `network`, `disk`,
  `MonitorOff`/`On` → `spawn_blocking`.
- `now_playing` on a dedicated MTA thread (fixes STA/MTA hang, caches manager).
- `audio_device` event-driven on Windows (device-change generation) + friendly Linux name.
- Linux polish: `session` "unknown"→"unlocked", tab-delimited `playerctl` parse,
  `CloseGame` zombie reaped, warn-once on missing Linux tools.
- Dead `volume_level` sensor got a real producer.
- Library save drops incomplete rows.

---

## ☐ Tier 0 — Dangerous (data loss / security / crash) — fix first

| # | Tag | File:line | Issue |
|---|-----|-----------|-------|
| 0.1 | **[mine]** | `ui/app.rs:60,99` | `Config::load().unwrap_or_default()` — on load failure the UI shows blank defaults as if real; Save overwrites `userConfig.json` and an empty password **deletes the credential file**. Needs load-error banner + disabled Save. |
| 0.2 | **[mine]** | `ui/app.rs:33-43,99` | `tray_enabled`, `autostart`, `confirm_destructive`, `ha_token`, `transport`, and the 8 `group_on[]` masters are editable but never persisted. Masters grey out rows, so "disable Hardware → Save" leaves it fully on. |
| 0.3 | [pre-existing] | `commands/executor.rs:114`, `mqtt/mod.rs:97` | Native destructive commands (Shutdown/Restart/Sleep/Lock/Logoff) run on topic-name match with **no feature-flag check**; `clean_session=false` + never-unsubscribe keeps disabled commands remotely triggerable. Gate each native command on its flag. |
| 0.4 | [mine]+[pre] | `commands/launcher_linux.rs:92`, `config.rs:478` | `CloseGame` can kill unrelated processes: Linux `close:` uses `pkill -f` (whole command line), and `matching_game_processes` uses a loose prefix (`cs`→`csrss.exe`). Use `pkill -x` + boundary-anchored match. |
| 0.5 | [pre-existing] | `steam/appinfo.rs:327` | `next_kv` unbounded recursion on unknown type markers → stack-overflow crash on a malformed `appinfo.vdf`. One-line fix: `loop`/`continue`. |
| 0.6 | [pre-existing] | `config.rs:886`, `credential.rs:114` | Non-atomic `fs::write` (truncate-then-write) → crash mid-write corrupts config / empties the credential file; hot-reload sees the partial write. Use temp + `rename`. |
| 0.7 | [pre-existing] | `credential.rs:26` | `encrypt()` silently stores the MQTT password as **plaintext** if `CryptProtectData` fails (Windows has no `0600` fallback). Treat encrypt failure as fatal. |

---

## ☐ Tier 1 — Real bugs — fix soon

| # | Tag | File:line | Issue |
|---|-----|-----------|-------|
| 1.1 | [pre-existing] | `sensors/process_watcher.rs:77` | `add_process` double-counts a re-seen PID (reconcile + delayed WMI creation event) → a game reports "running" forever + slow leak. Make idempotent per PID. |
| 1.2 | [pre-existing] | `mqtt/mod.rs:235` | Command dispatch `.await`s onto a bounded 16-slot channel inside the poll loop; a backed-up consumer stops keepalives → broker drops the connection. Use `try_send`. |
| 1.3 | [pre-existing] | `commands/executor_linux.rs:280` | Linux command timeout only `warn!`s; the child runs forever and pins a blocking thread (Windows runs `taskkill`). Capture PID + kill. |
| 1.4 | [mine-touched] | `mqtt/mod.rs` vs `discovery.rs` | Subscribe list ≠ discovery-register list: `DiscordJoin`/`VolumeSet` subscribed but gated differently than registered. Derive both from one source. |
| 1.5 | [pre-existing] | `commands/executor.rs:301` | `expand_env_vars` runs **after** `is_safe_path`/`is_safe_url`, so `%VAR%` re-introduces `;`/`&`/`(...)` post-validation (defeats the `allow_raw_commands=false` gate; local reachability). Validate the expanded string. |
| 1.6 | [pre-existing] | `hwinfo.rs:406` | `as_slice` length comes from the untrusted shared-memory header, clamped only to 4 MiB, not the real mapping → can span unmapped pages (crash). Clamp to `VirtualQuery` region size. |
| 1.7 | [pre-existing] | `commands/executor.rs:70,293` | Steam-launch holds a rate-limit permit up to ~102s; 5 cold launches pin all 5 permits and silently drop every other command incl. Shutdown/Lock. Wait outside the permit. |
| 1.8 | [pre-existing] | `sensors/games.rs:142` | `publish_game` joins ids with `,` and names with `", "` then re-splits — any game name containing `", "` misaligns every id↔name pair. Carry `Vec<(id,name)>`. |
| 1.9 | [pre-existing] | `updater.rs:271` | Updater verifies a SHA-256 fetched from the **same** release (integrity only) then auto-executes — no signature. A compromised release = silent RCE. Verify a detached signature against a pinned key. |
| 1.10 | [pre-existing] | `credential.rs:114` | Unix credential file created at umask (0644) then chmod'd to 0600 → world-readable TOCTOU window (plaintext on non-Windows). Create with `mode(0o600)`. |
| 1.11 | [pre-existing] | `steam/appinfo.rs:166` | `get_game_info` double-counts the 8-byte header (`offset + 8 + 44`) and reads the full `size` from a shifted offset → over-reads into the next entry; the **last game is silently dropped**. |
| 1.12 | [pre-existing] | `mqtt/mod.rs:243` | Discovery is not re-published on reconnect — if the broker restarts and loses its retained store, entities vanish from HA until the agent process restarts. |
| 1.13 | [pre-existing] | `mqtt/mod.rs:312` | Up-to-30s reconnect backoff `sleep().await` isn't a `select!` branch, so `biased` shutdown can't preempt it. |

---

## ☐ Tier 2 — Parity / correctness

| # | Tag | File:line | Issue |
|---|-----|-----------|-------|
| 2.1 | [pre-existing] | `system.rs`, `audio.rs`, `executor_linux.rs` | Media keys + `active_window` on Linux are X11-only (`xdotool`/`xprop`) → silent no-op on Wayland (the modern default). Route media keys via `playerctl`; warn on Wayland. |
| 2.2 | [pre-existing] | `idle_linux.rs:95`, `executor_linux.rs:156` | Linux `Sleep`/`Hibernate`/monitor/screensaver/xdotool run inline on the runtime (siblings use `spawn_blocking`). |
| 2.3 | [mine-touched] | `power/display_linux.rs` | Fire-and-forget `.spawn()` leaks zombies (should `.status()`); `monitor_off` rides this. |
| 2.4 | [pre-existing] | `audio.rs:168` | `DEVICE_CHANGED` is consumed once (`swap`) but `CACHED_ENDPOINT` is thread-local → other worker threads keep a stale endpoint after a device change. Use a per-thread generation compare. |
| 2.5 | [pre-existing] | `mqtt/mod.rs:88` | `ws://`/`wss://` are parsed and unit-tested but WebSocket transport is never wired — `wss://` connects as raw TLS. Wire it or reject the scheme. |
| 2.6 | [pre-existing] | `updater.rs` | `.update` leftover cleanup filename mismatch (leaks the file); beta channel can't advance between prereleases (`4.1.0-beta.1`↔`.2` compare equal); `exit(0)` from a bg task skips graceful shutdown and drops CLI args on restart. |
| 2.7 | [pre-existing] | `setup.rs:198` | Wizard saves configs it never `validate()`s → a scheme-less broker or `my-pc` name is saved but rejected on next `load()`, leaving a dead state. |
| 2.8 | [pre-existing] | `config.rs:892` | `device_name` not sanitized for MQTT metacharacters (`# + /`) → broken/wildcarded subscriptions. |
| 2.9 | [pre-existing] | `README` vs `config.rs` | README documents feature keys the code ignores (`game_detection`, `power_events`, …); no `deny_unknown_fields`, so hand-edits silently set nothing. (Addressed by README→UI plan below + `deny_unknown_fields`.) |
| 2.10 | [pre-existing] | `sensors/steam.rs:409` | `is_updating` bitmask catch-all (`flags != 0 && != FULLY_INSTALLED`) → benign bits report "updating". |
| 2.11 | [pre-existing] | `sensors/custom.rs` | Errors encoded into the sensor value string (`"error: …"`); HA can't tell failure from a value. Return `Result`, publish unavailable. |
| 2.12 | [pre-existing] | `notification.rs:117` | `notify-send` non-zero exit treated as success → gdbus fallback never triggers on failure. |
| 2.13 | [pre-existing] | `sensors/custom.rs`, `sensors/idle.rs:47`, `network.rs:61`, `gpu.rs:69` | Misc: Win/Linux "process exists" match differs; Windows `idle` has no reconnect handling (+ `idle_linux` missing `Skip`); network rate divides by nominal not elapsed time; `gpu.rs` stale comment + PDH wildcard read (likely under-reports) + dead `has_first_sample` branch. |
| 2.14 | [pre-existing] | `ui/app.rs:1273`, `setup.rs:99` | UI "Test connection" sets `connected=true` without testing; `print_header` underflows/panics if a title >42 chars. No password zeroize; intervals/`update_channel` unvalidated. |

---

## ☐ Tier 3 — Structural root cause (biggest lever, separate refactor)

- **N-way duplication of the command/sensor/entity ↔ feature mapping** across
  `register_discovery`, `feature_entities`, `build_subscribe_topics`, `topics.rs`,
  the three executor/dry-run routing copies, and the UI catalog. This is *why* most
  of Tier 1's drift bugs exist. → one data-driven registry (`enum Command`/`Entity`
  with feature-selector + routing) that all consume.
- **God functions**: `execute_command` (~264 lines), `MqttClient::new` (~280),
  `register_discovery` (~650), `HwInfoSensor::run`.
- **Duplication**: `games.rs` ↔ `games_linux.rs` (~90%, already drifted); the
  safety-critical launcher validators duplicated per platform (a fix on one can miss
  the other); 8 copy-paste sensor run-loops; 3 Win32 message-pump copies; 4 ascii-icase
  helpers. Stringly-typed commands. `executor.rs` tests are `#[cfg(windows)]` so they
  **never run in CI**, and one asserts the opposite of the code (`parse_vk_code("1")`).

---

## ⏸ Deferred (with rationale)

- **`hwinfo.rs` offload** — `Send` so viable, but the loop is entangled, it's the
  validated gaming-temp sensor I can't runtime-test, and the real cost is heap churn
  (sub-ms stall). Fix when verifiable: move the `Send` client in/out of `spawn_blocking`
  around just `snapshot()`, intern the stable sensor names.
- **`flag_get`/`flag_set` macro/table** — footgun but tested, not a live bug (subsumed by Tier 3).
- **`model.rs` mockup data** — hardcoded `sensor.dank0i_pc_*` entity ids + editable-but-dead
  example values on the real settings screen. Cosmetic; fold into the README→UI work.
- **Wayland media keys** — larger (route via `playerctl`); see 2.1.

---

## README → UI migration plan (awaiting go-ahead)

**Move into the UI** (as detail panels / field hints — the feature rows already have a
details expander): per-feature descriptions/defaults/entity ids, HWiNFO setup steps,
custom sensor/command type tables + security model, Launch payload formats, Linux
optional-tool requirements (ideally showing detected vs missing), Discord keybind/join help.

**Keep in README**: intro/badges/overview/platforms/download, the HA-side automation YAML
examples (notifications, Discord scripts), run-as-service, building from source, performance,
MQTT topic reference.

**Cut as redundant** once the UI covers it: the full `userConfig.json` schema dump, the
exhaustive entity/button lists, and the feature-flag tables (also fixes 2.9's stale keys).

---

## Suggested sequence

1. **Tier 0** (small, high-impact; 0.1/0.2 are mine) + `model.rs` mockup fix + README→UI (same files).
2. **Tier 1**.
3. **Tier 2** as capacity allows.
4. **Tier 3** as a dedicated, heavily-tested refactor (prevents whole classes of drift).
