//! Virtual Shelly Pro 3EM emulation. Listens on UDP and serves the
//! battery-specific allocations.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use parking_lot::Mutex;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// Battery is considered "rejoining" if we haven't seen a request from
/// it for this long — bumps the next log line back up to INFO so the
/// reconnect is visible in default-level logs.
const RECONNECT_THRESHOLD: Duration = Duration::from_secs(30);

use crate::config::Config;
use crate::rpc::{
    BleStatus, BtHomeStatus, CloudStatus, DeviceInfo, EmConfig, EmDataConfig, EmDataStatus,
    EmStatus, EthStatus, ModbusStatus, MqttStatus, RequestFrame, ResponseFrame, RpcError,
    ShellyStatus, SysStatus, TemperatureStatus, WifiStatus, WsStatus, error_codes,
};
use crate::state::{AppState, PhaseWatts};

const RECV_BUF: usize = 16 * 1024;

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    // Bind once at startup; UDP port and bind interface changes need a
    // restart to take effect.
    let initial = config.load_full();
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
    // DeviceContext is derived from MAC/hostname/firmware. The MAC fallback
    // does an OS lookup we don't want to repeat per request, so cache it.
    // Hostname/MAC/firmware edits via the admin UI require a restart.
    let device = DeviceContext::from_config(&initial);
    // Per-(battery, method) request counters — used to throttle "what we
    // sent" logging to every 10th response so the console isn't flooded.
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
        let config = config.clone();
        let socket = socket.clone();
        let device = device.clone();
        let counters = response_counters.clone();
        tokio::spawn(async move {
            // Re-read the config snapshot per request so live edits to
            // dispatcher / batteries / min_sample_period_ms etc. apply
            // immediately.
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
    // Multiplex drop-mode: if the dispatcher marked this battery as
    // inactive (it's not the lead of its circuit, or it's force-
    // deactivated for testing), we deliberately stop responding so
    // the inverter's CT-watchdog shuts it off. Bedrock of the safety
    // guarantee that "max one battery active per circuit".
    let inactive_info = {
        let allocs = state.allocations.read();
        allocs
            .get(&peer.ip())
            .map(|a| (a.battery_id.clone(), a.multiplex_inactive))
    };
    if let Some((ref id, true)) = inactive_info {
        debug!(
            battery = %id,
            peer = %peer,
            method = %request.method,
            "drop poll: multiplex-inactive"
        );
        // Still record the poll so the GUI can show "last_request"
        // for diagnostics, but don't send anything back.
        state.last_poll_at.write().insert(peer.ip(), now);
        return;
    }

    let log_state = {
        let prev = state.last_poll_at.write().insert(peer.ip(), now);
        let was_silent = prev
            .map(|t| now.saturating_duration_since(t) > RECONNECT_THRESHOLD)
            .unwrap_or(true);
        let battery_id = inactive_info.map(|(id, _)| id);
        match battery_id {
            Some(id) => PollLog::Known {
                battery_id: id,
                was_silent,
            },
            None => PollLog::Unknown,
        }
    };

    match &log_state {
        PollLog::Known { battery_id, was_silent: true } => {
            info!(
                battery = %battery_id,
                peer = %peer,
                method = %request.method,
                "battery polling started"
            );
        }
        PollLog::Known { battery_id, was_silent: false } => {
            debug!(
                battery = %battery_id,
                peer = %peer,
                method = %request.method,
                "battery poll"
            );
        }
        PollLog::Unknown => {
            debug!(
                peer = %peer,
                method = %request.method,
                "poll from unknown source"
            );
        }
    }

    let method = request.method.clone();
    let response = build_response(state, config, device, peer.ip(), request);
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

    // Log every 10th response per (peer, method) at INFO with a snapshot
    // of what we actually sent. EM.GetStatus is the loud one (~1 Hz);
    // other methods don't repeat often so they end up logged each time.
    let count = {
        let mut map = response_counters.lock();
        let counter = map
            .entry((peer.ip(), method.clone()))
            .or_insert_with(|| AtomicU64::new(0));
        counter.fetch_add(1, Ordering::Relaxed) + 1
    };
    if count.is_multiple_of(10) || count == 1 {
        let battery_id = match &log_state {
            PollLog::Known { battery_id, .. } => battery_id.clone(),
            PollLog::Unknown => format!("{}", peer.ip()),
        };
        let summary = summarize_response(&response.result);
        info!(
            battery = %battery_id,
            method = %method,
            count,
            sent = %summary,
            "response sent"
        );
    }
}

/// Compact one-line summary of the most relevant fields for logging.
fn summarize_response(result: &Option<Value>) -> String {
    let Some(v) = result else { return "<error>".into() };
    let mut out = String::new();
    let mut push = |k: &str| {
        if let Some(val) = v.get(k) {
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(k);
            out.push('=');
            out.push_str(&val.to_string());
        }
    };
    // EM.GetStatus
    push("a_act_power");
    push("b_act_power");
    push("c_act_power");
    push("total_act_power");
    // EMData.GetStatus
    push("total_act");
    push("total_act_ret");
    // Shelly.GetDeviceInfo
    push("id");
    push("model");
    push("gen");
    if out.is_empty() {
        // Fallback — just the type if nothing matched
        out = format!("<{} bytes>", serde_json::to_string(v).map(|s| s.len()).unwrap_or(0));
    }
    out
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

enum PollLog {
    Known { battery_id: String, was_silent: bool },
    Unknown,
}

pub fn build_response(
    state: &AppState,
    config: &Config,
    device: &DeviceContext,
    peer_ip: IpAddr,
    request: RequestFrame,
) -> ResponseFrame {
    let dst = request.src.clone();
    let id = request.id;
    let method = request.method.clone();

    match dispatch_method(state, config, device, peer_ip, &request) {
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
    peer_ip: IpAddr,
    request: &RequestFrame,
) -> Result<Value, RpcError> {
    let method_lc = request.method.to_lowercase();
    // Saldierende allocation: each battery sees its commanded value on
    // phase A (B/C zero). The dispatcher writes `phase_w` directly; we
    // pass it through unchanged.
    let phase_w = phase_w_for(state, peer_ip);

    match method_lc.as_str() {
        "em.getstatus" => Ok(serde_json::to_value(em_get_status(state, phase_w)?).unwrap()),
        "em.getconfig" => Ok(serde_json::to_value(EmConfig::default_em0()).unwrap()),
        "emdata.getstatus" => Ok(serde_json::to_value(emdata_status(state, phase_w)).unwrap()),
        "emdata.getconfig" => Ok(serde_json::to_value(EmDataConfig { id: 0 }).unwrap()),
        "shelly.getdeviceinfo" => Ok(serde_json::to_value(device_info(device)).unwrap()),
        "shelly.getstatus" => {
            Ok(serde_json::to_value(shelly_status(state, device, phase_w)?).unwrap())
        }
        "shelly.getconfig" => Ok(shelly_get_config(device)),
        "shelly.getcomponents" => Ok(shelly_get_components(state, device, phase_w)?),
        "shelly.reboot" => Ok(json!({})),
        "sys.getstatus" => Ok(serde_json::to_value(sys_status(state, device)).unwrap()),
        "sys.getconfig" => Ok(sys_config(device)),
        "wifi.getstatus" => Ok(serde_json::to_value(wifi_status(config)).unwrap()),
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
        "temperature.getstatus" => {
            Ok(serde_json::to_value(temperature_status()).unwrap())
        }
        "script.list" => Ok(json!({"scripts": []})),
        "script.getcode" => Ok(json!({"data": "", "left": 0})),
        _ => Err(RpcError {
            code: error_codes::METHOD_NOT_FOUND,
            message: format!("method '{}' not implemented", request.method),
        }),
    }
}

/// Look up the dispatcher's commanded phase split for a battery. If
/// the battery hasn't been allocated yet (first request after start),
/// fall back to all-zero — the inverter reads "no demand", which is
/// the safest possible state.
fn phase_w_for(state: &AppState, peer_ip: IpAddr) -> PhaseWatts {
    state
        .allocations
        .read()
        .get(&peer_ip)
        .map(|a| a.phase_w)
        .unwrap_or(PhaseWatts { a: 0.0, b: 0.0, c: 0.0 })
}

fn em_get_status(state: &AppState, phase_w: PhaseWatts) -> Result<EmStatus, RpcError> {
    let snap = state.snapshot.load_full();
    if snap.age.is_none() {
        return Err(RpcError {
            code: error_codes::NO_POWER_DATA,
            message: "no power data from real shelly yet".into(),
        });
    }
    let s = &snap.status;

    // Active power per phase comes straight from the dispatcher. With
    // saldierende allocation phase A carries the full commanded value
    // and B/C are zero. Inverters look at the total only.
    let a_p = round_w(Some(phase_w.a));
    let b_p = round_w(Some(phase_w.b));
    let c_p = round_w(Some(phase_w.c));

    // Apparent power: assume PF≈1 for the synthetic feed (the inverter
    // doesn't use this for control anyway).
    let a_s = a_p;
    let b_s = b_p;
    let c_s = c_p;

    // Synthesise current from P/U using the real Shelly's voltage.
    let a_current = current_from_power(phase_w.a, s.a_voltage);
    let b_current = current_from_power(phase_w.b, s.b_voltage);
    let c_current = current_from_power(phase_w.c, s.c_voltage);

    let total_p = sum_opt(&[a_p, b_p, c_p]);
    let total_s = sum_opt(&[a_s, b_s, c_s]);
    let total_i = sum_opt(&[a_current, b_current, c_current]);

    Ok(EmStatus {
        id: 0,
        a_current,
        a_voltage: s.a_voltage,
        a_act_power: a_p,
        a_aprt_power: a_s,
        a_pf: s.a_pf,
        a_freq: s.a_freq,
        a_errors: vec![],
        b_current,
        b_voltage: s.b_voltage,
        b_act_power: b_p,
        b_aprt_power: b_s,
        b_pf: s.b_pf,
        b_freq: s.b_freq,
        b_errors: vec![],
        c_current,
        c_voltage: s.c_voltage,
        c_act_power: c_p,
        c_aprt_power: c_s,
        c_pf: s.c_pf,
        c_freq: s.c_freq,
        c_errors: vec![],
        n_current: s.n_current,
        n_errors: vec![],
        total_current: total_i,
        total_act_power: total_p,
        total_aprt_power: total_s,
        user_calibrated_phase: vec![],
        errors: vec![],
    })
}

/// Round optional watt value to whole watts.
fn round_w(v: Option<f64>) -> Option<f64> {
    v.map(|x| x.round())
}

fn current_from_power(p_w: f64, u_v: Option<f64>) -> Option<f64> {
    let u = u_v.unwrap_or(230.0);
    if u.abs() < 1.0 {
        Some(0.0)
    } else {
        Some(p_w / u)
    }
}

fn emdata_status(state: &AppState, phase_w: PhaseWatts) -> EmDataStatus {
    let energy = state.energy.read();
    // Energy counters scale with the share of total active power each
    // phase contributes. With saldierende allocation phase A normally
    // carries everything, so the energy on A reflects the battery's
    // total throughput.
    let total_abs = phase_w.a.abs() + phase_w.b.abs() + phase_w.c.abs();
    let (sa, sb, sc) = if total_abs > 1e-3 {
        (
            phase_w.a.abs() / total_abs,
            phase_w.b.abs() / total_abs,
            phase_w.c.abs() / total_abs,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    let a_c = energy.a_consumed_wh * sa;
    let a_r = energy.a_returned_wh * sa;
    let b_c = energy.b_consumed_wh * sb;
    let b_r = energy.b_returned_wh * sb;
    let c_c = energy.c_consumed_wh * sc;
    let c_r = energy.c_returned_wh * sc;
    EmDataStatus {
        id: 0,
        a_total_act_energy: a_c,
        a_total_act_ret_energy: a_r,
        b_total_act_energy: b_c,
        b_total_act_ret_energy: b_r,
        c_total_act_energy: c_c,
        c_total_act_ret_energy: c_r,
        total_act: a_c + b_c + c_c,
        total_act_ret: a_r + b_r + c_r,
    }
}

fn shelly_status(
    state: &AppState,
    device: &DeviceContext,
    phase_w: PhaseWatts,
) -> Result<ShellyStatus, RpcError> {
    let em_0 = em_get_status(state, phase_w)?;
    let emdata_0 = emdata_status(state, phase_w);
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
            status: "got ip".into(),
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
    phase_w: PhaseWatts,
) -> Result<Value, RpcError> {
    let em = em_get_status(state, phase_w)?;
    let emdata = emdata_status(state, phase_w);
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

fn wifi_status(_config: &Config) -> WifiStatus {
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

fn sum_opt(values: &[Option<f64>]) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for v in values {
        if let Some(x) = v {
            total += x;
            any = true;
        }
    }
    if any { Some(total) } else { None }
}
