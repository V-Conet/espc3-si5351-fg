//! Text command protocol used by the BLE command characteristic.
//!
//! Commands are newline-terminated ASCII. Replies are short strings that fit in
//! a default 20-byte BLE payload. See README.md for the full table.

use crate::config::{MAX_PASSWORD_LEN, MAX_SSID_LEN};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    Status,
    Set {
        channel: usize,
        freq_hz: u64,
        enabled: Option<bool>,
        drive_ma: Option<u8>,
    },
    SetEnabled {
        channel: usize,
        enabled: bool,
    },
    Drive {
        channel: usize,
        drive_ma: u8,
    },
    Save,
    Reset,
    ForgetWifi,
    Sweep {
        channel: usize,
        start_hz: u64,
        end_hz: u64,
        step_hz: u64,
        dwell_ms: u32,
        repeat: bool,
    },
    SweepStop,
    Wifi {
        ssid: String,
        password: String,
    },
}

/// Parse one command line. The error is the reply to send back.
pub fn parse(line: &str) -> Result<Command, &'static str> {
    let mut parts = line.split_whitespace();
    let name = parts.next().unwrap_or("").to_ascii_uppercase();

    match name.as_str() {
        "PING" => Ok(Command::Ping),
        "STATUS" => Ok(Command::Status),
        "SET" => {
            let channel = channel_arg(parts.next()).ok_or("ERR CH")?;
            let freq_hz = parts
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or("ERR FREQ")?;
            let enabled = parts.next().and_then(|value| match value {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            });
            let drive_ma = match parts.next() {
                None => None,
                Some(value) => match value.parse::<u8>() {
                    Ok(ma @ (2 | 4 | 6 | 8)) => Some(ma),
                    _ => return Err("ERR DRIVE"),
                },
            };
            Ok(Command::Set {
                channel,
                freq_hz,
                enabled,
                drive_ma,
            })
        }
        "ON" | "OFF" => {
            let channel = channel_arg(parts.next()).ok_or("ERR CH")?;
            Ok(Command::SetEnabled {
                channel,
                enabled: name == "ON",
            })
        }
        "DRIVE" => {
            let channel = channel_arg(parts.next()).ok_or("ERR CH")?;
            let drive_ma = match parts.next().and_then(|value| value.parse::<u8>().ok()) {
                Some(ma @ (2 | 4 | 6 | 8)) => ma,
                _ => return Err("ERR DRIVE"),
            };
            Ok(Command::Drive { channel, drive_ma })
        }
        "SAVE" => Ok(Command::Save),
        "RESET" => Ok(Command::Reset),
        "FORGET" => Ok(Command::ForgetWifi),
        "SWEEP" => {
            let first = parts.next().unwrap_or("");
            if first.eq_ignore_ascii_case("STOP") {
                return Ok(Command::SweepStop);
            }
            let channel = channel_arg(Some(first)).ok_or("ERR CH")?;
            let start_hz = parse_u64(parts.next()).ok_or("ERR SWEEP")?;
            let end_hz = parse_u64(parts.next()).ok_or("ERR SWEEP")?;
            let step_hz = parse_u64(parts.next()).ok_or("ERR SWEEP")?;
            let dwell_ms = parts
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or("ERR SWEEP")?;
            let repeat = match parts.next() {
                None => false,
                Some("0") => false,
                Some("1") => true,
                Some(_) => return Err("ERR SWEEP"),
            };
            Ok(Command::Sweep {
                channel,
                start_hz,
                end_hz,
                step_hz,
                dwell_ms,
                repeat,
            })
        }
        "WIFI" => {
            let first = parts.next().unwrap_or("");
            if first.eq_ignore_ascii_case("CLEAR") || first.eq_ignore_ascii_case("FORGET") {
                return Ok(Command::ForgetWifi);
            }
            let ssid = hex_decode(first).ok_or("ERR WIFI")?;
            let password = hex_decode(parts.next().unwrap_or("-")).ok_or("ERR WIFI")?;
            let ssid = String::from_utf8(ssid).map_err(|_| "ERR WIFI")?;
            let password = String::from_utf8(password).map_err(|_| "ERR WIFI")?;
            if ssid.is_empty() || ssid.len() > MAX_SSID_LEN || password.len() > MAX_PASSWORD_LEN {
                return Err("ERR WIFI");
            }
            Ok(Command::Wifi { ssid, password })
        }
        _ => Err("ERR CMD"),
    }
}

fn channel_arg(value: Option<&str>) -> Option<usize> {
    value?.parse::<usize>().ok().filter(|channel| *channel <= 2)
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value?.parse::<u64>().ok()
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if value == "-" {
        return Some(Vec::new());
    }
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        output.push(((high << 4) | low) as u8);
    }
    Some(output)
}
