//! Decode the raw `u16` holding-register cache into named, scaled,
//! unit-bearing values per Marstek variant — the same view HA's
//! ViperRNMC integration shows users.
//!
//! Source of truth: the per-variant YAMLs in
//! `ViperRNMC/marstek_venus_modbus`. We mirror their address → name →
//! kind/scale/unit table so the admin UI can render exactly what HA
//! would, without needing HA to be installed.
//!
//! Output shape is JSON-friendly: each register becomes a
//! `DecodedRegister` with `value` already typed (number / string /
//! null) and `unit` set to whatever HA would display.

use serde::Serialize;
use std::collections::HashMap;

use crate::config::MarstekModel;

/// What we know about one named register / signal.
#[derive(Debug, Clone, Copy)]
pub struct RegisterDef {
    pub address: u16,
    pub name: &'static str,
    pub section: &'static str,
    pub unit: Option<&'static str>,
    pub kind: RegisterKind,
}

#[derive(Debug, Clone, Copy)]
pub enum RegisterKind {
    /// Unsigned 16-bit. `scale` applied as multiplication.
    Uint16 { scale: f64 },
    /// Signed 16-bit.
    Int16 { scale: f64 },
    /// Unsigned 32-bit across TWO registers (big-endian word order).
    Uint32 { scale: f64 },
    /// Signed 32-bit across TWO registers.
    Int32 { scale: f64 },
    /// ASCII string packed two chars per register. `regs` is the
    /// number of u16 cells.
    Char { regs: u16 },
    /// uint16 lookup against a (raw, label) table. Falls back to the
    /// raw integer when unmatched.
    Enum(&'static [(u16, &'static str)]),
}

/// One decoded register entry, ready for JSON serialisation.
#[derive(Debug, Serialize, Clone)]
pub struct DecodedRegister {
    pub address: u16,
    pub name: &'static str,
    pub section: &'static str,
    /// Number / string / null. Null = the register is in the variant's
    /// table but not in our cache yet.
    pub value: serde_json::Value,
    /// Unit string for UI rendering (e.g. "W", "V", "%", "kWh", "°C").
    pub unit: Option<&'static str>,
    /// Raw u16 (or first register for multi-register fields) — handy
    /// for debugging when the scaled value looks wrong.
    pub raw: Option<u16>,
}

// ---------------------------------------------------------------------------
// Public decode entry point.
// ---------------------------------------------------------------------------

/// Decode every register definition for `model` against the supplied
/// cache, returning the result in declaration order (so the UI gets a
/// stable layout).
pub fn decode(model: MarstekModel, cache: &HashMap<u16, u16>) -> Vec<DecodedRegister> {
    register_table(model)
        .iter()
        .map(|def| decode_one(def, cache))
        .collect()
}

fn decode_one(def: &RegisterDef, cache: &HashMap<u16, u16>) -> DecodedRegister {
    let raw = cache.get(&def.address).copied();
    let value = match def.kind {
        RegisterKind::Uint16 { scale } => raw
            .map(|r| serde_json::Value::from(scale_num(f64::from(r), scale)))
            .unwrap_or(serde_json::Value::Null),
        RegisterKind::Int16 { scale } => raw
            .map(|r| serde_json::Value::from(scale_num(f64::from(r as i16), scale)))
            .unwrap_or(serde_json::Value::Null),
        RegisterKind::Uint32 { scale } => {
            match (cache.get(&def.address).copied(), cache.get(&(def.address + 1)).copied()) {
                (Some(hi), Some(lo)) => {
                    let combined = ((u32::from(hi)) << 16) | u32::from(lo);
                    serde_json::Value::from(scale_num(f64::from(combined), scale))
                }
                _ => serde_json::Value::Null,
            }
        }
        RegisterKind::Int32 { scale } => {
            match (cache.get(&def.address).copied(), cache.get(&(def.address + 1)).copied()) {
                (Some(hi), Some(lo)) => {
                    let combined = ((u32::from(hi)) << 16) | u32::from(lo);
                    serde_json::Value::from(scale_num(f64::from(combined as i32), scale))
                }
                _ => serde_json::Value::Null,
            }
        }
        RegisterKind::Char { regs } => {
            let mut buf = String::new();
            let mut have_any = false;
            for i in 0..regs {
                let Some(w) = cache.get(&(def.address + i)).copied() else {
                    break;
                };
                have_any = true;
                // Two ASCII chars per register, high byte first.
                let hi = (w >> 8) as u8;
                let lo = (w & 0xFF) as u8;
                push_ascii(&mut buf, hi);
                push_ascii(&mut buf, lo);
            }
            // Trim NULs / trailing whitespace the firmware sometimes pads with.
            let trimmed = buf.trim_end_matches(|c: char| c == '\0' || c.is_whitespace());
            if !have_any {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(trimmed.to_string())
            }
        }
        RegisterKind::Enum(table) => match raw {
            Some(r) => {
                if let Some((_, label)) = table.iter().find(|(v, _)| *v == r) {
                    serde_json::Value::String(format!("{label} ({r})"))
                } else {
                    serde_json::Value::from(r)
                }
            }
            None => serde_json::Value::Null,
        },
    };
    DecodedRegister {
        address: def.address,
        name: def.name,
        section: def.section,
        value,
        unit: def.unit,
        raw,
    }
}

/// Round to a sensible number of decimals so the JSON stays readable.
fn scale_num(raw: f64, scale: f64) -> f64 {
    let v = raw * scale;
    // 4 decimals max — enough for cell voltages (0.001 V) and currents
    // at 0.004 A precision, while killing trailing 0.000000001 jitter
    // from f64 rounding.
    (v * 10_000.0).round() / 10_000.0
}

fn push_ascii(buf: &mut String, byte: u8) {
    // Marstek pads strings with NUL or whitespace; ASCII chars only.
    if byte != 0 && byte.is_ascii() {
        buf.push(byte as char);
    }
}

// ---------------------------------------------------------------------------
// Per-variant register tables.
//
// Keep these in sync with `MarstekModel::fast_registers()` +
// `slow_registers()` — every address that bulk_refresh tries to read
// should have a decoder entry so the UI can show what it means.
// ---------------------------------------------------------------------------

const INVERTER_STATE: &[(u16, &str)] = &[
    (0, "Sleep"),
    (1, "Standby"),
    (2, "Charge"),
    (3, "Discharge"),
    (4, "Backup"),
    (5, "OTA"),
    (6, "Bypass"),
];

const FORCE_MODE: &[(u16, &str)] = &[
    (0, "Standby"),
    (1, "Charge"),
    (2, "Discharge"),
];

const USER_WORK_MODE: &[(u16, &str)] = &[
    (0, "Manual"),
    (1, "AntiFeed"),
    (2, "Trade"),
];

const RS485_CTRL: &[(u16, &str)] = &[
    (21930, "On (control enabled)"),
    (21947, "Off (auto mode)"),
];

const ON_OFF: &[(u16, &str)] = &[(0, "Off"), (1, "On")];

pub fn register_table(model: MarstekModel) -> &'static [RegisterDef] {
    match model {
        MarstekModel::VenusEV1V2 => &VENUS_E_V1V2_TABLE,
        MarstekModel::VenusEV3 => &VENUS_E_V3_TABLE,
        MarstekModel::VenusD => &VENUS_D_TABLE,
        MarstekModel::VenusA => &VENUS_A_TABLE,
    }
}

// ----- Venus E V1/V2 -----

const VENUS_E_V1V2_TABLE: &[RegisterDef] = &[
    // ---- Metadata ----
    RegisterDef { address: 31000, name: "device_name", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 10 } },
    RegisterDef { address: 31200, name: "serial_number", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 10 } },
    RegisterDef { address: 31100, name: "software_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 31101, name: "ems_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 31102, name: "bms_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30800, name: "comm_module_firmware", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    RegisterDef { address: 30402, name: "mac_address", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    // ---- Connectivity ----
    RegisterDef { address: 30303, name: "wifi_signal_strength", section: "Connectivity", unit: Some("dBm"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 30300, name: "wifi_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    RegisterDef { address: 30301, name: "bluetooth_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    RegisterDef { address: 30302, name: "cloud_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    // ---- Battery DC ----
    RegisterDef { address: 32100, name: "battery_voltage", section: "Battery", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 32101, name: "battery_current", section: "Battery", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.01 } },
    RegisterDef { address: 32102, name: "battery_power", section: "Battery", unit: Some("W"), kind: RegisterKind::Int32 { scale: 1.0 } },
    RegisterDef { address: 32104, name: "battery_soc", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 32105, name: "battery_total_energy", section: "Battery", unit: Some("kWh"), kind: RegisterKind::Uint16 { scale: 0.001 } },
    // ---- AC grid ----
    RegisterDef { address: 32200, name: "ac_voltage", section: "AC Grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 32201, name: "ac_current", section: "AC Grid", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.01 } },
    RegisterDef { address: 32202, name: "ac_power", section: "AC Grid", unit: Some("W"), kind: RegisterKind::Int32 { scale: 1.0 } },
    RegisterDef { address: 32204, name: "ac_frequency", section: "AC Grid", unit: Some("Hz"), kind: RegisterKind::Int16 { scale: 0.01 } },
    // ---- AC off-grid (backup) ----
    RegisterDef { address: 32300, name: "ac_offgrid_voltage", section: "AC Off-grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 32301, name: "ac_offgrid_current", section: "AC Off-grid", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 32302, name: "ac_offgrid_power", section: "AC Off-grid", unit: Some("W"), kind: RegisterKind::Int32 { scale: 1.0 } },
    // ---- Energy counters ----
    RegisterDef { address: 33000, name: "total_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33002, name: "total_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 33004, name: "daily_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33006, name: "daily_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 33008, name: "monthly_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33010, name: "monthly_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    // ---- Temperatures ----
    RegisterDef { address: 35000, name: "internal_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35001, name: "mos1_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35002, name: "mos2_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35010, name: "max_cell_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 35011, name: "min_cell_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 1.0 } },
    // ---- Cell voltage extremes ----
    RegisterDef { address: 37007, name: "max_cell_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 37008, name: "min_cell_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    // ---- Inverter state ----
    RegisterDef { address: 35100, name: "inverter_state", section: "State", unit: None, kind: RegisterKind::Enum(INVERTER_STATE) },
    RegisterDef { address: 36000, name: "alarm_status_lo", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 36001, name: "alarm_status_hi", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 36100, name: "fault_status_1", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 36101, name: "fault_status_2", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 36102, name: "fault_status_3", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 36103, name: "fault_status_4", section: "State", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    // ---- Control registers ----
    RegisterDef { address: 42000, name: "rs485_control_mode", section: "Control", unit: None, kind: RegisterKind::Enum(RS485_CTRL) },
    RegisterDef { address: 42010, name: "force_mode", section: "Control", unit: None, kind: RegisterKind::Enum(FORCE_MODE) },
    RegisterDef { address: 42011, name: "charge_to_soc", section: "Control", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42020, name: "set_charge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42021, name: "set_discharge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 43000, name: "user_work_mode", section: "Control", unit: None, kind: RegisterKind::Enum(USER_WORK_MODE) },
    RegisterDef { address: 41010, name: "discharge_limit_mode", section: "Control", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 41100, name: "modbus_address", section: "Control", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 41200, name: "backup_function", section: "Control", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    // ---- BMS limits + grid standard ----
    RegisterDef { address: 44000, name: "bms_charging_cutoff", section: "BMS", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 44001, name: "bms_discharging_cutoff", section: "BMS", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 44002, name: "bms_max_charge_power", section: "BMS", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 44003, name: "bms_max_discharge_power", section: "BMS", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 44100, name: "grid_standard", section: "BMS", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
];

// ----- Venus E V3 -----

const VENUS_E_V3_TABLE: &[RegisterDef] = &[
    // ---- Metadata ----
    RegisterDef { address: 31000, name: "device_name", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 10 } },
    RegisterDef { address: 30200, name: "ems_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30202, name: "vms_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30204, name: "bms_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30350, name: "comm_module_firmware", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    RegisterDef { address: 30304, name: "mac_address", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    // ---- Connectivity ----
    RegisterDef { address: 30303, name: "wifi_signal_strength", section: "Connectivity", unit: Some("dBm"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 30300, name: "wifi_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    RegisterDef { address: 30301, name: "bluetooth_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    RegisterDef { address: 30302, name: "cloud_status", section: "Connectivity", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    // ---- Battery DC (V3 = 16-bit single-register power) ----
    RegisterDef { address: 30001, name: "battery_power", section: "Battery", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 30100, name: "battery_voltage", section: "Battery", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 30101, name: "battery_current", section: "Battery", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 34002, name: "battery_soc", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34003, name: "battery_cycle_count", section: "Battery", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 32105, name: "battery_total_energy", section: "Battery", unit: Some("kWh"), kind: RegisterKind::Uint16 { scale: 0.001 } },
    // ---- AC grid ----
    RegisterDef { address: 30006, name: "ac_power", section: "AC Grid", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 32200, name: "ac_voltage", section: "AC Grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 37004, name: "ac_current", section: "AC Grid", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.004 } },
    RegisterDef { address: 32204, name: "ac_frequency", section: "AC Grid", unit: Some("Hz"), kind: RegisterKind::Int16 { scale: 0.1 } },
    // ---- AC off-grid ----
    RegisterDef { address: 32300, name: "ac_offgrid_voltage", section: "AC Off-grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 32301, name: "ac_offgrid_current", section: "AC Off-grid", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 32302, name: "ac_offgrid_power", section: "AC Off-grid", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    // ---- Energy counters ----
    RegisterDef { address: 33000, name: "total_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33002, name: "total_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 33004, name: "daily_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33006, name: "daily_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 33008, name: "monthly_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33010, name: "monthly_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    // ---- Temperatures ----
    RegisterDef { address: 35000, name: "internal_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35001, name: "mos1_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35002, name: "mos2_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35010, name: "max_cell_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35011, name: "min_cell_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    // ---- Cell extremes ----
    RegisterDef { address: 37007, name: "max_cell_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 37008, name: "min_cell_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    // Per-cell voltages (16 cells on V3)
    RegisterDef { address: 34018, name: "cell_1_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34019, name: "cell_2_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34020, name: "cell_3_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34021, name: "cell_4_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34022, name: "cell_5_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34023, name: "cell_6_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34024, name: "cell_7_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34025, name: "cell_8_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34026, name: "cell_9_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34027, name: "cell_10_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34028, name: "cell_11_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34029, name: "cell_12_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34030, name: "cell_13_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34031, name: "cell_14_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34032, name: "cell_15_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    RegisterDef { address: 34033, name: "cell_16_voltage", section: "Cells", unit: Some("V"), kind: RegisterKind::Int16 { scale: 0.001 } },
    // ---- Inverter state ----
    RegisterDef { address: 35100, name: "inverter_state", section: "State", unit: None, kind: RegisterKind::Enum(INVERTER_STATE) },
    // ---- Control ----
    RegisterDef { address: 42000, name: "rs485_control_mode", section: "Control", unit: None, kind: RegisterKind::Enum(RS485_CTRL) },
    RegisterDef { address: 42010, name: "force_mode", section: "Control", unit: None, kind: RegisterKind::Enum(FORCE_MODE) },
    RegisterDef { address: 42011, name: "charge_to_soc", section: "Control", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42020, name: "set_charge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42021, name: "set_discharge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 43000, name: "user_work_mode", section: "Control", unit: None, kind: RegisterKind::Enum(USER_WORK_MODE) },
    RegisterDef { address: 41100, name: "modbus_address", section: "Control", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 41200, name: "backup_function", section: "Control", unit: None, kind: RegisterKind::Enum(ON_OFF) },
    // ---- BMS limits ----
    RegisterDef { address: 44002, name: "bms_max_charge_power", section: "BMS", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 44003, name: "bms_max_discharge_power", section: "BMS", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
];

// ----- Venus D -----

const VENUS_D_TABLE: &[RegisterDef] = &[
    RegisterDef { address: 31000, name: "device_name", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 10 } },
    RegisterDef { address: 30200, name: "ems_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30202, name: "vms_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30204, name: "bms_version", section: "Metadata", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30350, name: "comm_module_firmware", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    RegisterDef { address: 30304, name: "mac_address", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 6 } },
    RegisterDef { address: 30303, name: "wifi_signal_strength", section: "Connectivity", unit: Some("dBm"), kind: RegisterKind::Int16 { scale: 1.0 } },
    // Battery / power (16-bit on D)
    RegisterDef { address: 30001, name: "battery_power", section: "Battery", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 30100, name: "battery_voltage", section: "Battery", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 30101, name: "battery_current", section: "Battery", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 32104, name: "battery_soc", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 32105, name: "battery_total_energy", section: "Battery", unit: Some("kWh"), kind: RegisterKind::Uint16 { scale: 0.001 } },
    RegisterDef { address: 34003, name: "battery_cycle_count", section: "Battery", unit: None, kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 30006, name: "ac_power", section: "AC Grid", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 32200, name: "ac_voltage", section: "AC Grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 37004, name: "ac_current", section: "AC Grid", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.004 } },
    RegisterDef { address: 32204, name: "ac_frequency", section: "AC Grid", unit: Some("Hz"), kind: RegisterKind::Int16 { scale: 0.1 } },
    // MPPT — D + A have PV inputs
    RegisterDef { address: 30020, name: "mppt1_voltage", section: "MPPT", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30021, name: "mppt2_voltage", section: "MPPT", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30022, name: "mppt3_voltage", section: "MPPT", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30023, name: "mppt4_voltage", section: "MPPT", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30024, name: "mppt1_current", section: "MPPT", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30025, name: "mppt2_current", section: "MPPT", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30026, name: "mppt3_current", section: "MPPT", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30027, name: "mppt4_current", section: "MPPT", unit: Some("A"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30037, name: "mppt1_power", section: "MPPT", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30038, name: "mppt2_power", section: "MPPT", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30039, name: "mppt3_power", section: "MPPT", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30040, name: "mppt4_power", section: "MPPT", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    // Energy counters
    RegisterDef { address: 33000, name: "total_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33002, name: "total_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 33004, name: "daily_charging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Uint32 { scale: 0.01 } },
    RegisterDef { address: 33006, name: "daily_discharging_energy", section: "Energy", unit: Some("kWh"), kind: RegisterKind::Int32 { scale: 0.01 } },
    RegisterDef { address: 35000, name: "internal_temperature", section: "Temperature", unit: Some("°C"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 35100, name: "inverter_state", section: "State", unit: None, kind: RegisterKind::Enum(INVERTER_STATE) },
    RegisterDef { address: 42000, name: "rs485_control_mode", section: "Control", unit: None, kind: RegisterKind::Enum(RS485_CTRL) },
    RegisterDef { address: 42010, name: "force_mode", section: "Control", unit: None, kind: RegisterKind::Enum(FORCE_MODE) },
    RegisterDef { address: 42020, name: "set_charge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42021, name: "set_discharge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
];

// ----- Venus A -----

const VENUS_A_TABLE: &[RegisterDef] = &[
    RegisterDef { address: 31000, name: "device_name", section: "Metadata", unit: None, kind: RegisterKind::Char { regs: 10 } },
    RegisterDef { address: 30001, name: "battery_power", section: "Battery", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 30100, name: "battery_voltage", section: "Battery", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.01 } },
    RegisterDef { address: 30101, name: "battery_current", section: "Battery", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.1 } },
    RegisterDef { address: 32104, name: "battery_soc", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 34002, name: "battery_soc_unit1", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34102, name: "battery_soc_unit2", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34202, name: "battery_soc_unit3", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34302, name: "battery_soc_unit4", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34402, name: "battery_soc_unit5", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 34502, name: "battery_soc_unit6", section: "Battery", unit: Some("%"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30006, name: "ac_power", section: "AC Grid", unit: Some("W"), kind: RegisterKind::Int16 { scale: 1.0 } },
    RegisterDef { address: 32200, name: "ac_voltage", section: "AC Grid", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 37004, name: "ac_current", section: "AC Grid", unit: Some("A"), kind: RegisterKind::Int16 { scale: 0.004 } },
    RegisterDef { address: 30020, name: "mppt1_voltage", section: "MPPT", unit: Some("V"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 30037, name: "mppt1_power", section: "MPPT", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 0.1 } },
    RegisterDef { address: 35100, name: "inverter_state", section: "State", unit: None, kind: RegisterKind::Enum(INVERTER_STATE) },
    RegisterDef { address: 42010, name: "force_mode", section: "Control", unit: None, kind: RegisterKind::Enum(FORCE_MODE) },
    RegisterDef { address: 42020, name: "set_charge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
    RegisterDef { address: 42021, name: "set_discharge_power", section: "Control", unit: Some("W"), kind: RegisterKind::Uint16 { scale: 1.0 } },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_uint16_with_scale() {
        let mut cache = HashMap::new();
        cache.insert(32104, 42); // SoC raw value
        let table = &[RegisterDef {
            address: 32104,
            name: "battery_soc",
            section: "Battery",
            unit: Some("%"),
            kind: RegisterKind::Uint16 { scale: 1.0 },
        }];
        let decoded = decode_one(&table[0], &cache);
        assert_eq!(decoded.value, serde_json::json!(42.0));
        assert_eq!(decoded.unit, Some("%"));
        assert_eq!(decoded.raw, Some(42));
    }

    #[test]
    fn decodes_int16_negative() {
        let mut cache = HashMap::new();
        // -123 as int16: stored as 0xFF85
        cache.insert(30001, 0xFF85);
        let def = RegisterDef {
            address: 30001,
            name: "battery_power",
            section: "Battery",
            unit: Some("W"),
            kind: RegisterKind::Int16 { scale: 1.0 },
        };
        let decoded = decode_one(&def, &cache);
        assert_eq!(decoded.value, serde_json::json!(-123.0));
    }

    #[test]
    fn decodes_int32_two_registers_big_endian() {
        let mut cache = HashMap::new();
        // 500_000 = 0x0007A120 → hi=0x0007, lo=0xA120
        cache.insert(32102, 0x0007);
        cache.insert(32103, 0xA120);
        let def = RegisterDef {
            address: 32102,
            name: "battery_power",
            section: "Battery",
            unit: Some("W"),
            kind: RegisterKind::Int32 { scale: 1.0 },
        };
        let decoded = decode_one(&def, &cache);
        assert_eq!(decoded.value, serde_json::json!(500_000.0));
    }

    #[test]
    fn decodes_char_block_to_string() {
        let mut cache = HashMap::new();
        // "AB" + "CD" + "EF" + padding NULs
        cache.insert(31000, 0x4142); // 'A' 'B'
        cache.insert(31001, 0x4344); // 'C' 'D'
        cache.insert(31002, 0x4546); // 'E' 'F'
        cache.insert(31003, 0x0000); // pad
        let def = RegisterDef {
            address: 31000,
            name: "device_name",
            section: "Metadata",
            unit: None,
            kind: RegisterKind::Char { regs: 4 },
        };
        let decoded = decode_one(&def, &cache);
        assert_eq!(decoded.value, serde_json::json!("ABCDEF"));
    }

    #[test]
    fn decodes_enum_with_label() {
        let mut cache = HashMap::new();
        cache.insert(42010, 1); // Charge
        let def = RegisterDef {
            address: 42010,
            name: "force_mode",
            section: "Control",
            unit: None,
            kind: RegisterKind::Enum(FORCE_MODE),
        };
        let decoded = decode_one(&def, &cache);
        assert_eq!(decoded.value, serde_json::json!("Charge (1)"));
    }

    #[test]
    fn missing_register_returns_null() {
        let cache = HashMap::new();
        let def = RegisterDef {
            address: 32104,
            name: "battery_soc",
            section: "Battery",
            unit: Some("%"),
            kind: RegisterKind::Uint16 { scale: 1.0 },
        };
        let decoded = decode_one(&def, &cache);
        assert_eq!(decoded.value, serde_json::Value::Null);
        assert_eq!(decoded.raw, None);
    }

    #[test]
    fn variants_have_distinct_tables() {
        let v12 = register_table(MarstekModel::VenusEV1V2);
        let v3 = register_table(MarstekModel::VenusEV3);
        assert!(!v12.is_empty());
        assert!(!v3.is_empty());
        // V1/V2 SoC at 32104, V3 SoC at 34002.
        assert!(v12.iter().any(|d| d.address == 32104 && d.name == "battery_soc"));
        assert!(v3.iter().any(|d| d.address == 34002 && d.name == "battery_soc"));
    }
}
