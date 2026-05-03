//! Passive "stuck" detector. Watches the dispatcher's commands and
//! the real Shelly's CT readings, looking for batteries that don't
//! respond to step changes the way they should. Does NOT touch the
//! allocation directly — it only annotates `state.responsiveness`.
//!
//! Verdict policy (rolling 10-minute window):
//!   * collect at most one StepEvent per ~5 s per battery
//!   * a step is "answered" if observed_step_w >= 0.5 * commanded_step_w
//!   * battery is flagged stuck-in-direction-X if at least 6 step
//!     events for that direction occurred in the window AND fewer than
//!     30 % of them were answered
//!
//! Even after flagging the dispatcher does not change behaviour — the
//! flag is informational for the GUI. Active re-routing follows in a
//! later step once we trust the verdict.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::time;
use tracing::{debug, info};

use crate::config::{Config, PhaseAssignment};
use crate::state::{AppState, ResponsivenessTracker, StepEvent, StuckDirection};

const WINDOW: Duration = Duration::from_secs(600);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
/// A step must be at least this big to count — small dispatcher
/// adjustments aren't useful evidence either way.
const SIGNIFICANT_STEP_W: f64 = 200.0;
/// CT swing as a fraction of the commanded step that counts as "answered".
const ANSWERED_RATIO: f64 = 0.5;
/// At least N events of one direction in the window before we're
/// willing to flag stuck.
const MIN_EVENTS_FOR_VERDICT: usize = 6;
/// Fraction of events that must be UN-answered to call it stuck.
const STUCK_RATIO_THRESHOLD: f64 = 0.7;

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> anyhow::Result<()> {
    info!("stuck-detector started (passive, 10-min rolling)");
    let mut interval = time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    // Per battery: last observed allocated_w (for step detection) and
    // a pending Probe waiting for the CT to react.
    let mut last_alloc: HashMap<String, f64> = HashMap::new();
    let mut pending: HashMap<String, PendingProbe> = HashMap::new();

    loop {
        interval.tick().await;

        // Skip while phase-detection is driving allocations.
        if state.detection_active.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }

        let cfg = config.load_full();
        let snapshot = state.snapshot.load_full();
        if snapshot.age.is_none() {
            continue;
        }
        let phase_snap = (
            snapshot.status.a_act_power.unwrap_or(0.0),
            snapshot.status.b_act_power.unwrap_or(0.0),
            snapshot.status.c_act_power.unwrap_or(0.0),
        );

        let allocs = state.allocations.read().clone();

        // Check for ripe pending probes and collect events.
        let mut new_events: Vec<(String, StepEvent)> = Vec::new();
        let now = Instant::now();
        pending.retain(|battery_id, probe| {
            if now.saturating_duration_since(probe.armed_at) < probe.wait {
                return true;
            }
            // Settle window elapsed — sample the CT change.
            let observed = match probe.phase {
                PhaseAssignment::A => phase_snap.0 - probe.baseline_phase,
                PhaseAssignment::B => phase_snap.1 - probe.baseline_phase,
                PhaseAssignment::C => phase_snap.2 - probe.baseline_phase,
                PhaseAssignment::All => {
                    (phase_snap.0 + phase_snap.1 + phase_snap.2)
                        - probe.baseline_phase
                }
            };
            // For charging the CT should swing UP (battery pulls extra
            // load), for discharging DOWN. We compare absolute change.
            let observed_step_w = observed.abs();
            new_events.push((
                battery_id.clone(),
                StepEvent {
                    at: now,
                    direction: probe.direction,
                    commanded_step_w: probe.commanded_step_w,
                    observed_step_w,
                },
            ));
            false
        });

        // Detect new step changes.
        for battery in &cfg.batteries {
            let cur = allocs.get(&battery.address).map(|a| a.allocated_w).unwrap_or(0.0);
            let prev = last_alloc.get(&battery.id).copied().unwrap_or(0.0);
            let step = cur - prev;
            last_alloc.insert(battery.id.clone(), cur);

            if step.abs() < SIGNIFICANT_STEP_W {
                continue;
            }
            // Don't queue if a probe is already pending — collapse
            // back-to-back steps so we time the *settled* response.
            if pending.contains_key(&battery.id) {
                continue;
            }

            let phase = battery
                .detected_phase
                .as_ref()
                .map(|d| d.phase)
                .unwrap_or(battery.phase);
            let baseline_phase = match phase {
                PhaseAssignment::A => phase_snap.0,
                PhaseAssignment::B => phase_snap.1,
                PhaseAssignment::C => phase_snap.2,
                PhaseAssignment::All => phase_snap.0 + phase_snap.1 + phase_snap.2,
            };
            let direction = if step > 0.0 {
                StuckDirection::Discharging
            } else {
                StuckDirection::Charging
            };
            pending.insert(
                battery.id.clone(),
                PendingProbe {
                    armed_at: now,
                    wait: Duration::from_secs(8), // settle time
                    phase,
                    baseline_phase,
                    direction,
                    commanded_step_w: step.abs(),
                },
            );
        }

        let _ = phase_snap;

        // Apply collected events into the state's responsiveness map.
        if !new_events.is_empty() {
            let mut map = state.responsiveness.write();
            for (battery_id, ev) in new_events {
                let tracker = map.entry(battery_id.clone()).or_insert_with(|| {
                    ResponsivenessTracker {
                        battery_id: battery_id.clone(),
                        events: VecDeque::new(),
                        stuck_direction: None,
                        last_verdict_at: None,
                    }
                });
                debug!(
                    battery = %battery_id,
                    direction = ?ev.direction,
                    commanded = ev.commanded_step_w,
                    observed = ev.observed_step_w,
                    "stuck-detect: probe sample"
                );
                tracker.events.push_back(ev);
            }
        }

        // Prune + re-evaluate verdict every full second.
        let mut map = state.responsiveness.write();
        for tracker in map.values_mut() {
            while tracker
                .events
                .front()
                .map(|e| now.saturating_duration_since(e.at) > WINDOW)
                .unwrap_or(false)
            {
                tracker.events.pop_front();
            }
            tracker.stuck_direction = compute_verdict(&tracker.events);
            tracker.last_verdict_at = Some(now);
        }
    }
}

struct PendingProbe {
    armed_at: Instant,
    wait: Duration,
    phase: PhaseAssignment,
    baseline_phase: f64,
    direction: StuckDirection,
    commanded_step_w: f64,
}

fn compute_verdict(events: &VecDeque<StepEvent>) -> Option<StuckDirection> {
    for direction in [StuckDirection::Charging, StuckDirection::Discharging] {
        let same_dir: Vec<&StepEvent> =
            events.iter().filter(|e| e.direction == direction).collect();
        if same_dir.len() < MIN_EVENTS_FOR_VERDICT {
            continue;
        }
        let answered = same_dir
            .iter()
            .filter(|e| e.observed_step_w >= ANSWERED_RATIO * e.commanded_step_w)
            .count();
        let unanswered_ratio = 1.0 - (answered as f64 / same_dir.len() as f64);
        if unanswered_ratio >= STUCK_RATIO_THRESHOLD {
            return Some(direction);
        }
    }
    None
}
