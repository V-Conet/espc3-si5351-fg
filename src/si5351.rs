//! Minimal Si5351A driver (3 outputs, 25 MHz crystal, PLL A + PLL B).
//!
//! Output assignment:
//!
//! * CLK0 -> PLL A
//! * CLK1 -> PLL B
//! * CLK2 -> PLL A, or PLL B when PLL A cannot synthesize it
//!
//! Frequencies above 112.5 MHz tune the PLL so that the MultiSynth divider is an
//! even integer (4..900), as required by the chip's integer mode. Below that a
//! 900 MHz VCO with fractional MultiSynth dividers is used, and the R divider
//! (1..128) extends the range down to about 7.8 kHz.
//!
//! The divider math lives in the `planner` module. Register map reference: Silicon Labs
//! AN619 / Si5351A-B-C datasheet, see `docs/Si5351.pdf` in this repository.

mod planner;

use core::fmt;

use esp_idf_svc::hal::i2c::I2cDriver;

use planner::{divider_params, plan_group, MsPlan, PllPlan};

pub const DEFAULT_ADDRESS: u8 = 0x60;
pub const FREQ_MIN_HZ: u64 = 7_813;
pub const FREQ_MAX_HZ: u64 = 160_000_000;

const I2C_TIMEOUT_TICKS: u32 = 10; // 100 ms at the default 100 Hz FreeRTOS tick
const I2C_PROBE_TIMEOUT_TICKS: u32 = 5; // 50 ms

const REG_OUTPUT_ENABLE: u8 = 3;
const REG_CLK_BASE: u8 = 16;
const REG_PLL_SOURCE: u8 = 15;
const REG_MS_BASE: [u8; 3] = [42, 50, 58];
const REG_PLL_BASE: [u8; 2] = [26, 34];
const REG_PLL_RESET: u8 = 177;
const REG_XTAL_LOAD: u8 = 183;

const PLL_A: usize = 0;
const PLL_B: usize = 1;

#[derive(Debug, Clone)]
pub enum Error {
    /// The I2C bus or the chip did not respond.
    I2c(String),
    /// A value is out of range or unsupported.
    Config(String),
    /// The requested frequency combination cannot be synthesized.
    Synthesis(String),
}

impl Error {
    pub fn is_i2c(&self) -> bool {
        matches!(self, Self::I2c(_))
    }

    fn message(&self) -> &str {
        match self {
            Self::I2c(message) | Self::Config(message) | Self::Synthesis(message) => message,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
    pub freq_hz: u64,
    pub enabled: bool,
    /// Output drive strength in mA: 2, 4, 6 or 8.
    pub drive_ma: u8,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            freq_hz: 1_000_000,
            enabled: false,
            drive_ma: 8,
        }
    }
}

impl ChannelConfig {
    pub const fn new(freq_hz: u64, enabled: bool, drive_ma: u8) -> Self {
        Self {
            freq_hz,
            enabled,
            drive_ma,
        }
    }

    pub fn is_valid(&self) -> bool {
        (FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&self.freq_hz)
            && matches!(self.drive_ma, 2 | 4 | 6 | 8)
    }

    /// Format used by the BLE channel characteristics and NVS.
    pub fn value_string(&self) -> String {
        format!(
            "{},{},{}",
            self.freq_hz,
            u8::from(self.enabled),
            self.drive_ma
        )
    }
}

pub struct Si5351 {
    i2c: I2cDriver<'static>,
    address: u8,
}

impl Si5351 {
    /// Construct the driver without touching the bus. This never fails, so BLE
    /// and Wi-Fi can start even when the Si5351 is not connected yet.
    pub fn new(i2c: I2cDriver<'static>) -> Self {
        Self::with_address(i2c, DEFAULT_ADDRESS)
    }

    pub fn with_address(i2c: I2cDriver<'static>, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read register 0 to verify that the device acknowledges.
    pub fn probe(&mut self) -> Result<u8, Error> {
        let mut value = [0u8; 1];
        self.i2c
            .write_read(self.address, &[0u8], &mut value, I2C_PROBE_TIMEOUT_TICKS)
            .map_err(|err| Error::I2c(format!("I2C probe 0x{:02X} failed: {err}", self.address)))?;
        Ok(value[0])
    }

    /// Recompute and apply the complete clock tree. Called on every change so the
    /// PLL assignment stays consistent for all three outputs.
    pub fn configure(&mut self, channels: &[ChannelConfig; 3]) -> Result<(), Error> {
        for (index, channel) in channels.iter().enumerate() {
            validate_frequency(channel.freq_hz)
                .map_err(|message| Error::Config(format!("CLK{index}: {message}")))?;
            if !matches!(channel.drive_ma, 2 | 4 | 6 | 8) {
                return Err(Error::Config(format!(
                    "CLK{index}: drive must be 2, 4, 6 or 8 mA"
                )));
            }
        }

        let freqs = [
            channels[0].freq_hz,
            channels[1].freq_hz,
            channels[2].freq_hz,
        ];

        // Preferred: CLK0 + CLK2 share PLL A, CLK1 alone on PLL B.
        // Fallback: CLK0 alone on PLL A, CLK1 + CLK2 share PLL B.
        let (group_a, group_b) = match (
            plan_group(&[0, 2], &freqs, PLL_A),
            plan_group(&[1], &freqs, PLL_B),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            _ => (
                plan_group(&[0], &freqs, PLL_A)
                    .map_err(|message| Error::Synthesis(format!("PLL A: {message}")))?,
                plan_group(&[1, 2], &freqs, PLL_B)
                    .map_err(|message| Error::Synthesis(format!("PLL B: {message}")))?,
            ),
        };

        let mut plans: [Option<MsPlan>; 3] = [None, None, None];
        for (channel, plan) in group_a.outputs.iter().chain(group_b.outputs.iter()) {
            plans[*channel] = Some(*plan);
        }
        let plans = [
            plans[0].ok_or_else(|| Error::Synthesis("CLK0 was not planned".into()))?,
            plans[1].ok_or_else(|| Error::Synthesis("CLK1 was not planned".into()))?,
            plans[2].ok_or_else(|| Error::Synthesis("CLK2 was not planned".into()))?,
        ];

        // Put the chip into a known state before writing the clock tree.
        self.write_reg(REG_OUTPUT_ENABLE, 0xFF)?; // all outputs disabled
        for reg in REG_CLK_BASE..REG_CLK_BASE + 3 {
            self.write_reg(reg, 0x80)?; // power down CLK0..CLK2
        }
        self.write_reg(REG_PLL_SOURCE, 0x00)?; // PLLA/PLLB <- XTAL
        self.write_reg(REG_XTAL_LOAD, 0xC0)?; // 10 pF crystal load

        self.write_pll(PLL_A, &group_a.pll)?;
        self.write_pll(PLL_B, &group_b.pll)?;

        for (channel, plan) in plans.iter().enumerate() {
            self.write_ms(channel, plan)?;
        }

        self.write_reg(REG_PLL_RESET, 0xA0)?;

        for (channel, plan) in plans.iter().enumerate() {
            let drive = drive_code(channels[channel].drive_ma)?;
            let mut control = 0x0C | drive; // CLKx_SRC = MultiSynth
            if plan.pll == PLL_B {
                control |= 0x20; // MSx_SRC = PLL B
            }
            if plan.integer {
                control |= 0x40; // MSx_INT
            }
            self.write_reg(REG_CLK_BASE + channel as u8, control)?;
        }

        // Output enable bits are active low.
        let mut enable = 0xFFu8;
        for (channel, config) in channels.iter().enumerate() {
            if config.enabled {
                enable &= !(1 << channel);
            }
        }
        self.write_reg(REG_OUTPUT_ENABLE, enable)?;

        Ok(())
    }

    fn write_pll(&mut self, index: usize, plan: &PllPlan) -> Result<(), Error> {
        let base = REG_PLL_BASE[index];
        let (p1, p2, p3) = divider_params(plan.a, plan.b, plan.c);
        let bytes = [
            (p3 >> 8) as u8,
            p3 as u8,
            ((p1 >> 16) & 0x03) as u8,
            (p1 >> 8) as u8,
            p1 as u8,
            (((p3 >> 12) & 0xF0) | ((p2 >> 16) & 0x0F)) as u8,
            (p2 >> 8) as u8,
            p2 as u8,
        ];
        self.write_burst(base, &bytes)
    }

    fn write_ms(&mut self, channel: usize, plan: &MsPlan) -> Result<(), Error> {
        let base = REG_MS_BASE[channel];
        let (p1, p2, p3) = divider_params(plan.a, plan.b, plan.c);
        let bytes = [
            (p3 >> 8) as u8,
            p3 as u8,
            (((p1 >> 16) & 0x03) | (u32::from(plan.r_exp & 0x07) << 4)) as u8,
            (p1 >> 8) as u8,
            p1 as u8,
            (((p3 >> 12) & 0xF0) | ((p2 >> 16) & 0x0F)) as u8,
            (p2 >> 8) as u8,
            p2 as u8,
        ];
        self.write_burst(base, &bytes)
    }

    fn write_reg(&mut self, reg: u8, value: u8) -> Result<(), Error> {
        self.write_burst(reg, &[value])
    }

    fn write_burst(&mut self, reg: u8, data: &[u8]) -> Result<(), Error> {
        let mut buffer = [0u8; 9];
        if data.len() > 8 {
            return Err(Error::Config("I2C burst too long".into()));
        }
        buffer[0] = reg;
        buffer[1..=data.len()].copy_from_slice(data);
        self.i2c
            .write(self.address, &buffer[..=data.len()], I2C_TIMEOUT_TICKS)
            .map_err(|err| Error::I2c(format!("I2C write to 0x{:02X} failed: {err}", self.address)))
    }
}

fn validate_frequency(freq_hz: u64) -> Result<(), String> {
    if !(FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&freq_hz) {
        return Err(format!(
            "frequency {freq_hz} Hz out of range ({FREQ_MIN_HZ}..={FREQ_MAX_HZ} Hz)"
        ));
    }
    Ok(())
}

fn drive_code(drive_ma: u8) -> Result<u8, Error> {
    match drive_ma {
        2 => Ok(0),
        4 => Ok(1),
        6 => Ok(2),
        8 => Ok(3),
        _ => Err(Error::Config(format!(
            "drive must be 2, 4, 6 or 8 mA (got {drive_ma})"
        ))),
    }
}
