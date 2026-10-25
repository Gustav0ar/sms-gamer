use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use clap::Args;

use crate::{
    api::{self, *},
    battery_history, config,
    control::Command,
    energy,
    host::{self, Host},
    protection, protocol, rpc, runtime,
    serial::{self, Baud, Serial},
};

#[derive(Args)]
pub struct DaemonArgs {
    /// Unix socket; defaults to $XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Persistent JSON config; defaults to $XDG_CONFIG_HOME/sms-gamer/config.json.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Override the initial configured serial path without persisting it.
    #[arg(long)]
    pub device: Option<PathBuf>,
    /// Exercise protection and notifications without requesting a PC power-off.
    #[arg(long)]
    pub dry_run_protection: bool,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Clone)]
enum Outcome {
    Written(CommandReceipt),
    Failed {
        reason: String,
        may_have_been_sent: bool,
    },
}

struct ControlEntry {
    id: String,
    command: Command,
    device: String,
    outcome: Option<Outcome>,
}

struct State {
    snapshot: Snapshot,
    history: VecDeque<Sample>,
    events: VecDeque<Event>,
    controls: VecDeque<ControlEntry>,
    event_id: i64,
    sample_id: i64,
    runtime_tracker: runtime::Tracker,
    protection: protection::Engine,
    sample_at_ms: Option<u64>,
    energy: energy::Meter,
    battery_history: battery_history::History,
}

impl State {
    fn event(&mut self, kind: EventKind, message: impl Into<String>) {
        self.event_id += 1;
        self.events.push_back(Event {
            id: self.event_id,
            time: now(),
            time_unix_ms: Utc::now().timestamp_millis(),
            kind,
            message: message.into(),
        });
        while self.events.len() > 256 {
            self.events.pop_front();
        }
        self.snapshot.revision += 1;
    }

    fn connection(
        &mut self,
        connection: ConnectionState,
        device: Option<String>,
        error: Option<String>,
    ) {
        if self.snapshot.connection != connection
            || self.snapshot.device != device
            || self.snapshot.last_error != error
        {
            self.event(
                EventKind::connection,
                error
                    .clone()
                    .unwrap_or_else(|| format!("Connection {connection:?}")),
            );
            self.snapshot.connection = connection;
            self.snapshot.device = device;
            self.snapshot.last_error = error;
        }
        if self.snapshot.connection != ConnectionState::online {
            self.energy.break_continuity();
            self.battery_history.interrupt(
                if self.snapshot.connection == ConnectionState::stopped {
                    BatterySessionEndReason::daemon_stopped
                } else {
                    BatterySessionEndReason::telemetry_lost
                },
            );
            self.runtime_tracker.reset();
            for entry in &mut self.controls {
                if entry.outcome.is_none() {
                    entry.outcome = Some(Outcome::Failed {
                        reason: "Connection changed before the command was written".into(),
                        may_have_been_sent: false,
                    });
                }
            }
        }
    }

    fn sample(
        &mut self,
        identity: &protocol::Identity,
        ratings: &protocol::Ratings,
        status: protocol::Status,
    ) {
        if self.snapshot.sample.as_ref().is_some_and(|s| {
            s.identity.model != identity.model || s.identity.firmware != identity.firmware
        }) {
            self.energy.break_continuity();
            self.battery_history
                .interrupt(BatterySessionEndReason::device_changed);
            self.snapshot.statistics = None;
            self.history.clear();
            self.event(
                EventKind::device,
                "UPS identity changed; history and statistics reset",
            );
        }
        self.sample_id += 1;
        let topology = match status.frame_type {
            '=' => Topology::line_interactive,
            '>' => Topology::online_interactive,
            '<' => Topology::online,
            _ => unreachable!("validated frame header"),
        };
        let bits = status.status_byte;
        let vendor_flags = VendorFlags {
            beeper_enabled: bits & 1 != 0,
            shutdown_active: bits & 2 != 0,
            self_test_active: bits & 4 != 0,
            ups_ok: bits & 8 != 0,
            boost: bits & 16 != 0,
            bypass: bits & 32 != 0,
            low_battery: bits & 64 != 0,
            on_battery: bits & 128 != 0,
        };
        let telemetry = Telemetry {
            last_input_voltage_v: status.last_input_voltage_v,
            input_voltage_v: status.input_voltage_v,
            output_voltage_v: status.output_voltage_v,
            load_percent: status.load_percent,
            output_frequency_hz: status.output_frequency_hz,
            battery_charge_percent: status.battery_charge_percent,
            temperature_c: status.temperature_c,
            frame_type: status.frame_type.to_string(),
            status_byte: format!("0x{bits:02x}"),
            status_bits_msb_first: status.status_bits_msb_first,
        };
        let alerts = alerts(&telemetry, &self.snapshot.config);
        let mut runtime = self.runtime_tracker.estimate(
            &self.snapshot.config.runtime,
            f64::from(ratings.battery_nominal_v),
            telemetry.load_percent,
            telemetry.battery_charge_percent,
            vendor_flags.on_battery || vendor_flags.self_test_active,
            Instant::now(),
        );
        if let Some(estimate) = &mut runtime {
            estimate.mode_verified = self
                .snapshot
                .config
                .protection
                .as_ref()
                .is_some_and(|p| p.battery_mode_verified)
                && (vendor_flags.on_battery || !vendor_flags.self_test_active);
        }
        let sample_at = protection::now_ms();
        let protected = self
            .snapshot
            .config
            .protection
            .as_ref()
            .is_some_and(|p| p.enabled);
        let poll_ms = self.snapshot.config.poll_interval_ms as u64;
        let timeout_ms = self.snapshot.config.request_timeout_ms as u64;
        let freshness_ms = if protected {
            poll_ms.min(2000) + timeout_ms.min(2000)
        } else {
            poll_ms + timeout_ms
        } + 100;
        self.energy.observe(
            chrono::Local::now(),
            sample_at,
            runtime::estimated_load_w(&self.snapshot.config.runtime, telemetry.load_percent),
            freshness_ms,
        );
        self.battery_history.observe(battery_history::Observation {
            at_ms: Utc::now().timestamp_millis(),
            boot_ms: sample_at,
            on_battery: vendor_flags.on_battery,
            charge_percent: telemetry.battery_charge_percent,
            mode_verified: self
                .snapshot
                .config
                .protection
                .as_ref()
                .is_some_and(|p| p.battery_mode_verified),
            max_interval_ms: freshness_ms,
        });
        self.sample_at_ms = Some(sample_at);
        if self
            .snapshot
            .config
            .protection
            .as_ref()
            .is_some_and(|p| p.enabled)
        {
            if !vendor_flags.on_battery {
                self.protection.observe(false, 0, sample_at);
            } else if let Some(estimate) = &runtime {
                self.protection.observe(
                    vendor_flags.on_battery,
                    estimate.remaining_seconds,
                    sample_at,
                );
            }
        }
        if let Some(previous) = &self.snapshot.sample {
            let old_bits = previous.telemetry.status_byte.clone();
            let old_alerts = previous.alerts.clone();
            if old_bits != telemetry.status_byte {
                self.event(
                    EventKind::flags,
                    format!(
                        "Status flags {old_bits} to {}; meanings unverified",
                        telemetry.status_byte
                    ),
                );
            }
            if old_alerts != alerts {
                self.event(EventKind::alerts, format!("Alerts {alerts:?}"));
            }
        } else if alerts.low_charge || alerts.high_load || alerts.high_temperature {
            self.event(EventKind::alerts, format!("Alerts {alerts:?}"));
        }
        update_statistics(&mut self.snapshot.statistics, &telemetry);
        let sample = Sample {
            sequence: self.sample_id,
            time: now(),
            time_unix_ms: Utc::now().timestamp_millis(),
            identity: Identity {
                model: identity.model.clone(),
                firmware: identity.firmware.clone(),
            },
            ratings: Ratings {
                voltage_range_code: ratings.voltage_range_code.clone(),
                current_range_code: ratings.current_range_code.clone(),
                battery_nominal_v: ratings.battery_nominal_v.into(),
                output_frequency_nominal_hz: ratings.output_frequency_nominal_hz.into(),
            },
            telemetry,
            topology,
            vendor_flags,
            flags_verified: false,
            alerts,
            runtime,
        };
        self.history.push_back(sample.clone());
        while self.history.len() > self.snapshot.config.history_samples as usize {
            self.history.pop_front();
        }
        self.snapshot.sample = Some(sample);
        self.snapshot.revision += 1;
    }
}

fn alerts(t: &Telemetry, config: &Config) -> Alerts {
    Alerts {
        low_charge: config.low_charge_percent > 0.0
            && t.battery_charge_percent <= config.low_charge_percent,
        high_load: config.high_load_percent > 0.0 && t.load_percent > config.high_load_percent,
        high_temperature: config.high_temperature_c > 0.0
            && t.temperature_c >= config.high_temperature_c,
    }
}

fn update_statistics(stats: &mut Option<Statistics>, t: &Telemetry) {
    fn range(value: f64) -> Range {
        Range {
            minimum: value,
            maximum: value,
        }
    }
    let Some(stats) = stats else {
        *stats = Some(Statistics {
            samples: 1,
            input_voltage_v: range(t.input_voltage_v),
            output_voltage_v: range(t.output_voltage_v),
            load_percent: range(t.load_percent),
            output_frequency_hz: range(t.output_frequency_hz),
            battery_charge_percent: range(t.battery_charge_percent),
            temperature_c: range(t.temperature_c),
        });
        return;
    };
    stats.samples += 1;
    for (range, value) in [
        (&mut stats.input_voltage_v, t.input_voltage_v),
        (&mut stats.output_voltage_v, t.output_voltage_v),
        (&mut stats.load_percent, t.load_percent),
        (&mut stats.output_frequency_hz, t.output_frequency_hz),
        (&mut stats.battery_charge_percent, t.battery_charge_percent),
        (&mut stats.temperature_c, t.temperature_c),
    ] {
        range.minimum = range.minimum.min(value);
        range.maximum = range.maximum.max(value);
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    config_path: PathBuf,
    started: Instant,
    stop: Arc<AtomicBool>,
}

impl Shared {
    fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        let mut snapshot = state.snapshot.clone();
        snapshot.energy = Some(state.energy.status(chrono::Local::now()));
        snapshot.battery_history = Some(state.battery_history.status());
        snapshot.uptime_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        snapshot
    }

    fn connection(
        &self,
        connection: ConnectionState,
        device: Option<String>,
        error: Option<String>,
    ) {
        self.state
            .lock()
            .unwrap()
            .connection(connection, device, error);
        self.changed.notify_all();
    }

    fn config(&self) -> Config {
        self.state.lock().unwrap().snapshot.config.clone()
    }

    fn wait(&self, duration: Duration) {
        let guard = self.state.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout(guard, duration.min(Duration::from_millis(100)))
            .unwrap();
    }

    fn control<C: VarlinkCallError + ?Sized>(
        &self,
        command: Command,
        id: String,
        confirmation: Option<String>,
        call: &mut C,
    ) -> varlink::Result<()> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return call.reply_invalid_control(
                "request_id must contain 1..128 ASCII letters, digits, -, _, ., or :".into(),
            );
        }
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.controls.iter().find(|c| c.id == id) {
            if existing.command != command {
                return call.reply_request_id_conflict();
            }
        } else {
            if state.snapshot.connection != ConnectionState::online {
                return call.reply_device_unavailable("UPS is not connected".into());
            }
            if let Some(confirmation) = confirmation {
                if state
                    .snapshot
                    .sample
                    .as_ref()
                    .map(|s| s.identity.model.as_str())
                    != Some(confirmation.as_str())
                {
                    return call.reply_invalid_control(
                        "confirmation must equal the connected UPS model".into(),
                    );
                }
            }
            if state
                .controls
                .iter()
                .filter(|c| c.outcome.is_none())
                .count()
                >= 8
            {
                return call.reply_busy();
            }
            let device = state.snapshot.device.clone().expect("online device");
            state.controls.push_back(ControlEntry {
                id: id.clone(),
                command,
                device,
                outcome: None,
            });
            self.changed.notify_all();
        }
        loop {
            let outcome = state
                .controls
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.outcome.clone());
            match outcome {
                Some(Outcome::Written(receipt)) => {
                    drop(state);
                    return call.reply_struct(varlink::Reply::parameters(Some(
                        serde_json::json!({ "receipt": receipt }),
                    )));
                }
                Some(Outcome::Failed {
                    reason,
                    may_have_been_sent,
                }) => {
                    drop(state);
                    return call.reply_command_failed(reason, may_have_been_sent);
                }
                None if self.stop.load(Ordering::Relaxed) => {
                    drop(state);
                    return call.reply_command_failed(
                        "Daemon is stopping; check the UPS before retrying".into(),
                        true,
                    );
                }
                None => {
                    state = self
                        .changed
                        .wait_timeout(state, Duration::from_millis(100))
                        .unwrap()
                        .0
                }
            }
        }
    }
}

struct Monitor(Arc<Shared>);

impl api::VarlinkInterface for Monitor {
    fn get_snapshot(&self, call: &mut dyn Call_GetSnapshot) -> varlink::Result<()> {
        call.reply(self.0.snapshot())
    }

    fn watch(&self, call: &mut dyn Call_Watch) -> varlink::Result<()> {
        loop {
            let snapshot = self.0.snapshot();
            let revision = snapshot.revision;
            let more = call.wants_more() && !self.0.stop.load(Ordering::Relaxed);
            call.set_continues(more);
            call.reply(snapshot)?;
            if !more {
                return Ok(());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut guard = self.0.state.lock().unwrap();
            while guard.snapshot.revision == revision
                && !self.0.stop.load(Ordering::Relaxed)
                && Instant::now() < deadline
            {
                guard = self
                    .0
                    .changed
                    .wait_timeout(guard, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }
    }

    fn get_history(
        &self,
        call: &mut dyn Call_GetHistory,
        limit: Option<i64>,
    ) -> varlink::Result<()> {
        let limit = limit.unwrap_or(300);
        if !(1..=10_000).contains(&limit) {
            return call.reply_invalid_parameter("limit must be 1..10000".into());
        }
        let state = self.0.state.lock().unwrap();
        let samples = state
            .history
            .iter()
            .skip(state.history.len().saturating_sub(limit as usize))
            .cloned()
            .collect();
        drop(state);
        call.reply(samples)
    }

    fn get_events(&self, call: &mut dyn Call_GetEvents, limit: Option<i64>) -> varlink::Result<()> {
        let limit = limit.unwrap_or(100);
        if !(1..=256).contains(&limit) {
            return call.reply_invalid_parameter("limit must be 1..256".into());
        }
        let state = self.0.state.lock().unwrap();
        let events = state
            .events
            .iter()
            .skip(state.events.len().saturating_sub(limit as usize))
            .cloned()
            .collect();
        drop(state);
        call.reply(events)
    }

    fn get_battery_history(
        &self,
        call: &mut dyn Call_GetBatteryHistory,
        limit: Option<i64>,
        before_id: Option<i64>,
    ) -> varlink::Result<()> {
        let limit = limit.unwrap_or(100);
        if !(1..=1000).contains(&limit) {
            return call.reply_invalid_parameter("limit must be 1..1000".into());
        }
        if before_id.is_some_and(|id| id < 1) {
            return call.reply_invalid_parameter("before_id must be positive".into());
        }
        let state = self.0.state.lock().unwrap();
        let (sessions, has_more) = state.battery_history.page(limit as usize, before_id);
        let status = state.battery_history.status();
        drop(state);
        call.reply(sessions, has_more, status)
    }

    fn get_config(&self, call: &mut dyn Call_GetConfig) -> varlink::Result<()> {
        let snapshot = self.0.snapshot();
        call.reply(snapshot.config, snapshot.config_revision)
    }

    fn set_config(
        &self,
        call: &mut dyn Call_SetConfig,
        mut config: Config,
        expected_revision: Option<i64>,
    ) -> varlink::Result<()> {
        if let Err(error) = config.validate() {
            return call.reply_invalid_config(error.to_string());
        }
        let mut state = self.0.state.lock().unwrap();
        // Older full-replacement clients do not know the optional protection field.
        if config.protection.is_none() {
            config.protection = state.snapshot.config.protection.clone();
        }
        if config.device != state.snapshot.config.device {
            if let Some(protection) = &mut config.protection {
                protection.battery_mode_verified = false;
            }
        }
        if expected_revision.is_some_and(|r| r != state.snapshot.config_revision) {
            return call.reply_config_conflict(state.snapshot.config_revision);
        }
        if let Err(error) = config::save(&self.0.config_path, &config) {
            return call.reply_storage_failed(format!("{error:#}"));
        }
        if state.snapshot.config != config {
            if state.snapshot.config.runtime.rated_output_w != config.runtime.rated_output_w {
                state.energy.break_continuity();
            }
            if state.snapshot.config.runtime != config.runtime {
                state.runtime_tracker.reset();
                state.protection.reset();
            }
            if state.snapshot.config.device != config.device {
                state
                    .battery_history
                    .interrupt(BatterySessionEndReason::device_changed);
                state.connection(ConnectionState::connecting, None, None);
                state.snapshot.statistics = None;
                state.history.clear();
                state.protection.reset();
            }
            state.snapshot.config = config.clone();
            state.snapshot.config_revision += 1;
            if let Some(sample) = &mut state.snapshot.sample {
                sample.alerts = alerts(&sample.telemetry, &config);
            }
            while state.history.len() > config.history_samples as usize {
                state.history.pop_front();
            }
            state.event(EventKind::configuration, "Monitoring configuration updated");
        }
        let revision = state.snapshot.config_revision;
        drop(state);
        self.0.changed.notify_all();
        call.reply(config, revision)
    }

    fn toggle_beeper(
        &self,
        call: &mut dyn Call_ToggleBeeper,
        request_id: String,
    ) -> varlink::Result<()> {
        self.0
            .control(Command::ToggleBeeper, request_id, None, call)
    }
    fn start_self_test(
        &self,
        call: &mut dyn Call_StartSelfTest,
        request_id: String,
    ) -> varlink::Result<()> {
        self.0.control(Command::SelfTest, request_id, None, call)
    }
    fn cancel_battery_test(
        &self,
        call: &mut dyn Call_CancelBatteryTest,
        request_id: String,
    ) -> varlink::Result<()> {
        self.0.control(Command::CancelTest, request_id, None, call)
    }
    fn cancel_shutdown(
        &self,
        call: &mut dyn Call_CancelShutdown,
        request_id: String,
    ) -> varlink::Result<()> {
        self.0
            .control(Command::CancelShutdown, request_id, None, call)
    }
    fn start_discharge_test(
        &self,
        call: &mut dyn Call_StartDischargeTest,
        confirmation: String,
        request_id: String,
    ) -> varlink::Result<()> {
        self.0
            .control(Command::DischargeTest, request_id, Some(confirmation), call)
    }

    fn start_timed_battery_test(
        &self,
        call: &mut dyn Call_StartTimedBatteryTest,
        duration_minutes: i64,
        confirmation: String,
        request_id: String,
    ) -> varlink::Result<()> {
        if !(1..=600).contains(&duration_minutes) {
            return call.reply_invalid_control("duration_minutes must be 1..600".into());
        }
        self.0.control(
            Command::TimedTest {
                minutes: duration_minutes as u16,
            },
            request_id,
            Some(confirmation),
            call,
        )
    }

    fn schedule_shutdown(
        &self,
        call: &mut dyn Call_ScheduleShutdown,
        delay_seconds: i64,
        restore_after_minutes: Option<i64>,
        confirmation: String,
        request_id: String,
    ) -> varlink::Result<()> {
        if !(1..=600).contains(&delay_seconds)
            || restore_after_minutes.is_some_and(|m| !(1..=10_080).contains(&m))
        {
            return call.reply_invalid_control(
                "delay_seconds must be 1..600; restore_after_minutes must be 1..10080".into(),
            );
        }
        self.0.control(
            Command::Shutdown {
                delay_seconds: delay_seconds as u16,
                restore_after_minutes: restore_after_minutes.map(|m| m as u16),
            },
            request_id,
            Some(confirmation),
            call,
        )
    }
}

struct Received<R> {
    reply: R,
    missed_replies: usize,
}

/// A worker panic stops RPC so the service manager can restart the daemon.
fn spawn_worker(
    stop: Arc<AtomicBool>,
    work: impl FnOnce() + Send + 'static,
) -> thread::JoinHandle<()> {
    struct StopOnExit(Arc<AtomicBool>);
    impl Drop for StopOnExit {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    thread::spawn(move || {
        let _stop_on_exit = StopOnExit(stop);
        work();
    })
}

fn exchange<R: protocol::Response>(
    serial: &mut Serial,
    config: &Config,
    stop: &AtomicBool,
) -> Result<Received<R>> {
    const REPLY_RETRIES: usize = 3;
    let request = R::QUERY.request();
    let mut missed_replies = 0;
    loop {
        ensure!(
            serial.write_request(&request)? == request.len(),
            "Partial serial query write"
        );
        let bytes = serial.read_bytes(
            Duration::from_millis(config.request_timeout_ms as u64),
            18,
            stop,
        )?;
        if bytes.is_empty() && !stop.load(Ordering::Relaxed) {
            missed_replies += 1;
            ensure!(
                missed_replies <= REPLY_RETRIES,
                "UPS did not answer {} consecutive {:?} queries ({} ms each)",
                REPLY_RETRIES + 1,
                R::QUERY,
                config.request_timeout_ms
            );
            // No bytes arrived, so there is no partial frame to resynchronize.
            // Keep the port and discharge tracking alive for an isolated miss.
            eprintln!(
                "UPS {:?} reply timed out; retrying ({missed_replies}/{REPLY_RETRIES})",
                R::QUERY
            );
            continue;
        }
        return Ok(Received {
            reply: R::decode(&bytes)?,
            missed_replies,
        });
    }
}

fn execute_controls(shared: &Shared, serial: &mut Serial, device: &str) -> Result<bool> {
    let mut wrote = false;
    loop {
        let mut state = shared.state.lock().unwrap();
        let Some(entry) = state.controls.iter_mut().find(|c| c.outcome.is_none()) else {
            return Ok(wrote);
        };
        if entry.device != device {
            entry.outcome = Some(Outcome::Failed {
                reason: "Device changed".into(),
                may_have_been_sent: false,
            });
            shared.changed.notify_all();
            continue;
        }
        // Hold the state lock through the single nonblocking write. No RPC can queue
        // a duplicate ID between selecting the command and recording its result.
        let request = entry.command.request();
        let result = serial.write_request(&request);
        let (outcome, error) = match result {
            Ok(bytes) if bytes == request.len() => (
                Outcome::Written(CommandReceipt {
                    request_id: entry.id.clone(),
                    action: entry.command.name().into(),
                    time: now(),
                    request_hex: hex::encode(request),
                    bytes_written: bytes as i64,
                    acknowledged: false,
                }),
                None,
            ),
            Ok(bytes) => {
                let reason = format!("Partial control write: {bytes}/7 bytes; outcome unknown");
                (
                    Outcome::Failed {
                        reason: reason.clone(),
                        may_have_been_sent: bytes > 0,
                    },
                    Some(reason),
                )
            }
            Err(error) => {
                let reason = format!("Control write failed: {error:#}");
                (
                    Outcome::Failed {
                        reason: reason.clone(),
                        may_have_been_sent: false,
                    },
                    Some(reason),
                )
            }
        };
        let message = format!(
            "{} {}: {}",
            entry.id,
            entry.command.name(),
            error
                .as_deref()
                .unwrap_or("written; device acknowledgement unavailable")
        );
        entry.outcome = Some(outcome);
        state.event(EventKind::control, message);
        while state.controls.len() > 256 {
            if let Some(index) = state.controls.iter().position(|e| e.outcome.is_some()) {
                state.controls.remove(index);
            } else {
                break;
            }
        }
        drop(state);
        shared.changed.notify_all();
        if let Some(error) = error {
            anyhow::bail!(error);
        }
        wrote = true;
    }
}

fn connected(shared: &Shared) -> Result<()> {
    let initial = shared.config();
    let device = serial::resolve_port(initial.device.as_ref().map(PathBuf::from))?;
    let path = device.to_string_lossy().into_owned();
    let mut serial = Serial::open(&device, Baud::B2400)?;
    ensure!(
        serial
            .read_bytes(Duration::from_millis(100), 4096, &shared.stop)?
            .is_empty(),
        "Unsolicited serial data before handshake"
    );
    let identity = exchange::<protocol::Identity>(&mut serial, &initial, &shared.stop)?.reply;
    ensure!(
        identity.model.starts_with("GAM"),
        "Unexpected UPS model {}",
        identity.model
    );
    let ratings = exchange::<protocol::Ratings>(&mut serial, &initial, &shared.stop)?.reply;
    let mut next_poll = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        let mut config = shared.config();
        if config.protection.as_ref().is_some_and(|p| p.enabled) {
            // Protection cannot wait a minute to discover a mains outage.
            config.poll_interval_ms = config.poll_interval_ms.min(2000);
            config.request_timeout_ms = config.request_timeout_ms.min(2000);
        }
        if config.device != initial.device {
            break;
        }
        if execute_controls(shared, &mut serial, &path)? {
            // Vendor control methods pause 300 ms and define no response frame.
            crate::wait(Duration::from_millis(300), &shared.stop);
            let unsolicited = serial.read_bytes(Duration::from_millis(100), 4096, &shared.stop)?;
            ensure!(
                unsolicited.is_empty(),
                "Unexpected control response: {}; reconnecting before parsing more telemetry",
                hex::encode(unsolicited)
            );
            next_poll = Instant::now();
        }
        if Instant::now() >= next_poll {
            let received = exchange::<protocol::Status>(&mut serial, &config, &shared.stop)?;
            let mut state = shared.state.lock().unwrap();
            if state.snapshot.config.device != initial.device {
                break;
            }
            state.connection(ConnectionState::online, Some(path.clone()), None);
            if received.missed_replies > 0 {
                state.energy.break_continuity();
                state
                    .battery_history
                    .interrupt(BatterySessionEndReason::telemetry_lost);
            }
            state.sample(&identity, &ratings, received.reply);
            drop(state);
            shared.changed.notify_all();
            next_poll = Instant::now() + Duration::from_millis(config.poll_interval_ms as u64);
        }
        // A setting or queued control wakes this wait. Recheck the poll interval too.
        next_poll =
            next_poll.min(Instant::now() + Duration::from_millis(config.poll_interval_ms as u64));
        shared.wait(next_poll.saturating_duration_since(Instant::now()));
    }
    Ok(())
}

fn protect_host(shared: Arc<Shared>, path: PathBuf, boot_id: String, dry_run: bool) {
    let mut host = Host::default();
    host.dry_run = dry_run;
    let mut last_saved = 0;
    let mut had_checkpoint = true;
    let mut last_error = None;
    while !shared.stop.load(Ordering::Relaxed) {
        let now = protection::now_ms();
        let mut manual = false;
        let mut errors = Vec::new();
        match host.actions() {
            Ok(actions) => {
                for action in actions {
                    match action {
                        host::Action::PowerOff => manual = true,
                        host::Action::Open => {
                            if let Err(error) = Host::open_panel() {
                                errors.push(format!("Could not open instrument panel: {error:#}"));
                            }
                        }
                    }
                }
            }
            Err(error) => errors.push(format!("Notification actions unavailable: {error:#}")),
        }
        let (config, mut status, request, phase_changed) = {
            let mut state = shared.state.lock().unwrap();
            let config = state.snapshot.config.protection.clone().unwrap_or_default();
            let stale = state.snapshot.connection != ConnectionState::online
                || state
                    .sample_at_ms
                    .is_none_or(|at| now.saturating_sub(at) > 6000);
            let _ = state.protection.tick(&config, stale, dry_run, now);
            // Give a restarted daemon one handshake to observe restored mains.
            let observed_or_timed_out =
                state.sample_at_ms.is_some() || shared.started.elapsed() >= Duration::from_secs(5);
            let request =
                observed_or_timed_out && state.protection.request_shutdown(&config, manual, now);
            let status = state.protection.tick(&config, stale, dry_run, now);
            let phase_changed = state.snapshot.protection.phase != status.phase
                || state.snapshot.protection.warning_active != status.warning_active;
            if phase_changed {
                state.event(
                    EventKind::protection,
                    format!(
                        "PC protection {:?}; estimated seconds {:?}{}",
                        status.phase,
                        status.remaining_seconds,
                        if dry_run { " (dry run)" } else { "" }
                    ),
                );
            }
            if state.snapshot.protection != status {
                state.snapshot.protection = status.clone();
                state.snapshot.revision += 1;
                shared.changed.notify_all();
            }
            (config, status, request, phase_changed)
        };
        // Commit the power-off before any potentially slow notification call.
        if request {
            let result = host.power_off();
            let mut state = shared.state.lock().unwrap();
            match result {
                Ok(()) => {
                    state.protection.accepted();
                    state.event(
                        EventKind::protection,
                        if dry_run {
                            "DRY RUN: orderly PC power-off simulated"
                        } else {
                            "Orderly PC power-off accepted by logind"
                        },
                    );
                }
                Err(error) => {
                    let reason = format!("{error:#}");
                    state
                        .protection
                        .failed(reason.clone(), protection::now_ms());
                    state.event(
                        EventKind::protection,
                        format!("PC shutdown failed; retry in five seconds: {reason}"),
                    );
                }
            }
            status = state.protection.tick(
                &config,
                status.telemetry_stale,
                dry_run,
                protection::now_ms(),
            );
            state.snapshot.protection = status.clone();
            state.snapshot.revision += 1;
            shared.changed.notify_all();
        }
        if let Err(error) = host.update_notice(&status, &config, now) {
            errors.push(format!("Critical UPS notification failed: {error:#}"));
        }
        if phase_changed || request || now.saturating_sub(last_saved) >= 5000 {
            let checkpoint = {
                let state = shared.state.lock().unwrap();
                state
                    .protection
                    .checkpoint(&boot_id, &state.snapshot.config.runtime)
            };
            if checkpoint.is_some() || had_checkpoint {
                if let Err(error) = protection::save(&path, checkpoint.as_ref()) {
                    errors.push(format!(
                        "Could not checkpoint protection countdown: {error:#}"
                    ));
                }
            }
            had_checkpoint = checkpoint.is_some();
            last_saved = now;
        }
        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        if error != last_error {
            if let Some(message) = &error {
                eprintln!("{message}");
                shared
                    .state
                    .lock()
                    .unwrap()
                    .event(EventKind::protection, message);
                shared.changed.notify_all();
            }
        }
        last_error = error;
        crate::wait(Duration::from_millis(200), &shared.stop);
    }
}

pub fn run(args: DaemonArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let config_path = args.config.map_or_else(config::default_path, Ok)?;
    let socket = args.socket.map_or_else(rpc::default_socket, Ok)?;
    let mut config = config::load(&config_path)?;
    if let Some(device) = args.device {
        config.device = Some(device.to_string_lossy().into_owned());
    }
    config.validate()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let checkpoint_path = config_path.with_file_name("protection-state.json");
    let mut protection = protection::Engine::default();
    if config.protection.as_ref().is_some_and(|p| p.enabled) {
        match protection::load(&checkpoint_path) {
            Ok(Some(checkpoint)) => protection.restore(checkpoint, boot_id.trim(), &config.runtime),
            Ok(None) => {}
            Err(error) => eprintln!("Could not restore protection countdown: {error:#}"),
        }
    }
    let protection_status = protection.tick(
        config.protection.as_ref().expect("normalized config"),
        false,
        args.dry_run_protection,
        protection::now_ms(),
    );
    let endpoint = rpc::Endpoint::bind(socket.clone())?;
    let (energy_store, energy) =
        match energy::Store::open(config_path.with_file_name("energy.json")) {
            Ok(store) => {
                let meter = store.load(chrono::Local::now());
                (Some(Arc::new(store)), meter)
            }
            Err(error) => (
                None,
                energy::Meter::Unavailable(format!("Energy storage unavailable: {error:#}")),
            ),
        };
    if let Some(error) = energy.status(chrono::Local::now()).last_error {
        eprintln!("{error}");
    }
    let (battery_store, battery_history) =
        match battery_history::Store::open(config_path.with_file_name("battery-history.json")) {
            Ok(store) => {
                let history = store.load(Utc::now().timestamp_millis());
                (Some(Arc::new(store)), history)
            }
            Err(error) => (
                None,
                battery_history::History::Unavailable(format!(
                    "Battery history storage unavailable: {error:#}"
                )),
            ),
        };
    if let Some(error) = battery_history.status().last_error {
        eprintln!("{error}");
    }
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            snapshot: Snapshot {
                revision: 1,
                uptime_ms: 0,
                connection: ConnectionState::connecting,
                device: None,
                last_error: None,
                sample: None,
                statistics: None,
                config,
                config_revision: 1,
                protection: protection_status,
                energy: None,
                battery_history: None,
            },
            history: VecDeque::new(),
            events: VecDeque::new(),
            controls: VecDeque::new(),
            event_id: 0,
            sample_id: 0,
            runtime_tracker: runtime::Tracker::default(),
            protection,
            sample_at_ms: None,
            energy,
            battery_history,
        }),
        changed: Condvar::new(),
        config_path,
        started: Instant::now(),
        stop: stop.clone(),
    });
    let metered = shared.clone();
    let energy_writer = energy_store.clone();
    let battery_writer = battery_store.clone();
    let recorder = spawn_worker(stop.clone(), move || {
        record_usage(metered, energy_writer, battery_writer)
    });
    let protected = shared.clone();
    let protector = spawn_worker(stop.clone(), move || {
        protect_host(
            protected,
            checkpoint_path,
            boot_id.trim().into(),
            args.dry_run_protection,
        )
    });
    let polling = shared.clone();
    let worker = spawn_worker(stop.clone(), move || {
        while !polling.stop.load(Ordering::Relaxed) {
            if let Err(error) = connected(&polling) {
                if polling.stop.load(Ordering::Relaxed) {
                    break;
                }
                polling.connection(
                    ConnectionState::disconnected,
                    polling.snapshot().device,
                    Some(format!("{error:#}")),
                );
                let deadline = Instant::now()
                    + Duration::from_millis(polling.config().reconnect_interval_ms as u64);
                let revision = polling.snapshot().config_revision;
                while !polling.stop.load(Ordering::Relaxed)
                    && Instant::now() < deadline
                    && polling.snapshot().config_revision == revision
                {
                    polling.wait(deadline.saturating_duration_since(Instant::now()));
                }
            }
        }
        polling.connection(ConnectionState::stopped, polling.snapshot().device, None);
    });
    let service = varlink::VarlinkService::new(
        "sms-gamer",
        "SMS Gamer UPS",
        env!("CARGO_PKG_VERSION"),
        "https://varlink.org",
        vec![Box::new(api::new(Box::new(Monitor(shared.clone()))))],
    );
    eprintln!("SMS Gamer daemon listening at unix:{}", socket.display());
    let result = endpoint.serve(service, stop.clone());
    stop.store(true, Ordering::Relaxed);
    shared.changed.notify_all();
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("Serial polling thread panicked"))?;
    protector
        .join()
        .map_err(|_| anyhow::anyhow!("Host protection thread panicked"))?;
    recorder
        .join()
        .map_err(|_| anyhow::anyhow!("Usage recording thread panicked"))?;
    checkpoint_energy(&shared, energy_store.as_deref());
    checkpoint_battery_history(&shared, battery_store.as_deref());
    result.context("Varlink server failed")
}

/// Disk writes run outside the state lock and never block serial reads or shutdown policy.
fn checkpoint_energy(shared: &Shared, store: Option<&energy::Store>) {
    let Some(store) = store else {
        return;
    };
    let checkpoint = {
        let mut state = shared.state.lock().unwrap();
        state.energy.tick(chrono::Local::now());
        state.energy.checkpoint()
    };
    let Some(checkpoint) = checkpoint else {
        return;
    };
    let error = store
        .save(&checkpoint)
        .err()
        .map(|error| format!("Could not save energy counters: {error:#}"));
    let mut state = shared.state.lock().unwrap();
    if state.energy.save_error(error.clone()) {
        if let Some(message) = &error {
            eprintln!("{message}");
        }
        state.event(
            EventKind::energy,
            error
                .clone()
                .unwrap_or_else(|| "Energy storage available".into()),
        );
        shared.changed.notify_all();
    }
}

fn checkpoint_battery_history(shared: &Shared, store: Option<&battery_history::Store>) {
    let Some(store) = store else { return };
    let checkpoint = shared.state.lock().unwrap().battery_history.checkpoint();
    let Some(checkpoint) = checkpoint else { return };
    let error = store
        .save(&checkpoint)
        .err()
        .map(|error| format!("Could not save battery history: {error:#}"));
    let mut state = shared.state.lock().unwrap();
    if state.battery_history.save_error(error.clone()) {
        if let Some(message) = &error {
            eprintln!("{message}");
        }
        state.event(
            EventKind::battery_history,
            error.unwrap_or_else(|| "Battery history storage available".into()),
        );
        shared.changed.notify_all();
    }
}

fn record_usage(
    shared: Arc<Shared>,
    store: Option<Arc<energy::Store>>,
    battery_store: Option<Arc<battery_history::Store>>,
) {
    // Persist the tracking start immediately so an early crash still leaves a
    // baseline from which the next run can identify unobserved time.
    checkpoint_energy(&shared, store.as_deref());
    checkpoint_battery_history(&shared, battery_store.as_deref());
    let mut last_saved = Instant::now();
    let mut battery_saved = Instant::now();
    let mut battery_revision = shared
        .state
        .lock()
        .unwrap()
        .battery_history
        .status()
        .revision;
    while !shared.stop.load(Ordering::Relaxed) {
        let changed = {
            let mut state = shared.state.lock().unwrap();
            let energy_changed = state.energy.tick(chrono::Local::now());
            let battery_changed = state
                .battery_history
                .tick(Utc::now().timestamp_millis(), protection::now_ms());
            let changed = energy_changed || battery_changed;
            if changed {
                state.snapshot.revision += 1;
            }
            changed
        };
        if changed {
            shared.changed.notify_all();
        }
        if last_saved.elapsed() >= Duration::from_secs(30) {
            checkpoint_energy(&shared, store.as_deref());
            last_saved = Instant::now();
        }
        let battery = shared.state.lock().unwrap().battery_history.status();
        if battery.revision != battery_revision
            || battery_saved.elapsed() >= Duration::from_secs(30)
            || battery.active_session.is_some() && battery_saved.elapsed() >= Duration::from_secs(5)
        {
            checkpoint_battery_history(&shared, battery_store.as_deref());
            battery_revision = battery.revision;
            battery_saved = Instant::now();
        }
        crate::wait(Duration::from_secs(1), &shared.stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicked_worker_stops_the_rpc_server_instead_of_leaving_a_stale_service_running() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = rpc::Endpoint::bind(dir.path().join("monitor")).unwrap();
        let service = varlink::VarlinkService::new("test", "test", "0", "", vec![]);
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let (finished, result) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            finished.send(endpoint.serve(service, server_stop)).unwrap();
        });
        let worker = spawn_worker(stop, || panic!("simulated background worker failure"));
        assert!(worker.join().is_err());
        result
            .recv_timeout(Duration::from_secs(2))
            .expect("RPC server stayed alive after a worker failed")
            .unwrap();
        server.join().unwrap();
    }
}
