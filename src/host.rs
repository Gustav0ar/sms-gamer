//! Fixed D-Bus adapters. No shell, privilege escalation, forced power-off, or UPS writes.
use std::{
    io::Read,
    process::{Child, ChildStdout, Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use serde_json::Value;

use crate::api::{ProtectionConfig, ProtectionPhase, ProtectionStatus};

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATION_PATH: &str = "/org/freedesktop/Notifications";
const LOGIN: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

fn call(system: bool, arguments: &[&str]) -> Result<Value> {
    let output = Command::new("/usr/bin/busctl")
        .args([
            if system { "--system" } else { "--user" },
            "--timeout=2s",
            "--allow-interactive-authorization=no",
            "--json=short",
            "call",
        ])
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("Could not run busctl")?;
    ensure!(
        output.status.success(),
        "D-Bus call failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    if output.stdout.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

pub enum Action {
    Open,
    PowerOff,
}

struct Monitor {
    process: Child,
    output: ChildStdout,
    buffer: Vec<u8>,
}

impl Monitor {
    fn start() -> Result<Self> {
        let mut process = Command::new("/usr/bin/busctl")
            .args(["--user", "--json=short", "--match=type='signal',sender='org.freedesktop.Notifications',interface='org.freedesktop.Notifications'", "monitor"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let output = process.stdout.take().expect("piped stdout");
        let monitor = Self {
            process,
            output,
            buffer: Vec::new(),
        };
        fcntl_setfl(
            &monitor.output,
            fcntl_getfl(&monitor.output)? | OFlags::NONBLOCK,
        )?;
        Ok(monitor)
    }

    fn actions(&mut self, id: u64) -> Result<Vec<Action>> {
        let mut bytes = [0; 4096];
        loop {
            match self.output.read(&mut bytes) {
                Ok(0) => bail!("Notification action listener exited"),
                Ok(count) => self.buffer.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
            ensure!(
                self.buffer.len() <= 65536,
                "Notification action message exceeds 64 KiB"
            );
        }
        let mut actions = Vec::new();
        while let Some(index) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=index).collect();
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                if value["member"] == "ActionInvoked"
                    && value["payload"]["data"][0].as_u64() == Some(id)
                {
                    match value["payload"]["data"][1].as_str() {
                        Some("open") => actions.push(Action::Open),
                        Some("shutdown") => actions.push(Action::PowerOff),
                        _ => {}
                    }
                }
            }
        }
        Ok(actions)
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[derive(Default)]
pub struct Host {
    monitor: Option<Monitor>,
    notification_id: Option<u64>,
    last_deadline_ms: Option<u64>,
    last_phase: Option<ProtectionPhase>,
    last_attempt_phase: Option<ProtectionPhase>,
    next_notice_attempt_ms: u64,
    pub dry_run: bool,
}

impl Host {
    pub fn power_off(&self) -> Result<()> {
        if self.dry_run {
            eprintln!("DRY RUN: would request orderly PC power-off through logind");
            return Ok(());
        }
        call(
            true,
            &[LOGIN, LOGIN_PATH, MANAGER, "PowerOff", "b", "false"],
        )?;
        Ok(())
    }

    pub fn actions(&mut self) -> Result<Vec<Action>> {
        match (&mut self.monitor, self.notification_id) {
            (Some(monitor), Some(id)) => match monitor.actions(id) {
                Ok(actions) => Ok(actions),
                Err(error) => {
                    self.monitor = None;
                    Err(error)
                }
            },
            _ => Ok(Vec::new()),
        }
    }

    /// A non-expiring warning is sent once. Only a materially earlier deadline or
    /// a phase change replaces it, so dismissing it does not create a popup loop.
    pub fn update_notice(
        &mut self,
        status: &ProtectionStatus,
        config: &ProtectionConfig,
        now: u64,
    ) -> Result<()> {
        if !status.warning_active && status.phase != ProtectionPhase::shutdown_requested {
            return self.clear_notice();
        }
        if self.last_attempt_phase.as_ref() == Some(&status.phase)
            && now < self.next_notice_attempt_ms
        {
            return Ok(());
        }
        if self.monitor.is_none() {
            self.monitor = Some(Monitor::start()?);
        }
        let changed_phase = self.last_phase.as_ref() != Some(&status.phase);
        let deadline =
            now.saturating_add(status.shutdown_in_seconds.unwrap_or(0).max(0) as u64 * 1000);
        let earlier = self
            .last_deadline_ms
            .is_some_and(|previous| deadline.saturating_add(2000) < previous);
        if !changed_phase && !earlier {
            return Ok(());
        }
        let remaining = duration(status.remaining_seconds.unwrap_or(0));
        let pending = status.phase == ProtectionPhase::shutdown_requested;
        let title = if self.dry_run {
            "UPS protection simulation"
        } else if pending {
            "Shutting down the PC"
        } else if status.phase == ProtectionPhase::shutdown_failed {
            "PC shutdown failed"
        } else {
            "Save your work now"
        };
        let mut body = if self.dry_run && pending {
            "Simulated power-off request. The PC will stay on.".into()
        } else if pending {
            "Linux is closing applications and stopping services.".into()
        } else if !config.battery_mode_verified {
            format!(
                "Estimated battery time {remaining}. Automatic shutdown is blocked until battery detection is verified. Save your work and shut down the PC."
            )
        } else if status.phase == ProtectionPhase::shutdown_failed {
            format!(
                "Save your work and shut down manually. {}",
                status
                    .last_error
                    .as_deref()
                    .unwrap_or("Orderly shutdown could not be requested.")
            )
        } else {
            let at = chrono::Local::now()
                + chrono::Duration::seconds(status.shutdown_in_seconds.unwrap_or(0));
            format!(
                "Estimated battery time {remaining}. Save your work now. Automatic PC shutdown at {}. Follow the countdown in the bar.",
                at.format("%H:%M:%S")
            )
        };
        if status.telemetry_stale {
            body.push_str(" UPS readings were lost; using the last battery estimate.");
        }
        let body = body
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let id = self.notification_id.unwrap_or(0).to_string();
        let actions = if pending { "2" } else { "4" };
        let hints = if self.notification_id.is_some() {
            "2"
        } else {
            "1"
        };
        let mut args = vec![
            NOTIFICATIONS,
            NOTIFICATION_PATH,
            NOTIFICATIONS,
            "Notify",
            "susssasa{sv}i",
            "SMS Gamer",
            &id,
            "battery-caution",
            title,
            &body,
            actions,
            "open",
            "Open UPS",
        ];
        if !pending {
            args.extend(["shutdown", "Shut down PC"]);
        }
        args.extend([hints, "urgency", "y", "2"]);
        if self.notification_id.is_some() {
            args.extend(["suppress-sound", "b", "true"]);
        }
        args.push("0");
        self.last_attempt_phase = Some(status.phase.clone());
        self.next_notice_attempt_ms = now.saturating_add(5000);
        let reply = call(false, &args)?;
        self.notification_id = Some(
            reply["data"][0]
                .as_u64()
                .context("Invalid notification ID")?,
        );
        self.last_deadline_ms = Some(deadline);
        self.last_phase = Some(status.phase.clone());
        Ok(())
    }

    pub fn clear_notice(&mut self) -> Result<()> {
        let id = self.notification_id.take();
        self.last_deadline_ms = None;
        self.last_phase = None;
        self.last_attempt_phase = None;
        self.next_notice_attempt_ms = 0;
        self.monitor = None;
        if let Some(id) = id {
            call(
                false,
                &[
                    NOTIFICATIONS,
                    NOTIFICATION_PATH,
                    NOTIFICATIONS,
                    "CloseNotification",
                    "u",
                    &id.to_string(),
                ],
            )?;
        }
        Ok(())
    }

    pub fn open_panel() -> Result<()> {
        let output = Command::new("/usr/bin/timeout")
            .args([
                "2s",
                "/usr/bin/noctalia",
                "msg",
                "panel-open",
                "smsups/sms-gamer:panel",
            ])
            .stdin(Stdio::null())
            .output()?;
        ensure!(output.status.success(), "Could not open UPS panel");
        Ok(())
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.clear_notice();
    }
}

fn duration(seconds: i64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
