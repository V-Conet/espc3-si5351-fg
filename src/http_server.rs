//! HTTP front-end, served over the SoftAP and/or the station interface.
//!
//! * GET  /                     control page
//! * GET  /api/status           device state as JSON
//! * POST /api/channel?...      update one output
//! * POST /api/wifi?...         provision router credentials
//! * POST /api/wifi/forget      remove saved credentials and return to AP-only
//! * POST /api/sweep?...        start a frequency sweep
//! * POST /api/sweep/stop       stop the active sweep
//! * POST /api/save             persist channel settings
//! * POST /api/reset            restore default channel settings

use std::sync::atomic::Ordering;
use std::sync::Arc;

use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;

use crate::app::{App, SweepConfig};
use crate::config::{FW_VERSION, MAX_PASSWORD_LEN, MAX_SSID_LEN};

const INDEX_HTML: &str = include_str!("../web/index.html");
const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn start(app: Arc<App>) -> anyhow::Result<EspHttpServer<'static>> {
    let configuration = Configuration {
        stack_size: 10240,
        max_uri_handlers: 12,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&configuration)?;

    server.fn_handler("/", Method::Get, |request| {
        request
            .into_response(
                200,
                Some("OK"),
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Cache-Control", "no-store"),
                ],
            )?
            .write_all(INDEX_HTML.as_bytes())?;
        Ok::<_, anyhow::Error>(())
    })?;

    let status_app = app.clone();
    server.fn_handler("/api/status", Method::Get, move |request| {
        respond_json(request, 200, &status_json(&status_app))
    })?;

    let channel_app = app.clone();
    server.fn_handler("/api/channel", Method::Post, move |request| {
        let uri = request.uri().to_string();
        let channel = match query_param(&uri, "ch").and_then(|value| value.parse::<usize>().ok()) {
            Some(channel) if channel <= 2 => channel,
            _ => return respond_error(request, 400, "bad channel"),
        };
        let freq = match query_param(&uri, "freq") {
            Some(value) => match value.parse::<u64>() {
                Ok(freq) => Some(freq),
                Err(_) => return respond_error(request, 400, "bad frequency"),
            },
            None => None,
        };
        let enabled = query_param(&uri, "on").map(|value| value == "1" || value == "true");
        let drive = match query_param(&uri, "drive") {
            Some(value) => match value.parse::<u8>() {
                Ok(drive) => Some(drive),
                Err(_) => return respond_error(request, 400, "bad drive"),
            },
            None => None,
        };

        match channel_app.set_channel(channel, freq, enabled, drive) {
            Ok(config) => respond_json(
                request,
                200,
                &format!(
                    r#"{{"ok":true,"ch":{},"freq":{},"on":{},"drive":{}}}"#,
                    channel,
                    config.freq_hz,
                    u8::from(config.enabled),
                    config.drive_ma
                ),
            ),
            Err(err) => {
                log::warn!("HTTP channel update failed: {err}");
                respond_error(request, 400, &err.to_string())
            }
        }
    })?;

    let wifi_app = app.clone();
    server.fn_handler("/api/wifi", Method::Post, move |request| {
        let uri = request.uri().to_string();
        let ssid = query_param(&uri, "ssid").unwrap_or_default();
        let password = query_param(&uri, "pass").unwrap_or_default();
        if ssid.is_empty() || ssid.len() > MAX_SSID_LEN || password.len() > MAX_PASSWORD_LEN {
            return respond_error(request, 400, "bad credentials");
        }
        if wifi_app.request_wifi(ssid, password) {
            respond_json(request, 200, r#"{"ok":true}"#)
        } else {
            respond_error(request, 503, "wifi unavailable")
        }
    })?;

    let forget_app = app.clone();
    server.fn_handler("/api/wifi/forget", Method::Post, move |request| {
        if forget_app.request_forget_wifi() {
            respond_json(request, 200, r#"{"ok":true}"#)
        } else {
            respond_error(request, 503, "wifi unavailable")
        }
    })?;

    let sweep_app = app.clone();
    server.fn_handler("/api/sweep", Method::Post, move |request| {
        let uri = request.uri().to_string();
        let channel = match query_param(&uri, "ch").and_then(|value| value.parse::<usize>().ok()) {
            Some(channel) if channel <= 2 => channel,
            _ => return respond_error(request, 400, "bad channel"),
        };
        let start_hz = match query_param(&uri, "start").and_then(|value| value.parse::<u64>().ok())
        {
            Some(value) => value,
            None => return respond_error(request, 400, "bad start frequency"),
        };
        let end_hz = match query_param(&uri, "end").and_then(|value| value.parse::<u64>().ok()) {
            Some(value) => value,
            None => return respond_error(request, 400, "bad end frequency"),
        };
        let step_hz = match query_param(&uri, "step").and_then(|value| value.parse::<u64>().ok()) {
            Some(value) => value,
            None => return respond_error(request, 400, "bad step"),
        };
        let dwell_ms = match query_param(&uri, "dwell").and_then(|value| value.parse::<u32>().ok())
        {
            Some(value) => value,
            None => return respond_error(request, 400, "bad dwell"),
        };
        let repeat = query_param(&uri, "loop")
            .map(|value| value == "1" || value == "true")
            .unwrap_or(false);

        let config = SweepConfig {
            channel,
            start_hz,
            end_hz,
            step_hz,
            dwell_ms,
            repeat,
        };
        match sweep_app.request_sweep(config) {
            Ok(()) => respond_json(request, 200, r#"{"ok":true}"#),
            Err(err) => {
                log::warn!("HTTP sweep request rejected: {err}");
                respond_error(request, 400, &err)
            }
        }
    })?;

    let sweep_stop_app = app.clone();
    server.fn_handler("/api/sweep/stop", Method::Post, move |request| {
        if sweep_stop_app.request_sweep_stop() {
            respond_json(request, 200, r#"{"ok":true}"#)
        } else {
            respond_error(request, 503, "sweep unavailable")
        }
    })?;

    let save_app = app.clone();
    server.fn_handler("/api/save", Method::Post, move |request| {
        match save_app.save_channels() {
            Ok(()) => respond_json(request, 200, r#"{"ok":true}"#),
            Err(err) => {
                log::warn!("HTTP save failed: {err}");
                respond_error(request, 500, "save failed")
            }
        }
    })?;

    server.fn_handler("/api/reset", Method::Post, move |request| {
        match app.reset_defaults() {
            Ok(()) => respond_json(request, 200, r#"{"ok":true}"#),
            Err(err) => {
                log::warn!("HTTP reset failed: {err}");
                respond_error(request, 500, "reset failed")
            }
        }
    })?;

    Ok(server)
}

fn status_json(app: &App) -> String {
    let channels = app.channels();
    let wifi = app.wifi.lock().unwrap();
    let wifi_state = wifi.state as u8;
    let uptime = app.uptime_secs();
    let ble = app.ble_connected.load(Ordering::Relaxed);
    let (sweep_running, sweep_channel, sweep_current) = app.sweep_status();
    format!(
        concat!(
            r#"{{"fw":"{}","uptime":{},"ble":{},"si":{},"#,
            r#""ch":[{{"f":{},"on":{},"d":{}}},{{"f":{},"on":{},"d":{}}},{{"f":{},"on":{},"d":{}}}],"#,
            r#""wifi":{{"state":{},"ip":"{}","ssid":"{}","msg":"{}","reason":{}}},"#,
            r#""sweep":{{"running":{},"ch":{},"current":{}}}}}"#
        ),
        FW_VERSION,
        uptime,
        ble,
        u8::from(app.si_ok()),
        channels[0].freq_hz,
        u8::from(channels[0].enabled),
        channels[0].drive_ma,
        channels[1].freq_hz,
        u8::from(channels[1].enabled),
        channels[1].drive_ma,
        channels[2].freq_hz,
        u8::from(channels[2].enabled),
        channels[2].drive_ma,
        wifi_state,
        json_escape(&wifi.ip),
        json_escape(&wifi.ssid),
        json_escape(&wifi.message),
        app.wifi_reason(),
        sweep_running,
        sweep_channel,
        sweep_current
    )
}

fn respond_json<'r, 'c>(
    request: Request<&'r mut EspHttpConnection<'c>>,
    status: u16,
    body: &str,
) -> anyhow::Result<()> {
    request
        .into_response(
            status,
            Some(if status == 200 { "OK" } else { "Error" }),
            &[("Content-Type", JSON_TYPE), ("Cache-Control", "no-store")],
        )?
        .write_all(body.as_bytes())?;
    Ok(())
}

fn respond_error<'r, 'c>(
    request: Request<&'r mut EspHttpConnection<'c>>,
    status: u16,
    message: &str,
) -> anyhow::Result<()> {
    respond_json(
        request,
        status,
        &format!(r#"{{"ok":false,"err":"{}"}}"#, json_escape(message)),
    )
}

fn query_param(uri: &str, key: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if url_decode(name) == key {
            return Some(url_decode(value));
        }
    }
    None
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        output.push(((high << 4) | low) as u8);
                        index += 3;
                    }
                    _ => {
                        output.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn json_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => output.push(character),
        }
    }
    output
}
