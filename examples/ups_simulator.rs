//! A pseudo-terminal UPS for UI development. It never accesses physical USB hardware.
use clap::Parser;
use rustix::{
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    termios::{OptionalActions, tcgetattr, tcsetattr},
};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    time::Instant,
};

#[derive(Parser)]
struct Options {
    #[arg(long)]
    battery: bool,
    #[arg(long, default_value_t = 100.0)]
    charge_percent: f64,
    #[arg(long)]
    hold_charge: bool,
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    anyhow::ensure!(
        (0.0..=100.0).contains(&options.charge_percent),
        "Charge must be 0..100 percent"
    );
    let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    grantpt(&controller)?;
    unlockpt(&controller)?;
    let name = ptsname(&controller, Vec::new())?;
    let path = PathBuf::from(OsStr::from_bytes(name.to_bytes()));
    let slave = File::open(&path)?;
    let mut settings = tcgetattr(&slave)?;
    settings.make_raw();
    tcsetattr(&slave, OptionalActions::Now, &settings)?;
    let mut master = File::from(controller);
    let replies: Vec<Value> = include_str!("../tests/fixtures/gamer-2000.jsonl")
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    println!("{}", path.display());
    std::io::stdout().flush()?;
    let started = Instant::now();
    loop {
        let mut request = [0u8; 7];
        master.read_exact(&mut request)?;
        if request[..6].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0 || request[6] != 13 {
            anyhow::bail!("Invalid simulator request");
        }
        let query = match request[0] {
            b'I' => "identity",
            b'F' => "ratings",
            b'Q' => "status",
            _ => {
                eprintln!("Simulated control: {}", hex::encode(request));
                continue;
            }
        };
        let value = replies.iter().find(|r| r["query"] == query).unwrap()["hex"]
            .as_str()
            .unwrap();
        let mut bytes = hex::decode(value.split_whitespace().collect::<String>())?;
        if query == "status" && options.battery {
            bytes[15] |= 128;
            let used = if options.hold_charge {
                0
            } else {
                started.elapsed().as_secs() / 3
            };
            let charge =
                ((options.charge_percent * 10.0).round() as u64).saturating_sub(used) as u16;
            bytes[11..13].copy_from_slice(&charge.to_be_bytes());
            bytes[16] = bytes[..16]
                .iter()
                .fold(0u8, |sum, b| sum.wrapping_add(*b))
                .wrapping_neg();
        }
        master.write_all(&bytes)?;
    }
}
