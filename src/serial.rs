use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{Access, FlockOperation, Mode, OFlags, access, flock, open},
    io::Errno,
    termios::{
        ControlModes, InputModes, OptionalActions, Termios, ioctl_tiocexcl, ioctl_tiocnxcl,
        tcgetattr, tcsetattr,
    },
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Baud {
    #[value(name = "2400")]
    B2400,
    #[value(name = "9600")]
    B9600,
}

impl Baud {
    pub fn rate(self) -> u32 {
        match self {
            Self::B2400 => 2400,
            Self::B9600 => 9600,
        }
    }
}

pub struct UsbDevice {
    pub path: PathBuf,
    pub interfaces: Vec<(PathBuf, Option<PathBuf>)>,
    pub ports: Vec<PathBuf>,
}

/// Enumerate only this bridge; do not open or query unrelated USB devices.
pub fn usb_devices() -> Result<Vec<UsbDevice>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/bus/usb/devices")? {
        let path = entry?.path();
        let matches = ["idVendor", "idProduct"]
            .map(|name| fs::read_to_string(path.join(name)).unwrap_or_default());
        if matches[0].trim() != "067b" || matches[1].trim() != "23a3" {
            continue;
        }
        let mut interfaces = Vec::new();
        let mut ports = Vec::new();
        let mut directories = vec![path.clone()];
        // Sysfs links point back into the tree. Recurse through directories only.
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("ttyUSB") {
                    let port = Path::new("/dev").join(name.as_ref());
                    if port.exists() {
                        ports.push(port);
                    }
                }
                if entry.file_type()?.is_dir() {
                    if name.starts_with(&format!(
                        "{}:",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )) {
                        let driver = fs::canonicalize(entry.path().join("driver")).ok();
                        interfaces.push((entry.path(), driver));
                    }
                    directories.push(entry.path());
                }
            }
        }
        ports.sort();
        ports.dedup();
        interfaces.sort();
        devices.push(UsbDevice {
            path,
            interfaces,
            ports,
        });
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

pub fn diagnose() -> Result<()> {
    let uname = rustix::system::uname();
    let kernel = uname.release().to_string_lossy();
    println!("Running kernel: {kernel}");
    let mut trees = fs::read_dir("/usr/lib/modules")?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    trees.sort();
    println!("Installed module trees: {}", trees.join(", "));
    if !Path::new("/usr/lib/modules").join(kernel.as_ref()).exists() {
        println!(
            "NOTE: module tree for the running kernel is missing, likely after a kernel upgrade."
        );
    }
    let devices = usb_devices()?;
    ensure!(!devices.is_empty(), "USB bridge 067b:23a3 not found");
    let mut ready = false;
    for device in devices {
        println!(
            "USB bridge: {} 067b:23a3",
            device
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        for (interface, driver) in device.interfaces {
            println!(
                "  {} driver: {}",
                interface.file_name().unwrap_or_default().to_string_lossy(),
                driver
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|s| s.to_string_lossy())
                    .unwrap_or("NONE".into())
            );
        }
        if device.ports.is_empty() {
            println!("  BLOCKED: no serial port. The Linux pl2303 driver must bind first.");
        }
        for port in device.ports {
            let allowed = access(&port, Access::READ_OK | Access::WRITE_OK).is_ok();
            println!(
                "  {}: {}",
                port.display(),
                if allowed {
                    "read/write access available"
                } else {
                    "BLOCKED: permission denied"
                }
            );
            ready |= allowed;
        }
    }
    ensure!(ready, "No accessible UPS serial port");
    Ok(())
}

pub fn resolve_port(requested: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(port) = requested {
        return Ok(port);
    }
    let mut ports: Vec<_> = usb_devices()?
        .into_iter()
        .flat_map(|device| device.ports)
        .collect();
    ensure!(
        ports.len() == 1,
        "Expected one UPS serial port, found {}. Run 'sms-gamer diagnose'.",
        ports.len()
    );
    Ok(ports.remove(0))
}

/// Own the serial configuration and exclusive lock until the session ends.
pub struct Serial {
    file: File,
    original: Termios,
}

impl Serial {
    pub fn open(path: &Path, baud: Baud) -> Result<Self> {
        let file = File::from(
            open(
                path,
                OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .with_context(|| format!("Could not open {}", path.display()))?,
        );
        flock(&file, FlockOperation::NonBlockingLockExclusive)
            .context("Serial port is already locked")?;
        let original = tcgetattr(&file).context("Could not read serial configuration")?;
        ioctl_tiocexcl(&file).context("Could not claim serial port exclusively")?;
        let serial = Self { file, original };
        let mut settings = serial.original.clone();
        settings.make_raw();
        settings
            .input_modes
            .remove(InputModes::IXON | InputModes::IXOFF | InputModes::IXANY);
        settings.control_modes.remove(
            ControlModes::PARENB
                | ControlModes::CSTOPB
                | ControlModes::CSIZE
                | ControlModes::CRTSCTS
                | ControlModes::HUPCL,
        );
        settings
            .control_modes
            .insert(ControlModes::CS8 | ControlModes::CLOCAL | ControlModes::CREAD);
        settings.set_speed(baud.rate())?;
        tcsetattr(&serial.file, OptionalActions::Now, &settings)
            .context("Could not configure serial port")?;
        Ok(serial)
    }

    pub fn write_request(&mut self, request: &[u8; 7]) -> Result<usize> {
        self.file.write(request).context("Serial write failed")
    }

    /// A whole-frame deadline, not a per-byte timeout. CR may occur in binary fields.
    pub fn read_bytes(
        &mut self,
        timeout: Duration,
        limit: usize,
        stop: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut data = vec![0; limit];
        let mut count = 0;
        while count < limit && !stop.load(Ordering::Relaxed) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let wait = Timespec::try_from(remaining.min(Duration::from_millis(100)))?;
            let mut fds = [PollFd::new(&self.file, PollFlags::IN)];
            match poll(&mut fds, Some(&wait)) {
                Err(Errno::INTR) => continue,
                result => {
                    result.context("Serial poll failed")?;
                }
            }
            let events = fds[0].revents();
            if events.contains(PollFlags::IN) {
                match self.file.read(&mut data[count..]) {
                    Ok(0) => bail!("Serial device disconnected"),
                    Ok(read) => count += read,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error).context("Serial read failed"),
                }
            } else if events.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
                bail!("Serial device disconnected or failed: {events:?}");
            }
        }
        data.truncate(count);
        Ok(data)
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        if let Err(error) = tcsetattr(&self.file, OptionalActions::Now, &self.original) {
            eprintln!("Could not restore serial configuration: {error}");
        }
        if let Err(error) = ioctl_tiocnxcl(&self.file) {
            eprintln!("Could not release exclusive serial mode: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, thread};

    fn terminal() -> (File, PathBuf) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let path = PathBuf::from(OsStr::from_bytes(name.to_bytes()));
        (File::from(master), path)
    }

    #[test]
    fn exclusive_port_and_configuration_are_restored_on_drop() {
        let (_master, path) = terminal();
        let observer = File::open(&path).unwrap();
        let original = tcgetattr(&observer).unwrap();
        let serial = Serial::open(&path, Baud::B2400).unwrap();
        let configured = tcgetattr(&observer).unwrap();
        assert_eq!(configured.input_speed(), 2400);
        assert!(!configured.control_modes.contains(ControlModes::CRTSCTS));
        assert!(File::open(&path).is_err());
        drop(serial);
        assert!(File::open(&path).is_ok());
        let restored = tcgetattr(&observer).unwrap();
        assert_eq!(restored.input_modes, original.input_modes);
        assert_eq!(restored.output_modes, original.output_modes);
        assert_eq!(restored.control_modes, original.control_modes);
        assert_eq!(restored.local_modes, original.local_modes);
        assert_eq!(restored.input_speed(), original.input_speed());
        assert_eq!(restored.output_speed(), original.output_speed());
    }

    #[test]
    fn fragmented_binary_reply_with_embedded_cr_is_read_in_full() {
        let (mut master, path) = terminal();
        let mut serial = Serial::open(&path, Baud::B2400).unwrap();
        let bytes = hex::decode("3d0000080d086000e9025703e801e539fa0d").unwrap();
        let expected = bytes.clone();
        let writer = thread::spawn(move || {
            for chunk in bytes.chunks(5) {
                master.write_all(chunk).unwrap();
                thread::sleep(Duration::from_millis(10));
            }
            master
        });
        let received = serial
            .read_bytes(Duration::from_secs(1), 18, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(received, expected);
        let master = writer.join().unwrap();
        drop(serial);
        drop(master);
    }
}
