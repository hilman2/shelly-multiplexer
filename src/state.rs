//! Runtime state for the pulse dispatcher.
//!
//! Architecture (v0.3.0+):
//!   - We do NOT track a virtual integrator any more. The grid reading
//!     from the real Shelly is the residual error; the plug reading per
//!     battery is the ground truth for cap and headroom calculations.
//!   - Each dispatcher tick computes a one-shot delta per battery from
//!     the current grid_w, weighted by available headroom (plug_w + SoC
//!     limits). The delta is sent as a pulse_count-long CT burst.
//!   - A new pulse only queues once the previous has "landed" — defined
//!     as the plug having moved by >= deadband_w from the snapshot taken
//!     at pulse send. A settle_timeout_s fallback prevents lockups when
//!     the Marstek refuses to respond.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};

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
    /// SoC-based power tapering. See `BatteryConfig` for semantics.
    pub charge_taper_soc_pct: Option<f64>,
    pub charge_taper_w: Option<f64>,
    pub discharge_taper_soc_pct: Option<f64>,
    pub discharge_taper_w: Option<f64>,

    /// Single CT value to send to the Marstek on the next poll(s).
    /// Replaced on every new dispatch decision (not appended).
    pub pending_pulse_w: f64,
    pub pulse_remaining: u32,
    /// Plug reading at the moment the current/most-recent pulse cycle
    /// started. The next dispatch tick may only run once the plug has
    /// moved by >= deadband_w from this value (or the settle_timeout_s
    /// has elapsed).
    pub plug_w_at_pulse_send: Option<f64>,
    /// When pulse_remaining last decremented to 0 (the last pulse went
    /// out on the wire). Combined with plug_w_at_pulse_send, this lets
    /// pulse_settled wait for actual battery response and time-bound
    /// the wait so a refusing Marstek doesn't lock the dispatcher.
    pub last_pulse_completed_at: Option<Instant>,

    /// Latest plug reading (signed, our convention).
    pub last_plug_w: Option<f64>,
    pub last_plug_at: Option<Instant>,
    /// When the plug reading last changed by more than `plug_stable_w`
    /// from its previous value. Updated in `plug.rs` on every ingest.
    /// Combined with `last_pulse_completed_at`, this lets `pulse_settled`
    /// wait until the battery has finished implementing the delta — i.e.
    /// the plug has moved AND stopped moving — instead of firing the next
    /// pulse the moment movement begins.
    pub last_plug_movement_at: Option<Instant>,

    /// Diagnostics: when this Marstek last polled the virtual Shelly.
    pub last_marstek_poll_at: Option<Instant>,

    /// Telemetry sourced from the Marstek itself (or HA).
    pub soc_pct: Option<f64>,
    pub soc_at: Option<Instant>,
    /// Where the current SoC value came from (e.g. "ha:sensor.marstek_soc"
    /// or "marstek-direct"). Surfaced in /api/status so the user can see
    /// at a glance whether the dispatcher is reading the value they think
    /// it is.
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
            charge_taper_soc_pct: cfg.charge_taper_soc_pct,
            charge_taper_w: cfg.charge_taper_w,
            discharge_taper_soc_pct: cfg.discharge_taper_soc_pct,
            discharge_taper_w: cfg.discharge_taper_w,
            pending_pulse_w: 0.0,
            pulse_remaining: 0,
            plug_w_at_pulse_send: None,
            last_pulse_completed_at: None,
            last_plug_w: None,
            last_plug_at: None,
            last_plug_movement_at: None,
            last_marstek_poll_at: None,
            soc_pct: None,
            soc_at: None,
            soc_source: None,
            last_error: None,
        }
    }

    /// Is the battery ready for a new pulse to be queued?
    ///
    /// Conditions for "yes":
    ///   1. No pulse in flight (pulse_remaining == 0), AND
    ///   2. one of:
    ///      (a) we never sent a pulse yet (initial state),
    ///      (b) the plug has moved since the pulse and has now been STABLE
    ///          (no movement above plug_stable_w) for `stable_duration_s` —
    ///          i.e. Marstek has finished implementing the delta, or
    ///      (c) `settle_timeout_s` elapsed since the pulse cycle finished
    ///          (battery didn't respond, but we can't wait forever).
    ///
    /// The plug is "moving" while `last_plug_movement_at` keeps getting
    /// refreshed by the plug poller in response to >stable_w deltas
    /// between consecutive readings. Once readings settle within the
    /// stable_w band again, `last_plug_movement_at` stops advancing and
    /// the elapsed-time check eventually clears.
    pub fn pulse_settled(&self, stable_duration_s: f64, settle_timeout_s: f64) -> bool {
        if self.pulse_remaining > 0 {
            return false;
        }
        let Some(pulse_done) = self.last_pulse_completed_at else {
            return true; // initial state — never pulsed
        };
        let since_pulse = pulse_done.elapsed().as_secs_f64();
        if since_pulse >= settle_timeout_s {
            return true; // escape hatch — Marstek isn't reacting
        }
        // Movement must have occurred AFTER the pulse went out (proves
        // Marstek reacted), then stabilized for stable_duration_s.
        match self.last_plug_movement_at {
            Some(t) if t >= pulse_done => t.elapsed().as_secs_f64() >= stable_duration_s,
            _ => false,
        }
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

    /// Effective max charge power given the current SoC. Above the taper
    /// SoC the BMS would taper anyway; we model it explicitly so the
    /// dispatcher's `headroom()` is honest.
    pub fn effective_max_charge_w(&self) -> f64 {
        match (self.soc_pct, self.charge_taper_soc_pct, self.charge_taper_w) {
            (Some(soc), Some(taper_soc), Some(taper_w)) if soc >= taper_soc => taper_w,
            _ => self.max_charge_w,
        }
    }

    /// Effective max discharge power given the current SoC. Below the
    /// taper SoC the battery can't sustain full output; we cap it
    /// upstream so the integrator never overcommits.
    pub fn effective_max_discharge_w(&self) -> f64 {
        match (
            self.soc_pct,
            self.discharge_taper_soc_pct,
            self.discharge_taper_w,
        ) {
            (Some(soc), Some(taper_soc), Some(taper_w)) if soc <= taper_soc => taper_w,
            _ => self.max_discharge_w,
        }
    }

    /// True iff the charge taper is currently active (SoC high enough
    /// that we cap charge below the hardware max). Used for UI display.
    pub fn is_charge_tapered(&self) -> bool {
        self.effective_max_charge_w() < self.max_charge_w
    }

    /// True iff the discharge taper is currently active (SoC low enough
    /// that we cap discharge below the hardware max).
    pub fn is_discharge_tapered(&self) -> bool {
        self.effective_max_discharge_w() < self.max_discharge_w
    }

    /// True iff SoC is at or above the hard full cutoff — charge
    /// direction is fully gated to 0 W.
    pub fn is_soc_full_gated(&self, fallback_full_pct: f64) -> bool {
        match self.soc_pct {
            Some(soc) => soc >= self.effective_soc_full_pct(fallback_full_pct),
            None => false,
        }
    }

    /// True iff SoC is at or below the hard empty cutoff — discharge
    /// direction is fully gated to 0 W.
    pub fn is_soc_empty_gated(&self, fallback_empty_pct: f64) -> bool {
        match self.soc_pct {
            Some(soc) => soc <= self.effective_soc_empty_pct(fallback_empty_pct),
            None => false,
        }
    }

    /// True iff the plug is currently at ≥ 95 % of the effective max
    /// in the active direction. Operator-facing "the battery is doing
    /// everything it can right now" indicator. The 95 % threshold is
    /// hardcoded — this is purely cosmetic, not a control input.
    pub fn is_at_charge_limit(&self) -> bool {
        let plug = match self.last_plug_w {
            Some(p) => p,
            None => return false,
        };
        // Charging = negative plug. At limit when |plug| ≥ 95% of cap.
        if plug >= 0.0 {
            return false;
        }
        let cap = self.effective_max_charge_w();
        cap > 0.0 && -plug >= cap * 0.95
    }

    pub fn is_at_discharge_limit(&self) -> bool {
        let plug = match self.last_plug_w {
            Some(p) => p,
            None => return false,
        };
        if plug <= 0.0 {
            return false;
        }
        let cap = self.effective_max_discharge_w();
        cap > 0.0 && plug >= cap * 0.95
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
    /// Throttle for the grid-stale warning so a long real-Shelly outage
    /// doesn't spam the log.
    pub last_grid_stale_warn: Mutex<Option<Instant>>,
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
            last_grid_stale_warn: Mutex::new(None),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_battery(plug_w: Option<f64>) -> BatteryState {
        BatteryState {
            id: "test".into(),
            circuit: "c1".into(),
            address: "127.0.0.1".parse().unwrap(),
            max_charge_w: 2500.0,
            max_discharge_w: 800.0,
            capacity_wh: 2500.0,
            priority_weight: 1.0,
            soc_full_pct: None,
            soc_empty_pct: None,
            charge_taper_soc_pct: None,
            charge_taper_w: None,
            discharge_taper_soc_pct: None,
            discharge_taper_w: None,
            pending_pulse_w: 0.0,
            pulse_remaining: 0,
            plug_w_at_pulse_send: None,
            last_pulse_completed_at: None,
            last_plug_w: plug_w,
            last_plug_at: plug_w.map(|_| Instant::now()),
            last_plug_movement_at: None,
            last_marstek_poll_at: None,
            soc_pct: None,
            soc_at: None,
            soc_source: None,
            last_error: None,
        }
    }

    #[test]
    fn pulse_settled_initial_state() {
        let b = make_battery(None);
        // No prior pulse → settled.
        assert!(b.pulse_settled(15.0, 5.0));
    }

    #[test]
    fn pulse_settled_in_flight() {
        let mut b = make_battery(Some(0.0));
        b.pulse_remaining = 2;
        // Pulse still going out → not settled.
        assert!(!b.pulse_settled(15.0, 5.0));
    }

    #[test]
    fn pulse_settled_when_plug_moved_then_stable() {
        let mut b = make_battery(Some(120.0));
        b.plug_w_at_pulse_send = Some(0.0);
        // Pulse 4 s ago, last movement 2 s ago (i.e. plug has been stable
        // for 2 s, longer than stable_duration_s = 1.5).
        let now = Instant::now();
        b.last_pulse_completed_at = Some(now - Duration::from_secs(4));
        b.last_plug_movement_at = Some(now - Duration::from_secs(2));
        assert!(b.pulse_settled(1.5, 5.0));
    }

    #[test]
    fn pulse_settled_blocked_while_plug_still_moving() {
        let mut b = make_battery(Some(120.0));
        b.plug_w_at_pulse_send = Some(0.0);
        // Pulse 1 s ago, plug moved just now → not stable yet.
        let now = Instant::now();
        b.last_pulse_completed_at = Some(now - Duration::from_secs(1));
        b.last_plug_movement_at = Some(now);
        assert!(!b.pulse_settled(1.5, 5.0));
    }

    #[test]
    fn pulse_settled_blocked_when_marstek_didnt_react() {
        // Marstek refused: plug never moved after the pulse, last_movement
        // is from before the pulse went out.
        let mut b = make_battery(Some(2.0));
        b.plug_w_at_pulse_send = Some(0.0);
        let now = Instant::now();
        b.last_pulse_completed_at = Some(now - Duration::from_secs(1));
        b.last_plug_movement_at = Some(now - Duration::from_secs(10));
        // Pre-pulse movement doesn't count → blocked until timeout.
        assert!(!b.pulse_settled(1.5, 5.0));
    }

    #[test]
    fn pulse_settled_via_timeout() {
        let mut b = make_battery(Some(2.0));
        b.plug_w_at_pulse_send = Some(0.0);
        // Pretend the pulse completed long ago, plug never moved.
        b.last_pulse_completed_at = Some(Instant::now() - Duration::from_secs(10));
        b.last_plug_movement_at = None;
        assert!(b.pulse_settled(1.5, 5.0));
    }

    #[test]
    fn effective_max_charge_w_below_taper_soc_returns_full() {
        let mut b = make_battery(Some(0.0));
        b.charge_taper_soc_pct = Some(95.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(80.0);
        assert_eq!(b.effective_max_charge_w(), 2500.0);
    }

    #[test]
    fn effective_max_charge_w_at_or_above_taper_soc_returns_taper() {
        let mut b = make_battery(Some(0.0));
        b.charge_taper_soc_pct = Some(95.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(95.0);
        assert_eq!(b.effective_max_charge_w(), 1000.0);
        b.soc_pct = Some(98.0);
        assert_eq!(b.effective_max_charge_w(), 1000.0);
    }

    #[test]
    fn effective_max_charge_w_unset_returns_full() {
        // No taper config → falls back to hardware cap regardless of SoC.
        let mut b = make_battery(Some(0.0));
        b.soc_pct = Some(99.0);
        assert_eq!(b.effective_max_charge_w(), 2500.0);
    }

    #[test]
    fn effective_max_discharge_w_above_taper_soc_returns_full() {
        let mut b = make_battery(Some(0.0));
        b.discharge_taper_soc_pct = Some(15.0);
        b.discharge_taper_w = Some(400.0);
        b.soc_pct = Some(50.0);
        assert_eq!(b.effective_max_discharge_w(), 800.0);
    }

    #[test]
    fn effective_max_discharge_w_at_or_below_taper_soc_returns_taper() {
        let mut b = make_battery(Some(0.0));
        b.discharge_taper_soc_pct = Some(15.0);
        b.discharge_taper_w = Some(400.0);
        b.soc_pct = Some(15.0);
        assert_eq!(b.effective_max_discharge_w(), 400.0);
        b.soc_pct = Some(8.0);
        assert_eq!(b.effective_max_discharge_w(), 400.0);
    }

    #[test]
    fn effective_max_without_soc_returns_full() {
        // SoC unknown → can't apply the taper, return the hardware cap.
        // The hard `soc_full_pct` / `soc_empty_pct` gate in dispatcher.rs
        // also returns the hardware cap when soc is None, so we stay
        // consistent with the rest of the pipeline.
        let mut b = make_battery(Some(0.0));
        b.charge_taper_soc_pct = Some(95.0);
        b.charge_taper_w = Some(1000.0);
        b.discharge_taper_soc_pct = Some(15.0);
        b.discharge_taper_w = Some(400.0);
        b.soc_pct = None;
        assert_eq!(b.effective_max_charge_w(), 2500.0);
        assert_eq!(b.effective_max_discharge_w(), 800.0);
    }

    #[test]
    fn is_charge_tapered_true_when_effective_below_hardware() {
        let mut b = make_battery(Some(0.0));
        b.charge_taper_soc_pct = Some(90.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(92.0);
        assert!(b.is_charge_tapered());
        b.soc_pct = Some(80.0);
        assert!(!b.is_charge_tapered());
    }

    #[test]
    fn is_discharge_tapered_true_when_effective_below_hardware() {
        let mut b = make_battery(Some(0.0));
        b.discharge_taper_soc_pct = Some(15.0);
        b.discharge_taper_w = Some(400.0);
        b.soc_pct = Some(12.0);
        assert!(b.is_discharge_tapered());
        b.soc_pct = Some(50.0);
        assert!(!b.is_discharge_tapered());
    }

    #[test]
    fn is_soc_full_gated_uses_effective_threshold() {
        let mut b = make_battery(Some(0.0));
        b.soc_pct = Some(96.0);
        // No per-battery override, fallback 95 → gated.
        assert!(b.is_soc_full_gated(95.0));
        b.soc_pct = Some(94.0);
        assert!(!b.is_soc_full_gated(95.0));
        // Per-battery override wins.
        b.soc_full_pct = Some(98.0);
        b.soc_pct = Some(96.0);
        assert!(!b.is_soc_full_gated(95.0));
    }

    #[test]
    fn is_soc_empty_gated_uses_effective_threshold() {
        let mut b = make_battery(Some(0.0));
        b.soc_pct = Some(4.0);
        assert!(b.is_soc_empty_gated(5.0));
        b.soc_pct = Some(6.0);
        assert!(!b.is_soc_empty_gated(5.0));
    }

    #[test]
    fn is_soc_gated_false_without_soc() {
        let b = make_battery(Some(0.0));
        assert!(!b.is_soc_full_gated(95.0));
        assert!(!b.is_soc_empty_gated(5.0));
    }

    #[test]
    fn is_at_charge_limit_when_plug_near_effective_cap() {
        let mut b = make_battery(Some(-2400.0));
        // No taper → cap is 2500 W, 2400 / 2500 = 96 % ≥ 95 % → at limit.
        assert!(b.is_at_charge_limit());
        b.last_plug_w = Some(-1000.0);
        assert!(!b.is_at_charge_limit());
        // Taper engaged → cap is 1000 W, -950 ≥ 95 % of 1000 → at limit.
        b.charge_taper_soc_pct = Some(90.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(92.0);
        b.last_plug_w = Some(-960.0);
        assert!(b.is_at_charge_limit());
        // Plug positive (discharging) → not a charge-limit state.
        b.last_plug_w = Some(500.0);
        assert!(!b.is_at_charge_limit());
    }

    #[test]
    fn is_at_discharge_limit_when_plug_near_effective_cap() {
        let mut b = make_battery(Some(770.0));
        // No taper → cap is 800 W, 770 / 800 = 96.25 % ≥ 95 % → at limit.
        assert!(b.is_at_discharge_limit());
        b.last_plug_w = Some(200.0);
        assert!(!b.is_at_discharge_limit());
        b.last_plug_w = Some(-500.0);
        assert!(!b.is_at_discharge_limit());
    }

    #[test]
    fn energy_integrate_consumed_and_returned() {
        let mut e = EnergyCounters::default();
        // Importing 1000 W for 1 hour → 1000 Wh consumed.
        let snap_in = EmStatusIncoming {
            total_act_power: Some(1000.0),
            ..Default::default()
        };
        e.integrate(&snap_in, 3600.0);
        assert!((e.consumed_wh - 1000.0).abs() < 0.001);
        assert_eq!(e.returned_wh, 0.0);
        // Exporting 500 W for 1 hour → 500 Wh returned.
        let snap_out = EmStatusIncoming {
            total_act_power: Some(-500.0),
            ..Default::default()
        };
        e.integrate(&snap_out, 3600.0);
        assert!((e.consumed_wh - 1000.0).abs() < 0.001);
        assert!((e.returned_wh - 500.0).abs() < 0.001);
    }
}
