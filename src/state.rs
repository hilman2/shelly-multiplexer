//! Runtime state for the pulse dispatcher.
//!
//! Each battery owns a virtual integrator (`commanded_w`) whose value is the
//! cumulative sum of every delta pulse we have committed to that Marstek's
//! internal integrator. Marstek hardware integrates the same pulses on its
//! end; our `commanded_w` is what we *expect* it to be. The Shelly Plug
//! reading (`last_plug_w`) is the ground truth that resolves disagreements
//! and detects saturation.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::config::{BatteryConfig, CircuitConfig, Config};
use crate::rpc::EmStatusIncoming;

/// Snapshot of the real Shelly's last successful poll.
#[derive(Debug, Clone, Default)]
pub struct EmSnapshot {
    pub status: EmStatusIncoming,
    pub age: Option<Instant>,
}

/// Sign convention everywhere in this app:
///   - positive = battery DISCHARGES (power flowing out toward house/grid)
///   - negative = battery CHARGES   (power flowing in from grid)
///
/// CT signal we present to a Marstek follows the same convention: sending
/// a positive value tells the Marstek "the grid is importing this many W,
/// please discharge to compensate". The Marstek treats each value as a
/// delta to its internal target (no decay) — see memory/marstek_empirical.md.

#[derive(Debug)]
pub struct BatteryState {
    pub id: String,
    pub circuit: String,
    pub address: IpAddr,

    pub max_charge_w: f64,
    pub max_discharge_w: f64,
    pub capacity_wh: f64,
    pub priority_weight: f64,
    /// Per-battery SoC limits (None = inherit dispatcher defaults).
    pub soc_full_pct: Option<f64>,
    pub soc_empty_pct: Option<f64>,

    /// Our virtual integrator. Hardware-clamped to
    /// [-max_charge_w, +max_discharge_w].
    pub commanded_w: f64,

    /// Single CT value to send to the Marstek on the next poll(s).
    /// We never queue multiple delta values — if the dispatcher
    /// recomputes mid-pulse the previous (stale) delta is overwritten,
    /// not appended. `pulse_remaining` says how many more polls keep
    /// sending this value before the response reverts to 0.
    pub pending_pulse_w: f64,
    pub pulse_remaining: u32,

    /// Latest plug reading (signed, our convention).
    pub last_plug_w: Option<f64>,
    pub last_plug_at: Option<Instant>,

    /// Diagnostics: when this Marstek last polled the virtual Shelly.
    pub last_marstek_poll_at: Option<Instant>,

    /// When the dispatcher first noticed |commanded_w − plug_w| exceeding
    /// saturation_gap_w. After saturation_window_s of persistent drift the
    /// dispatcher resyncs commanded_w to the plug reading. This isn't real
    /// "saturation" (BMS taper at SoC extremes is handled by the SoC-aware
    /// soft bounds in compute_desired); it catches communication drift —
    /// lost UDP pulses, the HACS Marstek plugin overriding our CT signal,
    /// or transient BMS refusals.
    pub saturation_since: Option<Instant>,
    /// Reserved (kept for /api/status backward compat). Always false / None
    /// in v0.2.11+ — the BMS-saturation ceiling concept was retired because
    /// the SoC-aware soft bounds already cap commanded above 98 % / below
    /// floor and that's where the real BMS taper lives anyway.
    pub saturated: bool,
    pub saturation_ceiling_w: Option<f64>,

    /// Telemetry sourced from the Marstek itself (or HA).
    pub soc_pct: Option<f64>,
    pub soc_at: Option<Instant>,
    /// Where the current SoC value came from (e.g. "ha:sensor.marstek_soc"
    /// or "marstek-direct"). Surfaced in /api/status so the user can see
    /// at a glance whether the dispatcher is reading the value they think
    /// it is — important when an HA HACS plugin and our direct poll could
    /// each return different numbers.
    pub soc_source: Option<String>,

    /// Last error from any subsystem, surfaced in the UI.
    pub last_error: Option<String>,
}

impl BatteryState {
    pub fn from_config(cfg: &BatteryConfig) -> Self {
        Self {
            id: cfg.id.clone(),
            circuit: cfg.circuit.clone(),
            address: cfg.address,
            max_charge_w: cfg.max_charge_w,
            max_discharge_w: cfg.max_discharge_w,
            capacity_wh: if cfg.capacity_wh > 0.0 {
                cfg.capacity_wh
            } else {
                cfg.max_charge_w + cfg.max_discharge_w
            },
            priority_weight: cfg.priority_weight,
            soc_full_pct: cfg.soc_full_pct,
            soc_empty_pct: cfg.soc_empty_pct,
            commanded_w: 0.0,
            pending_pulse_w: 0.0,
            pulse_remaining: 0,
            last_plug_w: None,
            last_plug_at: None,
            last_marstek_poll_at: None,
            saturated: false,
            saturation_ceiling_w: None,
            saturation_since: None,
            soc_pct: None,
            soc_at: None,
            soc_source: None,
            last_error: None,
        }
    }

    /// Has the previous pulse landed (plug reads close to commanded_w)?
    /// Used to enforce "no new pulse while previous is still in flight".
    pub fn pulse_settled(&self, hit_tolerance_w: f64) -> bool {
        if self.pulse_remaining > 0 {
            return false;
        }
        let Some(plug) = self.last_plug_w else {
            return false;
        };
        (plug - self.commanded_w).abs() <= hit_tolerance_w
    }

    pub fn is_plug_fresh(&self, now: Instant, stale_s: f64) -> bool {
        match self.last_plug_at {
            Some(t) => now.duration_since(t).as_secs_f64() <= stale_s,
            None => false,
        }
    }

    /// Effective full / empty thresholds: per-battery override, falling
    /// back to dispatcher defaults if unset.
    pub fn effective_soc_full_pct(&self, fallback: f64) -> f64 {
        self.soc_full_pct.unwrap_or(fallback)
    }
    pub fn effective_soc_empty_pct(&self, fallback: f64) -> f64 {
        self.soc_empty_pct.unwrap_or(fallback)
    }
}

#[derive(Debug)]
pub struct CircuitState {
    pub config: CircuitConfig,
    pub member_ids: Vec<String>,
    /// Set when any plug in the circuit has been stale long enough that we
    /// can't trust the cap math. While set, virtual_shelly drops responses
    /// to all members so their watchdogs clear (60 s by default), then we
    /// resume only once plugs are healthy again.
    pub silent_until: Option<Instant>,
}

impl CircuitState {
    pub fn cap_w(&self) -> f64 {
        self.config.cap_w()
    }
}

pub struct AppState {
    pub snapshot: ArcSwap<EmSnapshot>,
    pub batteries: RwLock<HashMap<String, BatteryState>>,
    pub circuits: RwLock<HashMap<String, CircuitState>>,
    /// Marstek IP -> battery_id, derived from config at startup so the UDP
    /// responder can route polls to their pulse queues in O(1).
    pub by_addr: HashMap<IpAddr, String>,
    pub energy: RwLock<EnergyCounters>,
    pub started_at: Instant,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let mut batteries = HashMap::new();
        let mut by_addr = HashMap::new();
        for b in &cfg.batteries {
            let st = BatteryState::from_config(b);
            by_addr.insert(st.address, st.id.clone());
            batteries.insert(st.id.clone(), st);
        }
        let mut circuits = HashMap::new();
        for c in &cfg.circuits {
            let members: Vec<String> = cfg
                .batteries
                .iter()
                .filter(|b| b.circuit == c.id)
                .map(|b| b.id.clone())
                .collect();
            circuits.insert(
                c.id.clone(),
                CircuitState {
                    config: c.clone(),
                    member_ids: members,
                    silent_until: None,
                },
            );
        }
        Arc::new(Self {
            snapshot: ArcSwap::from_pointee(EmSnapshot::default()),
            batteries: RwLock::new(batteries),
            circuits: RwLock::new(circuits),
            by_addr,
            energy: RwLock::new(EnergyCounters::default()),
            started_at: Instant::now(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EnergyCounters {
    pub consumed_wh: f64,
    pub returned_wh: f64,
}

impl EnergyCounters {
    pub fn integrate(&mut self, snap: &EmStatusIncoming, dt_seconds: f64) {
        let dt_h = dt_seconds / 3600.0;
        let total = snap.total_act_power.unwrap_or(0.0);
        if total >= 0.0 {
            self.consumed_wh += total * dt_h;
        } else {
            self.returned_wh += -total * dt_h;
        }
    }
}
