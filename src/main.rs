mod api;
mod battery_history;
mod client;
mod config;
mod control;
mod daemon;
mod energy;
mod host;
mod protection;
mod protocol;
mod rpc;
mod runtime;
mod serial;

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use protocol::{Identity, Query, Ratings, Reply, Response, Status, parse_hex, serialize_hex_byte};
use serde::{Deserialize, Serialize};
use serial::{Baud, Serial};

#[derive(Parser)]
#[command(
    version,
    about = "SMS Gamer UPS daemon, Varlink client, and serial diagnostics"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Own the UPS connection and serve the Varlink interface.
    Daemon(daemon::DaemonArgs),
    /// Read the daemon's latest snapshot without opening the serial port.
    Status(client::RpcArgs),
    /// Stream daemon updates as JSONL; reconnect automatically after daemon restarts.
    Watch(client::RpcArgs),
    /// Read recent daemon samples, oldest first.
    History {
        #[command(flatten)]
        rpc: client::RpcArgs,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(i64).range(1..=10000))]
        limit: i64,
    },
    /// Read persistent battery sessions, newest first.
    BatteryHistory {
        #[command(flatten)]
        rpc: client::RpcArgs,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        before_id: Option<i64>,
    },
    /// Read recent daemon events, oldest first.
    Events {
        #[command(flatten)]
        rpc: client::RpcArgs,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=256))]
        limit: i64,
    },
    /// Read or persist monitoring and PC protection settings through Varlink.
    Config {
        #[command(flatten)]
        rpc: client::RpcArgs,
        #[command(subcommand)]
        action: client::ConfigAction,
    },
    /// Send an explicit UPS control through the daemon. Never retried automatically.
    Control {
        #[command(flatten)]
        rpc: client::RpcArgs,
        /// Stable caller-supplied ID. Reusing it deduplicates within this daemon session.
        #[arg(long)]
        request_id: String,
        #[command(subcommand)]
        action: client::ControlAction,
    },
    /// Inspect USB, driver, kernel, and permissions without opening the serial port.
    Diagnose,
    /// Validate and decode an existing capture without hardware access.
    Decode { path: PathBuf },
    /// Compare status bits in an existing capture without hardware access.
    Analyze { path: PathBuf },
    /// Capture serial data without sending a request.
    Listen(PortArgs),
    /// Send documented monitoring queries and capture their replies.
    Query {
        #[command(flatten)]
        port: PortArgs,
        #[arg(value_enum, required = true, num_args = 1..)]
        queries: Vec<Query>,
    },
    /// Stream live telemetry with identity and nominal ratings.
    Monitor {
        #[command(flatten)]
        port: PortArgs,
        #[arg(long, default_value = "2", value_parser = seconds)]
        interval: Duration,
        /// Number of samples; zero runs until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        count: u64,
    },
}

#[derive(Args)]
struct PortArgs {
    /// Serial path; defaults to the detected 067b:23a3 bridge.
    #[arg(long)]
    device: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "2400")]
    baud: Baud,
    #[arg(long, default_value = "2", value_parser = seconds)]
    timeout: Duration,
    /// New JSONL capture file; existing files are never overwritten.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn seconds(value: &str) -> Result<Duration, String> {
    let value: f64 = value
        .parse()
        .map_err(|_| "Expected a number of seconds".to_owned())?;
    if !value.is_finite() || value <= 0.0 || value > 60.0 {
        return Err("Must be greater than 0 and at most 60 seconds".to_owned());
    }
    let duration = Duration::try_from_secs_f64(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("Duration is below timer precision".to_owned());
    }
    Ok(duration)
}

/// Optional metadata keeps the original fixture and capture format readable.
#[derive(Debug, Deserialize, Serialize)]
struct RawCapture {
    hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baud: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ascii: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "direction")]
enum CaptureEvent {
    #[serde(rename = "rx-passive")]
    Passive {
        #[serde(flatten)]
        raw: RawCapture,
    },
    #[serde(rename = "tx")]
    Sent {
        query: Query,
        #[serde(flatten)]
        raw: RawCapture,
    },
    #[serde(rename = "rx")]
    Received {
        query: Query,
        #[serde(flatten)]
        raw: RawCapture,
    },
}

enum RecordKind {
    Passive,
    Sent(Query),
    Received(Query),
}

fn write_json(output: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

struct Recorder {
    file: Option<BufWriter<File>>,
    device: PathBuf,
    baud: Baud,
    echo: bool,
}

impl Recorder {
    fn new(output: Option<&Path>, device: PathBuf, baud: Baud, echo: bool) -> Result<Self> {
        let file = output
            .map(|path| -> Result<_> {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    fs::create_dir_all(parent)?;
                }
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .with_context(|| format!("Could not create new capture {}", path.display()))?;
                Ok(BufWriter::new(file))
            })
            .transpose()?;
        Ok(Self {
            file,
            device,
            baud,
            echo,
        })
    }

    fn record(&mut self, kind: RecordKind, data: &[u8]) -> Result<()> {
        let raw = RawCapture {
            hex: data
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" "),
            time: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)),
            device: Some(self.device.clone()),
            baud: Some(self.baud.rate()),
            ascii: Some(
                data.iter()
                    .map(|byte| {
                        if byte.is_ascii_graphic() || *byte == b' ' {
                            char::from(*byte)
                        } else {
                            '.'
                        }
                    })
                    .collect(),
            ),
        };
        let event = match kind {
            RecordKind::Passive => CaptureEvent::Passive { raw },
            RecordKind::Sent(query) => CaptureEvent::Sent { query, raw },
            RecordKind::Received(query) => CaptureEvent::Received { query, raw },
        };
        if let Some(file) = &mut self.file {
            write_json(file, &event)?;
        }
        if self.echo {
            write_json(&mut io::stdout().lock(), &event)?;
        }
        Ok(())
    }
}

struct Session {
    serial: Serial,
    recorder: Recorder,
    timeout: Duration,
    stop: Arc<AtomicBool>,
}

impl Session {
    fn open(port: PortArgs, echo: bool, stop: Arc<AtomicBool>) -> Result<Self> {
        let device = serial::resolve_port(port.device)?;
        let recorder = Recorder::new(port.output.as_deref(), device.clone(), port.baud, echo)?;
        let serial = Serial::open(&device, port.baud)?;
        Ok(Self {
            serial,
            recorder,
            timeout: port.timeout,
            stop,
        })
    }

    fn passive(&mut self) -> Result<Vec<u8>> {
        let bytes = self.serial.read_bytes(self.timeout, 4096, &self.stop)?;
        self.recorder.record(RecordKind::Passive, &bytes)?;
        Ok(bytes)
    }

    fn exchange(&mut self, query: Query) -> Result<Vec<u8>> {
        ensure!(!self.stop.load(Ordering::Relaxed), "Capture interrupted");
        let request = query.request();
        let count = self.serial.write_request(&request)?;
        self.recorder
            .record(RecordKind::Sent(query), &request[..count])?;
        ensure!(
            count == request.len(),
            "Partial serial write. Stopping without retrying."
        );
        let reply = self.serial.read_bytes(self.timeout, 18, &self.stop)?;
        self.recorder.record(RecordKind::Received(query), &reply)?;
        ensure!(
            !reply.is_empty(),
            "No reply to {query:?} at {} baud",
            self.recorder.baud.rate()
        );
        Ok(reply)
    }

    fn read<R: Response>(&mut self) -> Result<R> {
        R::decode(&self.exchange(R::QUERY)?)
    }
}

#[derive(Serialize)]
struct Sample<'a> {
    time: String,
    #[serde(flatten)]
    identity: &'a Identity,
    #[serde(flatten)]
    ratings: &'a Ratings,
    #[serde(flatten)]
    status: Status,
}

fn wait(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

/// Decode at the file boundary so callers see validated, typed replies.
fn capture_replies(
    path: &Path,
    mut visit: impl FnMut(RawCapture, Query, Reply) -> Result<()>,
) -> Result<()> {
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("Could not open {}", path.display()))?,
    );
    for (number, line) in reader.lines().enumerate() {
        let location = format!("{}:{}", path.display(), number + 1);
        let event: CaptureEvent = serde_json::from_str(&line?).with_context(|| location.clone())?;
        if let CaptureEvent::Received { query, raw } = event {
            let reply = parse_hex(&raw.hex)
                .and_then(|bytes| query.decode(&bytes))
                .with_context(|| location)?;
            visit(raw, query, reply)?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct DecodedEvent {
    query: Query,
    decoded: Reply,
}

#[derive(Debug, PartialEq, Serialize)]
struct BitChange {
    bit: u8,
    before: bool,
    after: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
enum AnalysisEvent {
    Baseline {
        time: Option<String>,
        readings: Status,
    },
    StatusChange {
        time: Option<String>,
        readings: Status,
        #[serde(serialize_with = "serialize_hex_byte")]
        previous_status_byte: u8,
        previous_frame_type: char,
        changed_bits: Vec<BitChange>,
    },
    Summary {
        status_samples: u64,
        status_changes: u64,
        observed_status_bytes: Vec<String>,
        observed_frame_types: BTreeSet<char>,
    },
}

#[derive(Default)]
struct Analysis {
    previous: Option<(char, u8)>,
    samples: u64,
    changes: u64,
    bytes: BTreeSet<u8>,
    headers: BTreeSet<char>,
}

impl Analysis {
    fn observe(&mut self, time: Option<String>, status: Status) -> Option<AnalysisEvent> {
        self.samples += 1;
        self.bytes.insert(status.status_byte);
        self.headers.insert(status.frame_type);
        let current = (status.frame_type, status.status_byte);
        let previous = self.previous.replace(current);
        match previous {
            None => Some(AnalysisEvent::Baseline {
                time,
                readings: status,
            }),
            Some(previous) if previous != current => {
                self.changes += 1;
                let changed_bits = (0..8)
                    .filter(|bit| (previous.1 ^ current.1) & (1 << bit) != 0)
                    .map(|bit| BitChange {
                        bit,
                        before: previous.1 & (1 << bit) != 0,
                        after: current.1 & (1 << bit) != 0,
                    })
                    .collect();
                Some(AnalysisEvent::StatusChange {
                    time,
                    readings: status,
                    previous_frame_type: previous.0,
                    previous_status_byte: previous.1,
                    changed_bits,
                })
            }
            Some(_) => None,
        }
    }

    fn finish(self) -> Result<AnalysisEvent> {
        ensure!(self.samples != 0, "No status replies in capture");
        Ok(AnalysisEvent::Summary {
            status_samples: self.samples,
            status_changes: self.changes,
            observed_status_bytes: self
                .bytes
                .iter()
                .map(|byte| format!("0x{byte:02x}"))
                .collect(),
            observed_frame_types: self.headers,
        })
    }
}

fn analyze_capture(path: &Path, output: &mut impl Write) -> Result<()> {
    let mut analysis = Analysis::default();
    capture_replies(path, |raw, _, reply| {
        if let Reply::Status(status) = reply {
            if let Some(event) = analysis.observe(raw.time, status) {
                write_json(output, &event)?;
            }
        }
        Ok(())
    })?;
    write_json(
        output,
        &analysis
            .finish()
            .with_context(|| path.display().to_string())?,
    )
}

fn run(command: Command, stop: Arc<AtomicBool>) -> Result<()> {
    match command {
        Command::Daemon(args) => daemon::run(args, stop),
        Command::Status(args) => client::status(args),
        Command::Watch(args) => client::watch(args, stop),
        Command::History { rpc, limit } => client::history(rpc, limit),
        Command::BatteryHistory {
            rpc,
            limit,
            before_id,
        } => client::battery_history(rpc, limit, before_id),
        Command::Events { rpc, limit } => client::events(rpc, limit),
        Command::Config { rpc, action } => client::config(rpc, action),
        Command::Control {
            rpc,
            request_id,
            action,
        } => client::control(rpc, request_id, action),
        Command::Diagnose => serial::diagnose(),
        Command::Decode { path } => capture_replies(&path, |_, query, decoded| {
            write_json(&mut io::stdout().lock(), &DecodedEvent { query, decoded })
        }),
        Command::Analyze { path } => analyze_capture(&path, &mut io::stdout().lock()),
        Command::Listen(port) => {
            Session::open(port, true, stop)?.passive()?;
            Ok(())
        }
        Command::Query { port, queries } => {
            let mut session = Session::open(port, true, stop)?;
            ensure!(
                session.passive()?.is_empty(),
                "Unsolicited data present. Inspect the passive capture before querying."
            );
            for query in queries {
                query.decode(&session.exchange(query)?)?;
            }
            Ok(())
        }
        Command::Monitor {
            port,
            interval,
            count,
        } => {
            let mut session = Session::open(port, false, stop.clone())?;
            ensure!(
                session.passive()?.is_empty(),
                "Unsolicited data present. Inspect the passive capture before querying."
            );
            let identity: Identity = session.read()?;
            let ratings: Ratings = session.read()?;
            let mut samples = 0;
            while !stop.load(Ordering::Relaxed) && (count == 0 || samples < count) {
                let start = Instant::now();
                let status: Status = session.read()?;
                write_json(
                    &mut io::stdout().lock(),
                    &Sample {
                        time: Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
                        identity: &identity,
                        ratings: &ratings,
                        status,
                    },
                )?;
                samples += 1;
                if count == 0 || samples < count {
                    wait(interval.saturating_sub(start.elapsed()), &stop);
                }
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if let Err(error) = signal_hook::flag::register(signal, stop.clone()) {
            eprintln!("ERROR: Could not register signal handler: {error}");
            return ExitCode::FAILURE;
        }
    }
    let result = run(cli.command, stop.clone());
    if stop.load(Ordering::Relaxed) {
        return ExitCode::from(130);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            }) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_changes_and_recovery_have_no_semantic_labels() {
        let mut capture = tempfile::NamedTempFile::new().unwrap();
        // Synthetic transitions based on the real baseline; not an AC-loss capture.
        for (second, flags) in [0x39, 0x39, 0xb9, 0x39].into_iter().enumerate() {
            let mut reply = hex::decode("3d0000086b086000e9025703e801e5399c0d").unwrap();
            reply[15] = flags;
            reply[16] = reply[..16]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
                .wrapping_neg();
            writeln!(capture, "{}", serde_json::json!({"direction":"rx", "query":"status", "time":second.to_string(), "hex":hex::encode(reply)})).unwrap();
        }
        let mut output = Vec::new();
        analyze_capture(capture.path(), &mut output).unwrap();
        let events: Vec<serde_json::Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["event"], "baseline");
        assert_eq!(events[1]["time"], "2");
        assert_eq!(
            events[1]["changed_bits"],
            serde_json::json!([{"bit":7,"before":false,"after":true}])
        );
        assert_eq!(
            events[2]["changed_bits"],
            serde_json::json!([{"bit":7,"before":true,"after":false}])
        );
        assert_eq!(
            events[3],
            serde_json::json!({"event":"summary", "status_samples":4, "status_changes":2, "observed_status_bytes":["0x39","0xb9"], "observed_frame_types":["="]})
        );
    }

    #[test]
    fn corrupt_capture_reports_source_line() {
        let mut capture = tempfile::NamedTempFile::new().unwrap();
        writeln!(capture, "{}", serde_json::json!({"direction":"rx", "query":"status", "hex":"3d0000086b086000e9025703e801e5b99c0d"})).unwrap();
        let error = analyze_capture(capture.path(), &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains(":1: Invalid reply checksum"));
    }

    #[test]
    fn capture_without_status_is_rejected() {
        let mut capture = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            capture,
            "{}",
            include_str!("../tests/fixtures/gamer-2000.jsonl")
                .lines()
                .next()
                .unwrap()
        )
        .unwrap();
        let error = analyze_capture(capture.path(), &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains("No status replies"));
    }
}
