//! BLE GATT server: device control plus Wi-Fi provisioning.
//!
//! Advertising name: `Si5351-FG`. The web controller filters on this name prefix.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use esp_idf_svc::ble::gap::{BleAdvFields, BleAdvParams, GapEvent};
use esp_idf_svc::ble::gatt::server::{BleGattRegister, GattsEvent};
use esp_idf_svc::ble::{ensure_addr, BleDriver, BleUuid, HostEvent};
use esp_idf_svc::gatt_services;
use esp_idf_svc::sys::ble_gatt_svc_def;

use crate::app::App;
use crate::config::DEVICE_NAME;

const ATT_ERR_READ_NOT_PERMITTED: u8 = 0x0A;
const MAX_WRITE_CHUNK: usize = 256;

const ADV_FLAGS: u8 = 0x06; // LE General Discoverable, BR/EDR not supported
const ADV_ITVL_MIN: u16 = 0x30; // 30 ms
const ADV_ITVL_MAX: u16 = 0x60; // 60 ms

pub const SERVICE_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0001_4a5b_8c9d_e0f1_2345_6789);
pub const CMD_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0002_4a5b_8c9d_e0f1_2345_6789);
pub const RSP_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0003_4a5b_8c9d_e0f1_2345_6789);
pub const CH0_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0004_4a5b_8c9d_e0f1_2345_6789);
pub const CH1_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0005_4a5b_8c9d_e0f1_2345_6789);
pub const CH2_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0006_4a5b_8c9d_e0f1_2345_6789);
pub const WIFI_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0007_4a5b_8c9d_e0f1_2345_6789);
pub const INFO_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0008_4a5b_8c9d_e0f1_2345_6789);
pub const SWEEP_UUID: BleUuid = BleUuid::uuid128(0x5351_0000_0009_4a5b_8c9d_e0f1_2345_6789);

gatt_services!(pub SERVICES {
    primary(SERVICE_UUID) {
        // Newline-terminated ASCII commands (see README / web/index.html).
        chr(CMD_UUID, Write | WriteNoRsp);
        // Last response; also pushed as an indication when the client subscribes.
        chr(RSP_UUID, Read | Indicate);
        // "freq_hz,enabled,drive_ma" for each output.
        chr(CH0_UUID, Read);
        chr(CH1_UUID, Read);
        chr(CH2_UUID, Read);
        // "state,ip,reason"
        chr(WIFI_UUID, Read);
        // "version,uptime_seconds,si_ok"
        chr(INFO_UUID, Read);
        // "running,channel,current_hz"
        chr(SWEEP_UUID, Read);
    }
});

pub fn install<S>(driver: &BleDriver<'_, S>, app: Arc<App>)
where
    S: AsRef<[ble_gatt_svc_def]>,
{
    let gatts_app = app.clone();
    driver.gatts_subscribe(move |event| match event {
        GattsEvent::Register(BleGattRegister::Characteristic {
            uuid, val_handle, ..
        }) => {
            remember_handle(&gatts_app, uuid, val_handle);
            0
        }
        GattsEvent::Read {
            attr_handle,
            mut reply,
            ..
        } => match gatts_app.read_characteristic(attr_handle) {
            Some(value) => match reply.append(value.as_bytes()) {
                Ok(()) => 0,
                Err(err) => {
                    log::warn!("GATT append failed: {err}");
                    ATT_ERR_READ_NOT_PERMITTED
                }
            },
            None => ATT_ERR_READ_NOT_PERMITTED,
        },
        GattsEvent::Write {
            attr_handle,
            data,
            conn_handle,
        } => {
            if attr_handle == gatts_app.handles.cmd.load(Ordering::Relaxed) {
                let mut buffer = [0u8; MAX_WRITE_CHUNK];
                match data.read(&mut buffer) {
                    Ok(length) => match gatts_app.push_ble_data(&buffer[..length]) {
                        Ok(lines) => {
                            for line in lines {
                                gatts_app.handle_command(&line, conn_handle);
                            }
                        }
                        Err(err) => {
                            log::warn!("BLE command rejected: {err}");
                            gatts_app.respond(conn_handle, "ERR LONG");
                        }
                    },
                    Err(err) => log::warn!("BLE write read failed: {err}"),
                }
            }
            0
        }
        GattsEvent::SubscriptionChanged {
            conn_handle,
            attr_handle,
            cur_indicate,
            ..
        } => {
            if attr_handle == gatts_app.handles.rsp.load(Ordering::Relaxed) {
                gatts_app.set_subscribed(conn_handle, cur_indicate);
            }
            0
        }
        _ => 0,
    });

    let host_app = app.clone();
    driver.host_subscribe(move |event| {
        if let HostEvent::Sync = event {
            log::info!("NimBLE host synchronized");
            host_app.adv_needed.store(true, Ordering::Relaxed);
        }
    });

    let gap_app = app.clone();
    driver.gap_subscribe(move |event| {
        match event {
            GapEvent::Connect {
                conn_handle,
                status,
            } => {
                if status.is_ok() {
                    log::info!("BLE client connected (handle {conn_handle})");
                    gap_app.ble_connected.store(true, Ordering::Relaxed);
                    gap_app.ble_conn.store(conn_handle, Ordering::Relaxed);
                } else {
                    log::warn!("BLE connection failed: {status:?}");
                    gap_app.adv_needed.store(true, Ordering::Relaxed);
                }
            }
            GapEvent::Disconnect {
                conn_handle,
                reason,
            } => {
                log::info!("BLE client disconnected ({reason}), re-advertising");
                gap_app.ble_connected.store(false, Ordering::Relaxed);
                gap_app.set_subscribed(conn_handle, false);
                gap_app.ble_conn.store(0, Ordering::Relaxed);
                gap_app.adv_needed.store(true, Ordering::Relaxed);
            }
            GapEvent::Mtu { conn_handle, value } => {
                log::info!("BLE MTU for {conn_handle}: {value}")
            }
            GapEvent::Other => {}
        }
        0
    });
}

fn remember_handle(app: &App, uuid: BleUuid, value_handle: u16) {
    let handles = &app.handles;
    let handle = if uuid == CMD_UUID {
        Some(&handles.cmd)
    } else if uuid == RSP_UUID {
        Some(&handles.rsp)
    } else if uuid == CH0_UUID {
        Some(&handles.ch[0])
    } else if uuid == CH1_UUID {
        Some(&handles.ch[1])
    } else if uuid == CH2_UUID {
        Some(&handles.ch[2])
    } else if uuid == WIFI_UUID {
        Some(&handles.wifi)
    } else if uuid == INFO_UUID {
        Some(&handles.info)
    } else if uuid == SWEEP_UUID {
        Some(&handles.sweep)
    } else {
        None
    };

    if let Some(handle) = handle {
        handle.store(value_handle, Ordering::Relaxed);
    }
}

/// Start connectable legacy advertising. Called from the main loop after the
/// host reports `Sync`, and again after every disconnect.
pub fn advertise<S>(driver: &BleDriver<'_, S>) -> Result<(), String> {
    ensure_addr(false).map_err(|err| format!("ensure_addr: {err}"))?;
    driver
        .set_device_name(DEVICE_NAME)
        .map_err(|err| format!("set_device_name: {err}"))?;

    let fields = BleAdvFields {
        flags: ADV_FLAGS,
        name: Some(DEVICE_NAME),
        ..Default::default()
    };
    driver
        .adv_set_fields(&fields)
        .map_err(|err| format!("adv_set_fields: {err}"))?;

    let params = BleAdvParams {
        conn_mode: 2, // BLE_GAP_CONN_MODE_UND
        disc_mode: 2, // BLE_GAP_DISC_MODE_GEN
        itvl_min: ADV_ITVL_MIN,
        itvl_max: ADV_ITVL_MAX,
        ..Default::default()
    };
    driver
        .adv_start(0 /* public address */, &params)
        .map_err(|err| format!("adv_start: {err}"))?;

    Ok(())
}
