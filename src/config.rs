use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub real_shelly: RealShellyConfig,
    pub virtual_shelly: VirtualShellyConfig,
    pub management: ManagementConfig,
    pub dispatcher: DispatcherConfig,
    /// Kept as a deprecated section for backwards compatibility — the
    /// dispatcher no longer reads it. Safety is now enforced
    /// physically: max 1 battery active per circuit, with circuits
    /// validating that no member exceeds the circuit's cap.
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub home_assistant: HomeAssistantConfig,
    /// `[[circuits]]` is the new name; `[[groups]]` still parses for
    /// backwards compatibility (deserde alias). Output writes always
    /// use the new key.
    #[serde(default, alias = "groups")]
    pub circuits: Vec<CircuitConfig>,
    #[serde(default)]
    pub batteries: Vec<BatteryConfig>,
}

/// Optional bridge to a Home Assistant instance. When `enabled` and a
/// battery has `soc_entity_id` set, we read SoC from HA via its REST
/// API instead of polling the inverter directly. Useful when HA already
/// owns the inverter's UDP port (e.g. an HA Marstek integration is
/// running) — the multiplexer doesn't need to compete for it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HomeAssistantConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the HA Core API. Inside an HA add-on this is
    /// `http://supervisor/core/api`; standalone setups use
    /// `http://<ha-host>:8123/api`.
    #[serde(default = "default_ha_url")]
    pub url: String,
    /// Long-lived access token (or `$SUPERVISOR_TOKEN` inside the add-on).
    #[serde(default)]
    pub token: String,
    /// HTTP request timeout per state lookup.
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
    // We always run with host_network: true (HA add-on) or on
    // separate hardware — never inside the hassio bridge — so the
    // `supervisor` hostname is never resolvable. Default to the
    // mDNS host name HA-OS publishes; user can override with a raw
    // IP for setups where mDNS doesn't reach.
    "http://homeassistant.local:8123/api".into()
}

fn default_ha_timeout_ms() -> u64 {
    3000
}

/// Global protective limit on the absolute sum of all battery
/// allocations. Default: 3000 W. Going higher than 3000 W requires
/// **both** acknowledgement flags to be set in the TOML *and* via the
/// admin UI confirm flow.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SafetyConfig {
    /// Maximum absolute sum of all battery allocations (W). Charging and
    /// discharging are capped against the same value.
    #[serde(default = "default_safety_cap")]
    pub max_total_w: f64,
    /// First acknowledgement: I understand that exceeding 3000 W can
    /// cause overload, fire, or damage if the wiring is not rated for it.
    #[serde(default)]
    pub acknowledged_higher_risk: bool,
    /// Second acknowledgement: I have verified that every battery is on
    /// its OWN protective device (no two batteries share a fuse / RCD)
    /// rated for the load it can produce.
    #[serde(default)]
    pub acknowledged_separate_fuses: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_total_w: default_safety_cap(),
            acknowledged_higher_risk: false,
            acknowledged_separate_fuses: false,
        }
    }
}

fn default_safety_cap() -> f64 {
    3000.0
}

pub const SAFETY_DEFAULT_CAP_W: f64 = 3000.0;

impl SafetyConfig {
    /// Returns the effective cap actually applied at runtime: the user's
    /// requested cap if both acknowledgements are present and it is at
    /// least equal to the default; otherwise the hard default of 3000 W.
    pub fn effective_cap_w(&self) -> f64 {
        if self.max_total_w <= SAFETY_DEFAULT_CAP_W {
            // Lowering the cap is always allowed — never below 0 though.
            self.max_total_w.max(0.0)
        } else if self.acknowledged_higher_risk && self.acknowledged_separate_fuses {
            self.max_total_w
        } else {
            SAFETY_DEFAULT_CAP_W
        }
    }

    pub fn override_active(&self) -> bool {
        self.max_total_w > SAFETY_DEFAULT_CAP_W
            && self.acknowledged_higher_risk
            && self.acknowledged_separate_fuses
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RealShellyConfig {
    pub host: IpAddr,
    pub udp_port: u16,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
}

fn default_poll_interval() -> u64 {
    250
}
fn default_request_timeout() -> u64 {
    1000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VirtualShellyConfig {
    #[serde(default = "default_bind_interface")]
    pub bind_interface: String,
    #[serde(default = "default_virtual_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_virtual_http_port")]
    pub http_port: u16,
    #[serde(default = "default_min_sample_period")]
    pub min_sample_period_ms: u64,
    #[serde(default)]
    pub device_mac: String,
    #[serde(default)]
    pub device_hostname: String,
    #[serde(default = "default_firmware")]
    pub firmware: String,
    /// Advertise via mDNS so inverters can discover us as a Shelly.
    /// Disabled by default in the HA add-on because HA OS already runs
    /// Avahi on UDP/5353 and the mdns-sd daemon's worker thread dies
    /// silently when it can't claim the multicast group.
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
fn default_min_sample_period() -> u64 {
    1000
}
fn default_firmware() -> String {
    "1.4.4".into()
}
fn default_enable_mdns() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManagementConfig {
    #[serde(default = "default_management_bind")]
    pub bind_address: String,
}

fn default_management_bind() -> String {
    "0.0.0.0:8080".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DispatcherConfig {
    #[serde(default = "default_strategy")]
    pub strategy: AllocationStrategy,
    #[serde(default)]
    pub rate_limit_w_per_s: f64,
    #[serde(default = "default_deadband")]
    pub deadband_w: f64,
}

fn default_strategy() -> AllocationStrategy {
    AllocationStrategy::Equal
}
fn default_deadband() -> f64 {
    30.0
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationStrategy {
    Equal,
    ByCapacity,
    /// Soft priority based on SoC. When discharging, batteries with higher
    /// SoC contribute more; when charging, batteries with lower SoC absorb
    /// more. Falls back to `Equal` for batteries without SoC telemetry.
    BySoc,
    Priority,
}

/// A `Circuit` represents a shared protective device (MCB/RCD)
/// downstream of which one or more batteries hang. The dispatcher
/// guarantees that **at most one** battery in a circuit is active at
/// a time, so the circuit's cap_w must only be ≥ the largest single
/// member's max(charge_w, discharge_w). Validated at startup.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CircuitConfig {
    pub id: String,
    pub fuse_amps: f64,
    #[serde(default = "default_phases")]
    pub phases: u8,
    #[serde(default = "default_voltage")]
    pub voltage: f64,
}

/// Old name kept as an alias so existing configs still parse.
pub type GroupConfig = CircuitConfig;

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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatteryConfig {
    pub id: String,
    pub address: IpAddr,
    #[serde(default = "default_vendor")]
    pub vendor: BatteryVendor,
    /// Circuit (= shared MCB downstream) the battery sits on.
    /// Mandatory — drives the multiplex layer that guarantees max one
    /// active battery per circuit. Old configs may still use `group`
    /// (deserde alias).
    #[serde(alias = "group")]
    pub circuit: String,
    #[serde(default = "default_phase")]
    pub phase: PhaseAssignment,
    pub max_charge_w: f64,
    pub max_discharge_w: f64,
    /// Minimum SoC (%): once at or below this, the battery is excluded
    /// from discharge allocations. Default 12 % — typical Marstek DoD.
    #[serde(default = "default_min_soc")]
    pub min_soc_percent: f64,
    /// Maximum SoC (%): once at or above this, the battery is excluded
    /// from charge allocations. Default 100 %.
    #[serde(default = "default_max_soc")]
    pub max_soc_percent: f64,
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Marstek Open API UDP port (default 30000). Only used when vendor
    /// is `marstek`. Marstek requires send and receive to use the same
    /// configured port — see Marstek Open API docs.
    #[serde(default = "default_marstek_port")]
    pub marstek_port: u16,
    /// How often to poll the battery for SoC and actual power.
    #[serde(default = "default_telemetry_interval_ms")]
    pub telemetry_interval_ms: u64,
    /// If set AND `home_assistant.enabled = true`, the multiplexer
    /// reads this entity's state from HA instead of polling the
    /// inverter directly — avoids fighting HA for the inverter's UDP
    /// port. Example: `sensor.marstek_venus_e_battery_soc`.
    #[serde(default)]
    pub soc_entity_id: Option<String>,
    /// Result of the active phase-detection pass. Populated by the
    /// detection routine and persisted so it survives restarts.
    /// `phase` here is informational and may differ from the manually
    /// configured `phase` field above (which is used by group-cap
    /// estimation when set).
    #[serde(default)]
    pub detected_phase: Option<DetectedPhase>,
}

/// Outcome of a phase-detection probe for one battery.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DetectedPhase {
    pub phase: PhaseAssignment,
    /// 0.0 (not detected) to 1.0 (perfectly clear). Below ~0.5 the
    /// detection should be considered unreliable.
    pub confidence: f64,
    /// ISO-8601 timestamp of the detection run.
    pub detected_at: String,
    /// Largest signed delta observed on each phase between charge and
    /// discharge probes — useful for diagnosing low-confidence runs.
    pub delta_a_w: f64,
    pub delta_b_w: f64,
    pub delta_c_w: f64,
}

fn default_min_soc() -> f64 {
    12.0
}
fn default_max_soc() -> f64 {
    100.0
}

fn default_vendor() -> BatteryVendor {
    BatteryVendor::Generic
}
fn default_marstek_port() -> u16 {
    30000
}
fn default_telemetry_interval_ms() -> u64 {
    60000
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BatteryVendor {
    Marstek,
    Hoymiles,
    Generic,
}

fn default_phase() -> PhaseAssignment {
    PhaseAssignment::All
}
fn default_priority() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PhaseAssignment {
    A,
    B,
    C,
    All,
}

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
        // Circuits must be unique and well-formed.
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
            if c.voltage <= 0.0 {
                anyhow::bail!("circuit {}: voltage must be > 0", c.id);
            }
        }

        // Batteries must each reference an existing circuit and stay
        // within that circuit's cap on their own. The multiplex layer
        // guarantees only one is ever active per circuit, so the cap
        // is checked per-member, not per-sum.
        let mut seen_ids = std::collections::HashSet::new();
        for b in &self.batteries {
            if !seen_ids.insert(b.id.clone()) {
                anyhow::bail!("duplicate battery id: {}", b.id);
            }
            if b.circuit.trim().is_empty() {
                anyhow::bail!(
                    "battery {}: `circuit` is required (every battery must belong to a circuit)",
                    b.id
                );
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
                    "battery {} (max {} W) exceeds circuit '{}' cap ({} W) — the inverter \
                     could overload the shared protective device on its own. Either lower \
                     max_charge_w/max_discharge_w on the battery or move it to a circuit \
                     with a larger fuse.",
                    b.id,
                    largest,
                    b.circuit,
                    cap
                );
            }
            if b.max_charge_w < 0.0 || b.max_discharge_w < 0.0 {
                anyhow::bail!("battery {} has negative power limits", b.id);
            }
            if !(0.0..=100.0).contains(&b.min_soc_percent) {
                anyhow::bail!(
                    "battery {}: min_soc_percent must be in [0, 100], got {}",
                    b.id,
                    b.min_soc_percent
                );
            }
            if !(0.0..=100.0).contains(&b.max_soc_percent) {
                anyhow::bail!(
                    "battery {}: max_soc_percent must be in [0, 100], got {}",
                    b.id,
                    b.max_soc_percent
                );
            }
            if b.min_soc_percent >= b.max_soc_percent {
                anyhow::bail!(
                    "battery {}: min_soc_percent ({}) must be strictly less than max_soc_percent ({})",
                    b.id,
                    b.min_soc_percent,
                    b.max_soc_percent
                );
            }
            if b.telemetry_interval_ms == 0 {
                anyhow::bail!("battery {}: telemetry_interval_ms must be > 0", b.id);
            }
            if b.marstek_port == 0 {
                anyhow::bail!("battery {}: marstek_port must be > 0", b.id);
            }
        }

        if self.dispatcher.deadband_w < 0.0 {
            anyhow::bail!("dispatcher.deadband_w must not be negative");
        }
        if self.dispatcher.rate_limit_w_per_s < 0.0 {
            anyhow::bail!("dispatcher.rate_limit_w_per_s must not be negative");
        }
        if self.real_shelly.poll_interval_ms == 0 {
            anyhow::bail!("real_shelly.poll_interval_ms must be > 0");
        }
        Ok(())
    }
}
