//! Shelly Pro 3EM JSON-RPC wire format.
//!
//! Field names and structure match what `sdeigm/uni-meter` produces, which
//! is in turn derived from real Shelly Pro 3EM firmware. Keep field names
//! exact — battery firmware parses by exact key match.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct RequestFrame {
    pub id: Option<i64>,
    pub src: Option<String>,
    pub dst: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: Option<i64>,
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub dst: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationFrame {
    pub src: String,
    pub dst: Option<String>,
    pub method: String,
    pub params: Value,
}

/// EM.GetStatus response — exact field names per Shelly Pro 3EM firmware.
#[derive(Debug, Clone, Serialize, Default)]
pub struct EmStatus {
    pub id: i32,

    pub a_current: Option<f64>,
    pub a_voltage: Option<f64>,
    pub a_act_power: Option<f64>,
    pub a_aprt_power: Option<f64>,
    pub a_pf: Option<f64>,
    pub a_freq: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub a_errors: Vec<String>,

    pub b_current: Option<f64>,
    pub b_voltage: Option<f64>,
    pub b_act_power: Option<f64>,
    pub b_aprt_power: Option<f64>,
    pub b_pf: Option<f64>,
    pub b_freq: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub b_errors: Vec<String>,

    pub c_current: Option<f64>,
    pub c_voltage: Option<f64>,
    pub c_act_power: Option<f64>,
    pub c_aprt_power: Option<f64>,
    pub c_pf: Option<f64>,
    pub c_freq: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub c_errors: Vec<String>,

    pub n_current: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub n_errors: Vec<String>,

    pub total_current: Option<f64>,
    pub total_act_power: Option<f64>,
    pub total_aprt_power: Option<f64>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub user_calibrated_phase: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// EM.GetStatus response as parsed from the real Shelly. Same field set,
/// but every field is optional during deserialization since older firmwares
/// may omit some.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EmStatusIncoming {
    #[serde(default)]
    pub id: i32,

    pub a_current: Option<f64>,
    pub a_voltage: Option<f64>,
    pub a_act_power: Option<f64>,
    pub a_aprt_power: Option<f64>,
    pub a_pf: Option<f64>,
    pub a_freq: Option<f64>,

    pub b_current: Option<f64>,
    pub b_voltage: Option<f64>,
    pub b_act_power: Option<f64>,
    pub b_aprt_power: Option<f64>,
    pub b_pf: Option<f64>,
    pub b_freq: Option<f64>,

    pub c_current: Option<f64>,
    pub c_voltage: Option<f64>,
    pub c_act_power: Option<f64>,
    pub c_aprt_power: Option<f64>,
    pub c_pf: Option<f64>,
    pub c_freq: Option<f64>,

    pub n_current: Option<f64>,

    pub total_current: Option<f64>,
    pub total_act_power: Option<f64>,
    pub total_aprt_power: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EmDataStatus {
    pub id: i32,
    pub a_total_act_energy: f64,
    pub a_total_act_ret_energy: f64,
    pub b_total_act_energy: f64,
    pub b_total_act_ret_energy: f64,
    pub c_total_act_energy: f64,
    pub c_total_act_ret_energy: f64,
    pub total_act: f64,
    pub total_act_ret: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmConfig {
    pub id: i32,
    pub name: Option<String>,
    pub blink_mode_selector: &'static str,
    pub phase_selector: &'static str,
    pub monitor_phase_sequence: bool,
    pub reverse: ReverseConfig,
    pub ct_type: &'static str,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReverseConfig {
    pub a: Option<bool>,
    pub b: Option<bool>,
    pub c: Option<bool>,
}

impl EmConfig {
    pub fn default_em0() -> Self {
        Self {
            id: 0,
            name: None,
            blink_mode_selector: "active_energy",
            phase_selector: "a",
            monitor_phase_sequence: true,
            reverse: ReverseConfig::default(),
            ct_type: "120A",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmDataConfig {
    pub id: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: Option<String>,
    pub id: String,
    pub mac: String,
    pub slot: i32,
    pub model: &'static str,
    #[serde(rename = "gen")]
    pub generation: i32,
    pub fw_id: String,
    pub ver: &'static str,
    pub app: &'static str,
    pub auth_en: bool,
    pub auth_domain: Option<String>,
    pub profile: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SysStatus {
    pub mac: String,
    pub restart_required: bool,
    pub time: String,
    pub unixtime: i64,
    pub uptime: u64,
    pub ram_size: u64,
    pub ram_free: u64,
    pub ram_min_free: u64,
    pub fs_size: u64,
    pub fs_free: u64,
    pub cfg_rev: u32,
    pub kvs_rev: u32,
    pub schedule_rev: u32,
    pub webhook_rev: u32,
    pub available_updates: serde_json::Map<String, Value>,
    pub reset_reason: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct WifiStatus {
    pub sta_ip: Option<String>,
    pub status: &'static str,
    pub ssid: Option<String>,
    pub rssi: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EthStatus {
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudStatus {
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsStatus {
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MqttStatus {
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModbusStatus {}

#[derive(Debug, Clone, Serialize)]
pub struct BleStatus {}

#[derive(Debug, Clone, Serialize)]
pub struct BtHomeStatus {}

#[derive(Debug, Clone, Serialize)]
#[allow(non_snake_case)]
pub struct TemperatureStatus {
    pub id: i32,
    pub tC: f64,
    pub tF: f64,
}

/// Top-level Shelly.GetStatus response.
#[derive(Debug, Clone, Serialize)]
pub struct ShellyStatus {
    pub ble: BleStatus,
    pub bthome: BtHomeStatus,
    pub cloud: CloudStatus,
    #[serde(rename = "em:0")]
    pub em_0: EmStatus,
    #[serde(rename = "emdata:0")]
    pub emdata_0: EmDataStatus,
    pub eth: EthStatus,
    pub modbus: ModbusStatus,
    pub mqtt: MqttStatus,
    pub sys: SysStatus,
    #[serde(rename = "temperature:0")]
    pub temperature_0: TemperatureStatus,
    pub wifi: WifiStatus,
    pub ws: WsStatus,
}

/// Standard JSON-RPC error codes used by Shelly.
pub mod error_codes {
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const NO_POWER_DATA: i32 = -114;
    pub const TEMPORARILY_UNAVAILABLE: i32 = -503;
}

/// Build a default `EmDataStatus` (energy counters) with zeroed fields.
impl EmDataStatus {
    pub fn zeroed() -> Self {
        Self::default()
    }
}
