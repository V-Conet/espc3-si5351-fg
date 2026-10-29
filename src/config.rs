//! Build-time configuration shared by the firmware modules.

use core::net::Ipv4Addr;

pub const FW_VERSION: &str = "v0.1.0";

/// BLE advertising name and default SoftAP SSID.
pub const DEVICE_NAME: &str = "Si5351-FG";
pub const AP_SSID: &str = DEVICE_NAME;

/// I2C wiring. Keep in sync with the GPIO pins passed to `I2cDriver::new`.
pub const I2C_SDA_GPIO: u8 = 1;
pub const I2C_SCL_GPIO: u8 = 0;
pub const I2C_BAUD_HZ: u32 = 100_000;

/// Open SoftAP.
pub const AP_CHANNEL: u8 = 6;
pub const AP_MAX_CONNECTIONS: u16 = 4;
pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 23, 1);
pub const AP_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

pub const BLE_PREFERRED_MTU: u16 = 247;

pub const MAX_SSID_LEN: usize = 32;
pub const MAX_PASSWORD_LEN: usize = 64;
pub const MAX_BLE_COMMAND_LEN: usize = 1024;
pub const MAX_BLE_REPLY_LEN: usize = 64;

pub const NVS_NAMESPACE: &str = "siggen";
pub const NVS_KEY_CHANNELS: &str = "channels";
pub const NVS_KEY_WIFI_SSID: &str = "wifi_ssid";
pub const NVS_KEY_WIFI_PASS: &str = "wifi_pass";
