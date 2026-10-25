# SMS Gamer for Linux

Rust daemon, CLI, Varlink API, and Noctalia instrument panel for the SMS Gamer 2000 UPS over USB.

Tested with model `GAM2000BiBiW`, firmware `1.0`, using the Prolific PL2303GC bridge `067b:23a3` at 2400 baud, 8N1. Other models and firmware need verification. This is an independent project, unaffiliated with SMS or Legrand.

## Features

- Input and output voltage, load, frequency, charge, temperature, identity, ratings, and raw status flags.
- Battery runtime estimates from an editable battery profile.
- Persistent estimated output kWh for today, this month, and all time, with missing-time accounting.
- Persistent battery sessions with duration, charge changes, and interruption reasons.
- Local Varlink API for other applications. One process owns the serial port.
- Noctalia panel with graphs, history, settings, and reviewed UPS controls.
- Optional desktop warnings and orderly PC shutdown through logind.

Watts, kWh, and remaining runtime are estimates. The UPS reports load percentage, not measured watts. Unknown intervals are excluded from energy and battery-session durations.

## Build and run

Requires Linux, Rust 1.85 or newer, and a C linker. PC protection also needs systemd-logind and `busctl`. The Noctalia plugin targets Noctalia 5.3.0, Luau API 34.

```sh
cargo build --release --locked
./target/release/sms-gamer diagnose
./target/release/sms-gamer daemon
```

In another terminal:

```sh
./target/release/sms-gamer status
./target/release/sms-gamer watch
./target/release/sms-gamer battery-history
```

The daemon autodetects a single `067b:23a3` serial bridge. Use `daemon --device /dev/serial/by-id/...` if multiple adapters are attached. Read-only RPC clients never open the serial port.

For installation, see [the service and USB access guide](docs/daemon.md#user-service-and-usb-permissions) and [the Noctalia plugin guide](integrations/noctalia/sms-gamer/README.md).

## Operating limits

Automatic PC shutdown is disabled by default. Verify battery detection on the connected UPS and check the battery profile before enabling it. The default policy warns at 90 seconds and requests shutdown at 50 seconds remaining. [Protection timing](docs/protection.md) explains permissions, stale readings, restarts, and the limits of an estimated reserve.

UPS output controls match the vendor command format and are tested against pseudo-terminals. Their execution on physical hardware remains unverified. A successful write is not a device acknowledgement. Controls are explicit, model-confirmed where disruptive, and never automatically retried.

Intermittent USB no-reply stalls have been observed on the tested hardware. Empty monitoring replies are retried three times before reconnecting. This handles isolated missed replies; it does not resolve the underlying stalls. Lost readings stay marked as stale.

The supplied service runs under the logged-in user. Protection needs that user manager to remain active and cannot execute while the PC is suspended. Other users cannot access the default `0600` socket; applications running as the same user can monitor, configure, and control the UPS.

## API and diagnostics

The [Varlink interface](interfaces/io.smsups.Monitor.varlink) defines the API and generates the Rust bindings. Noctalia uses the Rust CLI, with one shared telemetry subscription.

- [Daemon, API, energy, and battery history](docs/daemon.md)
- [Runtime model and assumptions](docs/runtime.md)
- [PC protection](docs/protection.md)
- [Protocol layouts and evidence](docs/protocol.md)

Direct serial diagnostics require the daemon to be stopped:

```sh
sms-gamer monitor --count 5 --output captures/session.jsonl
sms-gamer query identity status ratings --output captures/queries.jsonl
sms-gamer decode captures/queries.jsonl
sms-gamer analyze captures/session.jsonl
```

Capture files are local diagnostics and are excluded from Git. Do not post unredacted paths, USB serial numbers, or operational history in issues.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo deny check advisories licenses sources
```

Integration tests need `dbus-daemon`, `dbus-test-tool`, `busctl`, and `kill`. They use Rust pseudo-terminal UPS simulators and private D-Bus services. They never open physical USB devices or call the host's power-off service.

With the Luau CLI on `PATH`, run the panel's pagination and lifecycle tests:

```sh
node tests/noctalia.cjs
```

CI tests stable Rust and Rust 1.85, checks dependencies, and compiles the Luau plugin. Build the interactive simulator with `cargo run --example ups_simulator -- --help`.

## License

[MIT](LICENSE). Vendor installers, decompiled vendor files, raw USB traces, and local workstation data are not distributed in this repository. Protocol fixtures contain representative wire replies without capture metadata.
