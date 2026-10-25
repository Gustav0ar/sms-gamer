# Install the SMS Gamer Noctalia plugin

The plugin targets Noctalia 5.3.0 with Luau API 34. First [install and start the Rust daemon](../../../docs/daemon.md#user-service-and-usb-permissions). Ensure `sms-gamer` is on the shell's PATH and `sms-gamer status` returns a snapshot.

From the repository root, copy the plugin into Noctalia's data directory:

```sh
mkdir -p ~/.local/share/noctalia/plugins/sms-gamer
cp integrations/noctalia/sms-gamer/*.luau integrations/noctalia/sms-gamer/plugin.toml ~/.local/share/noctalia/plugins/sms-gamer/
cp -R integrations/noctalia/sms-gamer/translations ~/.local/share/noctalia/plugins/sms-gamer/
```

For a custom `XDG_DATA_HOME`, use `$XDG_DATA_HOME/noctalia/plugins/sms-gamer` instead.

Merge these entries into `~/.config/noctalia/config.toml`, preserving existing enabled plugins and widgets:

```toml
[plugins]
enabled = ["smsups/sms-gamer"]

[widget.ups]
type = "smsups/sms-gamer:ups"

[bar.main]
end = ["ups", "clock"]
```

Noctalia reloads its configuration. Left-click the UPS widget to open the instrument panel. Right-click it to open Settings. On mains power, the widget shows charge and load. On battery, it shows charge and estimated minutes remaining.

When upgrading a previous local installation, disable its old plugin ID and remove its widget entry before enabling `smsups/sms-gamer`. Keep the UPS daemon's data directory. The public plugin ID is `smsups/sms-gamer`; its panel is `smsups/sms-gamer:panel`.

The panel provides these views:

- **Overview** shows six live measurements, recent graphs, and remaining runtime during battery operation.
- **Energy** shows estimated output kWh for today, this month, and the lifetime of the counters, plus current estimated watts and recording coverage. Counters survive daemon restarts. Missing readings and downtime add missing time and no energy. Tracking starts when the energy-enabled daemon first runs; totals do not include earlier usage.
- **History** switches between **Readings** and persistent **Battery sessions**. Sessions show start time, observed duration, charge change, and ending reason. Click the time or chevron for exact timestamps and verification details. **Load older sessions** pages through saved records. **Events** shows connections, flag changes, warnings, configuration changes, and command writes.
- **Settings** edits monitoring, alert thresholds, PC protection, the battery profile, and the USB path. Apply saves through Varlink with a revision check. Background service buttons require the supplied systemd user unit.
- **Controls** provides beeper, battery tests, output shutdown, restore, and cancellation. The review screen requires the model name for disruptive operations. A receipt means bytes were written; the UPS does not acknowledge commands.
- **Details** shows identity, nominal ratings, raw flags, session extrema, and runtime model outputs.

Ordinary notifications can be disabled in Noctalia's plugin settings. Critical PC-protection warnings come from the daemon and remain active separately. At the warning threshold, the bar text turns red and shows a countdown. **Open UPS** and **Shut down PC** notification actions operate the panel and the Linux host. See [PC protection](../../../docs/protection.md) for verification, thresholds, and timing limits. Disconnects retain the last successful sample and label it as stale. Telemetry readings and events reset when the daemon stops. Battery sessions and energy counters persist.

Battery history starts when its recording feature first runs and keeps the latest 10,000 sessions. A missed reply or stopped monitor interrupts a session at the last battery reading. A later battery reading starts a new session; records can be fragments of the same outage. The panel shows reported durations and never adds unobserved time. See [battery-session storage and timing](../../../docs/daemon.md#persistent-battery-sessions) for details.

See [the runtime model](../../../docs/runtime.md) for battery assumptions and [the control reference](../../../docs/daemon.md#ups-controls) for protocol limits. Physical UPS control execution remains unverified. All native UI control checks used a Rust pseudo-terminal simulator.

To validate plugin files without starting a shell:

```sh
noctalia plugins lint integrations/noctalia/sms-gamer
```
