//! Virtual Shelly Pro 3EM emulation. Listens on UDP and serves a per-Marstek
//! pulse value drawn from each battery's pulse queue.
//!
//! Each Marstek polls us at ~600 ms intervals. On every poll we identify
//! the battery by source IP, then:
//!   1. If the battery's circuit is currently muted (stale plug somewhere),
//!      DROP the response. The Marstek's CT-watchdog will clear its
//!      integrator after ~30-60 s. This is the safety guarantee for a
//!      degraded plug situation.
//!   2. Otherwise, pop one value from the pulse queue. If queue is empty,
//!      send 0 W. The Marstek's integrator then either accumulates the
//!      delta (queue had a value) or holds its current target (queue empty).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::rpc::{
    BleStatus, BtHomeStatus, CloudStatus, DeviceInfo, EmConfig, EmDataConfig, EmDataStatus,
    EmStatus, EthStatus, ModbusStatus, MqttStatus, RequestFrame, ResponseFrame, RpcError,
    ShellyStatus, SysStatus, TemperatureStatus, WifiStatus, WsStatus, error_codes,
};
use crate::state::AppState;

const RECV_BUF: usize = 16 * 1024;
const RECONNECT_THRESHOLD: Duration = Duration::from_secs(30);
/// Maximum number of (peer.ip, method) entries we track for response
/// counting. Sized for "many batteries × many methods" with churn margin
/// — not a security limit, just a memory-leak guard.
const MAX_RESPONSE_COUNTERS: usize = 1024;

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let initial = config.load_full();
    // In modbus dispatch mode the virtual Shelly Pro 3EM CT feed is
    // entirely unused — the Marstek operates on Modbus setpoints, not
    // CT. We don't bind the UDP port at all (leaves 1010 free for
    // anything else on the host) and never process any inbound polls.
    if matches!(initial.dispatcher.mode, crate::config::DispatchMode::Modbus) {
        info!("dispatcher.mode = modbus → virtual Shelly disabled");
        std::future::pending::<()>().await;
        return Ok(());
    }
    let bind = format!(
        "{}:{}",
        initial.virtual_shelly.bind_interface, initial.virtual_shelly.udp_port
    );
    let socket = UdpSocket::bind(&bind)
        .await
        .with_context(|| format!("binding virtual shelly UDP on {bind}"))?;
    info!(bind = %bind, "virtual shelly UDP server listening");

    let socket = Arc::new(socket);
    let mut buf = vec![0u8; RECV_BUF];
    let device = DeviceContext::from_config(&initial);
    let response_counters: Arc<Mutex<HashMap<(IpAddr, String), AtomicU64>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "udp recv_from failed");
                continue;
            }
        };
        let payload = buf[..len].to_vec();
        let state = state.clone();
        let socket = socket.clone();
        let device = device.clone();
        let counters = response_counters.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let cfg = config.load_full();
            handle_request(&state, &cfg, &device, &socket, peer, &payload, &counters).await;
        });
    }
}

async fn handle_request(
    state: &AppState,
    config: &Config,
    device: &DeviceContext,
    socket: &UdpSocket,
    peer: SocketAddr,
    payload: &[u8],
    response_counters: &Mutex<HashMap<(IpAddr, String), AtomicU64>>,
) {
    let request: RequestFrame = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => {
            warn!(peer = %peer, error = %e, "invalid request frame");
            return;
        }
    };

    let now = Instant::now();
    let battery_id = state.by_addr.get(&peer.ip()).cloned();

    // Mute decision: if battery known and its circuit is silent, drop.
    if let Some(ref bid) = battery_id {
        let mute = {
            let bats = state.batteries.read();
            let circuits = state.circuits.read();
            if let Some(b) = bats.get(bid) {
                circuits
                    .get(&b.circuit)
                    .and_then(|c| c.silent_until)
                    .map(|t| t > now)
                    .unwrap_or(false)
            } else {
                false
            }
        };
        if mute {
            debug!(
                battery = %bid,
                method = %request.method,
                "drop poll: circuit muted (stale plug)"
            );
            // Update last_marstek_poll_at for diagnostics.
            let mut bats = state.batteries.write();
            if let Some(b) = bats.get_mut(bid) {
                b.last_marstek_poll_at = Some(now);
            }
            return;
        }
    }

    // Drain one pulse for this battery (or 0 if no pulse pending / battery unknown).
    let (pulse_value_w, was_silent) = {
        let prev = {
            let bats = state.batteries.read();
            battery_id.as_ref().and_then(|id| {
                bats.get(id).and_then(|b| b.last_marstek_poll_at)
            })
        };
        let was_silent = prev
            .map(|t| now.saturating_duration_since(t) > RECONNECT_THRESHOLD)
            .unwrap_or(true);
        let mut value = 0.0;
        if let Some(ref bid) = battery_id {
            let mut bats = state.batteries.write();
            if let Some(b) = bats.get_mut(bid) {
                b.last_marstek_poll_at = Some(now);
                if b.pulse_remaining > 0 {
                    value = b.pending_pulse_w;
                    b.pulse_remaining -= 1;
                    if b.pulse_remaining == 0 {
                        // Reset stored value so a stale read can't reappear,
                        // and stamp the completion time so the dispatcher's
                        // settle_timeout_s fallback works.
                        b.pending_pulse_w = 0.0;
                        b.last_pulse_completed_at = Some(now);
                    }
                }
            }
        }
        (value, was_silent)
    };

    match (&battery_id, was_silent) {
        (Some(id), true) => info!(
            battery = %id,
            peer = %peer,
            method = %request.method,
            pulse_w = pulse_value_w,
            "battery polling started"
        ),
        (Some(id), false) => debug!(
            battery = %id,
            peer = %peer,
            method = %request.method,
            pulse_w = pulse_value_w,
            "battery poll"
        ),
        (None, _) => debug!(
            peer = %peer,
            method = %request.method,
            "poll from unknown source"
        ),
    }

    let method = request.method.clone();
    let response = build_response(state, config, device, pulse_value_w, request);
    let bytes = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "serializing response");
            return;
        }
    };
    if let Err(e) = socket.send_to(&bytes, peer).await {
        warn!(peer = %peer, error = %e, "sending udp response");
        return;
    }

    let count = {
        let mut map = response_counters.lock();
        if map.len() >= MAX_RESPONSE_COUNTERS
            && !map.contains_key(&(peer.ip(), method.clone()))
        {
            // Drop an arbitrary entry (HashMap's iteration order is
            // randomised, so this is effectively random eviction). We
            // only do this when the new key isn't already present, so
            // existing battery counters are stable across the eviction.
            if let Some(victim_key) = map.keys().next().cloned() {
                map.remove(&victim_key);
            }
        }
        let counter = map
            .entry((peer.ip(), method.clone()))
            .or_insert_with(|| AtomicU64::new(0));
        counter.fetch_add(1, Ordering::Relaxed) + 1
    };
    if count.is_multiple_of(10) || count == 1 {
        let label = battery_id.unwrap_or_else(|| format!("{}", peer.ip()));
        info!(
            battery = %label,
            method = %method,
            count,
            pulse_w = pulse_value_w,
            "response sent"
        );
    }
}

#[derive(Debug, Clone)]
pub struct DeviceContext {
    mac_hex: String,
    hostname: String,
    firmware: String,
}

impl DeviceContext {
    pub fn mac_hex(&self) -> &str {
        &self.mac_hex
    }
    pub fn hostname(&self) -> &str {
        &self.hostname
    }
    pub fn firmware(&self) -> &str {
        &self.firmware
    }

    pub fn from_config(config: &Config) -> Self {
        let mac_hex = if !config.virtual_shelly.device_mac.is_empty() {
            config.virtual_shelly.device_mac.clone()
        } else {
            mac_address::get_mac_address()
                .ok()
                .flatten()
                .map(|m| {
                    let bytes = m.bytes();
                    format!(
                        "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
                    )
                })
                .unwrap_or_else(|| "B827EB364242".to_string())
        };
        let hostname = if !config.virtual_shelly.device_hostname.is_empty() {
            config.virtual_shelly.device_hostname.clone()
        } else {
            format!("shellypro3em-{}", mac_hex.to_lowercase())
        };
        Self {
            mac_hex,
            hostname,
            firmware: config.virtual_shelly.firmware.clone(),
        }
    }
}

pub fn build_response(
    state: &AppState,
    config: &Config,
    device: &DeviceContext,
    pulse_value_w: f64,
    request: RequestFrame,
) -> ResponseFrame {
    let dst = request.src.clone();
    let id = request.id;
    let method = request.method.clone();

    match dispatch_method(state, config, device, pulse_value_w, &request) {
        Ok(value) => ResponseFrame {
            id,
            src: device.hostname.clone(),
            dst,
            result: Some(value),
            error: None,
        },
        Err(err) => {
            warn!(method, code = err.code, "rpc error: {}", err.message);
            ResponseFrame {
                id,
                src: device.hostname.clone(),
                dst,
                result: None,
                error: Some(err),
            }
        }
    }
}

fn dispatch_method(
    state: &AppState,
    config: &Config,
    device: &DeviceContext,
    pulse_value_w: f64,
    request: &RequestFrame,
) -> Result<Value, RpcError> {
    let grid_stale_s = config.dispatcher.grid_stale_s;
    let method_lc = request.method.to_lowercase();
    match method_lc.as_str() {
        "em.getstatus" => Ok(serde_json::to_value(em_get_status(state, pulse_value_w, grid_stale_s)?).unwrap()),
        "em.getconfig" => Ok(serde_json::to_value(EmConfig::default_em0()).unwrap()),
        "emdata.getstatus" => Ok(serde_json::to_value(emdata_status(state)).unwrap()),
        "emdata.getconfig" => Ok(serde_json::to_value(EmDataConfig { id: 0 }).unwrap()),
        "shelly.getdeviceinfo" => Ok(serde_json::to_value(device_info(device)).unwrap()),
        "shelly.getstatus" => {
            Ok(serde_json::to_value(shelly_status(state, device, pulse_value_w, grid_stale_s)?).unwrap())
        }
        "shelly.getconfig" => Ok(shelly_get_config(device)),
        "shelly.getcomponents" => Ok(shelly_get_components(state, device, pulse_value_w, grid_stale_s)?),
        "shelly.reboot" => Ok(json!({})),
        "sys.getstatus" => Ok(serde_json::to_value(sys_status(state, device)).unwrap()),
        "sys.getconfig" => Ok(sys_config(device)),
        "wifi.getstatus" => Ok(serde_json::to_value(wifi_status()).unwrap()),
        "wifi.getconfig" => Ok(wifi_config()),
        "cloud.getstatus" => Ok(serde_json::to_value(CloudStatus { connected: false }).unwrap()),
        "cloud.getconfig" => Ok(json!({"enable": false, "server": null})),
        "ws.getstatus" => Ok(serde_json::to_value(WsStatus { connected: false }).unwrap()),
        "ws.getconfig" => Ok(json!({"enable": false, "server": null, "ssl_ca": null})),
        "mqtt.getstatus" => Ok(serde_json::to_value(MqttStatus { connected: false }).unwrap()),
        "mqtt.getconfig" => Ok(json!({"enable": false})),
        "eth.getstatus" => Ok(serde_json::to_value(eth_status()).unwrap()),
        "eth.getconfig" => Ok(json!({"enable": true, "ipv4mode": "dhcp"})),
        "ble.getstatus" => Ok(serde_json::to_value(BleStatus {}).unwrap()),
        "ble.getconfig" => Ok(json!({"enable": false, "rpc": {"enable": false}})),
        "modbus.getstatus" => Ok(serde_json::to_value(ModbusStatus {}).unwrap()),
        "modbus.getconfig" => Ok(json!({"enable": false})),
        "temperature.getstatus" => Ok(serde_json::to_value(temperature_status()).unwrap()),
        "script.list" => Ok(json!({"scripts": []})),
        "script.getcode" => Ok(json!({"data": "", "left": 0})),
        _ => Err(RpcError {
            code: error_codes::METHOD_NOT_FOUND,
            message: format!("method '{}' not implemented", request.method),
        }),
    }
}

fn em_get_status(
    state: &AppState,
    pulse_value_w: f64,
    grid_stale_s: f64,
) -> Result<EmStatus, RpcError> {
    let snap = state.snapshot.load_full();
    let now = Instant::now();
    match snap.age {
        None => {
            return Err(RpcError {
                code: error_codes::NO_POWER_DATA,
                message: "no power data from real shelly yet".into(),
            });
        }
        Some(t) if now.duration_since(t).as_secs_f64() > grid_stale_s => {
            // Returning an error here makes the Marstek's CT input go silent
            // exactly like our circuit-mute path — same safety effect,
            // tighter response time than waiting for the dispatcher cycle.
            let age = now.duration_since(t).as_secs_f64();
            return Err(RpcError {
                code: error_codes::NO_POWER_DATA,
                message: format!("real shelly snapshot stale ({age:.1}s old)"),
            });
        }
        _ => {}
    }
    let s = &snap.status;

    // The pulse value goes on phase A; B/C are 0. Marstek aggregates the
    // total anyway and treats it as a delta to its internal target.
    let v = (pulse_value_w * 1000.0).round() / 1000.0;
    let i = v / 230.0;

    Ok(EmStatus {
        id: 0,
        a_current: Some(i),
        a_voltage: s.a_voltage.or(Some(230.0)),
        a_act_power: Some(v),
        a_aprt_power: Some(v.abs()),
        a_pf: s.a_pf.or(Some(1.0)),
        a_freq: s.a_freq.or(Some(50.0)),
        a_errors: vec![],
        b_current: Some(0.0),
        b_voltage: s.b_voltage.or(Some(230.0)),
        b_act_power: Some(0.0),
        b_aprt_power: Some(0.0),
        b_pf: s.b_pf.or(Some(1.0)),
        b_freq: s.b_freq.or(Some(50.0)),
        b_errors: vec![],
        c_current: Some(0.0),
        c_voltage: s.c_voltage.or(Some(230.0)),
        c_act_power: Some(0.0),
        c_aprt_power: Some(0.0),
        c_pf: s.c_pf.or(Some(1.0)),
        c_freq: s.c_freq.or(Some(50.0)),
        c_errors: vec![],
        n_current: Some(0.0),
        n_errors: vec![],
        total_current: Some(i),
        total_act_power: Some(v),
        total_aprt_power: Some(v.abs()),
        user_calibrated_phase: vec![],
        errors: vec![],
    })
}

fn emdata_status(state: &AppState) -> EmDataStatus {
    let energy = state.energy.read();
    EmDataStatus {
        id: 0,
        a_total_act_energy: energy.consumed_wh,
        a_total_act_ret_energy: energy.returned_wh,
        b_total_act_energy: 0.0,
        b_total_act_ret_energy: 0.0,
        c_total_act_energy: 0.0,
        c_total_act_ret_energy: 0.0,
        total_act: energy.consumed_wh,
        total_act_ret: energy.returned_wh,
    }
}

fn shelly_status(
    state: &AppState,
    device: &DeviceContext,
    pulse_value_w: f64,
    grid_stale_s: f64,
) -> Result<ShellyStatus, RpcError> {
    let em_0 = em_get_status(state, pulse_value_w, grid_stale_s)?;
    let emdata_0 = emdata_status(state);
    Ok(ShellyStatus {
        ble: BleStatus {},
        bthome: BtHomeStatus {},
        cloud: CloudStatus { connected: false },
        em_0,
        emdata_0,
        eth: eth_status(),
        modbus: ModbusStatus {},
        mqtt: MqttStatus { connected: false },
        sys: sys_status(state, device),
        temperature_0: temperature_status(),
        wifi: WifiStatus {
            sta_ip: None,
            status: "got ip",
            ssid: None,
            rssi: -55,
        },
        ws: WsStatus { connected: false },
    })
}

fn shelly_get_config(device: &DeviceContext) -> Value {
    json!({
        "ble": {"enable": false, "rpc": {"enable": false}},
        "cloud": {"enable": false, "server": null},
        "em:0": EmConfig::default_em0(),
        "emdata:0": EmDataConfig { id: 0 },
        "eth": {"enable": true, "ipv4mode": "dhcp"},
        "modbus": {"enable": false},
        "mqtt": {"enable": false},
        "sys": sys_config(device),
        "wifi": wifi_config(),
        "ws": {"enable": false, "server": null, "ssl_ca": null}
    })
}

fn shelly_get_components(
    state: &AppState,
    device: &DeviceContext,
    pulse_value_w: f64,
    grid_stale_s: f64,
) -> Result<Value, RpcError> {
    let em = em_get_status(state, pulse_value_w, grid_stale_s)?;
    let emdata = emdata_status(state);
    Ok(json!({
        "components": [
            {"key": "em:0", "status": em, "config": EmConfig::default_em0()},
            {"key": "emdata:0", "status": emdata, "config": EmDataConfig { id: 0 }},
            {"key": "sys", "status": sys_status(state, device), "config": sys_config(device)},
            {"key": "wifi", "status": WifiStatus { sta_ip: None, status: "got ip", ssid: None, rssi: -55 }, "config": wifi_config()},
        ],
        "cfg_rev": 1,
        "offset": 0,
        "total": 4
    }))
}

fn device_info(device: &DeviceContext) -> DeviceInfo {
    DeviceInfo {
        name: None,
        id: device.hostname.clone(),
        mac: device.mac_hex.clone(),
        slot: 1,
        model: "SPEM-003CEBEU",
        generation: 2,
        fw_id: format!("v{}", device.firmware),
        ver: "1.4.4",
        app: "Pro3EM",
        auth_en: false,
        auth_domain: None,
        profile: "triphase",
    }
}

fn sys_status(state: &AppState, device: &DeviceContext) -> SysStatus {
    let uptime = state.started_at.elapsed().as_secs();
    let unixtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let time = format!(
        "{:02}:{:02}",
        (unixtime / 3600) % 24,
        (unixtime / 60) % 60
    );
    SysStatus {
        mac: device.mac_hex.clone(),
        restart_required: false,
        time,
        unixtime,
        uptime,
        ram_size: 259_176,
        ram_free: 87_268,
        ram_min_free: 74_044,
        fs_size: 524_288,
        fs_free: 196_608,
        cfg_rev: 1,
        kvs_rev: 0,
        schedule_rev: 0,
        webhook_rev: 0,
        available_updates: serde_json::Map::new(),
        reset_reason: 3,
    }
}

fn sys_config(device: &DeviceContext) -> Value {
    json!({
        "device": {
            "name": null,
            "mac": device.mac_hex,
            "fw_id": format!("v{}", device.firmware),
            "discoverable": true,
            "eco_mode": false
        },
        "location": {"tz": "Etc/UTC", "lat": null, "lon": null},
        "debug": {"mqtt": {"enable": false}, "websocket": {"enable": false}, "udp": {"addr": null}},
        "ui_data": {},
        "rpc_udp": {"dst_addr": null, "listen_port": null},
        "sntp": {"server": "time.google.com"},
        "cfg_rev": 1
    })
}

fn wifi_status() -> WifiStatus {
    WifiStatus {
        sta_ip: None,
        status: "got ip",
        ssid: None,
        rssi: -55,
    }
}

fn wifi_config() -> Value {
    json!({
        "ap": {"ssid": "ShellyPro3EM", "is_open": true, "enable": false},
        "sta": {"ssid": null, "is_open": true, "enable": true, "ipv4mode": "dhcp"},
        "sta1": {"ssid": null, "is_open": true, "enable": false},
        "roam": {"rssi_thr": -80, "interval": 60}
    })
}

fn eth_status() -> EthStatus {
    EthStatus { ip: None }
}

fn temperature_status() -> TemperatureStatus {
    TemperatureStatus {
        id: 0,
        tC: 35.0,
        tF: 95.0,
    }
}
