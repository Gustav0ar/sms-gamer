use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize, Serializer};

/// Only documented monitoring requests can be represented or transmitted.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Query {
    Identity,
    Status,
    Ratings,
}

impl Query {
    pub fn request(self) -> [u8; 7] {
        let command = match self {
            Self::Identity => b'I',
            Self::Status => b'Q',
            Self::Ratings => b'F',
        };
        let checksum = command.wrapping_sub(4).wrapping_neg();
        [command, 0xff, 0xff, 0xff, 0xff, checksum, b'\r']
    }

    pub fn decode(self, bytes: &[u8]) -> Result<Reply> {
        match self {
            Self::Identity => Identity::decode(bytes).map(Reply::Identity),
            Self::Status => Status::decode(bytes).map(Reply::Status),
            Self::Ratings => Ratings::decode(bytes).map(Reply::Ratings),
        }
    }
}

pub trait Response: Sized {
    const QUERY: Query;
    fn decode(bytes: &[u8]) -> Result<Self>;
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Reply {
    Identity(Identity),
    Status(Status),
    Ratings(Ratings),
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Identity {
    pub model: String,
    pub firmware: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Ratings {
    pub voltage_range_code: String,
    pub current_range_code: String,
    pub battery_nominal_v: u16,
    pub output_frequency_nominal_hz: u16,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Status {
    pub last_input_voltage_v: f64,
    pub input_voltage_v: f64,
    pub output_voltage_v: f64,
    pub load_percent: f64,
    pub output_frequency_hz: f64,
    pub battery_charge_percent: f64,
    pub temperature_c: f64,
    pub frame_type: char,
    #[serde(serialize_with = "serialize_hex_byte")]
    pub status_byte: u8,
    pub status_bits_msb_first: String,
}

pub fn serialize_hex_byte<S: Serializer>(byte: &u8, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("0x{byte:02x}"))
}

fn frame(query: Query, bytes: &[u8]) -> Result<&[u8; 18]> {
    let bytes: &[u8; 18] = bytes.try_into().with_context(|| {
        format!(
            "Expected an 18-byte reply, got {} bytes: {}",
            bytes.len(),
            hex::encode(bytes)
        )
    })?;
    ensure!(bytes[17] == b'\r', "Reply must end in CR");
    let checksum = bytes[..17]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    ensure!(
        checksum == 0,
        "Invalid reply checksum: {}",
        hex::encode(bytes)
    );
    let headers: &[u8] = match query {
        Query::Status => b"<=>",
        Query::Identity | Query::Ratings => b":;",
    };
    ensure!(
        headers.contains(&bytes[0]),
        "Unexpected {query:?} reply header: 0x{:02x}",
        bytes[0]
    );
    Ok(bytes)
}

fn ascii(bytes: &[u8]) -> Result<&str> {
    ensure!(
        bytes
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' '),
        "Non-printable ASCII field in reply"
    );
    std::str::from_utf8(bytes).context("Invalid ASCII field in reply")
}

impl Response for Identity {
    const QUERY: Query = Query::Identity;

    fn decode(bytes: &[u8]) -> Result<Self> {
        let bytes = frame(Self::QUERY, bytes)?;
        Ok(Self {
            model: ascii(&bytes[1..13])?.trim().to_owned(),
            firmware: ascii(&bytes[13..16])?.trim().to_owned(),
        })
    }
}

impl Response for Ratings {
    const QUERY: Query = Query::Ratings;

    fn decode(bytes: &[u8]) -> Result<Self> {
        let bytes = frame(Self::QUERY, bytes)?;
        Ok(Self {
            voltage_range_code: ascii(&bytes[1..8])?.to_owned(),
            current_range_code: ascii(&bytes[8..11])?.to_owned(),
            battery_nominal_v: ascii(&bytes[11..14])?
                .trim()
                .parse()
                .context("Invalid nominal battery voltage")?,
            output_frequency_nominal_hz: ascii(&bytes[14..16])?
                .trim()
                .parse()
                .context("Invalid nominal frequency")?,
        })
    }
}

impl Response for Status {
    const QUERY: Query = Query::Status;

    fn decode(bytes: &[u8]) -> Result<Self> {
        let bytes = frame(Self::QUERY, bytes)?;
        let word =
            |offset| f64::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]])) / 10.0;
        Ok(Self {
            last_input_voltage_v: word(1),
            input_voltage_v: word(3),
            output_voltage_v: word(5),
            load_percent: word(7),
            output_frequency_hz: word(9),
            battery_charge_percent: word(11),
            temperature_c: word(13),
            frame_type: char::from(bytes[0]),
            // Preserve unknown flags without assigning semantics to them.
            status_byte: bytes[15],
            status_bits_msb_first: format!("{:08b}", bytes[15]),
        })
    }
}

pub fn parse_hex(value: &str) -> Result<Vec<u8>> {
    hex::decode(value.split_ascii_whitespace().collect::<String>())
        .context("Invalid hexadecimal capture")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture(query: &str) -> Vec<u8> {
        include_str!("../tests/fixtures/gamer-2000.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["query"] == query)
            .map(|event| parse_hex(event["hex"].as_str().unwrap()).unwrap())
            .unwrap()
    }

    #[test]
    fn real_identity_and_ratings() {
        assert_eq!(
            Identity::decode(&fixture("identity")).unwrap(),
            Identity {
                model: "GAM2000BiBiW".to_owned(),
                firmware: "1.0".to_owned(),
            }
        );
        assert_eq!(
            Ratings::decode(&fixture("ratings")).unwrap(),
            Ratings {
                voltage_range_code: "EBiS220".to_owned(),
                current_range_code: "000".to_owned(),
                battery_nominal_v: 24,
                output_frequency_nominal_hz: 60,
            }
        );
    }

    #[test]
    fn real_status_preserves_fractional_values_and_unknown_bits() {
        let status = Status::decode(&fixture("status")).unwrap();
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({
                "last_input_voltage_v": 0.0, "input_voltage_v": 215.5, "output_voltage_v": 214.4,
                "load_percent": 23.3, "output_frequency_hz": 59.9, "battery_charge_percent": 100.0,
                "temperature_c": 48.5, "frame_type": "=", "status_byte": "0x39",
                "status_bits_msb_first": "00111001",
            })
        );
    }

    #[test]
    fn corrupt_incomplete_and_wrong_replies_are_rejected() {
        let mut corrupt = fixture("status");
        corrupt[3] ^= 1;
        let mut oversized = fixture("status");
        oversized.push(0);
        for bytes in [
            corrupt,
            fixture("status")[..17].to_vec(),
            oversized,
            fixture("identity"),
        ] {
            assert!(Status::decode(&bytes).is_err());
        }
    }

    #[test]
    fn exact_monitoring_requests_and_allowlist() {
        assert_eq!(hex::encode(Query::Identity.request()), "49ffffffffbb0d");
        assert_eq!(hex::encode(Query::Status.request()), "51ffffffffb30d");
        assert_eq!(hex::encode(Query::Ratings.request()), "46ffffffffbe0d");
        assert!(serde_json::from_str::<Query>("\"shutdown\"").is_err());
    }
}
