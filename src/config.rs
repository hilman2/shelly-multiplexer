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
    pub location: LocationConfig,
    /// Re-publish each battery's Modbus telemetry on a virtual Modbus
    /// TCP server we host ourselves. Lets HA's existing Marstek-Modbus
    /// integrations keep working even though we now own the inverters'
    /// single Modbus slot.
    #[serde(default)]
    pub virtual_modbus: VirtualModbusConfig,
    #[serde(default)]
    pub circuits: Vec<CircuitConfig>,
    #[serde(default)]
    pub batteries: Vec<BatteryConfig>,
}

/// Geographic location — only used by the night-cutoff feature (see
/// `DispatcherConfig::night_cutoff_enabled`). Both fields must be set
/// for sunrise / sunset computation; either Some(unset) disables the
/// feature with a startup warning.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LocationConfig {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

// ---------------------------------------------------------------------------
// Virtual Modbus server (re-publish telemetry so HA integrations keep working)
// ---------------------------------------------------------------------------

/// We hold a persistent Modbus TCP connection to each Marstek (via its
/// RS485-to-LAN bridge) for setpoint writes + SoC reads. Most Marstek
/// variants — and definitely the RS485 bridges — only accept ONE Modbus
/// client at a time, so a user's existing HA Modbus integration that
/// used to read from the bridge directly now gets blocked.
///
/// This server fixes that: every `bulk_refresh_ms` we read a range of
/// the inverter's holding registers via the connection we already own
/// and cache the raw u16 values per battery. The server listens on
/// `bind_address` and serves cached values for any read in the covered
/// ranges. Writes are rejected (ILLEGAL_FUNCTION) — we own setpoint
/// control, HA shouldn't accidentally write conflicting commands.
///
/// Per-battery routing uses Modbus unit IDs: each battery's
/// `virtual_unit_id` (default = `index + 1`) becomes the unit ID HA
/// has to talk to. Unit IDs must be unique across the configured
/// batteries; the original `modbus_unit_id` (per-bridge) is unrelated.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VirtualModbusConfig {
    #[serde(default = "default_virtual_modbus_enabled")]
    pub enabled: bool,
    /// Bind address (host:port). Default 1502 — non-privileged port, no
    /// CAP_NET_BIND_SERVICE needed. Override to 502 if your HA
    /// integration insists on the standard port and the addon has
    /// capability granted.
    #[serde(default = "default_virtual_modbus_bind")]
    pub bind_address: String,
    /// How often we refresh the cached register block (ms). Bulk-read
    /// happens on the BatteryWriter's existing connection, so this is
    /// effectively the freshness ceiling for HA telemetry.
    #[serde(default = "default_virtual_modbus_refresh")]
    pub bulk_refresh_ms: u64,
}

impl Default for VirtualModbusConfig {
    fn default() -> Self {
        Self {
            enabled: default_virtual_modbus_enabled(),
            bind_address: default_virtual_modbus_bind(),
            bulk_refresh_ms: default_virtual_modbus_refresh(),
        }
    }
}

fn default_virtual_modbus_enabled() -> bool {
    true
}
fn default_virtual_modbus_bind() -> String {
    "0.0.0.0:1502".into()
}
fn default_virtual_modbus_refresh() -> u64 {
    // Marstek inverters publish slow-moving signals (SoC, voltage,
    // energy counters) so 5 s is plenty fresh for HA dashboards while
    // costing us only ~2 Modbus round-trips per battery per refresh.
    5_000
}

/// Register ranges we bulk-read on every refresh. Aims to cover every
/// holding-register address the ViperRNMC integration's variant YAMLs
/// (`e_v12.yaml`, `e_v3.yaml`, `d.yaml`, `a.yaml`) might reference —
/// inverter status, AC/DC, energy counters, SoC, BMS state, control
/// registers. 125-register chunk size is the Modbus protocol maximum
/// per request, and we issue these sequentially against the existing
/// persistent connection so the bridge sees the same pace as before.
pub const BULK_READ_RANGES: &[(u16, u16)] = &[
    (30000, 125), // 30000..30124 — status block (every variant)
    (30125, 125), // 30125..30249 — continuation
    (31000, 125), // 31000..31124 — AC / inverter telemetry on some variants
    (32000, 100), // 32000..32099 — gap-filler before 32100
    (32100, 125), // 32100..32224 — V1/V2 SoC + power + energy
    (32225, 75),  // 32225..32299 — V1/V2 tail
    (33000, 125), // 33000..33124 — energy counters / daily stats
    (34000, 125), // 34000..34124 — V3 SoC + status
    (34125, 75),  // 34125..34199 — V3 tail
    (42000, 30),  // 42000..42029 — RS485 control + force_mode + setpoints
    (43000, 10),  // 43000..43009 — user_work_mode + related
    (44000, 10),  // 44000..44009 — BMS charging/discharging cutoffs
];

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

/// How the dispatcher commands the batteries.
///
/// - `Modbus` (v0.7+ default): writes absolute power setpoints directly
///   via Modbus TCP (register 42010 for force mode + 42020/42021 for the
///   wattage). Eliminates the entire pulse/delta machinery — every
///   battery is told EXACTLY what to do, every cycle. Requires
///   `modbus_host` on each battery.
/// - `Pulse` (legacy): emulates a Shelly Pro 3EM and steers each
///   Marstek via per-poll CT deltas. Kept for installs without
///   per-battery Modbus access (no RS485 bridge), and for non-Marstek
///   inverters that integrate via the Shelly 3EM protocol.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DispatchMode {
    Modbus,
    Pulse,
}

impl Default for DispatchMode {
    fn default() -> Self {
        DispatchMode::Modbus
    }
}

/// Dispatcher tuning — kept deliberately small (12 essentials). The
/// rest of what used to be exposed is now hardcoded with sensible
/// defaults; see `MODBUS_*`, `EMERGENCY_*`, `NIGHT_*` and the pulse-mode
/// constants in `dispatcher.rs`. Stripping the surface area follows
/// the principle of the upstream Python reference impl that worked
/// well with ~4 control knobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DispatcherConfig {
    /// `modbus` (default): direct power setpoints. `pulse` (legacy):
    /// Shelly-Pro-3EM CT emulation, for installs without per-battery
    /// Modbus reach.
    #[serde(default)]
    pub mode: DispatchMode,

    /// Tick rate (ms) — how often the dispatcher recomputes targets.
    /// Marstek inverters take 1-3 s to ramp toward a setpoint, so
    /// going much faster than 1 Hz just queues commands the inverter
    /// can't act on. Default 2 s.
    #[serde(default = "default_cycle_ms")]
    pub cycle_ms: u64,

    /// Grid-imbalance noise floor (W). Imbalances smaller than this
    /// are ignored — Marstek quantisation + plug measurement noise.
    /// Default 50 W.
    #[serde(default = "default_deadband_w")]
    pub deadband_w: f64,

    /// Asymmetric "never cross zero" margin (W). The dispatcher always
    /// aims for grid_w = ±bias depending on direction — never zero.
    /// Charging: leaves this much grid EXPORT unaddressed (never pulls
    /// import). Discharging: leaves this much grid IMPORT unaddressed
    /// (never pushes into export). 100 W is comfortable against
    /// inverter ramp lag and noisy load profiles.
    #[serde(default = "default_grid_bias_w")]
    pub grid_bias_w: f64,

    /// Max change of a battery's setpoint per dispatcher cycle (W).
    /// Smooths big steps into a ramp — going from 0 W to 2 kW takes
    /// (2000 / rate_limit) cycles. Replaces the EMA grid smoother
    /// + per-write throttle + heartbeat we used to expose: one knob,
    /// applied at the algorithm level. Inspired by the Python ref
    /// impl's "max 750 W/cycle" rate limit.
    #[serde(default = "default_rate_limit_w")]
    pub rate_limit_w_per_cycle: f64,

    /// SoC at/above which charging is gated to 0 W. BMS-reported
    /// cutoff (Modbus reg 44000) takes precedence when available.
    #[serde(default = "default_soc_full")]
    pub soc_full_pct: f64,

    /// SoC at/below which discharging is gated to 0 W. BMS cutoff
    /// (reg 44001) takes precedence.
    #[serde(default = "default_soc_empty")]
    pub soc_empty_pct: f64,

    /// Fraction of the fuse cap that the dispatcher will actually
    /// use. 0.95 = 5 % jitter buffer.
    #[serde(default = "default_circuit_headroom")]
    pub circuit_headroom: f64,

    /// Plug silent this long → mute its circuit.
    #[serde(default = "default_plug_stale_s")]
    pub plug_stale_s: f64,

    /// Real Shelly silent this long → mute every circuit.
    #[serde(default = "default_grid_stale_s")]
    pub grid_stale_s: f64,

    /// Settle escape hatch (s): after a battery write, accept the
    /// cycle as done after this long even without observable plug
    /// movement. Bounds the wait if a Marstek refuses to react.
    #[serde(default = "default_settle_timeout_s")]
    pub settle_timeout_s: f64,

    /// Hardware safety: when the SIGNED plug-power sum on a circuit
    /// exceeds `cap × headroom + this`, the dispatcher physically
    /// opens the worst offender's Shelly Plug PM Gen3 relay. 0
    /// disables. Grace + recovery are hardcoded at 5 s / 600 s.
    #[serde(default = "default_emergency_cutoff_margin_w")]
    pub emergency_cutoff_margin_w: f64,

    /// Optional efficiency feature: between sunset and sunrise,
    /// disconnect empty batteries to skip the Marstek inverter's
    /// ~5-15 W standby loss. Requires `[location]` lat/lon.
    #[serde(default)]
    pub night_cutoff_enabled: bool,
}

// -------- removed knobs (still ignored on TOML load via serde default) --
// hit_tolerance_w, plug_stable_w, plug_stable_duration_s, pulse_count,
// group_silent_after_stale_s, soc_unknown_lockout_s, setpoint_deadband_w,
// modbus_heartbeat_s, modbus_watchdog_grace_s, modbus_connect_timeout_ms,
// modbus_request_timeout_ms, modbus_write_retries, grid_smoothing_s,
// modbus_min_write_interval_s, emergency_cutoff_grace_s,
// emergency_cutoff_recovery_s, night_cutoff_soc_margin_pct
//
// These are now compile-time constants. See `dispatcher.rs` and
// `modbus.rs` for the values. v0.8 simplification — was 25+ knobs
// in v0.7, now 13.


impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            mode: DispatchMode::default(),
            cycle_ms: default_cycle_ms(),
            deadband_w: default_deadband_w(),
            grid_bias_w: default_grid_bias_w(),
            rate_limit_w_per_cycle: default_rate_limit_w(),
            soc_full_pct: default_soc_full(),
            soc_empty_pct: default_soc_empty(),
            circuit_headroom: default_circuit_headroom(),
            plug_stale_s: default_plug_stale_s(),
            grid_stale_s: default_grid_stale_s(),
            settle_timeout_s: default_settle_timeout_s(),
            emergency_cutoff_margin_w: default_emergency_cutoff_margin_w(),
            night_cutoff_enabled: false,
        }
    }
}

fn default_cycle_ms() -> u64 {
    2000
}
fn default_deadband_w() -> f64 {
    50.0
}
fn default_grid_bias_w() -> f64 {
    100.0
}
fn default_rate_limit_w() -> f64 {
    // 500 W per cycle. With cycle_ms = 2 s, that's 250 W/s ramp —
    // going 0 → 2.5 kW takes 5 cycles (10 s). Smooth + matches the
    // upstream Python ref impl's "max 750 W/cycle" approach.
    500.0
}
fn default_soc_full() -> f64 {
    95.0
}
fn default_soc_empty() -> f64 {
    5.0
}
fn default_circuit_headroom() -> f64 {
    0.95
}
fn default_plug_stale_s() -> f64 {
    5.0
}
fn default_grid_stale_s() -> f64 {
    5.0
}
fn default_settle_timeout_s() -> f64 {
    10.0
}
fn default_emergency_cutoff_margin_w() -> f64 {
    200.0
}

// ---------------------------------------------------------------------------
// Hardcoded constants that used to be config knobs (v0.7 had ~25, v0.8 → 13).
// Exposed as `pub const` so dispatcher / modbus / writers can reference them
// without each module copy-pasting the value.
// ---------------------------------------------------------------------------

/// Identical CT samples per delta in pulse mode. Marstek commits after
/// 2 polls; 3 is a safety margin.
pub const PULSE_COUNT: u32 = 3;
/// Plug-movement threshold for "is the plug stable?" (pulse mode).
pub const PLUG_STABLE_W: f64 = 10.0;
/// Plug must be within `PLUG_STABLE_W` for this long before pulse-settled.
pub const PLUG_STABLE_DURATION_S: f64 = 3.0;
/// Post-stale circuit silence in pulse mode (lets the Marstek CT
/// integrator clear). Modbus mode uses 0 (force_mode bypasses CT).
pub const GROUP_SILENT_AFTER_STALE_S: f64 = 60.0;
/// Pulse-mode empirical refusal lockout duration.
pub const SOC_UNKNOWN_LOCKOUT_S: f64 = 600.0;

/// Modbus connect TCP timeout (ms). V3 talks directly to the inverter
/// and is noticeably slower than v1/v2 behind an Elfin/Waveshare bridge.
pub const MODBUS_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Per-register Modbus request timeout (ms).
pub const MODBUS_REQUEST_TIMEOUT_MS: u64 = 5_000;
/// Per-write retry budget on transient Modbus failures (200 ms × attempt).
pub const MODBUS_WRITE_RETRIES: u32 = 2;

/// Sustained-overload grace before the emergency plug relay opens.
pub const EMERGENCY_CUTOFF_GRACE_S: f64 = 5.0;
/// Auto-recovery window after an emergency cutoff. Manual reset is
/// always available via the admin API.
pub const EMERGENCY_CUTOFF_RECOVERY_S: f64 = 600.0;

/// Hysteresis above effective empty SoC for the night cutoff to fire
/// (and below + margin to recover).
pub const NIGHT_CUTOFF_SOC_MARGIN_PCT: f64 = 2.0;

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
    /// Unit ID this battery will be exposed under on our own virtual
    /// Modbus TCP server (see `[virtual_modbus]`). HA's existing
    /// Marstek-Modbus integration just re-points its host at us and
    /// uses this unit ID per battery. Must be unique across all
    /// configured batteries. `None` = derive from the battery's index
    /// (index + 1).
    #[serde(default)]
    pub virtual_unit_id: Option<u8>,
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
    // Venus E v1/v2 is the most-installed variant in the wild.
    MarstekModel::VenusEV1V2
}
fn default_modbus_port() -> u16 {
    502
}
fn default_modbus_unit_id() -> u8 {
    1
}
fn default_soc_interval_ms() -> u64 {
    // 60 s. SoC moves at most ~1 % per minute under full power on a
    // 5 kWh Marstek; faster polling adds nothing useful. In modbus
    // dispatch mode this is just an upper bound — the SoC read is
    // piggybacked onto the BatteryWriter's existing connection
    // whenever a setpoint write or heartbeat happens, so SoC actually
    // refreshes on whichever cadence is smaller (typically the
    // heartbeat at 30 s). In pulse mode it's the standalone poll
    // interval. The point: SoC reads never open their own TCP
    // connection in modbus mode, so the bus stays free for writes.
    60_000
}

/// Marstek hardware variants distinguished by their Modbus register
/// map. Sourced from the ViperRNMC marstek_venus_modbus integration's
/// per-variant YAMLs — the upstream HA add-on's "Mit Marstek Venus per
/// Modbus verbinden" dialog uses the same four-way split.
///
/// | variant       | SoC reg | SoC scale | bp reg | bp dtype | BMS cutoffs  |
/// |---------------|---------|-----------|--------|----------|--------------|
/// | Venus A       | 32104   | 1         | 30001  | int16    | not defined  |
/// | Venus D       | 32104   | 1         | 30001  | int16    | not defined  |
/// | Venus E v1/v2 | 32104   | 1         | 32102  | **int32**| 44000/44001  |
/// | Venus E v3    | 34002   | **0.1**   | 30001  | int16    | not defined  |
///
/// Control registers (RS485 control 42000, force_mode 42010, charge
/// power 42020, discharge power 42021, user work mode 43000) are
/// identical across all four variants.
///
/// Serde aliases preserve backward-compat with pre-v0.7.2 configs that
/// used `marstek_model = "venus_e"` (now Venus E v3) and
/// `marstek_model = "venus_e_v12"` (now Venus E v1/v2 — the earlier
/// naming wrongly read "v12" as "v1.2"; upstream file `e_v12.yaml`
/// combines v1 AND v2).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum MarstekModel {
    #[serde(rename = "venus_a")]
    VenusA,
    #[serde(rename = "venus_d")]
    VenusD,
    /// Venus E v1 / v2 — upstream file `e_v12.yaml`. By far the most
    /// installed Venus E in the wild → multiplexer default. Aliases
    /// `venus_e_v12` and `venus_e_v1v2` keep old configs loading.
    #[serde(
        rename = "venus_e_v1_v2",
        alias = "venus_e_v12",
        alias = "venus_e_v1v2"
    )]
    VenusEV1V2,
    /// Venus E v3 — newer firmware with native Ethernet Modbus support.
    /// Alias `venus_e` for pre-v0.7.2 configs.
    #[serde(rename = "venus_e_v3", alias = "venus_e")]
    VenusEV3,
}

impl MarstekModel {
    /// Holding-register address that holds battery SoC (uint16). Scale
    /// varies per variant — see `soc_scale`.
    pub fn soc_register(self) -> u16 {
        match self {
            MarstekModel::VenusA => 32104,
            MarstekModel::VenusD => 32104,
            MarstekModel::VenusEV1V2 => 32104,
            MarstekModel::VenusEV3 => 34002,
        }
    }

    /// Multiplier applied to the raw SoC register value. V3 reports
    /// decipercent (raw 0..=1000 → 0..=100 %); all others report whole
    /// percent.
    pub fn soc_scale(self) -> f64 {
        match self {
            MarstekModel::VenusEV3 => 0.1,
            _ => 1.0,
        }
    }

    /// Holding-register address that reports current battery output
    /// power (signed watts). See `battery_power_is_int32` for encoding.
    pub fn battery_power_register(self) -> u16 {
        match self {
            MarstekModel::VenusA => 30001,
            MarstekModel::VenusD => 30001,
            // Venus E v1/v2 packs power into TWO consecutive registers
            // as a signed 32-bit int — the only variant that does this.
            MarstekModel::VenusEV1V2 => 32102,
            MarstekModel::VenusEV3 => 30001,
        }
    }

    /// True iff battery_power_register spans 2 registers as int32 (big-
    /// endian word order). Single-register int16 otherwise.
    pub fn battery_power_is_int32(self) -> bool {
        matches!(self, MarstekModel::VenusEV1V2)
    }

    /// True iff this variant's firmware exposes the BMS-configured
    /// charging / discharging cutoff registers (44000 / 44001). The
    /// upstream YAMLs only list those for Venus E v1/v2; for the other
    /// variants we fall back to the dispatcher's TOML default.
    pub fn supports_bms_cutoffs(self) -> bool {
        matches!(self, MarstekModel::VenusEV1V2)
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
    /// Effective virtual Modbus unit ID. Either the explicit
    /// `virtual_unit_id` from the TOML, or `index + 1` as a fallback
    /// (so the first battery in the file gets unit 1, second unit 2,
    /// etc.).
    pub fn effective_virtual_unit_id(&self, index: usize) -> u8 {
        self.virtual_unit_id
            .unwrap_or_else(|| (index + 1).min(247) as u8)
    }

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
        if !(0.0..=1.0).contains(&self.dispatcher.circuit_headroom) {
            anyhow::bail!("dispatcher.circuit_headroom must be in [0, 1]");
        }
        if self.dispatcher.grid_bias_w < 0.0 {
            anyhow::bail!("dispatcher.grid_bias_w must not be negative");
        }
        if self.dispatcher.rate_limit_w_per_cycle <= 0.0 {
            anyhow::bail!("dispatcher.rate_limit_w_per_cycle must be > 0");
        }
        if self.dispatcher.plug_stale_s < 0.0 {
            anyhow::bail!("dispatcher.plug_stale_s must not be negative");
        }
        if self.dispatcher.grid_stale_s < 0.0 {
            anyhow::bail!("dispatcher.grid_stale_s must not be negative");
        }
        if self.dispatcher.settle_timeout_s < 0.0 {
            anyhow::bail!("dispatcher.settle_timeout_s must not be negative");
        }
        if self.dispatcher.emergency_cutoff_margin_w < 0.0 {
            anyhow::bail!("dispatcher.emergency_cutoff_margin_w must not be negative");
        }
        if self.dispatcher.night_cutoff_enabled {
            match (self.location.latitude, self.location.longitude) {
                (Some(lat), Some(lon)) => {
                    if !(-90.0..=90.0).contains(&lat) {
                        anyhow::bail!("location.latitude must be in [-90, 90]");
                    }
                    if !(-180.0..=180.0).contains(&lon) {
                        anyhow::bail!("location.longitude must be in [-180, 180]");
                    }
                }
                _ => anyhow::bail!(
                    "dispatcher.night_cutoff_enabled = true requires [location] latitude AND longitude"
                ),
            }
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

        // Virtual Modbus server validation
        if self.virtual_modbus.enabled {
            if self.virtual_modbus.bulk_refresh_ms == 0 {
                anyhow::bail!("virtual_modbus.bulk_refresh_ms must be > 0");
            }
            self.virtual_modbus
                .bind_address
                .parse::<std::net::SocketAddr>()
                .with_context(|| {
                    format!(
                        "virtual_modbus.bind_address `{}` is not a valid SocketAddr (expected host:port)",
                        self.virtual_modbus.bind_address
                    )
                })?;
            // Unit-ID uniqueness across batteries. The effective unit ID
            // is either the explicit `virtual_unit_id` or the index+1
            // fallback — both share one namespace.
            let mut seen_units = std::collections::HashSet::new();
            for (idx, b) in self.batteries.iter().enumerate() {
                let unit = b.effective_virtual_unit_id(idx);
                if unit == 0 {
                    anyhow::bail!(
                        "battery {}: virtual_unit_id must be in 1..=247 (got {})",
                        b.id,
                        unit
                    );
                }
                if !seen_units.insert(unit) {
                    anyhow::bail!(
                        "battery {}: virtual_unit_id {} clashes with another battery — \
                         every battery needs a unique unit ID on the virtual Modbus server",
                        b.id,
                        unit
                    );
                }
            }
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
    /// ViperRNMC integration. The four-way split matches the upstream
    /// HA add-on's connection dialog (Venus A / D / E v1/v2 / E v3).
    #[test]
    fn marstek_model_register_map() {
        // SoC register addresses
        assert_eq!(MarstekModel::VenusA.soc_register(), 32104);
        assert_eq!(MarstekModel::VenusD.soc_register(), 32104);
        assert_eq!(MarstekModel::VenusEV1V2.soc_register(), 32104);
        assert_eq!(MarstekModel::VenusEV3.soc_register(), 34002);
        // SoC scale (only v3 reports decipercent)
        assert_eq!(MarstekModel::VenusA.soc_scale(), 1.0);
        assert_eq!(MarstekModel::VenusEV3.soc_scale(), 0.1);
        // battery_power register
        assert_eq!(MarstekModel::VenusA.battery_power_register(), 30001);
        assert_eq!(MarstekModel::VenusD.battery_power_register(), 30001);
        assert_eq!(MarstekModel::VenusEV1V2.battery_power_register(), 32102);
        assert_eq!(MarstekModel::VenusEV3.battery_power_register(), 30001);
        // int32 encoding (only v1/v2)
        assert!(!MarstekModel::VenusA.battery_power_is_int32());
        assert!(!MarstekModel::VenusD.battery_power_is_int32());
        assert!(MarstekModel::VenusEV1V2.battery_power_is_int32());
        assert!(!MarstekModel::VenusEV3.battery_power_is_int32());
        // BMS cutoffs (only v1/v2 has them defined)
        assert!(!MarstekModel::VenusA.supports_bms_cutoffs());
        assert!(!MarstekModel::VenusD.supports_bms_cutoffs());
        assert!(MarstekModel::VenusEV1V2.supports_bms_cutoffs());
        assert!(!MarstekModel::VenusEV3.supports_bms_cutoffs());
    }

    /// Backward-compat aliases for pre-v0.7.2 configs.
    #[test]
    fn marstek_model_legacy_aliases() {
        use serde::de::IntoDeserializer;
        // Old "venus_e" → VenusEV3
        let m: MarstekModel = serde::Deserialize::deserialize(
            <&str as IntoDeserializer<serde::de::value::Error>>::into_deserializer("venus_e"),
        )
        .unwrap();
        assert_eq!(m, MarstekModel::VenusEV3);
        // Old "venus_e_v12" → VenusEV1V2
        let m: MarstekModel = serde::Deserialize::deserialize(
            <&str as IntoDeserializer<serde::de::value::Error>>::into_deserializer(
                "venus_e_v12",
            ),
        )
        .unwrap();
        assert_eq!(m, MarstekModel::VenusEV1V2);
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
        // Legacy "venus_e_v12" alias maps to the new VenusEV1V2 variant.
        assert_eq!(cfg.batteries[0].marstek_model, MarstekModel::VenusEV1V2);
    }
}
