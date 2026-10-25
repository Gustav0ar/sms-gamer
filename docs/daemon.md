# Rust daemon and Varlink API

The daemon is a userspace driver on top of Linux's `pl2303` serial driver. One polling thread owns the port. Other applications read and configure it through a local Unix socket. Noctalia uses the Rust client, so the shell does not implement the serial or Varlink protocol.

A panic in a monitoring, protection, or persistence worker stops the RPC server and exits the daemon with an error. The supplied systemd unit restarts failed processes instead of leaving a service running with a dead worker.

## Run and inspect

```sh
cargo build --release --locked
./target/release/sms-gamer daemon
```

The default socket is `$XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor`, mode `0600`. Only applications running as the same user can connect. Settings default to `$XDG_CONFIG_HOME/sms-gamer/config.json`, or `~/.config/sms-gamer/config.json`. Use `daemon --socket /absolute/path --config /path/config.json --device /dev/serial/by-id/...` for an isolated instance or a particular serial port.

From another terminal:

```sh
./target/release/sms-gamer status
./target/release/sms-gamer watch
./target/release/sms-gamer history --limit 300
./target/release/sms-gamer events --limit 100
./target/release/sms-gamer config get
```

`watch` emits JSONL events. A `snapshot` event contains the full snapshot; an `unavailable` event reports a connection failure to the daemon. The client reconnects after a daemon restart. The daemon independently reconnects after USB disconnects. A disconnected snapshot retains the last successful reading; clients must check `connection` and the sample's timestamp before presenting it as live.

An empty monitoring reply is retried three times on the same serial connection before reporting a disconnect, for four attempts total. This applies to the identity and ratings handshake as well as periodic status reads. It avoids disconnect/reconnect alerts for isolated missed replies and preserves discharge tracking. Each attempt uses `request_timeout_ms`, capped at two seconds for status reads when PC protection is enabled. Serial errors and invalid or partial frames still disconnect immediately. UPS control commands are never retried.

Telemetry history, extrema, events, and command receipts live in memory. Configuration persists atomically. Telemetry history is bounded by `history_samples`; events and completed command receipts retain at most 256 entries each. Statistics cover the daemon session and reset when the configured device or detected identity changes. They do not represent lifetime UPS statistics.

`Snapshot.energy` contains persistent estimated output-energy counters for today, the current calendar month, and all time since tracking began. Each period reports `estimated_kwh`, `observed_ms`, and `missing_ms`. The dates follow the host's local timezone. `tracked_since` identifies partial days and months after initial installation. These counters cover equipment connected to the UPS and exclude its own mains-side losses and battery charging.

The daemon integrates estimated watts between valid consecutive readings using the trapezoidal rule. A missed reply, USB disconnect, daemon downtime, suspend-sized delay, clock discontinuity, device change, or change to rated output watts breaks continuity. Unknown intervals contribute missing time and zero estimated energy. The first fresh reading after a gap establishes a new baseline. Previous totals are preserved when settings change.

Counters are stored in `energy.json` beside `config.json`, with mode `0600`, atomic replacement, an exclusive writer lock, an initial checkpoint, checkpoints every 30 seconds, and a final save on graceful exit. An abrupt stop can lose accounting since the last successful checkpoint; the next run marks that time as missing instead of reconstructing usage. The file retains 400 local daily buckets and the lifetime aggregate. A corrupt or unreadable counter file remains untouched, and `Snapshot.energy.last_error` reports the problem while UPS monitoring and PC protection continue. An older daemon omits the optional energy field.

To inspect energy through the CLI:

```sh
sms-gamer status | jq '.energy'
```

## Persistent battery sessions

`Snapshot.battery_history` reports the active battery session, retained session count, history revision, and storage errors. `GetBatteryHistory(limit, before_id)` returns records newest first. The default limit is 100, with a maximum of 1,000. Pass the oldest returned ID as `before_id` to page backwards. The CLI exposes the same method:

```sh
sms-gamer battery-history --limit 100
sms-gamer battery-history --limit 100 --before-id 42
```

Each session records its first and last battery timestamps, observed duration in milliseconds, first and last reported charge, detection verification, and ending reason. The daemon uses battery flag bit 7, which was physically verified on this UPS. A self-test flag alone does not create a battery session. `mode_verified` records whether the battery-mode setting remained verified throughout the session. `start_observed` indicates that a fresh mains reading preceded the first battery reading. A session discovered while monitoring starts or resumes can have begun earlier.

A fresh mains reading closes the session with `mains_restored`. Duration runs from the first battery reading to the first mains reading, within polling precision. USB errors, retried empty replies, expired readings, suspend-sized gaps, device changes, clock discontinuities, and daemon stops interrupt the session at its last battery reading. Unknown time is excluded. A subsequent battery reading starts another session. Interrupted records are fragments of observed battery operation; they do not prove that mains returned, the battery depleted, or a separate outage occurred.

Sessions persist in `battery-history.json` beside `config.json`. The file retains the latest 10,000 sessions, uses an exclusive writer lock and atomic replacement with mode `0600`, and remains separate from the energy counters. A worker saves session transitions within approximately one second, active progress every five seconds, and a final checkpoint on orderly daemon exit. Abrupt termination can lose updates since the last checkpoint. A saved active session is marked `monitoring_restarted` on the next launch; downtime is never added to battery duration. Disk writes run outside the telemetry and protection lock.

Invalid, unreadable, or locked storage leaves history unavailable with an explicit error while monitoring, energy accounting, and PC protection continue. Saved history remains untouched. Recording starts when the feature first runs; earlier battery sessions cannot be reconstructed from the energy counters.

## Call from any Varlink client

The authoritative interface is [io.smsups.Monitor.varlink](../interfaces/io.smsups.Monitor.varlink). The build generates Rust server bindings, client bindings, and types from it. Standard `org.varlink.service` introspection is available.

```sh
varlinkctl info "unix:$XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor"
varlinkctl introspect "unix:$XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor" io.smsups.Monitor
varlinkctl call "unix:$XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor" io.smsups.Monitor.GetSnapshot '{}'
varlinkctl --more call "unix:$XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor" io.smsups.Monitor.Watch '{}'
```

`Watch` with `more` streams updates and a heartbeat at least every five seconds. Without `more`, it returns one snapshot. RPC clients never cause additional serial polls. A slow client has a two-second write deadline, and there are at most 32 concurrent connections. Requests are limited to 64 KiB. Protocol upgrade is unsupported.

## Monitoring configuration

`GetConfig` returns the config and its revision. `SetConfig` accepts a complete replacement and optional `expected_revision`; a stale revision returns `ConfigConflict`. A successful update persists before publishing the new settings. An unchanged update keeps the same revision.

| Setting | Default | Accepted values |
| --- | --- | --- |
| `device` | omitted, autodetect | Absolute serial path, or omit to autodetect |
| `poll_interval_ms` | 2000 | 1000 through 60000 |
| `request_timeout_ms` | 2000 | 100 through 10000 |
| `reconnect_interval_ms` | 3000 | 1000 through 60000 |
| `history_samples` | 1800 | 1 through 10000 |
| `low_charge_percent` | 33 | 0 through 100 |
| `high_load_percent` | 90 | 0 through 200 |
| `high_temperature_c` | 60 | 0 through 150 |

The required `runtime` object configures the battery model:

| Setting | Default | Accepted values |
| --- | --- | --- |
| `rated_output_w` | 1400 | 1 through 100000 |
| `battery_capacity_ah` | 9 | 0.1 through 2000 |
| `battery_health_percent` | 100 | 0 through 100 |
| `inverter_efficiency_percent` | 85 | 1 through 100 |
| `idle_power_w` | 15 | 1 through 1000 |
| `peukert_exponent` | 1.25 | 1 through 2 |
| `capacity_rating_hours` | 20 | 1 through 100 |

Runtime settings take effect on the next sample. Changing the profile resets its discharge integration. See [the runtime model](runtime.md) for the assumptions and equations. The Noctalia settings panel exposes these fields.

Zero disables a warning threshold. Low-charge warnings compare device-reported charge with the threshold, including while charging. They do not depend on the unverified battery-mode flag. High load means above the threshold; high temperature and low charge include the threshold itself. These are software warnings, not independent UPS alarm measurements.

To edit through a file, save the `config` object returned by `config get`, then use:

```sh
./target/release/sms-gamer config set --file /path/settings.json --revision 1
```

The daemon emits all eight vendor flag names along with `flags_verified: false` and the raw status byte. That flag covers the entire mapping. Battery mode was separately verified on this unit and can be recorded in `Config.protection.battery_mode_verified`. Other vendor flags remain unverified.

The optional `protection` object configures the [PC warning and shutdown policy](protection.md). Defaults are disabled, with warning at 90 seconds and orderly host shutdown at 50 seconds of estimated battery time. `Snapshot.protection` reports its current state. The policy runs in the daemon and can continue a confirmed battery countdown after USB disconnects.

## UPS controls

Controls are explicit RPC methods. The polling thread serializes their writes between monitoring reads. The implementation matches the supplied PowerView 5.01.07 command builder. It has been tested against a Rust UPS simulator, not by executing controls on the physical Gamer.

| Method | Wire command | Parameters |
| --- | --- | --- |
| `ToggleBeeper` | `M` | Toggle, not set-to-value |
| `StartSelfTest` | `T` | Vendor ten-second preset, timer value 16 |
| `StartTimedBatteryTest` | `T` | 1 through 600 minutes, encoded as minutes × 100 |
| `StartDischargeTest` | `L` | Test until low battery |
| `CancelBatteryTest` | `D` | Cancel test |
| `ScheduleShutdown` | `S` | Delay 1 through 600 seconds, no restore |
| `ScheduleShutdown` | `R` | Delay in seconds, restore after 1 through 10080 minutes |
| `CancelShutdown` | `C` | Cancel pending shutdown or restore |

Timed/discharge tests and shutdown require `confirmation` equal to the connected model. Controls return `DeviceUnavailable` while disconnected. They return a `CommandReceipt` after the serial write, with `acknowledged: false`. The vendor defines no acknowledgement frame. A receipt does not establish that the UPS accepted or executed the command, and no pending shutdown state is inferred from it.

Every control requires a caller-generated `request_id`. Reusing an ID with the same action and parameters returns the original receipt, including after a partial write. Reusing it for a different command returns `RequestIdConflict`. IDs deduplicate only within this daemon session while retained in its 256-entry cache. The CLI never retries controls automatically. After an uncertain outcome, inspect the UPS before submitting a new ID. Scheduled operations already sent to the UPS may outlive the daemon.

These UPS controls operate its outputs. [PC protection](protection.md) separately requests an orderly Linux shutdown through logind, without sending an output shutdown command to the UPS.

## User service and USB permissions

The project includes a [systemd user unit](../packaging/systemd/sms-gamer.service) and a [udev rule template](../packaging/udev/70-sms-gamer.rules). Building and testing do not install or enable them.

Build and install the binary and service:

```sh
cargo build --release --locked
install -Dm755 target/release/sms-gamer ~/.local/bin/sms-gamer.new
mv ~/.local/bin/sms-gamer.new ~/.local/bin/sms-gamer
install -Dm644 packaging/systemd/sms-gamer.service ~/.config/systemd/user/sms-gamer.service
```

Check the UPS port's properties to find `ID_SERIAL_SHORT`:

```sh
udevadm info --query=property --name=/dev/ttyUSB0
```

Replace `YOUR_UPS_SERIAL` below with that value. Keep the generated rule local; USB IDs alone also match unrelated serial adapters.

```sh
mkdir -p .cache/udev
sed 's/REPLACE_WITH_DEVICE_SERIAL/YOUR_UPS_SERIAL/' packaging/udev/70-sms-gamer.rules > .cache/udev/70-sms-gamer.rules
sudo install -m644 .cache/udev/70-sms-gamer.rules /etc/udev/rules.d/70-sms-gamer.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=tty --action=add
systemctl --user daemon-reload
systemctl --user enable --now sms-gamer.service
sms-gamer status
```

The rule grants the active local session access through `uaccess`. Install the rule before starting the daemon. Ensure `~/.local/bin` is on `PATH` for CLI and Noctalia access. If no serial port appears, run `sms-gamer diagnose` and check the matching kernel's `pl2303` module.

For upgrades, back up `~/.config/sms-gamer` while mains power is available. Replace the binary using the same `.new` and `mv` commands, then run `systemctl --user restart sms-gamer.service`. Keep the data directory to retain settings, energy counters, and battery sessions. Reload the Noctalia plugin so its subscriber uses the new binary.

This service runs while the user manager runs. It is not a system-wide UPS protection service. Automatic shutdown stays disabled until explicitly configured and verified.

## Noctalia integration

The [Noctalia plugin](../integrations/noctalia/sms-gamer/plugin.toml) targets Noctalia 5.3.0 Luau API 34. Its headless service subscribes once using `sms-gamer watch` and publishes `snapshot` and `transport` through `noctalia.state`. The shared `rpc.luau` module exposes telemetry history, battery history, events, configuration, and explicit controls using argument arrays.

The instrument panel includes live metrics, graphs, history, events, settings, UPS controls, raw flags, and session extrema. On battery, the bar and overview show estimated time remaining. Settings use revision checks to avoid overwriting changes from another app. Disruptive controls require a review step, and timed tests, discharge tests, and shutdown require the model name as confirmation.

The plugin's Energy tab shows daily, monthly, and lifetime estimated output kWh, estimated watts, and observed and missing time for each period. History has separate Readings and Battery sessions views. The persistent session log includes expandable details, a live observed-duration readout, and cursor-based loading of older records. The plugin was rendered and exercised in an isolated native Noctalia session connected to Rust pseudo-terminal UPS simulators. Follow [the plugin installation guide](../integrations/noctalia/sms-gamer/README.md) to enable it. When upgrading the daemon, reload the plugin too so its `watch` subscriber uses the new executable and forwards the energy and battery-history fields.

## Verification

```sh
cargo test --locked
cargo clippy --all-targets -- -D warnings
varlinkctl validate-idl interfaces/io.smsups.Monitor.varlink
noctalia plugins lint integrations/noctalia/sms-gamer
```

Integration tests run the compiled daemon against Rust pseudo-terminal UPS simulators. They cover streamed updates, config conflicts and persistence, exact control frames and duplicate IDs, stale telemetry, unplug/replug recovery, graceful exit, socket cleanup, and serial-setting restoration. They never open `/dev/ttyUSB0`.
