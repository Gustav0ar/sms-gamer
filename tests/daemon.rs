use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{ffi::OsStrExt, fs::symlink, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    termios::{OptionalActions, tcgetattr, tcsetattr},
};
use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_sms-gamer");

struct Simulator {
    path: PathBuf,
    slave: File,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    status: Arc<Mutex<Option<(u8, u16)>>>,
    dropped_replies: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Simulator {
    fn new() -> Self {
        let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&controller).unwrap();
        unlockpt(&controller).unwrap();
        let name = ptsname(&controller, Vec::new()).unwrap();
        let path = PathBuf::from(OsStr::from_bytes(name.to_bytes()));
        let slave = File::open(&path).unwrap();
        // A hardware UPS does not echo terminal input back to the host.
        let mut settings = tcgetattr(&slave).unwrap();
        settings.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &settings).unwrap();
        let mut master = File::from(controller);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(None::<(u8, u16)>));
        let status_reply = status.clone();
        let dropped_replies = Arc::new(AtomicUsize::new(0));
        let drop_reply = dropped_replies.clone();
        let seen = requests.clone();
        let quit = stop.clone();
        let worker = thread::spawn(move || {
            let replies: Vec<Value> = include_str!("fixtures/gamer-2000.jsonl")
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let mut request = Vec::new();
            while !quit.load(Ordering::Relaxed) {
                let mut fds = [PollFd::new(&master, PollFlags::IN)];
                poll(
                    &mut fds,
                    Some(&Timespec::try_from(Duration::from_millis(50)).unwrap()),
                )
                .unwrap();
                if !fds[0].revents().contains(PollFlags::IN) {
                    continue;
                }
                let mut bytes = [0u8; 64];
                let Ok(count) = master.read(&mut bytes) else {
                    break;
                };
                request.extend_from_slice(&bytes[..count]);
                while request.len() >= 7 {
                    let frame: Vec<u8> = request.drain(..7).collect();
                    assert_eq!(frame[6], 13);
                    assert_eq!(
                        frame[..6].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)),
                        0
                    );
                    let query = match frame[0] {
                        b'I' => Some("identity"),
                        b'F' => Some("ratings"),
                        b'Q' => Some("status"),
                        _ => None,
                    };
                    seen.lock().unwrap().push(frame);
                    if let Some(query) = query {
                        if drop_reply
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                                remaining.checked_sub(1)
                            })
                            .is_ok()
                        {
                            continue;
                        }
                        let reply = replies.iter().find(|r| r["query"] == query).unwrap();
                        let hex: String =
                            reply["hex"].as_str().unwrap().split_whitespace().collect();
                        let mut bytes = hex::decode(hex).unwrap();
                        if query == "status" {
                            if let Some((flags, charge)) = *status_reply.lock().unwrap() {
                                bytes[15] = flags;
                                bytes[11..13].copy_from_slice(&charge.to_be_bytes());
                                bytes[16] = bytes[..16]
                                    .iter()
                                    .fold(0u8, |sum, b| sum.wrapping_add(*b))
                                    .wrapping_neg();
                            }
                        }
                        master.write_all(&bytes).unwrap();
                    }
                }
            }
        });
        Self {
            path,
            slave,
            requests,
            status,
            dropped_replies,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Simulator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(dir: &Path, device: &Path) -> Self {
        Self::start_with_bus(dir, device, None)
    }

    fn start_with_bus(dir: &Path, device: &Path, bus: Option<&str>) -> Self {
        let socket = dir.join("run/monitor");
        let mut command = Command::new(BINARY);
        command
            .arg("daemon")
            .arg("--socket")
            .arg(&socket)
            .arg("--config")
            .arg("config.json")
            .current_dir(dir)
            .arg("--device")
            .arg(device)
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(dir.join("daemon.log")).unwrap()));
        if let Some(bus) = bus {
            // Both adapters point at a private test bus. No host login1 is reachable.
            command
                .env("DBUS_SESSION_BUS_ADDRESS", bus)
                .env("DBUS_SYSTEM_BUS_ADDRESS", bus);
        }
        let child = command.spawn().unwrap();
        let mut daemon = Self { child, socket };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !daemon.socket.exists() {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                fs::read_to_string(dir.join("daemon.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "daemon socket timeout");
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn call(&self, method: &str, parameters: Value) -> Value {
        let mut socket = UnixStream::connect(&self.socket).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let request =
            json!({"method": format!("io.smsups.Monitor.{method}"), "parameters": parameters});
        serde_json::to_writer(&mut socket, &request).unwrap();
        socket.write_all(&[0]).unwrap();
        let mut bytes = Vec::new();
        BufReader::new(socket).read_until(0, &mut bytes).unwrap();
        assert_eq!(bytes.pop(), Some(0));
        serde_json::from_slice(&bytes).unwrap()
    }

    fn wait_for(&self, condition: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let reply = self.call("GetSnapshot", json!({}));
            let snapshot = &reply["parameters"]["snapshot"];
            if condition(snapshot) {
                return snapshot.clone();
            }
            assert!(Instant::now() < deadline, "snapshot timeout: {snapshot}");
            thread::sleep(Duration::from_millis(30));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(4);
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                self.child.kill().unwrap();
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.child.wait().unwrap();
    }
}

#[test]
fn daemon_serves_real_protocol_data_config_streams_and_deduplicated_controls() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let original = tcgetattr(&simulator.slave).unwrap();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let snapshot = daemon.wait_for(|s| s["connection"] == "online");
    assert_eq!(snapshot["sample"]["telemetry"]["load_percent"], 23.3);
    assert_eq!(snapshot["sample"]["ratings"]["battery_nominal_v"], 24);
    assert_eq!(snapshot["sample"]["vendor_flags"]["beeper_enabled"], true);
    assert_eq!(snapshot["sample"]["flags_verified"], false);
    let mut stream = UnixStream::connect(&daemon.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .write_all(b"{\"method\":\"io.smsups.Monitor.Watch\",\"more\":true}\0")
        .unwrap();
    let mut stream = BufReader::new(stream);
    let mut first = Vec::new();
    stream.read_until(0, &mut first).unwrap();
    let first: Value = serde_json::from_slice(&first[..first.len() - 1]).unwrap();
    assert_eq!(first["continues"], true);
    let mut config = snapshot["config"].clone();
    config["high_temperature_c"] = json!(45.0);
    config["poll_interval_ms"] = json!(1000);
    config["history_samples"] = json!(2);
    let reply = daemon.call("SetConfig", json!({"config":config, "expected_revision":1}));
    assert_eq!(reply["parameters"]["revision"], 2);
    assert_eq!(
        daemon.call("SetConfig", json!({"config":config,"expected_revision":1}))["error"],
        "io.smsups.Monitor.ConfigConflict"
    );
    assert_eq!(
        daemon.wait_for(|s| s["config_revision"] == 2)["sample"]["alerts"]["high_temperature"],
        true
    );
    let persisted: Value =
        serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(persisted, config);
    let mut changed = Vec::new();
    stream.read_until(0, &mut changed).unwrap();
    let changed: Value = serde_json::from_slice(&changed[..changed.len() - 1]).unwrap();
    assert_eq!(changed["parameters"]["snapshot"]["config_revision"], 2);
    config["poll_interval_ms"] = json!(0);
    assert_eq!(
        daemon.call("SetConfig", json!({"config":config}))["error"],
        "io.smsups.Monitor.InvalidConfig"
    );
    let commands = [
        ("ToggleBeeper", json!({"request_id":"beeper"}), b'M'),
        ("StartSelfTest", json!({"request_id":"test"}), b'T'),
        (
            "StartTimedBatteryTest",
            json!({"request_id":"timed", "duration_minutes":1,"confirmation":"GAM2000BiBiW"}),
            b'T',
        ),
        (
            "StartDischargeTest",
            json!({"request_id":"discharge","confirmation":"GAM2000BiBiW"}),
            b'L',
        ),
        (
            "CancelBatteryTest",
            json!({"request_id":"cancel-test"}),
            b'D',
        ),
        (
            "ScheduleShutdown",
            json!({"request_id":"shutdown","delay_seconds":300,"restore_after_minutes":60,"confirmation":"GAM2000BiBiW"}),
            b'R',
        ),
        (
            "CancelShutdown",
            json!({"request_id":"cancel-shutdown"}),
            b'C',
        ),
    ];
    for (method, params, command) in commands {
        let receipt = daemon.call(method, params.clone());
        assert_eq!(
            receipt["parameters"]["receipt"]["acknowledged"], false,
            "{receipt}"
        );
        assert_eq!(receipt["parameters"]["receipt"]["bytes_written"], 7);
        assert_eq!(
            daemon.call(method, params),
            receipt,
            "retries must return the same receipt"
        );
        assert_eq!(
            simulator
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r[0] == command)
                .count(),
            if command == b'T' && method == "StartTimedBatteryTest" {
                2
            } else {
                1
            }
        );
    }
    assert_eq!(
        daemon.call("StartSelfTest", json!({"request_id":"beeper"}))["error"],
        "io.smsups.Monitor.RequestIdConflict"
    );
    assert_eq!(
        daemon.call(
            "ScheduleShutdown",
            json!({"request_id":"reject","delay_seconds":300,"confirmation":"wrong-model"})
        )["error"],
        "io.smsups.Monitor.InvalidControl"
    );
    let history = daemon.call("GetHistory", json!({"limit":2}));
    assert!(
        !history["parameters"]["samples"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(history["parameters"]["samples"].as_array().unwrap().len() <= 2);
    let cli = Command::new(BINARY)
        .arg("status")
        .arg("--socket")
        .arg(&daemon.socket)
        .output()
        .unwrap();
    assert!(cli.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cli.stdout).unwrap()["connection"],
        "online"
    );
    let socket_path = daemon.socket.clone();
    drop(daemon);
    drop(stream);
    assert!(!socket_path.exists());
    let restored = tcgetattr(&simulator.slave).unwrap();
    assert_eq!(restored.input_modes, original.input_modes);
    assert_eq!(restored.output_modes, original.output_modes);
    assert_eq!(restored.control_modes, original.control_modes);
    assert_eq!(restored.local_modes, original.local_modes);
    assert_eq!(restored.input_speed(), original.input_speed());
    assert_eq!(restored.output_speed(), original.output_speed());
}

#[test]
fn energy_counts_real_samples_marks_a_retried_read_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let first = daemon.wait_for(|s| s["connection"] == "online");
    let mut config = first["config"].clone();
    config["poll_interval_ms"] = json!(1000);
    config["request_timeout_ms"] = json!(100);
    daemon.call("SetConfig", json!({"config": config}));
    let observed = daemon.wait_for(|s| {
        s["energy"]["counters"]["total"]["observed_ms"]
            .as_i64()
            .unwrap_or(0)
            >= 1000
    });
    let baseline = &observed["energy"]["counters"]["total"];
    assert!(
        dir.path().join("energy.json").exists(),
        "Tracking start must be durable before the first 30-second checkpoint"
    );
    let kwh = baseline["estimated_kwh"].as_f64().unwrap();
    let known_ms = baseline["observed_ms"].as_i64().unwrap();
    let expected = 1400.0 * 0.233 * known_ms as f64 / 3_600_000_000.0;
    assert!((kwh - expected).abs() < 1e-10, "{observed}");
    simulator.dropped_replies.store(1, Ordering::Relaxed);
    let sequence = observed["sample"]["sequence"].as_i64().unwrap();
    let recovered = daemon.wait_for(|s| s["sample"]["sequence"].as_i64().unwrap_or(0) > sequence);
    let after_miss = &recovered["energy"]["counters"]["total"];
    assert_eq!(recovered["connection"], "online");
    assert_eq!(after_miss["estimated_kwh"], baseline["estimated_kwh"]);
    assert!(after_miss["missing_ms"].as_i64().unwrap() > baseline["missing_ms"].as_i64().unwrap());
    let sequence = recovered["sample"]["sequence"].as_i64().unwrap();
    daemon.wait_for(|s| s["sample"]["sequence"].as_i64().unwrap_or(0) > sequence);
    drop(daemon);

    let persisted: Value =
        serde_json::from_slice(&fs::read(dir.path().join("energy.json")).unwrap()).unwrap();
    let restored = Daemon::start(dir.path(), &simulator.path);
    let snapshot = restored.wait_for(|s| s["connection"] == "online");
    assert_eq!(
        snapshot["energy"]["counters"]["total"]["estimated_kwh"],
        persisted["total"]["estimated_kwh"]
    );
    assert!(
        snapshot["energy"]["counters"]["total"]["missing_ms"]
            .as_i64()
            .unwrap()
            >= persisted["total"]["missing_ms"].as_i64().unwrap()
    );
    assert!(snapshot["energy"]["last_error"].is_null());
}

#[test]
fn corrupt_energy_storage_does_not_stop_monitoring_or_overwrite_saved_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("energy.json");
    fs::write(&path, b"{broken counter file").unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let snapshot = daemon.wait_for(|s| s["connection"] == "online");
    assert!(snapshot["sample"].is_object());
    assert!(snapshot["energy"]["counters"].is_null());
    assert!(snapshot["energy"]["last_error"].is_string());
    drop(daemon);
    assert_eq!(fs::read(&path).unwrap(), b"{broken counter file");
}

#[test]
fn malformed_and_oversized_rpc_frames_cannot_stop_monitoring() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let initial = daemon.wait_for(|s| s["connection"] == "online");
    let frames = [
        b"{invalid json}\0".to_vec(),
        vec![b'x'; 65_537],
        b"{\"method\":\"org.varlink.service.GetInfo\",\"upgrade\":true}\0".to_vec(),
        b"{\"method\":\"org.varlink.service.GetInfo\",\"oneway\":true,\"more\":true}\0".to_vec(),
    ];
    for frame in frames {
        let mut client = UnixStream::connect(&daemon.socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(&frame).unwrap();
        match client.read(&mut [0u8]) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("Malformed request was not closed: {result:?}"),
        }
        assert_eq!(
            daemon.call("GetSnapshot", json!({}))["parameters"]["snapshot"]["connection"],
            "online"
        );
    }
    daemon.wait_for(|s| {
        s["sample"]["sequence"].as_i64().unwrap() > initial["sample"]["sequence"].as_i64().unwrap()
    });
}

#[test]
fn battery_history_records_real_transitions_splits_retried_reads_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let first = daemon.wait_for(|s| s["connection"] == "online");
    let mut config = first["config"].clone();
    config["poll_interval_ms"] = json!(1000);
    config["request_timeout_ms"] = json!(100);
    config["protection"]["battery_mode_verified"] = json!(true);
    daemon.call("SetConfig", json!({"config":config}));
    // Self-test alone is not evidence of the physically verified battery flag.
    *simulator.status.lock().unwrap() = Some((0x3d, 1000));
    let self_test = daemon.wait_for(|s| s["sample"]["telemetry"]["status_byte"] == "0x3d");
    assert!(self_test["battery_history"]["active_session"].is_null());
    *simulator.status.lock().unwrap() = Some((0xb9, 1000));
    let active = daemon.wait_for(|s| {
        s["battery_history"]["active_session"]["duration_ms"]
            .as_i64()
            .unwrap_or(0)
            >= 1000
    });
    let id = active["battery_history"]["active_session"]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        active["battery_history"]["active_session"]["start_observed"],
        true
    );
    assert_eq!(
        active["battery_history"]["active_session"]["mode_verified"],
        true
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let saved: Value =
            serde_json::from_slice(&fs::read(dir.path().join("battery-history.json")).unwrap())
                .unwrap();
        if saved["sessions"]
            .as_array()
            .unwrap()
            .last()
            .is_some_and(|session| session["id"] == id && session["end"].is_null())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Active session was not saved while the daemon was running"
        );
        thread::sleep(Duration::from_millis(20));
    }
    simulator.dropped_replies.store(1, Ordering::Relaxed);
    let resumed = daemon.wait_for(|s| {
        s["battery_history"]["active_session"]["id"]
            .as_i64()
            .unwrap_or(0)
            > id
    });
    assert_eq!(resumed["connection"], "online");
    assert_eq!(
        resumed["battery_history"]["active_session"]["start_observed"],
        false
    );
    let newest = daemon.call("GetBatteryHistory", json!({"limit":1}));
    assert_eq!(newest["parameters"]["has_more"], true);
    let cursor = newest["parameters"]["sessions"][0]["id"].clone();
    let older = daemon.call("GetBatteryHistory", json!({"limit":1,"before_id":cursor}));
    assert_eq!(
        older["parameters"]["sessions"][0]["end"]["reason"],
        "telemetry_lost"
    );
    assert_eq!(
        older["parameters"]["sessions"][0]["duration_ms"],
        active["battery_history"]["active_session"]["duration_ms"]
    );
    *simulator.status.lock().unwrap() = Some((0x39, 990));
    daemon.wait_for(|s| {
        s["sample"]["vendor_flags"]["on_battery"] == false
            && s["battery_history"]["active_session"].is_null()
    });
    let all = daemon.call("GetBatteryHistory", json!({}));
    assert_eq!(
        all["parameters"]["sessions"][0]["end"]["reason"],
        "mains_restored"
    );
    assert!(
        all["parameters"]["sessions"][0]["duration_ms"]
            .as_i64()
            .unwrap()
            >= 1000
    );
    let cli = Command::new(BINARY)
        .args(["battery-history", "--socket"])
        .arg(&daemon.socket)
        .args(["--limit", "1"])
        .output()
        .unwrap();
    assert!(cli.status.success());
    let cli: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(cli["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        daemon.call("GetBatteryHistory", json!({"limit":0}))["error"],
        "org.varlink.service.InvalidParameter"
    );
    assert_eq!(
        daemon.call("GetBatteryHistory", json!({"before_id":0}))["error"],
        "org.varlink.service.InvalidParameter"
    );
    drop(daemon);
    let persisted: Value =
        serde_json::from_slice(&fs::read(dir.path().join("battery-history.json")).unwrap())
            .unwrap();
    assert_eq!(persisted["sessions"].as_array().unwrap().len(), 2);
    let restored = Daemon::start(dir.path(), &simulator.path);
    restored.wait_for(|s| s["connection"] == "online");
    assert_eq!(
        restored.call("GetBatteryHistory", json!({}))["parameters"]["sessions"],
        all["parameters"]["sessions"]
    );
    assert!(
        simulator
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|frame| b"IFQ".contains(&frame[0]))
    );
}

#[test]
fn corrupt_battery_history_is_preserved_while_monitoring_and_energy_continue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("battery-history.json");
    fs::write(&path, b"{broken battery history").unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let snapshot = daemon.wait_for(|s| s["connection"] == "online");
    assert!(snapshot["sample"].is_object());
    assert!(snapshot["energy"]["counters"].is_object());
    assert!(snapshot["battery_history"]["last_error"].is_string());
    let history = daemon.call("GetBatteryHistory", json!({}));
    assert!(history["parameters"]["status"]["last_error"].is_string());
    assert_eq!(history["parameters"]["sessions"], json!([]));
    drop(daemon);
    assert_eq!(fs::read(&path).unwrap(), b"{broken battery history");
}

#[test]
fn missed_monitoring_replies_do_not_emit_disconnect_and_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let daemon = Daemon::start(dir.path(), &simulator.path);
    let online = daemon.wait_for(|s| s["connection"] == "online");
    let mut config = online["config"].clone();
    config["poll_interval_ms"] = json!(1000);
    config["request_timeout_ms"] = json!(100);
    config["reconnect_interval_ms"] = json!(1000);
    daemon.call("SetConfig", json!({"config": config}));

    // The USB/serial connection stays open, but individual Q responses are lost.
    let mut sequence = online["sample"]["sequence"].as_i64().unwrap();
    for missed in [1, 2, 3] {
        simulator.dropped_replies.store(missed, Ordering::Relaxed);
        let recovered =
            daemon.wait_for(|s| s["sample"]["sequence"].as_i64().unwrap_or(0) > sequence);
        sequence = recovered["sample"]["sequence"].as_i64().unwrap();
        assert_eq!(simulator.dropped_replies.load(Ordering::Relaxed), 0);
    }
    let events = daemon.call("GetEvents", json!({}));
    let connections: Vec<_> = events["parameters"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "connection")
        .collect();
    assert_eq!(
        connections.len(),
        1,
        "A missed reading produced false connection notifications: {connections:?}"
    );
    assert_eq!(
        simulator
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame[0] == b'I')
            .count(),
        1,
        "A transient timeout must not reopen the port and restart its handshake"
    );

    // A silent UPS must still become disconnected after the bounded retry budget.
    simulator.dropped_replies.store(4, Ordering::Relaxed);
    let offline = daemon.wait_for(|s| s["connection"] == "disconnected");
    assert_eq!(offline["sample"]["sequence"], sequence);
    assert!(
        offline["last_error"]
            .as_str()
            .unwrap()
            .contains("4 consecutive Status queries")
    );
    daemon.wait_for(|s| s["connection"] == "online");
    drop(daemon);

    // Missed handshake responses need the same retries as periodic status reads.
    simulator.dropped_replies.store(3, Ordering::Relaxed);
    let restarted = Daemon::start(dir.path(), &simulator.path);
    restarted.wait_for(|s| s["connection"] == "online");
    assert_eq!(simulator.dropped_replies.load(Ordering::Relaxed), 0);
    let events = restarted.call("GetEvents", json!({}));
    let connections: Vec<_> = events["parameters"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "connection")
        .collect();
    assert_eq!(
        connections.len(),
        1,
        "A missed handshake reply produced false connection notifications: {connections:?}"
    );
}

#[test]
fn daemon_retains_stale_samples_and_reconnects_after_device_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let simulator = Simulator::new();
    let device = dir.path().join("device");
    symlink(&simulator.path, &device).unwrap();
    fs::write(dir.path().join("config.json"), json!({"device":device,"poll_interval_ms":1000,"request_timeout_ms":200,"reconnect_interval_ms":1000,"history_samples":10,"low_charge_percent":33.0,"high_load_percent":90.0,"high_temperature_c":60.0,"runtime":{"rated_output_w":1400.0,"battery_capacity_ah":9.0,"battery_health_percent":100.0,"inverter_efficiency_percent":85.0,"idle_power_w":15.0,"peukert_exponent":1.25,"capacity_rating_hours":20.0}}).to_string()).unwrap();
    let daemon = Daemon::start(dir.path(), &device);
    let online = daemon.wait_for(|s| s["connection"] == "online");
    drop(simulator);
    let offline = daemon.wait_for(|s| s["connection"] == "disconnected");
    assert_eq!(offline["sample"], online["sample"]);
    assert!(offline["last_error"].is_string());
    assert_eq!(
        daemon.call("StartSelfTest", json!({"request_id":"offline-test"}))["error"],
        "io.smsups.Monitor.DeviceUnavailable"
    );
    let replacement = Simulator::new();
    fs::remove_file(&device).unwrap();
    symlink(&replacement.path, &device).unwrap();
    let recovered = daemon.wait_for(|s| s["connection"] == "online");
    assert!(
        recovered["sample"]["sequence"].as_i64().unwrap()
            > online["sample"]["sequence"].as_i64().unwrap()
    );
    assert!(recovered.get("last_error").is_none() || recovered["last_error"].is_null());
}

struct FakeLogin {
    bus: Child,
    service: Child,
    monitor: Child,
    address: String,
    log: PathBuf,
}
impl FakeLogin {
    fn start(dir: &Path) -> Self {
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_string();
        assert!(address.starts_with("unix:"));
        assert!(!address.contains("/run/user/1000/bus"));
        let service = Command::new("dbus-test-tool")
            .args(["echo", "--session", "--name=org.freedesktop.login1"])
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let reply = Command::new("busctl")
                .args([
                    "--address",
                    &address,
                    "--json=short",
                    "call",
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "NameHasOwner",
                    "s",
                    "org.freedesktop.login1",
                ])
                .output()
                .unwrap();
            if serde_json::from_slice::<Value>(&reply.stdout).unwrap()["data"][0] == true {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let log = dir.join("fake-login.jsonl");
        let monitor = Command::new("busctl")
            .args(["--address", &address, "--json=short", "--match=type='method_call',destination='org.freedesktop.login1',interface='org.freedesktop.login1.Manager',member='PowerOff'", "monitor"])
            .stdout(Stdio::from(File::create(&log).unwrap())).stderr(Stdio::null()).spawn().unwrap();
        Self {
            bus,
            service,
            monitor,
            address,
            log,
        }
    }
    fn power_off_calls(&self) -> usize {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .filter(|line| {
                let message: Value = serde_json::from_str(line).unwrap();
                if message["member"] != "PowerOff" {
                    return false;
                }
                assert_eq!(message["payload"]["data"], json!([false]));
                true
            })
            .count()
    }
}
impl Drop for FakeLogin {
    fn drop(&mut self) {
        for process in [&mut self.monitor, &mut self.service, &mut self.bus] {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

#[test]
fn protection_warns_cancels_and_calls_only_fake_logind_once() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeLogin::start(dir.path());
    let simulator = Simulator::new();
    let config = json!({"poll_interval_ms":1000,"request_timeout_ms":200,"reconnect_interval_ms":1000,"history_samples":10,"low_charge_percent":33.0,"high_load_percent":90.0,"high_temperature_c":60.0,
        "runtime":{"rated_output_w":1400.0,"battery_capacity_ah":9.0,"battery_health_percent":100.0,"inverter_efficiency_percent":85.0,"idle_power_w":15.0,"peukert_exponent":1.25,"capacity_rating_hours":20.0},
        "protection":{"enabled":true,"warning_seconds":90,"shutdown_seconds":50,"battery_mode_verified":true}});
    fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    let daemon = Daemon::start_with_bus(dir.path(), &simulator.path, Some(&fake.address));
    daemon.wait_for(|s| s["protection"]["phase"] == "waiting" && s["connection"] == "online");
    *simulator.status.lock().unwrap() = Some((0xb9, 110));
    let warning = daemon.wait_for(|s| s["protection"]["warning_active"] == true);
    assert_eq!(warning["protection"]["phase"], "warning");
    assert!(
        warning["protection"]["shutdown_in_seconds"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert_eq!(fake.power_off_calls(), 0);
    // Restored mains cancels the countdown before the shutdown threshold.
    *simulator.status.lock().unwrap() = Some((0x39, 110));
    daemon.wait_for(|s| {
        s["protection"]["phase"] == "waiting" && s["protection"]["warning_active"] == false
    });
    assert_eq!(fake.power_off_calls(), 0);
    // A second episode starts below the shutdown threshold.
    *simulator.status.lock().unwrap() = Some((0xb9, 60));
    daemon.wait_for(|s| s["protection"]["phase"] == "shutdown_requested");
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let checkpoint = fs::read(dir.path().join("protection-state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if checkpoint.is_some_and(|value| value["requested"] == true) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Power-off was not accepted: {}",
            fs::read_to_string(dir.path().join("daemon.log")).unwrap()
        );
        thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(fake.power_off_calls(), 1);
    drop(daemon);
    // Restarting with an accepted request never sends another power-off.
    let restarted = Daemon::start_with_bus(dir.path(), &simulator.path, Some(&fake.address));
    restarted.wait_for(|s| {
        s["protection"]["phase"] == "shutdown_requested" && s["connection"] == "online"
    });
    assert_eq!(fake.power_off_calls(), 1);
    assert!(
        simulator
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|frame| b"IFQ".contains(&frame[0]))
    );
}
