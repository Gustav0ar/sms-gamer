use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    api::{self, VarlinkClientInterface},
    rpc,
};

#[derive(Args)]
pub struct RpcArgs {
    /// Unix socket; defaults to $XDG_RUNTIME_DIR/sms-gamer/io.smsups.Monitor.
    #[arg(long)]
    pub socket: Option<PathBuf>,
}

impl RpcArgs {
    fn address(self) -> Result<String> {
        Ok(format!(
            "unix:{}",
            self.socket.map_or_else(rpc::default_socket, Ok)?.display()
        ))
    }
    fn connect(self) -> Result<api::VarlinkClient> {
        connect(&self.address()?)
    }
}

fn connect(address: &str) -> Result<api::VarlinkClient> {
    let connection = varlink::Connection::with_address(address)
        .with_context(|| format!("Cannot connect to {address}"))?;
    Ok(api::VarlinkClient::new(connection))
}

fn output(value: &impl Serialize) -> Result<()> {
    crate::write_json(&mut io::stdout().lock(), value)
}

pub fn status(args: RpcArgs) -> Result<()> {
    output(&args.connect()?.get_snapshot().call()?.snapshot)
}
pub fn history(args: RpcArgs, limit: i64) -> Result<()> {
    output(&args.connect()?.get_history(Some(limit)).call()?)
}
pub fn events(args: RpcArgs, limit: i64) -> Result<()> {
    output(&args.connect()?.get_events(Some(limit)).call()?)
}

pub fn battery_history(args: RpcArgs, limit: i64, before_id: Option<i64>) -> Result<()> {
    output(
        &args
            .connect()?
            .get_battery_history(Some(limit), before_id)
            .call()?,
    )
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WatchEvent<'a> {
    Snapshot { snapshot: &'a api::Snapshot },
    Unavailable { message: String },
}

/// Restart only the subscription. Controls are always separate, single-shot RPCs.
pub fn watch(args: RpcArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let address = args.address()?;
    let mut last_error = None;
    while !stop.load(Ordering::Relaxed) {
        let result = (|| -> Result<()> {
            let mut client = connect(&address)?;
            for reply in client.watch().more()? {
                let snapshot = reply?.snapshot;
                output(&WatchEvent::Snapshot {
                    snapshot: &snapshot,
                })?;
                last_error = None;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            anyhow::bail!("Daemon subscription ended");
        })();
        if let Err(error) = result {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            }) {
                return Err(error);
            }
            let message = format!("{error:#}");
            if !stop.load(Ordering::Relaxed) && last_error.as_ref() != Some(&message) {
                output(&WatchEvent::Unavailable {
                    message: message.clone(),
                })?;
                last_error = Some(message);
            }
        }
        crate::wait(Duration::from_secs(2), &stop);
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum ConfigAction {
    Get,
    Set {
        /// Complete Config JSON object. Prefer --file for manual use.
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        json: Option<String>,
        #[arg(long, conflicts_with = "json")]
        file: Option<PathBuf>,
        /// Reject a change if another app has updated the config since this revision.
        #[arg(long)]
        revision: Option<i64>,
    },
}

pub fn config(args: RpcArgs, action: ConfigAction) -> Result<()> {
    let mut client = args.connect()?;
    match action {
        ConfigAction::Get => output(&client.get_config().call()?),
        ConfigAction::Set {
            json,
            file,
            revision,
        } => {
            let text = if let Some(file) = file {
                std::fs::read_to_string(file)?
            } else {
                json.expect("clap requires JSON or file")
            };
            let config: api::Config = serde_json::from_str(&text).context("Invalid Config JSON")?;
            config.validate()?;
            output(&client.set_config(config, revision).call()?)
        }
    }
}

#[derive(Subcommand)]
pub enum ControlAction {
    ToggleBeeper,
    SelfTest,
    TimedTest {
        #[arg(long)]
        minutes: i64,
        #[arg(long)]
        confirm_model: String,
    },
    DischargeTest {
        #[arg(long)]
        confirm_model: String,
    },
    CancelTest,
    Shutdown {
        #[arg(long)]
        delay_seconds: i64,
        #[arg(long)]
        restore_after_minutes: Option<i64>,
        #[arg(long)]
        confirm_model: String,
    },
    CancelShutdown,
}

pub fn control(args: RpcArgs, id: String, action: ControlAction) -> Result<()> {
    let mut client = args.connect()?;
    let receipt = match action {
        ControlAction::ToggleBeeper => client.toggle_beeper(id).call()?.receipt,
        ControlAction::SelfTest => client.start_self_test(id).call()?.receipt,
        ControlAction::TimedTest {
            minutes,
            confirm_model,
        } => {
            client
                .start_timed_battery_test(minutes, confirm_model, id)
                .call()?
                .receipt
        }
        ControlAction::DischargeTest { confirm_model } => {
            client
                .start_discharge_test(confirm_model, id)
                .call()?
                .receipt
        }
        ControlAction::CancelTest => client.cancel_battery_test(id).call()?.receipt,
        ControlAction::Shutdown {
            delay_seconds,
            restore_after_minutes,
            confirm_model,
        } => {
            client
                .schedule_shutdown(delay_seconds, restore_after_minutes, confirm_model, id)
                .call()?
                .receipt
        }
        ControlAction::CancelShutdown => client.cancel_shutdown(id).call()?.receipt,
    };
    output(&receipt)
}
