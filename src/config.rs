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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DispatcherConfig {
    /// Which dispatch backend to use. Default `modbus` since v0.7 — the
    /// pulse path is kept for legacy installs without Modbus access.
    #[serde(default)]
    pub mode: DispatchMode,
    /// Recompute interval for desired_w + (pulse generation OR modbus
    /// setpoint write). In modbus mode we re-evaluate every cycle but
    /// only WRITE when the setpoint changes by more than
    /// `setpoint_deadband_w` OR `modbus_heartbeat_s` has elapsed since
    /// the last write — keeps Modbus traffic low while still serving
    /// as an "I'm still alive" heartbeat for crash detection.
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

    // ----- Modbus-dispatch specific tuning (mode = "modbus" only) -----

    /// Don't write a new setpoint to Modbus unless the desired value
    /// has shifted by at least this many watts from the last
    /// successfully written value. Below the deadband we skip the
    /// write (saves Modbus traffic over flaky RS485-to-LAN bridges).
    /// Default 20 W matches typical Marstek quantisation noise.
    #[serde(default = "default_setpoint_deadband_w")]
    pub setpoint_deadband_w: f64,

    /// Re-write the current setpoint to Modbus at least this often,
    /// even if it hasn't changed. Two purposes: (a) recover from any
    /// bridge that dropped a write, (b) serve as an "I'm still alive"
    /// heartbeat — if our process dies, the Marstek doesn't auto-
    /// revert (no firmware watchdog) and would otherwise stay on the
    /// last setpoint forever. See also `modbus_watchdog_grace_s`.
    #[serde(default = "default_modbus_heartbeat_s")]
    pub modbus_heartbeat_s: f64,

    /// Safety watchdog inside this process: if the main dispatcher
    /// loop hasn't ticked for this many seconds, a background task
    /// force-writes `force_mode = 0` to every battery and exits the
    /// process. Catches hangs that the SIGTERM handler can't see
    /// (e.g. tokio runtime deadlock). 0.0 disables the watchdog.
    #[serde(default = "default_modbus_watchdog_grace_s")]
    pub modbus_watchdog_grace_s: f64,

    // ----- Emergency plug cutoff (hard safety relay) -----

    /// When the SIGNED plug-power sum on a circuit exceeds the
    /// effective cap (= fuse_amps × voltage × phases × circuit_headroom)
    /// by MORE than this many watts, the dispatcher considers the
    /// circuit to be in physical danger. The condition has to persist
    /// for `emergency_cutoff_grace_s` seconds before the worst
    /// offending plug is cut. 0 W disables the entire feature.
    /// Default 200 W — accounts for measurement jitter while still
    /// catching real overloads quickly.
    #[serde(default = "default_emergency_cutoff_margin_w")]
    pub emergency_cutoff_margin_w: f64,

    /// Sustained-overload time (in seconds) before the emergency
    /// cutoff fires. Default 5 s: long enough that brief startup
    /// transients (large appliances kicking in) don't trip the relay,
    /// short enough that genuine cable / fuse stress is bounded.
    #[serde(default = "default_emergency_cutoff_grace_s")]
    pub emergency_cutoff_grace_s: f64,

    /// How long the plug stays off after an emergency cutoff before
    /// the dispatcher attempts to re-enable it. Default 600 s.
    /// Re-enable is automatic after the recovery window; a manual
    /// reset via the admin API is also available.
    #[serde(default = "default_emergency_cutoff_recovery_s")]
    pub emergency_cutoff_recovery_s: f64,

    // ----- Smoothing & pacing -----

    /// Exponential-moving-average time constant (seconds) applied to the
    /// raw grid_w reading before the dispatcher uses it. Filters out
    /// sub-second PV-inverter PWM ripple (often ±2 kW @ 4 Hz) so the
    /// dispatcher tracks meaningful load changes rather than noise.
    /// Default 5.0 s. Set to 0 to disable smoothing.
    #[serde(default = "default_grid_smoothing_s")]
    pub grid_smoothing_s: f64,

    /// Per-battery minimum interval (seconds) between consecutive Modbus
    /// setpoint writes. Hard throttle on top of `setpoint_deadband_w` —
    /// even if the dispatcher thinks the setpoint changed enough to
    /// warrant a write, the writer task will refuse to fire faster than
    /// this. Gives each Marstek time to actually ramp toward the new
    /// setpoint before we change it again. Default 10 s.
    #[serde(default = "default_modbus_min_write_interval_s")]
    pub modbus_min_write_interval_s: f64,

    // ----- Night cutoff (efficiency) -----

    /// Disconnect a battery's plug between sunset and sunrise if its
    /// SoC is at the empty cutoff. The Marstek's inverter standby
    /// loss (~5-15 W per unit) over a winter night is non-trivial.
    /// Requires `[location].latitude` and `longitude` to be set —
    /// without them the feature stays inactive with a startup warning.
    /// Default `false` (opt-in).
    #[serde(default)]
    pub night_cutoff_enabled: bool,

    /// SoC margin above `effective_soc_empty_pct` at which a battery
    /// still counts as "empty" for night-cutoff purposes. Acts as
    /// hysteresis: a battery has to hover within this margin of empty
    /// before the cutoff fires, and rise more than this margin above
    /// empty before recovery re-enables the plug. Default 2 %.
    #[serde(default = "default_night_cutoff_soc_margin_pct")]
    pub night_cutoff_soc_margin_pct: f64,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            mode: DispatchMode::default(),
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
            setpoint_deadband_w: default_setpoint_deadband_w(),
            modbus_heartbeat_s: default_modbus_heartbeat_s(),
            modbus_watchdog_grace_s: default_modbus_watchdog_grace_s(),
            emergency_cutoff_margin_w: default_emergency_cutoff_margin_w(),
            emergency_cutoff_grace_s: default_emergency_cutoff_grace_s(),
            emergency_cutoff_recovery_s: default_emergency_cutoff_recovery_s(),
            night_cutoff_enabled: false,
            night_cutoff_soc_margin_pct: default_night_cutoff_soc_margin_pct(),
            grid_smoothing_s: default_grid_smoothing_s(),
            modbus_min_write_interval_s: default_modbus_min_write_interval_s(),
        }
    }
}

fn default_cycle_ms() -> u64 {
    // 1 s. Slower than the 200 ms used earlier because Marstek inverters
    // need 1-3 s to ramp toward a new setpoint, and a 4 Hz dispatch
    // cycle on top of that just generates write queue churn.
    1000
}
fn default_deadband_w() -> f64 {
    // 50 W. Below this we treat the grid as "balanced enough". Larger
    // than v0.7.0 (30 W) because raw grid_w noise often clears 30 W
    // even on a quiet grid.
    50.0
}
fn default_hit_tolerance_w() -> f64 {
    15.0
}
fn default_plug_stable_w() -> f64 {
    10.0
}
fn default_plug_stable_duration_s() -> f64 {
    // 3 s. Marstek inverters can take ~2 s to ramp from one setpoint
    // to the next; we want the plug to be visibly steady before we
    // conclude the previous command has landed.
    3.0
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
    // 10 s. Marstek ramp time + a safety margin. Lower values lead to
    // the dispatcher giving up before the inverter has actually
    // committed to the new setpoint.
    10.0
}
fn default_soc_unknown_lockout_s() -> f64 {
    600.0
}
fn default_setpoint_deadband_w() -> f64 {
    // 100 W. Anything smaller is below Marstek quantisation + plug
    // measurement noise. Writing for sub-100 W deltas just generates
    // churn on the bus without changing real-world behaviour.
    100.0
}
fn default_modbus_heartbeat_s() -> f64 {
    // 30 s. We re-arm RS485 control mode (42000=21930) on every
    // setpoint write anyway, so the heartbeat just covers "dropped
    // last write" recovery + acts as our process-liveness signal.
    // 30 s is fast enough that a dead controller would be detected
    // well before any real damage.
    30.0
}
fn default_modbus_watchdog_grace_s() -> f64 {
    30.0
}
fn default_emergency_cutoff_margin_w() -> f64 {
    200.0
}
fn default_emergency_cutoff_grace_s() -> f64 {
    5.0
}
fn default_emergency_cutoff_recovery_s() -> f64 {
    600.0
}
fn default_night_cutoff_soc_margin_pct() -> f64 {
    2.0
}
fn default_grid_smoothing_s() -> f64 {
    5.0
}
fn default_modbus_min_write_interval_s() -> f64 {
    10.0
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
    30000
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
        if self.dispatcher.setpoint_deadband_w < 0.0 {
            anyhow::bail!("dispatcher.setpoint_deadband_w must not be negative");
        }
        if self.dispatcher.modbus_heartbeat_s < 0.0 {
            anyhow::bail!("dispatcher.modbus_heartbeat_s must not be negative");
        }
        if self.dispatcher.modbus_watchdog_grace_s < 0.0 {
            anyhow::bail!("dispatcher.modbus_watchdog_grace_s must not be negative");
        }
        if self.dispatcher.emergency_cutoff_margin_w < 0.0 {
            anyhow::bail!("dispatcher.emergency_cutoff_margin_w must not be negative");
        }
        if self.dispatcher.emergency_cutoff_grace_s < 0.0 {
            anyhow::bail!("dispatcher.emergency_cutoff_grace_s must not be negative");
        }
        if self.dispatcher.emergency_cutoff_recovery_s < 0.0 {
            anyhow::bail!("dispatcher.emergency_cutoff_recovery_s must not be negative");
        }
        if self.dispatcher.grid_smoothing_s < 0.0 {
            anyhow::bail!("dispatcher.grid_smoothing_s must not be negative");
        }
        if self.dispatcher.modbus_min_write_interval_s < 0.0 {
            anyhow::bail!("dispatcher.modbus_min_write_interval_s must not be negative");
        }
        if self.dispatcher.night_cutoff_soc_margin_pct < 0.0 {
            anyhow::bail!("dispatcher.night_cutoff_soc_margin_pct must not be negative");
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
