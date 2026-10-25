# PC shutdown protection

When enabled, the Rust daemon warns at 90 seconds of estimated battery time and requests an orderly PC shutdown at 50 seconds. Both thresholds are configurable in **Settings → PC protection**, using minutes:seconds. The daemon runs this policy independently of the Noctalia panel.

At the warning threshold, a critical desktop notification asks you to save your work. It stays until dismissed, with **Open UPS** and **Shut down PC** actions. The UPS bar text and icon turn red and show battery time plus the countdown to PC shutdown. A materially earlier deadline updates the notification; normal countdown ticks do not reopen a dismissed warning. Critical protection notifications remain active when ordinary plugin notifications are disabled.

At the shutdown threshold, the daemon calls `org.freedesktop.login1.Manager.PowerOff(false)` on the system bus. This requests the normal Linux shutdown sequence. It does not force power off, cut UPS outputs, or run a shell command supplied through configuration. Failed requests are reported and retried every five seconds. An accepted request is not repeated.

A sudden estimate drop can cross both thresholds in one reading. In that case the daemon requests shutdown immediately rather than delaying it to provide a full warning interval.

## Configure protection

The optional `Config.protection` object has these fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Enable warning and PC shutdown protection |
| `warning_seconds` | `90` | Warn at or below this estimate; 11 through 7200 seconds |
| `shutdown_seconds` | `50` | Request shutdown at or below this estimate; 10 through 3600 seconds |
| `battery_mode_verified` | `false` | Battery-mode detection has been checked on the connected UPS |

The warning threshold must be greater than the shutdown threshold. Older config files load with protection disabled. Older API clients that omit `protection` preserve the existing policy when replacing the rest of the config.

An unverified battery-mode setting blocks automatic power-off. It still permits a warning and an explicit **Shut down PC** action. Changing the configured USB path clears battery-mode verification. To configure through another app, use `GetConfig` and `SetConfig` with its revision, as described in [the API guide](daemon.md#monitoring-configuration).

A physical mains disconnect and reconnect changed the status from `0x39` to `0xb9` and back, confirming bit 7 on the tested model `GAM2000BiBiW`, firmware `1.0`. Verify detection on each connected UPS before marking it verified. Other flag meanings remain unverified. A self-test flag alone does not trigger PC shutdown.

## Countdown and recovery

Two consecutive battery samples establish an outage. One valid mains sample cancels an armed countdown and closes the warning before a shutdown has been requested. After logind accepts shutdown, returning mains does not undo it.

Protection caps effective polling and reply timeouts at two seconds. It uses a boot-time clock, so wall-clock adjustments do not change the countdown. It continues from the last battery estimate if USB readings stop. The notification and API mark those readings as stale.

Once warning has started, a higher runtime estimate does not postpone the deadline. This avoids extending shutdown because of a fluctuating charge value or a lower instantaneous load. The protection estimate can therefore be shorter than the general runtime prediction shown in Details.

The daemon checkpoints the deadline in `protection-state.json` beside its config file. A same-boot restart with the same battery profile restores that deadline conservatively. A restored daemon allows up to five seconds for a fresh sample to observe returned mains. Checkpoints from another boot or battery profile are discarded. Checkpoint failures are logged without disabling the current in-memory countdown.

`Snapshot.protection` publishes the phase, remaining battery seconds, seconds until shutdown, warning state, stale-reading state, errors, and whether the daemon is running in dry-run mode. Protection events appear in the event log.

## Verify without powering off the PC

`sms-gamer daemon --dry-run-protection` exercises the policy and desktop notification but simulates power-off. Use a separate config and socket with a pseudo-terminal UPS for development. Keep simulated battery readings separate from the physical UPS.

Rust tests cover warning and shutdown boundaries, mains recovery, stale readings, verification, retries, and restart recovery. The integration test runs the production D-Bus adapter against a private bus with a fake login1 service. It verifies one `PowerOff(false)` call and no UPS control writes. Check the current session's power-off permission without shutting down:

```sh
busctl --system call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager CanPowerOff
```

A reply of `yes` allows the non-interactive request used by the daemon. A reply of `challenge` requires authorization that this daemon does not request.

## Timing limits

The 50-second reserve is a target, not proof that the PC will be off before the battery empties. Runtime is modeled rather than measured, and shutdown can be delayed by applications, services, or storage. Increase the reserve if this PC needs more time. Measure shutdown duration on the target PC before relying on the configured reserve.

The user daemon must remain running. It cannot perform a shutdown while the PC is suspended or after the user manager has stopped. On resume, elapsed boot time is included. Closing Noctalia does not stop the daemon's shutdown policy, but a desktop notification server is required to display warnings.
