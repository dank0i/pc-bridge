# pc-bridge test kit

End-to-end integration harness for pc-bridge. It drives the **real binary** the
way Home Assistant does - over MQTT - and asserts how every command is resolved.

It is a standalone crate, intentionally outside the pc-bridge package, so its
dependencies never affect the production binary's size or `cargo-deny` policy.

## What it does

1. Writes a throwaway `userConfig.json` (test broker, no credentials) into a
   temp dir and spawns `pc-bridge --dry-run` pointed at it via
   `PC_BRIDGE_CONFIG_DIR`.
2. Waits for the binary to announce `availability = online`.
3. Publishes every command on its real MQTT topic with a production-shaped
   payload and asserts the `pc-bridge/test/executed/<device>` record the binary
   reports back.
4. Prints a PASS/FAIL table and exits non-zero on any failure (CI-gating).

**Dry-run** means destructive commands (`Sleep`, `Shutdown`, `Restart`) are
exercised safely: the binary resolves and reports what it *would* do, without
doing it. The reported action is a canonical, platform-neutral string (e.g.
`launch:steam:230410`, `native:sleep`), so the kit gives identical results
whether it runs against the Windows production binary, a Linux CI runner, or a
local build. Platform-specific shell expansion is covered by the launcher unit
tests in `src/commands/launcher.rs`.

## Commands covered

Power (`Wake`/`Lock`/`Sleep`/`Hibernate`/`Shutdown`/`Restart`/`Screensaver`),
`RefreshSteamGames`, `Launch` with every launcher shortcut
(`steam:`/`update:`/`epic:`/`exe:`/`lnk:`/`url:`/`close:`), media transport,
volume set/mute, `DiscordLeaveChannel`, a config-defined custom command, and
`notification`.

## Modes

- **dry-run** (default): drives every command via dry-run and asserts the
  canonical action. Validates routing for the whole surface. Runs anywhere,
  including macOS and CI.
- **live** (`--live`): drives a real `Launch` (no dry-run) and verifies the
  binary actually spawns the process, checked against the OS with `pgrep`. This
  exercises the real launch path (MQTT → executor → real spawn) that dry-run
  skips, and works on macOS where the bridge's own `/proc`-based detection does
  not. On a Linux/Windows host the same launch additionally drives the bridge's
  `runninggames` detection; the Steam-specific cold-boot path can only be
  exercised on the real PC.

## Running it

You need an MQTT broker and a built pc-bridge binary.

```sh
# 1. Build the binary (from the pc-bridge dir)
cargo build            # or: cargo build --release

# 2. Start a broker (any MQTT broker works; mosquitto shown)
mosquitto -c /path/to/anonymous.conf   # listener 1883 / allow_anonymous true

# 3. Run the kit (from the testkit dir)
cd testkit
PC_BRIDGE_BIN=../target/debug/pc-bridge cargo run            # dry-run: routing
PC_BRIDGE_BIN=../target/debug/pc-bridge cargo run -- --live  # live: real launch
```

Expected tails:

```
ALL 25 CASES PASSED          # dry-run
LIVE LAUNCH TEST PASSED      # live
```

### Environment overrides

| Variable          | Default            | Meaning                                  |
| ----------------- | ------------------ | ---------------------------------------- |
| `TESTKIT_BROKER`  | `127.0.0.1:1883`   | `host:port` of the MQTT broker           |
| `TESTKIT_DEVICE`  | `testkit-pc`       | device name used in topics               |
| `PC_BRIDGE_BIN`   | search `../target` | path to the pc-bridge binary             |

## CI

pc-bridge builds and runs on Linux, so the kit runs unmodified in CI. Provide a
broker with a service container:

```yaml
services:
  mosquitto:
    image: eclipse-mosquitto:2
    ports: ['1883:1883']
    # mount a config with: listener 1883 / allow_anonymous true
steps:
  - run: cargo build
  - run: cd testkit && PC_BRIDGE_BIN=../target/debug/pc-bridge cargo run
```

## Notes

- `pc-bridge --dry-run` kills other running `pc-bridge` instances on startup
  (its normal single-instance behavior). Don't run the kit on the same machine
  as a live production bridge.
- The `--live` test verifies the bridge really spawns a launched process. On
  macOS it uses a controllable marker process (the bridge's `/proc`-based game
  detection can't run there). The *intermittent cold-boot Steam race* - the
  original bug - can only be reproduced on the real Windows PC with Steam: point
  `--live` at a real Steam game and run it after a boot.
