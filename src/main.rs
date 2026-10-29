//! ESP32-C3 + Si5351A signal generator.
//!
//! BLE is always available. Wi-Fi starts as an open SoftAP and can be switched
//! to AP+STA with router credentials; both transports expose the same controls
//! over HTTP.

mod app;
mod ble_server;
mod config;
mod http_server;
mod protocol;
mod si5351;
mod sweep;
mod wifi_manager;

// The timer-queue symbols from this crate are required by esp-idf-svc's
// `embassy-time-driver` feature even though application code does not call it.
use embassy_time as _;

use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use esp_idf_svc::ble::BleDriver;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::i2c::{config::Config as I2cConfig, I2cDriver};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi, WifiEvent};
use log::{debug, info, warn};

use crate::app::App;
use crate::config::{
    BLE_PREFERRED_MTU, DEVICE_NAME, FW_VERSION, I2C_BAUD_HZ, I2C_SCL_GPIO, I2C_SDA_GPIO,
};
use crate::si5351::Si5351;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    info!("ESP32-C3 Si5351 signal generator {FW_VERSION} starting");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs_partition = EspDefaultNvsPartition::take()?;

    // --- I2C + Si5351 -------------------------------------------------
    // 100 kHz is deliberately conservative for dupont wiring. Raise it once
    // the bus is verified. Internal pull-ups supplement the module's 10 kΩ.
    // GPIO0/1 are valid I2C pins via the GPIO matrix (GPIO4/5 also work).
    // Keep the pin numbers in sync with config::I2C_SDA_GPIO / I2C_SCL_GPIO.
    let i2c_config = I2cConfig::new()
        .baudrate(I2C_BAUD_HZ.into())
        .sda_enable_pullup(true)
        .scl_enable_pullup(true);
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio1,
        peripherals.pins.gpio0,
        &i2c_config,
    )?;
    let mut si5351 = Si5351::new(i2c);
    match si5351.probe() {
        Ok(status) => info!(
            "Si5351 ACK at 0x60, status 0x{status:02X} \
             (SDA=GPIO{I2C_SDA_GPIO}, SCL=GPIO{I2C_SCL_GPIO})"
        ),
        Err(err) => warn!(
            "{err} -- Si5351 not responding; BLE/Wi-Fi will still start. \
             Check VIN (3.3/5 V), GND, SDA=GPIO{I2C_SDA_GPIO}, SCL=GPIO{I2C_SCL_GPIO}."
        ),
    }

    let settings_nvs = app::open_nvs(nvs_partition.clone());
    let channels = app::load_channels(&settings_nvs);
    let wifi_credentials = app::load_wifi_credentials(&settings_nvs);

    let (out_tx, out_rx) = mpsc::channel();
    let (wifi_tx, wifi_rx) = mpsc::channel();
    let (sweep_tx, sweep_rx) = mpsc::channel();
    let app = Arc::new(App::new(si5351, channels, settings_nvs, out_tx));
    app.set_wifi_sender(wifi_tx);
    app.set_sweep_sender(sweep_tx);

    let sweep_app = app.clone();
    std::thread::Builder::new()
        .name("sweep".into())
        .stack_size(8192)
        .spawn(move || sweep::run(sweep_app, sweep_rx))?;

    // Capture the ESP-IDF disconnect reason so it can be shown over BLE/HTTP.
    // The subscription must stay alive for the lifetime of the firmware.
    let wifi_event_app = app.clone();
    let _wifi_events = sys_loop.subscribe::<WifiEvent, _>(move |event| {
        if let WifiEvent::StaDisconnected(info) = event {
            warn!(
                "Wi-Fi disconnected: reason {} rssi {}",
                info.reason(),
                info.rssi()
            );
            wifi_event_app.set_wifi_reason(info.reason());
        }
    })?;

    if let Err(err) = app.apply_current() {
        warn!("initial Si5351 configuration failed: {err}");
        if err.is_i2c() {
            warn!("Si5351 is not responding yet; retrying every 5 s in the background");
        } else if let Err(err) = app.set_all(app::DEFAULT_CHANNELS) {
            warn!("could not fall back to default outputs: {err}");
        }
    } else {
        info!("initial outputs configured");
    }

    // --- Modem split: Wi-Fi and BLE share the radio -------------------
    let (wifi_modem, bt_modem) = peripherals.modem.split();
    let wifi = BlockingWifi::wrap(
        EspWifi::new(wifi_modem, sys_loop.clone(), Some(nvs_partition))?,
        sys_loop,
    )?;

    // Bind the HTTP server before Wi-Fi starts; it becomes reachable as soon as
    // the SoftAP (or STA) interface gets an address.
    let _http_server = match http_server::start(app.clone()) {
        Ok(server) => {
            info!("HTTP server listening on port 80");
            Some(server)
        }
        Err(err) => {
            warn!("HTTP server could not start: {err}");
            None
        }
    };

    // --- BLE ----------------------------------------------------------
    let driver = BleDriver::new_with_services(bt_modem, &ble_server::SERVICES)?;
    ble_server::install(&driver, app.clone());
    if let Err(err) = esp_idf_svc::ble::gatt::set_preferred_mtu(BLE_PREFERRED_MTU) {
        warn!("could not set preferred BLE MTU: {err}");
    }
    driver.start()?;
    info!("NimBLE started, advertising as {DEVICE_NAME}");

    // Start Wi-Fi only after the BLE host is up; the worker may connect
    // immediately if credentials were stored in NVS.
    let wifi_app = app.clone();
    std::thread::Builder::new()
        .name("wifi".into())
        .stack_size(8192)
        .spawn(move || wifi_manager::run(wifi, wifi_rx, wifi_app, wifi_credentials))?;

    // --- Housekeeping loop -------------------------------------------
    let mut last_si_retry = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(20));

        if !app.si_ok() && last_si_retry.elapsed() >= Duration::from_secs(5) {
            last_si_retry = Instant::now();
            match app.apply_current() {
                Ok(()) => info!("Si5351 detected and configured"),
                Err(err) if !err.is_i2c() => {
                    warn!("stored Si5351 configuration is unusable ({err}); using defaults");
                    if let Err(err) = app.set_all(app::DEFAULT_CHANNELS) {
                        warn!("default configuration failed: {err}");
                    }
                }
                Err(_) => {} // still no ACK; keep trying quietly
            }
        }

        if app.adv_needed.swap(false, Ordering::Relaxed) {
            match ble_server::advertise(&driver) {
                Ok(()) => info!("BLE advertising as {DEVICE_NAME}"),
                Err(err) => warn!("BLE advertising failed: {err}"),
            }
        }

        while let Ok(message) = out_rx.try_recv() {
            let handle = app.handles.rsp.load(Ordering::Relaxed);
            if handle != 0 && app.is_subscribed(message.conn) {
                if let Err(err) = driver.indicate(message.conn, handle, message.data.as_bytes()) {
                    debug!("BLE indication failed: {err}");
                }
            }
        }
    }
}
