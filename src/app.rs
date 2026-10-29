//! Shared application state and command execution.
//!
//! The BLE GATT server, HTTP server and Wi-Fi worker mutate this state. All
//! Si5351 updates go through `Core`'s mutex, so a multi-register clock-tree
//! write is never interleaved with another channel change.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Instant;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use crate::config::{
    FW_VERSION, MAX_BLE_COMMAND_LEN, MAX_BLE_REPLY_LEN, MAX_PASSWORD_LEN, MAX_SSID_LEN,
    NVS_KEY_CHANNELS, NVS_KEY_WIFI_PASS, NVS_KEY_WIFI_SSID, NVS_NAMESPACE,
};
use crate::protocol::{self, Command};
use crate::si5351::{ChannelConfig, Error as Si5351Error, Si5351, FREQ_MAX_HZ, FREQ_MIN_HZ};

pub const DEFAULT_CHANNELS: [ChannelConfig; 3] = [
    ChannelConfig::new(10_000_000, true, 8),
    ChannelConfig::new(1_000_000, false, 8),
    ChannelConfig::new(1_000_000, false, 8),
];

pub const MAX_SWEEP_POINTS: u64 = 20_000;
pub const SWEEP_MIN_DWELL_MS: u32 = 20;
pub const SWEEP_MAX_DWELL_MS: u32 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    pub channel: usize,
    pub start_hz: u64,
    pub end_hz: u64,
    pub step_hz: u64,
    pub dwell_ms: u32,
    pub repeat: bool,
}

impl SweepConfig {
    pub fn points(&self) -> u64 {
        if self.step_hz == 0 {
            return 0;
        }
        self.start_hz.abs_diff(self.end_hz) / self.step_hz + 1
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.channel > 2 {
            return Err("sweep channel must be 0, 1 or 2".into());
        }
        for (name, freq) in [("start", self.start_hz), ("end", self.end_hz)] {
            if !(FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&freq) {
                return Err(format!(
                    "sweep {name} frequency out of range ({FREQ_MIN_HZ}..={FREQ_MAX_HZ} Hz)"
                ));
            }
        }
        if self.step_hz == 0 || self.step_hz > FREQ_MAX_HZ {
            return Err("sweep step must be 1..=160000000 Hz".into());
        }
        if !(SWEEP_MIN_DWELL_MS..=SWEEP_MAX_DWELL_MS).contains(&self.dwell_ms) {
            return Err(format!(
                "sweep dwell must be {SWEEP_MIN_DWELL_MS}..={SWEEP_MAX_DWELL_MS} ms"
            ));
        }
        if self.points() > MAX_SWEEP_POINTS {
            return Err(format!("sweep is limited to {MAX_SWEEP_POINTS} points"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SweepState {
    pub running: bool,
    pub config: Option<SweepConfig>,
    pub current_hz: u64,
}

pub enum SweepCmd {
    Start(SweepConfig),
    Stop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum WifiState {
    #[default]
    Idle = 0,
    Connecting = 1,
    Connected = 2,
    Error = 3,
    /// Open AP is running, no STA connection.
    AccessPoint = 4,
}

#[derive(Clone, Debug, Default)]
pub struct WifiInfo {
    pub state: WifiState,
    pub ssid: String,
    pub ip: String,
    pub message: String,
}

pub enum WifiCmd {
    Connect { ssid: String, password: String },
    Forget,
}

pub struct Outbound {
    pub conn: u16,
    pub data: String,
}

/// Attribute handles learned from the GATT registration events.
#[derive(Default)]
pub struct BleHandles {
    pub cmd: AtomicU16,
    pub rsp: AtomicU16,
    pub ch: [AtomicU16; 3],
    pub wifi: AtomicU16,
    pub info: AtomicU16,
    pub sweep: AtomicU16,
}

struct Core {
    si: Si5351,
    channels: [ChannelConfig; 3],
}

pub struct App {
    core: Mutex<Core>,
    pub wifi: Mutex<WifiInfo>,
    nvs: Mutex<Option<EspNvs<NvsDefault>>>,
    wifi_tx: Mutex<Option<Sender<WifiCmd>>>,
    out_tx: Mutex<Sender<Outbound>>,
    sweep: Mutex<SweepState>,
    sweep_tx: Mutex<Option<Sender<SweepCmd>>>,
    pub ble_conn: AtomicU16,
    pub ble_subscribers: Mutex<Vec<u16>>,
    pub last_rsp: Mutex<String>,
    cmd_buf: Mutex<Vec<u8>>,
    pub adv_needed: AtomicBool,
    pub ble_connected: AtomicBool,
    wifi_reason: AtomicU16,
    si_ok: AtomicBool,
    pub handles: BleHandles,
    started: Instant,
}

impl App {
    pub fn new(
        si: Si5351,
        channels: [ChannelConfig; 3],
        nvs: Option<EspNvs<NvsDefault>>,
        out_tx: Sender<Outbound>,
    ) -> Self {
        Self {
            core: Mutex::new(Core { si, channels }),
            wifi: Mutex::new(WifiInfo::default()),
            nvs: Mutex::new(nvs),
            wifi_tx: Mutex::new(None),
            out_tx: Mutex::new(out_tx),
            sweep: Mutex::new(SweepState::default()),
            sweep_tx: Mutex::new(None),
            ble_conn: AtomicU16::new(0),
            ble_subscribers: Mutex::new(Vec::new()),
            last_rsp: Mutex::new("READY".to_string()),
            cmd_buf: Mutex::new(Vec::new()),
            adv_needed: AtomicBool::new(false),
            ble_connected: AtomicBool::new(false),
            wifi_reason: AtomicU16::new(0),
            si_ok: AtomicBool::new(false),
            handles: BleHandles::default(),
            started: Instant::now(),
        }
    }

    pub fn set_wifi_sender(&self, tx: Sender<WifiCmd>) {
        *self.wifi_tx.lock().unwrap() = Some(tx);
    }

    pub fn set_sweep_sender(&self, tx: Sender<SweepCmd>) {
        *self.sweep_tx.lock().unwrap() = Some(tx);
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn channels(&self) -> [ChannelConfig; 3] {
        self.core.lock().unwrap().channels
    }

    pub fn si_ok(&self) -> bool {
        self.si_ok.load(Ordering::Relaxed)
    }

    /// Rewrite the currently stored channel configuration.
    pub fn apply_current(&self) -> Result<(), Si5351Error> {
        let mut core = self.core.lock().unwrap();
        let channels = core.channels;
        match core.si.configure(&channels) {
            Ok(()) => {
                self.si_ok.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => {
                // A planning/validation error never touches the bus, so the
                // previous health state remains valid.
                if err.is_i2c() {
                    self.si_ok.store(false, Ordering::Relaxed);
                }
                Err(err)
            }
        }
    }

    /// Update one output. The whole clock tree is re-planned and written; an
    /// I2C failure attempts to restore the previous configuration.
    pub fn set_channel(
        &self,
        channel: usize,
        freq_hz: Option<u64>,
        enabled: Option<bool>,
        drive_ma: Option<u8>,
    ) -> Result<ChannelConfig, Si5351Error> {
        if channel > 2 {
            return Err(Si5351Error::Config("channel must be 0, 1 or 2".into()));
        }

        let mut core = self.core.lock().unwrap();
        let mut next = core.channels;
        if let Some(freq) = freq_hz {
            next[channel].freq_hz = freq;
        }
        if let Some(enabled) = enabled {
            next[channel].enabled = enabled;
        }
        if let Some(drive) = drive_ma {
            next[channel].drive_ma = drive;
        }

        self.apply_locked(&mut core, next)?;
        Ok(next[channel])
    }

    /// Apply a complete three-channel configuration atomically.
    pub fn set_all(&self, next: [ChannelConfig; 3]) -> Result<(), Si5351Error> {
        let mut core = self.core.lock().unwrap();
        self.apply_locked(&mut core, next)
    }

    pub fn reset_defaults(&self) -> Result<(), Si5351Error> {
        self.set_all(DEFAULT_CHANNELS)
    }

    fn apply_locked(&self, core: &mut Core, next: [ChannelConfig; 3]) -> Result<(), Si5351Error> {
        let previous = core.channels;
        match core.si.configure(&next) {
            Ok(()) => {
                core.channels = next;
                self.si_ok.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => {
                // Only an I2C failure can leave the chip partially written;
                // planning/validation errors happen before the first write.
                if err.is_i2c() {
                    let restored = core.si.configure(&previous);
                    self.si_ok.store(restored.is_ok(), Ordering::Relaxed);
                }
                Err(err)
            }
        }
    }

    pub fn channel_value(&self, channel: usize) -> String {
        self.channels()
            .get(channel)
            .copied()
            .unwrap_or_default()
            .value_string()
    }

    pub fn wifi_value(&self) -> String {
        let wifi = self.wifi.lock().unwrap();
        format!("{},{},{}", wifi.state as u8, wifi.ip, self.wifi_reason())
    }

    pub fn info_value(&self) -> String {
        format!(
            "{},{},{}",
            FW_VERSION,
            self.uptime_secs(),
            u8::from(self.si_ok())
        )
    }

    pub fn read_characteristic(&self, handle: u16) -> Option<String> {
        if handle == 0 {
            return None;
        }
        let handles = &self.handles;
        if handle == handles.ch[0].load(Ordering::Relaxed) {
            return Some(self.channel_value(0));
        }
        if handle == handles.ch[1].load(Ordering::Relaxed) {
            return Some(self.channel_value(1));
        }
        if handle == handles.ch[2].load(Ordering::Relaxed) {
            return Some(self.channel_value(2));
        }
        if handle == handles.wifi.load(Ordering::Relaxed) {
            return Some(self.wifi_value());
        }
        if handle == handles.info.load(Ordering::Relaxed) {
            return Some(self.info_value());
        }
        if handle == handles.sweep.load(Ordering::Relaxed) {
            return Some(self.sweep_value());
        }
        if handle == handles.rsp.load(Ordering::Relaxed) {
            return Some(self.last_rsp.lock().unwrap().clone());
        }
        None
    }

    // ------------------------------------------------------------------
    // BLE commands
    // ------------------------------------------------------------------

    /// Append a characteristic write; returns all complete newline-terminated
    /// command lines. Fragmented writes (MTU-sized chunks) are reassembled here.
    pub fn push_ble_data(&self, data: &[u8]) -> Result<Vec<String>, String> {
        let mut buffer = self.cmd_buf.lock().unwrap();
        if buffer.len() + data.len() > MAX_BLE_COMMAND_LEN {
            buffer.clear();
            return Err("command too long".into());
        }
        buffer.extend_from_slice(data);

        let mut lines = Vec::new();
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim()
                .to_string();
            if !line.is_empty() {
                lines.push(line);
            }
        }
        Ok(lines)
    }

    pub fn handle_command(&self, line: &str, conn: u16) {
        self.ble_conn.store(conn, Ordering::Relaxed);

        let reply = match protocol::parse(line) {
            Ok(Command::Ping) => "OK PONG".to_string(),
            Ok(Command::Status) => "OK".to_string(),
            Ok(Command::Set {
                channel,
                freq_hz,
                enabled,
                drive_ma,
            }) => match self.set_channel(channel, Some(freq_hz), enabled, drive_ma) {
                Ok(config) => format!("OK S{channel} {}", config.freq_hz),
                Err(err) => self.error_reply("SET", &err),
            },
            Ok(Command::SetEnabled { channel, enabled }) => {
                match self.set_channel(channel, None, Some(enabled), None) {
                    Ok(_) => format!("OK O{channel} {}", u8::from(enabled)),
                    Err(err) => self.error_reply("output change", &err),
                }
            }
            Ok(Command::Drive { channel, drive_ma }) => {
                match self.set_channel(channel, None, None, Some(drive_ma)) {
                    Ok(_) => format!("OK D{channel} {drive_ma}"),
                    Err(err) => self.error_reply("drive change", &err),
                }
            }
            Ok(Command::Save) => match self.save_channels() {
                Ok(()) => "OK SAVE".to_string(),
                Err(err) => {
                    log::warn!("save failed: {err}");
                    "ERR SAVE".to_string()
                }
            },
            Ok(Command::Reset) => match self.reset_defaults() {
                Ok(()) => "OK RESET".to_string(),
                Err(err) => self.error_reply("reset", &err),
            },
            Ok(Command::ForgetWifi) => {
                if self.request_forget_wifi() {
                    "OK FORGET".to_string()
                } else {
                    "ERR WIFI".to_string()
                }
            }
            Ok(Command::Sweep {
                channel,
                start_hz,
                end_hz,
                step_hz,
                dwell_ms,
                repeat,
            }) => {
                let config = SweepConfig {
                    channel,
                    start_hz,
                    end_hz,
                    step_hz,
                    dwell_ms,
                    repeat,
                };
                match self.request_sweep(config) {
                    Ok(()) => "OK SWEEP".to_string(),
                    Err(err) => {
                        log::warn!("sweep request rejected: {err}");
                        "ERR SWEEP".to_string()
                    }
                }
            }
            Ok(Command::SweepStop) => {
                if self.request_sweep_stop() {
                    "OK STOP".to_string()
                } else {
                    "ERR SWEEP".to_string()
                }
            }
            Ok(Command::Wifi { ssid, password }) => {
                if self.request_wifi(ssid, password) {
                    "OK WIFI".to_string()
                } else {
                    "ERR WIFI".to_string()
                }
            }
            Err(reply) => reply.to_string(),
        };

        self.respond(conn, &reply);
    }

    fn error_reply(&self, action: &str, err: &Si5351Error) -> String {
        log::warn!("{action} failed: {err}");
        if err.is_i2c() {
            "ERR I2C".to_string()
        } else {
            "ERR SYNTH".to_string()
        }
    }

    pub fn respond(&self, conn: u16, message: &str) {
        let message: String = message.chars().take(MAX_BLE_REPLY_LEN).collect();
        *self.last_rsp.lock().unwrap() = message.clone();
        if self.is_subscribed(conn) {
            let _ = self.out_tx.lock().unwrap().send(Outbound {
                conn,
                data: message,
            });
        }
    }

    pub fn notify_last_conn(&self, message: &str) {
        let conn = self.ble_conn.load(Ordering::Relaxed);
        if conn != 0 {
            self.respond(conn, message);
        }
    }

    pub fn set_subscribed(&self, conn: u16, subscribed: bool) {
        let mut subscribers = self.ble_subscribers.lock().unwrap();
        subscribers.retain(|&existing| existing != conn);
        if subscribed {
            subscribers.push(conn);
        }
    }

    pub fn is_subscribed(&self, conn: u16) -> bool {
        self.ble_subscribers.lock().unwrap().contains(&conn)
    }

    // ------------------------------------------------------------------
    // Frequency sweep
    // ------------------------------------------------------------------

    pub fn request_sweep(&self, config: SweepConfig) -> Result<(), String> {
        config.validate()?;
        if self.send_sweep_command(SweepCmd::Start(config)) {
            Ok(())
        } else {
            Err("sweep worker is unavailable".into())
        }
    }

    pub fn request_sweep_stop(&self) -> bool {
        self.send_sweep_command(SweepCmd::Stop)
    }

    fn send_sweep_command(&self, command: SweepCmd) -> bool {
        let sender = self.sweep_tx.lock().unwrap();
        match sender.as_ref() {
            Some(sender) => sender.send(command).is_ok(),
            None => false,
        }
    }

    pub fn sweep_status(&self) -> (bool, usize, u64) {
        let sweep = self.sweep.lock().unwrap();
        (
            sweep.running,
            sweep.config.map(|config| config.channel).unwrap_or(0),
            sweep.current_hz,
        )
    }

    pub fn sweep_value(&self) -> String {
        let (running, channel, current) = self.sweep_status();
        format!("{},{channel},{current}", u8::from(running))
    }

    pub fn set_sweep_started(&self, config: SweepConfig) {
        let mut sweep = self.sweep.lock().unwrap();
        sweep.running = true;
        sweep.config = Some(config);
        sweep.current_hz = config.start_hz;
    }

    pub fn set_sweep_current(&self, freq_hz: u64) {
        self.sweep.lock().unwrap().current_hz = freq_hz;
    }

    pub fn set_sweep_stopped(&self) {
        self.sweep.lock().unwrap().running = false;
    }

    // ------------------------------------------------------------------
    // Wi-Fi
    // ------------------------------------------------------------------

    pub fn request_wifi(&self, ssid: String, password: String) -> bool {
        self.send_wifi_command(WifiCmd::Connect { ssid, password })
    }

    pub fn request_forget_wifi(&self) -> bool {
        self.send_wifi_command(WifiCmd::Forget)
    }

    fn send_wifi_command(&self, command: WifiCmd) -> bool {
        let sender = self.wifi_tx.lock().unwrap();
        match sender.as_ref() {
            Some(sender) => sender.send(command).is_ok(),
            None => false,
        }
    }

    pub fn set_wifi_connecting(&self, ssid: &str) {
        self.wifi_reason.store(0, Ordering::Relaxed);
        let mut wifi = self.wifi.lock().unwrap();
        wifi.state = WifiState::Connecting;
        wifi.ssid = ssid.to_string();
        wifi.ip.clear();
        wifi.message.clear();
    }

    pub fn set_wifi_connected(&self, ssid: &str, ip: &str) {
        self.wifi_reason.store(0, Ordering::Relaxed);
        let mut wifi = self.wifi.lock().unwrap();
        wifi.state = WifiState::Connected;
        wifi.ssid = ssid.to_string();
        wifi.ip = ip.to_string();
        wifi.message.clear();
    }

    pub fn set_wifi_access_point(&self, ssid: &str, ip: &str) {
        self.wifi_reason.store(0, Ordering::Relaxed);
        let mut wifi = self.wifi.lock().unwrap();
        wifi.state = WifiState::AccessPoint;
        wifi.ssid = ssid.to_string();
        wifi.ip = ip.to_string();
        wifi.message.clear();
    }

    /// Record a failed STA attempt. The disconnect reason is kept as-is; it is
    /// filled in by the system-event subscription in `main`.
    pub fn set_wifi_error(&self, ssid: &str, message: &str) {
        let mut wifi = self.wifi.lock().unwrap();
        wifi.state = WifiState::Error;
        wifi.ssid = ssid.to_string();
        wifi.ip.clear();
        wifi.message = message.to_string();
    }

    pub fn set_wifi_reason(&self, reason: u16) {
        self.wifi_reason.store(reason, Ordering::Relaxed);
    }

    pub fn wifi_reason(&self) -> u16 {
        self.wifi_reason.load(Ordering::Relaxed)
    }

    // ------------------------------------------------------------------
    // NVS persistence
    // ------------------------------------------------------------------

    pub fn save_channels(&self) -> Result<(), String> {
        let channels = self.channels();
        let value = channels
            .iter()
            .map(ChannelConfig::value_string)
            .collect::<Vec<_>>()
            .join(";");
        let nvs = self.nvs.lock().unwrap();
        let nvs = nvs.as_ref().ok_or("NVS is not available")?;
        nvs.set_str(NVS_KEY_CHANNELS, &value)
            .map_err(|err| format!("NVS write failed: {err}"))
    }

    pub fn save_wifi_credentials(&self, ssid: &str, password: &str) {
        let nvs = self.nvs.lock().unwrap();
        if let Some(nvs) = nvs.as_ref() {
            if let Err(err) = nvs.set_str(NVS_KEY_WIFI_SSID, ssid) {
                log::warn!("could not save Wi-Fi SSID: {err}");
            }
            if let Err(err) = nvs.set_str(NVS_KEY_WIFI_PASS, password) {
                log::warn!("could not save Wi-Fi password: {err}");
            }
        }
    }

    /// Remove the saved STA credentials so the next boot stays in AP-only mode.
    pub fn clear_wifi_credentials(&self) {
        let nvs = self.nvs.lock().unwrap();
        if let Some(nvs) = nvs.as_ref() {
            for key in [NVS_KEY_WIFI_SSID, NVS_KEY_WIFI_PASS] {
                if let Err(err) = nvs.remove(key) {
                    log::warn!("could not remove NVS key {key}: {err}");
                    let _ = nvs.set_str(key, "");
                }
            }
        }
    }
}

pub fn open_nvs(partition: EspDefaultNvsPartition) -> Option<EspNvs<NvsDefault>> {
    EspNvs::new(partition, NVS_NAMESPACE, true)
        .map_err(|err| log::warn!("could not open NVS namespace: {err}"))
        .ok()
}

pub fn load_channels(nvs: &Option<EspNvs<NvsDefault>>) -> [ChannelConfig; 3] {
    let Some(nvs) = nvs.as_ref() else {
        return DEFAULT_CHANNELS;
    };
    let mut buffer = [0u8; 128];
    let Ok(Some(value)) = nvs.get_str(NVS_KEY_CHANNELS, &mut buffer) else {
        return DEFAULT_CHANNELS;
    };
    parse_channels(value).unwrap_or(DEFAULT_CHANNELS)
}

pub fn load_wifi_credentials(nvs: &Option<EspNvs<NvsDefault>>) -> Option<(String, String)> {
    let nvs = nvs.as_ref()?;
    let mut ssid_buf = [0u8; MAX_SSID_LEN + 1];
    let ssid = nvs
        .get_str(NVS_KEY_WIFI_SSID, &mut ssid_buf)
        .ok()
        .flatten()?
        .to_string();
    if ssid.is_empty() {
        return None;
    }
    let mut pass_buf = [0u8; MAX_PASSWORD_LEN + 1];
    let password = nvs
        .get_str(NVS_KEY_WIFI_PASS, &mut pass_buf)
        .ok()
        .flatten()
        .unwrap_or("")
        .to_string();
    Some((ssid, password))
}

fn parse_channels(value: &str) -> Option<[ChannelConfig; 3]> {
    let mut channels = DEFAULT_CHANNELS;
    let mut count = 0;
    for (index, item) in value.split(';').enumerate() {
        if index > 2 {
            return None;
        }
        let mut fields = item.split(',');
        let channel = ChannelConfig::new(
            fields.next()?.parse::<u64>().ok()?,
            fields.next()? == "1",
            fields.next()?.parse::<u8>().ok()?,
        );
        if !channel.is_valid() {
            return None;
        }
        channels[index] = channel;
        count += 1;
    }
    (count == 3).then_some(channels)
}
