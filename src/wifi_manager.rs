//! Wi-Fi worker.
//!
//! The device always runs an open SoftAP (`Si5351-FG`) at 192.168.23.1 so the
//! control page is reachable before any router is configured. When STA
//! credentials are supplied over BLE or HTTP, Wi-Fi switches to AP+STA and the
//! station connects to the router while the AP stays available.

use std::convert::TryInto;
use std::net::Ipv4Addr;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use embedded_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration,
};

use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::sys::{
    esp_ip4_addr_t, esp_netif_dhcps_start, esp_netif_dhcps_stop, esp_netif_ip_info_t,
    esp_netif_set_ip_info, esp_wifi_get_config, esp_wifi_set_config, wifi_config_t,
    wifi_interface_t_WIFI_IF_STA, wifi_sae_pwe_method_t_WPA3_SAE_PWE_BOTH,
    ESP_ERR_ESP_NETIF_DHCP_ALREADY_STARTED, ESP_OK,
};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

use crate::app::{App, WifiCmd};
use crate::config::{
    AP_CHANNEL, AP_IP, AP_MAX_CONNECTIONS, AP_NETMASK, AP_SSID, MAX_PASSWORD_LEN, MAX_SSID_LEN,
};

const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// How a connection attempt should treat the existing Wi-Fi mode.
#[derive(Clone, Copy)]
enum Attempt {
    /// Rebuild the mode (AP or AP+STA) and restart the driver.
    Reconfigure { notify: bool },
    /// Keep the running AP+STA mode and only retry the station link.
    Reconnect,
}

pub fn run(
    mut wifi: BlockingWifi<EspWifi<'static>>,
    rx: Receiver<WifiCmd>,
    app: Arc<App>,
    initial_credentials: Option<(String, String)>,
) {
    let mut credentials = initial_credentials;

    match credentials.clone() {
        Some((ssid, password)) => {
            // AP + STA from the start; the AP comes up even if the STA fails.
            attempt(
                &mut wifi,
                &app,
                &ssid,
                &password,
                Attempt::Reconfigure { notify: false },
            );
        }
        None => match start_ap_only(&mut wifi) {
            Ok(()) => {
                log::info!("open AP {AP_SSID:?} started at {AP_IP}");
                app.set_wifi_access_point(AP_SSID, &AP_IP.to_string());
            }
            Err(err) => {
                log::warn!("could not start AP: {err}");
                app.set_wifi_error(AP_SSID, &err);
            }
        },
    }

    loop {
        match rx.recv_timeout(RETRY_INTERVAL) {
            Ok(WifiCmd::Connect { ssid, password }) => {
                log::info!("Wi-Fi provisioning for SSID {ssid:?}");
                app.save_wifi_credentials(&ssid, &password);
                credentials = Some((ssid.clone(), password.clone()));
                attempt(
                    &mut wifi,
                    &app,
                    &ssid,
                    &password,
                    Attempt::Reconfigure { notify: true },
                );
            }
            Ok(WifiCmd::Forget) => {
                log::info!("forgetting saved Wi-Fi credentials");
                app.clear_wifi_credentials();
                credentials = None;
                match start_ap_only(&mut wifi) {
                    Ok(()) => {
                        log::info!("AP-only mode restored at {AP_IP}");
                        app.set_wifi_access_point(AP_SSID, &AP_IP.to_string());
                    }
                    Err(err) => {
                        log::warn!("could not restore AP-only mode: {err}");
                        app.set_wifi_error(AP_SSID, &err);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some((ssid, password)) = credentials.as_ref() {
                    // AP+STA `is_connected` also requires the AP to be running;
                    // the AP is always started by this worker.
                    if !wifi.is_connected().unwrap_or(false) {
                        let keep_mode = wifi.is_started().unwrap_or(false)
                            && matches!(wifi.get_configuration(), Ok(Configuration::Mixed(_, _)));
                        let attempt_kind = if keep_mode {
                            Attempt::Reconnect
                        } else {
                            Attempt::Reconfigure { notify: false }
                        };
                        attempt(&mut wifi, &app, ssid, password, attempt_kind);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn attempt(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    app: &Arc<App>,
    ssid: &str,
    password: &str,
    kind: Attempt,
) {
    let (reconfigure, notify) = match kind {
        Attempt::Reconfigure { notify } => (true, notify),
        Attempt::Reconnect => (false, false),
    };

    app.set_wifi_connecting(ssid);
    if notify {
        app.notify_last_conn("WIFI...");
    }

    match connect_sta(wifi, ssid, password, reconfigure) {
        Ok(ip) => {
            log::info!("Wi-Fi connected to {ssid}, IP {ip}");
            app.set_wifi_connected(ssid, &ip);
            if notify {
                app.notify_last_conn(&format!("OK {ip}"));
            }
        }
        Err(err) => {
            // The disconnect event carries the ESP-IDF failure reason; give the
            // event loop a moment to deliver it before building the reply.
            std::thread::sleep(Duration::from_millis(150));
            let reason = app.wifi_reason();
            log::warn!("Wi-Fi connection to {ssid} failed: {err} (reason {reason})");
            app.set_wifi_error(ssid, &err);
            if notify {
                if reason != 0 {
                    app.notify_last_conn(&format!("ERR WIFI {reason}"));
                } else {
                    app.notify_last_conn("ERR WIFI");
                }
            }
        }
    }
}

fn client_config(ssid: &str, password: &str) -> Result<ClientConfiguration, String> {
    let ssid = ssid
        .try_into()
        .map_err(|_| format!("SSID must be 1..={MAX_SSID_LEN} bytes"))?;
    let password = password
        .try_into()
        .map_err(|_| format!("password must be at most {MAX_PASSWORD_LEN} bytes"))?;

    Ok(ClientConfiguration {
        ssid,
        password,
        // OPEN as the scan threshold accepts WPA2, WPA3, WPA2/WPA3 mixed and
        // open networks. The AP's real auth mode is used for association.
        auth_method: AuthMethod::None,
        ..Default::default()
    })
}

fn ap_config() -> AccessPointConfiguration {
    AccessPointConfiguration {
        ssid: AP_SSID.try_into().expect("AP SSID fits"),
        ssid_hidden: false,
        channel: AP_CHANNEL,
        auth_method: AuthMethod::None,
        password: "".try_into().expect("empty password"),
        max_connections: AP_MAX_CONNECTIONS,
        ..Default::default()
    }
}

fn start_ap_only(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<(), String> {
    restart(wifi, Configuration::AccessPoint(ap_config()), "AP", false)?;
    if let Err(err) = set_ap_ip(wifi) {
        log::warn!("could not set AP IP {AP_IP}: {err}");
    }
    Ok(())
}

fn connect_sta(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    ssid: &str,
    password: &str,
    reconfigure: bool,
) -> Result<String, String> {
    if reconfigure {
        let configuration = Configuration::Mixed(client_config(ssid, password)?, ap_config());
        restart(wifi, configuration, "AP+STA", true)?;
    } else {
        if !wifi.is_started().unwrap_or(false) {
            return Err("Wi-Fi driver is not started".into());
        }
        if wifi.is_connected().unwrap_or(false) {
            return sta_ip(wifi);
        }
    }

    let expected_ip = AP_IP.octets();
    let ap_ip_ok = wifi
        .wifi()
        .ap_netif()
        .get_ip_info()
        .map(|info| info.ip.octets() == expected_ip)
        .unwrap_or(false);
    if reconfigure || !ap_ip_ok {
        if let Err(err) = set_ap_ip(wifi) {
            log::warn!("could not set AP IP {AP_IP}: {err}");
        }
    }

    wifi.connect().map_err(|err| format!("connect: {err}"))?;
    wifi.wait_netif_up()
        .map_err(|err| format!("wait_netif_up: {err}"))?;
    sta_ip(wifi)
}

fn restart(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    configuration: Configuration,
    context: &str,
    tune_sta: bool,
) -> Result<(), String> {
    if wifi.is_started().unwrap_or(false) {
        let _ = wifi.stop();
    }
    wifi.set_configuration(&configuration)
        .map_err(|err| format!("set_configuration({context}): {err}"))?;
    // Station tuning must happen before esp_wifi_start().
    if tune_sta {
        if let Err(err) = tune_sta_config() {
            log::warn!("could not tune WPA3 station config: {err}");
        }
    }
    wifi.start()
        .map_err(|err| format!("start({context}): {err}"))?;
    Ok(())
}

/// esp-idf-svc's ClientConfiguration conversion leaves the SAE PWE method at
/// zero (unspecified) and PMF capability off. Both matter for WPA3-SAE APs;
/// the ESP-IDF defaults are "both PWE methods" and PMF-capable, so restore them.
fn tune_sta_config() -> Result<(), String> {
    unsafe {
        let mut config: wifi_config_t = core::mem::zeroed();
        let result = esp_wifi_get_config(wifi_interface_t_WIFI_IF_STA, &mut config);
        if result != ESP_OK {
            return Err(format!("esp_wifi_get_config: {result}"));
        }
        config.sta.sae_pwe_h2e = wifi_sae_pwe_method_t_WPA3_SAE_PWE_BOTH;
        config.sta.pmf_cfg.capable = true;
        config.sta.pmf_cfg.required = false;
        let result = esp_wifi_set_config(wifi_interface_t_WIFI_IF_STA, &mut config);
        if result != ESP_OK {
            return Err(format!("esp_wifi_set_config: {result}"));
        }
    }
    Ok(())
}

fn sta_ip(wifi: &BlockingWifi<EspWifi<'static>>) -> Result<String, String> {
    wifi.wifi()
        .sta_netif()
        .get_ip_info()
        .map_err(|err| format!("get_ip_info: {err}"))
        .map(|info| info.ip.to_string())
}

/// Set the SoftAP address and restart its DHCP server.
fn set_ap_ip(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<(), String> {
    let handle = wifi.wifi().ap_netif().handle();
    if handle.is_null() {
        return Err("AP netif handle is null".into());
    }

    let info = esp_netif_ip_info_t {
        ip: esp_ip(AP_IP),
        netmask: esp_ip(AP_NETMASK),
        gw: esp_ip(AP_IP),
    };

    unsafe {
        // The DHCP server must be stopped before changing the interface IP.
        let _ = esp_netif_dhcps_stop(handle);
        let result = esp_netif_set_ip_info(handle, &info);
        if result != ESP_OK {
            return Err(format!("esp_netif_set_ip_info: {result}"));
        }

        // The AP netif can still be settling right after wifi_start(); retry
        // starting the DHCP server briefly.
        let mut result = ESP_ERR_ESP_NETIF_DHCP_ALREADY_STARTED;
        for attempt in 0..4 {
            result = esp_netif_dhcps_start(handle);
            if result == ESP_OK || result == ESP_ERR_ESP_NETIF_DHCP_ALREADY_STARTED {
                break;
            }
            if attempt < 3 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if result != ESP_OK && result != ESP_ERR_ESP_NETIF_DHCP_ALREADY_STARTED {
            return Err(format!("esp_netif_dhcps_start: {result}"));
        }
    }

    log::info!("AP IP set to {AP_IP}");
    Ok(())
}

/// lwIP stores IPv4 addresses in network byte order; on the little-endian
/// RISC-V core that is exactly the octets laid out low-to-high.
fn esp_ip(address: Ipv4Addr) -> esp_ip4_addr_t {
    esp_ip4_addr_t {
        addr: u32::from_le_bytes(address.octets()),
    }
}
