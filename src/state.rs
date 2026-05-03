use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::config::{SafetyConfig, SAFETY_DEFAULT_CAP_W};
use crate::rpc::EmStatusIncoming;

/// Snapshot of the real Shelly's last successful poll.
#[derive(Debug, Clone, Default)]
pub struct EmSnapshot {
    pub status: EmStatusIncoming,
    pub age: Option<Instant>,
}

/// Per-battery allocation produced by the dispatcher. Multipliers are
/// applied per phase: `0.5` means "this battery sees 50% of the real
/// power on that phase". A battery on a single phase will have a
/// non-zero factor only on its phase and `0.0` on the others.
#[derive(Debug, Clone, Copy, Default)]
pub struct PhaseFactors {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

impl PhaseFactors {
    pub const fn uniform(f: f64) -> Self {
        Self { a: f, b: f, c: f }
    }
}

/// Per-phase battery allocation in watts. Sign follows the Shelly
/// convention: positive = battery discharges into that phase, negative
/// = battery charges from that phase's surplus.
#[derive(Debug, Clone, Copy, Default)]
pub struct PhaseWatts {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

#[derive(Debug, Clone)]
pub struct Allocation {
    pub battery_id: String,
    pub factors: PhaseFactors,
    /// Per-phase allocation in watts (same sign convention as the
    /// Shelly: + = discharge, − = charge). This is what the battery
    /// actually sees per phase via `EM.GetStatus`.
    pub phase_w: PhaseWatts,
    /// Net DC-side allocation (= a + b + c). On a battery that is
    /// charging on one phase and discharging on another, this can be
    /// near zero even though the battery is doing work — use
    /// `phase_w` and `magnitude_w` to see the actual activity.
    pub allocated_w: f64,
    /// Sum of |a| + |b| + |c|. Visible measure of how hard the battery
    /// is working, regardless of whether different phases cancel out.
    pub magnitude_w: f64,
    pub group: Option<String>,
    /// Why the allocation is what it is — populated by the dispatcher
    /// for cases the user might find non-obvious (battery full, empty,
    /// rate-limited, etc.).
    pub note: Option<String>,
}

/// Shared, lock-light state passed to all subsystems.
pub struct AppState {
    /// Last successful real-Shelly snapshot. Lock-free read via ArcSwap.
    pub snapshot: ArcSwap<EmSnapshot>,
    /// Allocations keyed by battery IP. Read on every UDP request, written
    /// by the dispatcher; an RwLock is fine — read is a quick HashMap lookup.
    pub allocations: RwLock<HashMap<IpAddr, Allocation>>,
    /// Time of last poll request received from a battery, keyed by IP.
    /// Written exclusively by `virtual_shelly` on each incoming request,
    /// read by `http_admin` for status display. Kept separate from
    /// `allocations` so the dispatcher's full-map replacement on every
    /// tick doesn't race with poll-time updates.
    pub last_poll_at: RwLock<HashMap<IpAddr, Instant>>,
    /// Per-battery telemetry (SoC, actual power, flags) keyed by battery id.
    pub telemetry: RwLock<HashMap<String, BatteryTelemetry>>,
    /// Cumulative energy counters (Wh) integrated from observed power.
    pub energy: RwLock<EnergyCounters>,
    /// Runtime-mutable safety cap. Initialised from `config.safety` at
    /// startup and adjustable via the admin UI's two-step confirm flow.
    /// Not persisted on purpose: a restart resets to the TOML value, so
    /// an emergency reboot always lands on a known-safe configuration.
    pub safety: RwLock<RuntimeSafety>,
    /// True while phase-detection is actively driving batteries — the
    /// dispatcher skips its loop in this case so it doesn't fight the
    /// detection routine.
    pub detection_active: AtomicBool,
    /// Rolling 10-minute responsiveness statistics per battery, used
    /// for passive "stuck" detection. Updated by the dispatcher
    /// whenever it issues a non-trivial command and observes the CT
    /// reaction.
    pub responsiveness: RwLock<HashMap<String, ResponsivenessTracker>>,
    pub started_at: Instant,
}

/// Live safety state. The dispatcher reads `effective_cap_w` on every tick.
#[derive(Debug, Clone)]
pub struct RuntimeSafety {
    pub effective_cap_w: f64,
    pub acknowledged_higher_risk: bool,
    pub acknowledged_separate_fuses: bool,
    /// Where the current cap came from: `"config"` or `"runtime"`.
    pub source: &'static str,
    pub last_changed_at: Option<Instant>,
}

impl RuntimeSafety {
    pub fn from_config(c: &SafetyConfig) -> Self {
        Self {
            effective_cap_w: c.effective_cap_w(),
            acknowledged_higher_risk: c.acknowledged_higher_risk,
            acknowledged_separate_fuses: c.acknowledged_separate_fuses,
            source: "config",
            last_changed_at: None,
        }
    }

    pub fn override_active(&self) -> bool {
        self.effective_cap_w > SAFETY_DEFAULT_CAP_W
    }
}

/// Rolling-window evidence of how well one battery is responding to
/// the dispatcher's commands. We don't have direct telemetry of
/// *what the inverter actually does*, so the heuristic is: whenever
/// we issue a step change in factor, the CT phase that the battery
/// sits on should react proportionally a few seconds later. If many
/// such expected reactions don't materialise over a 10-minute window,
/// the battery is probably saturated (full when charging, empty when
/// discharging — or just hung).
///
/// This struct holds raw observations; the verdict (stuck / fine /
/// unknown) is computed on demand from them.
#[derive(Debug, Clone, Default)]
pub struct ResponsivenessTracker {
    pub battery_id: String,
    /// Append-only ring of recent step events. Pruned to the last
    /// 10 minutes on every insert.
    pub events: std::collections::VecDeque<StepEvent>,
    /// Direction the battery is currently believed to be stuck in,
    /// derived once per minute by the dispatcher.
    pub stuck_direction: Option<StuckDirection>,
    /// When the verdict was last refreshed.
    pub last_verdict_at: Option<Instant>,
}

/// One recorded "we asked for a change of X W and saw a CT swing of Y W"
/// observation. Compared against an expected swing range to score it.
#[derive(Debug, Clone, Copy)]
pub struct StepEvent {
    pub at: Instant,
    /// Direction expected (charging = negative target, discharging = positive).
    pub direction: StuckDirection,
    /// Magnitude of the *commanded* step in W (always positive; the
    /// direction field carries the sign).
    pub commanded_step_w: f64,
    /// Magnitude of the observed CT swing on the assigned phase, also
    /// always positive.
    pub observed_step_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StuckDirection {
    Charging,
    Discharging,
}

#[derive(Debug, Clone, Default)]
pub struct BatteryTelemetry {
    pub battery_id: String,
    /// State of charge in percent. The only piece of telemetry the
    /// dispatcher consumes — used for the min/max SoC eligibility gate.
    pub soc_percent: Option<f64>,
    pub last_update: Option<Instant>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EnergyCounters {
    pub a_consumed_wh: f64,
    pub a_returned_wh: f64,
    pub b_consumed_wh: f64,
    pub b_returned_wh: f64,
    pub c_consumed_wh: f64,
    pub c_returned_wh: f64,
}

impl EnergyCounters {
    /// Integrate one sample. `dt_seconds` is the elapsed time since the
    /// previous integration; energy adds in Wh.
    pub fn integrate(&mut self, snap: &EmStatusIncoming, dt_seconds: f64) {
        let dt_h = dt_seconds / 3600.0;
        for (power, consumed, returned) in [
            (snap.a_act_power, &mut self.a_consumed_wh, &mut self.a_returned_wh),
            (snap.b_act_power, &mut self.b_consumed_wh, &mut self.b_returned_wh),
            (snap.c_act_power, &mut self.c_consumed_wh, &mut self.c_returned_wh),
        ] {
            if let Some(p) = power {
                if p >= 0.0 {
                    *consumed += p * dt_h;
                } else {
                    *returned += -p * dt_h;
                }
            }
        }
    }
}

impl AppState {
    pub fn new(safety: &SafetyConfig) -> Arc<Self> {
        Arc::new(Self {
            snapshot: ArcSwap::from_pointee(EmSnapshot::default()),
            allocations: RwLock::new(HashMap::new()),
            last_poll_at: RwLock::new(HashMap::new()),
            telemetry: RwLock::new(HashMap::new()),
            energy: RwLock::new(EnergyCounters::default()),
            safety: RwLock::new(RuntimeSafety::from_config(safety)),
            detection_active: AtomicBool::new(false),
            responsiveness: RwLock::new(HashMap::new()),
            started_at: Instant::now(),
        })
    }
}
