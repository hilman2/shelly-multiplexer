//! TOML configuration for the pulse-based multi-battery dispatcher.
//!
//! Green-field schema: no compatibility with previous multiplex configs.
//! Every battery MUST have a Shelly Plug PM Gen3 — plug measurements are
//! the authoritative ground truth for circuit-cap enforcement.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub real_shelly: RealShellyConfig,
    pub virtual_shelly: VirtualShellyConfig,
    pub management: ManagementConfig,
    #[serde(default)]
    pub dispatcher: DispatcherConfig,
    #[serde(default)]
    pub home_assistant: HomeAssistantConfig,
    #[serde(default)]
    pub circuits: Vec<CircuitConfig>,
    #[serde(default)]
    pub batteries: Vec<BatteryConfig>,
}

// ---------------------------------------------------------------------------
// Real Shelly (grid measurement source)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RealShellyConfig {
    pub host: IpAddr,
    pub udp_port: u16,
    #[serde(default = "default_real_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
}

fn default_real_poll_interval() -> u64 {
    250
}
fn default_request_timeout() -> u64 {
    1000
}

// ---------------------------------------------------------------------------
// Virtual Shelly (the face we present to the Marsteks)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VirtualShellyConfig {
    #[serde(default = "default_bind_interface")]
    pub bind_interface: String,
    #[serde(default = "default_virtual_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_virtual_http_port")]
    pub http_port: u16,
    #[serde(default)]
    pub device_mac: String,
    #[serde(default)]
    pub device_hostname: String,
    #[serde(default = "default_firmware")]
    pub firmware: String,
    #[serde(default = "default_enable_mdns")]
    pub enable_mdns: bool,
}

fn default_bind_interface() -> String {
    "0.0.0.0".into()
}
fn default_virtual_udp_port() -> u16 {
    1010
}
fn default_virtual_http_port() -> u16 {
    80
}
fn default_firmware() -> String {
    "1.4.4".into()
}
fn default_enable_mdns() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Management UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManagementConfig {
    #[serde(default = "default_management_bind")]
    pub bind_address: String,
}

fn default_management_bind() -> String {
    "0.0.0.0:8080".into()
}

// ---------------------------------------------------------------------------
// Dispatcher (pulse-based)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DispatcherConfig {
    /// Recompute interval for desired_w + pulse generation.
    #[serde(default = "default_cycle_ms")]
    pub cycle_ms: u64,
    /// Δ below this is ignored — Marstek-quantisation noise. Also the
    /// minimum pulse magnitude the dispatcher will issue.
    #[serde(default = "default_deadband_w")]
    pub deadband_w: f64,
    /// DEPRECATED since v0.4.4 — kept for config-file compatibility only.
    /// The "pulse landed" criterion is now (a) any plug movement above
    /// `plug_stable_w` proving Marstek reacted, then (b) plug stable for
    /// `plug_stable_duration_s`. Old configs still load with this field set.
    #[serde(default = "default_hit_tolerance_w")]
    pub hit_tolerance_w: f64,
    /// Plug-reading delta (W) below which two consecutive readings are
    /// considered "the same" — i.e. the plug is not moving. The pulse-
    /// settled check waits until no >stable_w deltas have arrived for
    /// `plug_stable_duration_s` so the next pulse only fires once the
    /// previous delta has FULLY landed (not just started landing).
    #[serde(default = "default_plug_stable_w")]
    pub plug_stable_w: f64,
    /// How long the plug must stay within `plug_stable_w` (i.e. no
    /// movement) before the dispatcher considers the previous pulse done
    /// and queues the next one. Roughly: Marstek's typical reaction time
    /// (~1-2 s) plus a debounce margin.
    #[serde(default = "default_plug_stable_duration_s")]
    pub plug_stable_duration_s: f64,
    /// Pulses sent per delta change. Marstek needs ≥2; 3 = safety margin.
    #[serde(default = "default_pulse_count")]
    pub pulse_count: u32,
    /// SoC at/above which charging is skipped for the battery.
    #[serde(default = "default_soc_full")]
    pub soc_full_pct: f64,
    /// SoC at/below which discharging is skipped for the battery.
    #[serde(default = "default_soc_empty")]
    pub soc_empty_pct: f64,
    /// Plug silent for this long → group goes safe.
    #[serde(default = "default_plug_stale_s")]
    pub plug_stale_s: f64,
    /// Real-Shelly snapshot silent for this long → ALL circuits muted
    /// (we can't trust grid_w any more). Symmetric to plug_stale_s for
    /// the upstream measurement.
    #[serde(default = "default_grid_stale_s")]
    pub grid_stale_s: f64,
    /// After a stale plug recovers, mute the group's CT signal for this
    /// long (Marstek watchdog clears integrator) before resuming.
    #[serde(default = "default_group_silent_s")]
    pub group_silent_after_stale_s: f64,
    /// Use only this fraction of the circuit cap (95 %) — jitter buffer.
    #[serde(default = "default_circuit_headroom")]
    pub circuit_headroom: f64,
    /// Asymmetric grid target bias. The dispatcher never tries to bring
    /// grid_w to 0 — it leaves a margin of `grid_bias_w` on the import
    /// side when discharging (so an unmodelled load doesn't push us into
    /// export) and on the export side when charging (so we don't
    /// accidentally pay for a few watts of grid import while charging).
    /// Set to 0 to dispatch to exact 0.
    #[serde(default = "default_grid_bias_w")]
    pub grid_bias_w: f64,
    /// Time-based pulse-settle fallback: even if the plug reading hasn't
    /// moved by `hit_tolerance_w` yet, after this many seconds the
    /// dispatcher accepts the cycle as done and is free to queue the
    /// next corrective pulse. Marstek typically reacts in 1-2 s; 5 s is
    /// a safe upper bound that prevents lockups when a Marstek refuses.
    #[serde(default = "default_settle_timeout_s")]
    pub settle_timeout_s: f64,
    /// Empirical full/empty detection lockout duration. When a battery
    /// refuses a directional pulse — significant delta queued, full
    /// `settle_timeout_s` elapsed, plug never moved — the dispatcher
    /// locks that DIRECTION for this many seconds and redistributes the
    /// load to the other batteries. After the lockout expires the
    /// direction is retried; a successful pulse clears the lockout
    /// early. The OPPOSITE direction is never affected: a battery that
    /// refuses charge (= full) keeps participating in discharge, and
    /// vice versa.
    ///
    /// Primarily useful for installations without a SoC source (no
    /// Modbus bridge, no HA sensor) — derived "full"/"empty" replaces
    /// the SoC gate. With SoC available it acts as a backstop in case
    /// the SoC reading is wrong or stale.
    #[serde(default = "default_soc_unknown_lockout_s")]
    pub soc_unknown_lockout_s: f64,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            cycle_ms: default_cycle_ms(),
            deadband_w: default_deadband_w(),
            hit_tolerance_w: default_hit_tolerance_w(),
            plug_stable_w: default_plug_stable_w(),
            plug_stable_duration_s: default_plug_stable_duration_s(),
            pulse_count: default_pulse_count(),
            soc_full_pct: default_soc_full(),
            soc_empty_pct: default_soc_empty(),
            plug_stale_s: default_plug_stale_s(),
            grid_stale_s: default_grid_stale_s(),
            group_silent_after_stale_s: default_group_silent_s(),
            circuit_headroom: default_circuit_headroom(),
            grid_bias_w: default_grid_bias_w(),
            settle_timeout_s: default_settle_timeout_s(),
            soc_unknown_lockout_s: default_soc_unknown_lockout_s(),
        }
    }
}

fn default_cycle_ms() -> u64 {
    200
}
fn default_deadband_w() -> f64 {
    30.0
}
fn default_hit_tolerance_w() -> f64 {
    15.0
}
fn default_plug_stable_w() -> f64 {
    10.0
}
fn default_plug_stable_duration_s() -> f64 {
    1.5
}
fn default_pulse_count() -> u32 {
    3
}
fn default_soc_full() -> f64 {
    95.0
}
fn default_soc_empty() -> f64 {
    5.0
}
fn default_plug_stale_s() -> f64 {
    2.0
}
fn default_grid_stale_s() -> f64 {
    5.0
}
fn default_group_silent_s() -> f64 {
    60.0
}
fn default_circuit_headroom() -> f64 {
    0.95
}
fn default_grid_bias_w() -> f64 {
    30.0
}
fn default_settle_timeout_s() -> f64 {
    5.0
}
fn default_soc_unknown_lockout_s() -> f64 {
    600.0
}

// ---------------------------------------------------------------------------
// Optional Home Assistant SoC source (no plug equivalent for SoC yet)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HomeAssistantConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ha_url")]
    pub url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default = "default_ha_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_ha_url(),
            token: String::new(),
            timeout_ms: default_ha_timeout_ms(),
        }
    }
}

fn default_ha_url() -> String {
    "http://homeassistant.local:8123/api".into()
}

fn default_ha_timeout_ms() -> u64 {
    3000
}

// ---------------------------------------------------------------------------
// Circuit (shared protective device) — now allows multiple active batteries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CircuitConfig {
    pub id: String,
    pub fuse_amps: f64,
    #[serde(default = "default_phases")]
    pub phases: u8,
    #[serde(default = "default_voltage")]
    pub voltage: f64,
}

fn default_phases() -> u8 {
    1
}
fn default_voltage() -> f64 {
    230.0
}

impl CircuitConfig {
    pub fn cap_w(&self) -> f64 {
        self.fuse_amps * self.voltage * f64::from(self.phases)
    }
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatteryConfig {
    pub id: String,
    /// Static IP the Marstek polls us from. Used to route per-Marstek
    /// pulse queues without parsing the Shelly src field.
    pub address: IpAddr,
    /// Circuit (= shared MCB) the battery and its plug sit on.
    pub circuit: String,
    /// HTTP base URL of the dedicated Shelly Plug PM Gen3.
    /// Mandatory: plug measurements are authoritative for circuit cap.
    pub plug_url: String,
    pub max_charge_w: f64,
    pub max_discharge_w: f64,
    /// Capacity-weighted distribution input. If unset, falls back to
    /// max_charge_w + max_discharge_w as the proxy weight.
    #[serde(default)]
    pub capacity_wh: f64,
    /// Manual weight multiplier on top of capacity (default 1.0).
    #[serde(default = "default_priority_weight")]
    pub priority_weight: f64,
    /// Marstek model — drives Modbus register-map selection. The Venus E
    /// variants differ in their SoC register address; see
    /// https://github.com/ViperRNMC/marstek_venus_modbus for the full map.
    #[serde(default = "default_marstek_model")]
    pub marstek_model: MarstekModel,
    /// Modbus TCP host. Needed in Modbus mode
    /// (`home_assistant.enabled = false`); ignored in HA mode.
    ///
    /// For nearly every Marstek variant (A / D / E V1 / V2 / V1.2 /
    /// E 2.0) this is the LAN IP of an external RS485-to-LAN bridge
    /// (Waveshare, Elfin EW11, PUSR DR134, M5Stack Atom S3 + RS485,
    /// …) wired to the battery's RS485 port — they do NOT expose
    /// Modbus on their own WiFi. Only Venus E V3 with an Ethernet
    /// cable and recent firmware speaks Modbus TCP natively; in that
    /// case set `modbus_host` to the same value as `address`.
    ///
    /// Not auto-derived from `address` on purpose: typing the IP twice
    /// for V3 setups is cheap insurance against the much-more-common
    /// failure where someone forgets to configure the bridge.
    ///
    /// Until this is set, the battery is INACTIVE — the dispatcher
    /// skips it and the modbus poller doesn't try to read its SoC.
    /// Old v0.4.x configs (with `vendor` / `marstek_port` from the
    /// retired Local API path) load unchanged; their batteries just
    /// stay idle until the user wires up `modbus_host` here.
    #[serde(default)]
    pub modbus_host: Option<IpAddr>,
    /// Modbus TCP port (default 502). Some RS485-to-LAN bridges expose
    /// Modbus on a non-standard port.
    #[serde(default = "default_modbus_port")]
    pub modbus_port: u16,
    /// Modbus unit / slave ID (default 1).
    #[serde(default = "default_modbus_unit_id")]
    pub modbus_unit_id: u8,
    /// SoC poll interval.
    #[serde(default = "default_soc_interval_ms")]
    pub soc_interval_ms: u64,
    /// Optional HA entity for SoC. Required when `home_assistant.enabled`
    /// is true — Modbus is not used in HA mode.
    #[serde(default)]
    pub soc_entity_id: Option<String>,
    /// Per-battery override for the dispatcher-level full/empty thresholds.
    /// If unset, the dispatcher's `soc_full_pct` / `soc_empty_pct` apply.
    /// Useful for mixing batteries with different DoD specs or different
    /// reserve preferences (e.g. one Marstek aggressive at 5/95, an older
    /// LiFePO4 conservative at 15/90).
    #[serde(default)]
    pub soc_full_pct: Option<f64>,
    #[serde(default)]
    pub soc_empty_pct: Option<f64>,
    /// SoC-based power tapering. Real batteries can't accept full
    /// `max_charge_w` near 100 % SoC nor sustain full `max_discharge_w`
    /// near the empty cutoff — the BMS tapers. Modelling this in the
    /// dispatcher (rather than letting Marstek silently undershoot a
    /// commanded value) keeps `headroom()` honest and prevents the
    /// integrator-overcommit loop near the SoC edges.
    ///
    /// Semantics — both pairs are independent step functions:
    ///   • SoC ≥ `charge_taper_soc_pct` → effective max charge is
    ///     `charge_taper_w` instead of `max_charge_w`.
    ///   • SoC ≤ `discharge_taper_soc_pct` → effective max discharge is
    ///     `discharge_taper_w` instead of `max_discharge_w`.
    ///   • At/past the hard `soc_full_pct` / `soc_empty_pct` the
    ///     direction is fully gated to 0 W (unchanged from before).
    ///
    /// All four fields are optional. Setting one direction's pair
    /// enables tapering for that direction; leaving both `_w` fields
    /// at None falls back to the unmodified hardware caps.
    #[serde(default)]
    pub charge_taper_soc_pct: Option<f64>,
    #[serde(default)]
    pub charge_taper_w: Option<f64>,
    #[serde(default)]
    pub discharge_taper_soc_pct: Option<f64>,
    #[serde(default)]
    pub discharge_taper_w: Option<f64>,
}

fn default_priority_weight() -> f64 {
    1.0
}
fn default_marstek_model() -> MarstekModel {
    MarstekModel::VenusE
}
fn default_modbus_port() -> u16 {
    502
}
fn default_modbus_unit_id() -> u8 {
    1
}
fn default_soc_interval_ms() -> u64 {
    30000
}

/// Marstek hardware variants distinguished by their Modbus register map.
/// Mapping per the ViperRNMC marstek_venus_modbus integration:
///   - Venus E v1 / v2 / E v3  → SoC at holding register 34002
///   - Venus E v1.2            → SoC at holding register 32104
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarstekModel {
    /// Venus E (v1, v2, v3) — SoC register 34002.
    VenusE,
    /// Venus E v1.2 — SoC register 32104.
    VenusEV12,
}

impl MarstekModel {
    /// Holding-register address that holds battery SoC (uint16, percent).
    pub fn soc_register(self) -> u16 {
        match self {
            MarstekModel::VenusE => 34002,
            MarstekModel::VenusEV12 => 32104,
        }
    }
}

impl BatteryConfig {
    /// Effective Modbus endpoint. `modbus_host` is checked by callers via
    /// `has_soc_source()` before any poll fires, so the `address`
    /// fallback here is just a safety net — it never reaches the wire
    /// in practice because batteries without a configured SoC source
    /// are skipped entirely.
    pub fn modbus_target(&self) -> std::net::SocketAddr {
        let host = self.modbus_host.unwrap_or(self.address);
        std::net::SocketAddr::new(host, self.modbus_port)
    }

    /// Does this battery have a SoC source configured for the current
    /// dispatcher mode? Batteries that return `false` here are INACTIVE:
    /// the dispatcher excludes them from distribution, the modbus poller
    /// skips them, and the UI flags them as "no SoC source".
    ///
    /// This is the soft-migration path for configs that predate v0.5.0
    /// (where SoC came from the now-removed Local API): the file still
    /// loads, but until the user adds `soc_entity_id` or `modbus_host`
    /// the affected battery just sits idle.
    pub fn has_soc_source(&self, ha_enabled: bool) -> bool {
        if ha_enabled {
            self.soc_entity_id
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        } else {
            self.modbus_host.is_some()
        }
    }
}

// ---------------------------------------------------------------------------
// Loading + validation
// ---------------------------------------------------------------------------

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen_circuits = std::collections::HashSet::new();
        for c in &self.circuits {
            if !seen_circuits.insert(c.id.clone()) {
                anyhow::bail!("duplicate circuit id: {}", c.id);
            }
            if ![1u8, 3u8].contains(&c.phases) {
                anyhow::bail!("circuit {}: phases must be 1 or 3", c.id);
            }
            if c.fuse_amps <= 0.0 {
                anyhow::bail!("circuit {}: fuse_amps must be > 0", c.id);
            }
        }

        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_addrs = std::collections::HashSet::new();
        for b in &self.batteries {
            if !seen_ids.insert(b.id.clone()) {
                anyhow::bail!("duplicate battery id: {}", b.id);
            }
            if !seen_addrs.insert(b.address) {
                anyhow::bail!("duplicate battery address: {}", b.address);
            }
            if b.circuit.trim().is_empty() {
                anyhow::bail!("battery {}: `circuit` is required", b.id);
            }
            let Some(circuit) = self.circuits.iter().find(|c| c.id == b.circuit) else {
                anyhow::bail!(
                    "battery {} references unknown circuit '{}'",
                    b.id,
                    b.circuit
                );
            };
            let cap = circuit.cap_w();
            let largest = b.max_charge_w.max(b.max_discharge_w);
            if largest > cap {
                anyhow::bail!(
                    "battery {} (max {} W) exceeds circuit '{}' cap ({} W) on its own",
                    b.id,
                    largest,
                    b.circuit,
                    cap
                );
            }
            if b.max_charge_w < 0.0 || b.max_discharge_w < 0.0 {
                anyhow::bail!("battery {}: power limits must not be negative", b.id);
            }
            if b.plug_url.trim().is_empty() {
                anyhow::bail!(
                    "battery {}: plug_url is required (Shelly Plug PM Gen3 mandatory)",
                    b.id
                );
            }
            if b.priority_weight <= 0.0 {
                anyhow::bail!("battery {}: priority_weight must be > 0", b.id);
            }
            if let Some(s) = b.soc_full_pct {
                if !(0.0..=100.0).contains(&s) {
                    anyhow::bail!("battery {}: soc_full_pct must be in [0, 100]", b.id);
                }
            }
            if let Some(s) = b.soc_empty_pct {
                if !(0.0..=100.0).contains(&s) {
                    anyhow::bail!("battery {}: soc_empty_pct must be in [0, 100]", b.id);
                }
            }
            if let (Some(f), Some(e)) = (b.soc_full_pct, b.soc_empty_pct) {
                if e >= f {
                    anyhow::bail!(
                        "battery {}: soc_empty_pct ({}) must be < soc_full_pct ({})",
                        b.id,
                        e,
                        f
                    );
                }
            }
            // Taper validation: each direction's pair must be set together,
            // taper_w must be > 0 and ≤ the hardware cap, and taper_soc must
            // sit strictly between the empty and full cutoffs (otherwise the
            // taper either shadows the hard gate or never triggers).
            let effective_full = b
                .soc_full_pct
                .unwrap_or(self.dispatcher.soc_full_pct);
            let effective_empty = b
                .soc_empty_pct
                .unwrap_or(self.dispatcher.soc_empty_pct);
            match (b.charge_taper_soc_pct, b.charge_taper_w) {
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => anyhow::bail!(
                    "battery {}: charge_taper_soc_pct and charge_taper_w must both be set or both unset",
                    b.id
                ),
                (Some(soc), Some(w)) => {
                    if !(0.0..=100.0).contains(&soc) {
                        anyhow::bail!("battery {}: charge_taper_soc_pct must be in [0, 100]", b.id);
                    }
                    if w <= 0.0 || w >= b.max_charge_w {
                        anyhow::bail!(
                            "battery {}: charge_taper_w ({}) must be in (0, max_charge_w={})",
                            b.id, w, b.max_charge_w
                        );
                    }
                    if soc >= effective_full {
                        anyhow::bail!(
                            "battery {}: charge_taper_soc_pct ({}) must be < soc_full_pct ({})",
                            b.id, soc, effective_full
                        );
                    }
                    if soc <= effective_empty {
                        anyhow::bail!(
                            "battery {}: charge_taper_soc_pct ({}) must be > soc_empty_pct ({})",
                            b.id, soc, effective_empty
                        );
                    }
                }
            }
            match (b.discharge_taper_soc_pct, b.discharge_taper_w) {
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => anyhow::bail!(
                    "battery {}: discharge_taper_soc_pct and discharge_taper_w must both be set or both unset",
                    b.id
                ),
                (Some(soc), Some(w)) => {
                    if !(0.0..=100.0).contains(&soc) {
                        anyhow::bail!("battery {}: discharge_taper_soc_pct must be in [0, 100]", b.id);
                    }
                    if w <= 0.0 || w >= b.max_discharge_w {
                        anyhow::bail!(
                            "battery {}: discharge_taper_w ({}) must be in (0, max_discharge_w={})",
                            b.id, w, b.max_discharge_w
                        );
                    }
                    if soc <= effective_empty {
                        anyhow::bail!(
                            "battery {}: discharge_taper_soc_pct ({}) must be > soc_empty_pct ({})",
                            b.id, soc, effective_empty
                        );
                    }
                    if soc >= effective_full {
                        anyhow::bail!(
                            "battery {}: discharge_taper_soc_pct ({}) must be < soc_full_pct ({})",
                            b.id, soc, effective_full
                        );
                    }
                }
            }
            if b.modbus_port == 0 {
                anyhow::bail!("battery {}: modbus_port must be > 0", b.id);
            }
            if b.modbus_unit_id == 0 {
                anyhow::bail!("battery {}: modbus_unit_id must be > 0", b.id);
            }
            if b.soc_interval_ms == 0 {
                anyhow::bail!("battery {}: soc_interval_ms must be > 0", b.id);
            }
            // SoC source is a single global toggle (HA enabled → HA, else
            // Modbus). Missing the relevant field for the active mode is
            // NOT a hard error — older v0.4.x configs (with `vendor` /
            // `marstek_port` from the removed Local API path) must keep
            // loading on upgrade. Instead we treat such batteries as
            // INACTIVE: the dispatcher skips them entirely until the user
            // wires up either an `soc_entity_id` (HA mode) or a
            // `modbus_host` (Modbus mode). See `BatteryConfig::has_soc_source`
            // and the dispatcher's eligibility filter.
        }

        if self.dispatcher.cycle_ms == 0 {
            anyhow::bail!("dispatcher.cycle_ms must be > 0");
        }
        if self.dispatcher.deadband_w < 0.0 {
            anyhow::bail!("dispatcher.deadband_w must not be negative");
        }
        if self.dispatcher.pulse_count < 2 {
            anyhow::bail!(
                "dispatcher.pulse_count must be ≥ 2 (Marstek requires at least 2 polls to commit)"
            );
        }
        if !(0.0..=1.0).contains(&self.dispatcher.circuit_headroom) {
            anyhow::bail!("dispatcher.circuit_headroom must be in [0, 1]");
        }
        if self.dispatcher.grid_bias_w < 0.0 {
            anyhow::bail!("dispatcher.grid_bias_w must not be negative");
        }
        // Validation philosophy: only reject configurations that are
        // mathematically meaningless (negative durations, ranges with
        // empty interiors). "Suboptimal but functional" is left alone —
        // existing user configs from older versions must keep loading.
        if self.dispatcher.hit_tolerance_w < 0.0 {
            anyhow::bail!("dispatcher.hit_tolerance_w must not be negative");
        }
        if self.dispatcher.plug_stable_w < 0.0 {
            anyhow::bail!("dispatcher.plug_stable_w must not be negative");
        }
        if self.dispatcher.plug_stable_duration_s < 0.0 {
            anyhow::bail!("dispatcher.plug_stable_duration_s must not be negative");
        }
        if self.dispatcher.plug_stale_s < 0.0 {
            anyhow::bail!("dispatcher.plug_stale_s must not be negative");
        }
        if self.dispatcher.grid_stale_s < 0.0 {
            anyhow::bail!("dispatcher.grid_stale_s must not be negative");
        }
        if self.dispatcher.group_silent_after_stale_s < 0.0 {
            anyhow::bail!("dispatcher.group_silent_after_stale_s must not be negative");
        }
        if self.dispatcher.settle_timeout_s < 0.0 {
            anyhow::bail!("dispatcher.settle_timeout_s must not be negative");
        }
        if self.dispatcher.soc_unknown_lockout_s < 0.0 {
            anyhow::bail!("dispatcher.soc_unknown_lockout_s must not be negative");
        }
        if !(0.0..=100.0).contains(&self.dispatcher.soc_full_pct) {
            anyhow::bail!("dispatcher.soc_full_pct must be in [0, 100]");
        }
        if !(0.0..=100.0).contains(&self.dispatcher.soc_empty_pct) {
            anyhow::bail!("dispatcher.soc_empty_pct must be in [0, 100]");
        }
        if self.dispatcher.soc_empty_pct >= self.dispatcher.soc_full_pct {
            anyhow::bail!(
                "dispatcher.soc_empty_pct ({}) must be < soc_full_pct ({})",
                self.dispatcher.soc_empty_pct,
                self.dispatcher.soc_full_pct
            );
        }
        if self.real_shelly.poll_interval_ms == 0 {
            anyhow::bail!("real_shelly.poll_interval_ms must be > 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load a TOML string and run the full validate(). This is the exact
    /// pipeline used at startup, so anything that passes here will load
    /// inside the add-on too.
    fn load_str(s: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(s).context("parse")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Regression test for v0.4.0 → v0.4.1: a user had `hit_tolerance_w = 50`
    /// in their v0.3-era config (where the field was unused). v0.4.0 added
    /// a `hit_tolerance_w ≤ deadband_w` constraint that made the add-on
    /// fail to start on update. This must keep loading.
    #[test]
    fn loads_v03_config_with_hit_tolerance_above_deadband() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[dispatcher]
deadband_w = 30
hit_tolerance_w = 50

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
"#,
        );
        assert!(cfg.is_ok(), "expected valid config; got {:?}", cfg.err());
    }

    /// A config that uses every dispatcher field at its v0.3 default
    /// values must still load on v0.4+. This catches regressions where
    /// new validation accidentally rejects historical defaults.
    #[test]
    fn loads_v03_default_dispatcher_values() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]
http_port = 0

[management]

[dispatcher]
cycle_ms = 200
deadband_w = 30
hit_tolerance_w = 15
pulse_count = 3
soc_full_pct = 95
soc_empty_pct = 5
plug_stale_s = 2.0
group_silent_after_stale_s = 60.0
circuit_headroom = 0.95

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
"#,
        );
        assert!(cfg.is_ok(), "expected valid config; got {:?}", cfg.err());
    }

    /// Mathematically meaningless values still rejected. Negative durations
    /// would underflow or panic later in the pipeline.
    #[test]
    fn rejects_negative_durations() {
        let bad = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[dispatcher]
plug_stale_s = -1.0

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
"#;
        assert!(load_str(bad).is_err());
    }

    /// soc_empty must remain strictly less than soc_full — empty interval
    /// would make the SoC gates contradict each other.
    #[test]
    fn rejects_inverted_soc_bounds() {
        let bad = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[dispatcher]
soc_full_pct = 5
soc_empty_pct = 95

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
"#;
        assert!(load_str(bad).is_err());
    }

    /// In HA mode, a battery without `soc_entity_id` LOADS but is
    /// marked inactive (`has_soc_source = false`). The dispatcher then
    /// skips it until the user fills the entity ID in. Soft migration:
    /// old configs without an entity ID don't fail the add-on on
    /// upgrade.
    #[test]
    fn ha_mode_without_entity_id_loads_as_inactive() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[home_assistant]
enabled = true
token = "x"

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
"#,
        )
        .unwrap();
        assert!(!cfg.batteries[0].has_soc_source(true));
    }

    #[test]
    fn ha_mode_accepts_battery_with_entity_id() {
        let ok = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[home_assistant]
enabled = true
token = "x"

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
soc_entity_id = "sensor.battery_a"
"#;
        assert!(load_str(ok).is_ok());
    }

    /// MarstekModel enum is wired up to the SoC register map per the
    /// ViperRNMC integration. Regression test for the v0.5.0 Modbus rewrite.
    #[test]
    fn marstek_model_register_map() {
        assert_eq!(MarstekModel::VenusE.soc_register(), 34002);
        assert_eq!(MarstekModel::VenusEV12.soc_register(), 32104);
    }

    /// In Modbus mode a battery WITHOUT modbus_host loads but is marked
    /// inactive. This is the v0.5.0 soft-migration path: old configs
    /// that still have the retired `vendor` / `marstek_port` Local-API
    /// fields keep loading; their batteries just sit idle until the
    /// user wires up `modbus_host`.
    #[test]
    fn modbus_mode_without_modbus_host_loads_as_inactive() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
"#,
        )
        .unwrap();
        assert!(!cfg.batteries[0].has_soc_source(false));
    }

    /// Regression test for the v0.5.0 soft-migration: an old config that
    /// still carries the retired Local-API fields (`vendor`, `marstek_port`)
    /// MUST keep loading. Serde ignores unknown fields, and the battery
    /// becomes inactive (no modbus_host) until the user adds one via UI.
    #[test]
    fn loads_v04_config_with_retired_local_api_fields() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
vendor = "marstek"
marstek_port = 30000
soc_interval_ms = 30000
"#,
        )
        .unwrap();
        // Loaded successfully despite the unknown fields. The battery is
        // INACTIVE because no SoC source is configured for v0.5.0.
        assert_eq!(cfg.batteries.len(), 1);
        assert!(!cfg.batteries[0].has_soc_source(false));
    }

    /// HA-mode configs don't need modbus_host — the modbus poller is
    /// idle there, so the field is irrelevant.
    #[test]
    fn ha_mode_does_not_require_modbus_host() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[home_assistant]
enabled = true
token = "x"

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
soc_entity_id = "sensor.battery_a"
"#,
        )
        .unwrap();
        assert_eq!(cfg.batteries[0].modbus_host, None);
    }

    #[test]
    fn modbus_host_overrides_address() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
modbus_port = 5020
"#,
        )
        .unwrap();
        let b = &cfg.batteries[0];
        assert_eq!(b.modbus_target().ip().to_string(), "192.168.1.91");
        assert_eq!(b.modbus_target().port(), 5020);
    }

    #[test]
    fn marstek_model_deserializes_snake_case() {
        let cfg = load_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 16

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
marstek_model = "venus_e_v12"
modbus_host = "192.168.1.91"
"#,
        )
        .unwrap();
        assert_eq!(cfg.batteries[0].marstek_model, MarstekModel::VenusEV12);
    }
}
