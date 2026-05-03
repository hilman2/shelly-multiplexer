//! Multiplex dispatcher. NO saldierung, NO PI loop, NO output
//! estimation — those are all cans of worms with no telemetry.
//!
//! Per circuit we pick exactly one "lead" battery. Every lead gets
//! the **raw** CT readings from the real Shelly, divided by the
//! number of active leads across all circuits. The inverter's own
//! CT-based control loop does the saldierung. Non-lead batteries
//! receive no responses from the virtual Shelly so the inverter's
//! watchdog shuts them off.
//!
//! Sicherheit: the per-circuit cap is enforced by `Config::validate`
//! refusing any battery whose `max(charge_w, discharge_w)` exceeds
//! its circuit's `cap_w`. Combined with "max one lead per circuit",
//! the shared protective device can never be overloaded.
//!
//! Lead selection (per circuit):
//!   1. Honour `force_inactive_until` (Test-Deactivate button) —
//!      skip the marked battery.
//!   2. Filter by SoC eligibility for the *current* direction hint
//!      (high SoC required if recently discharging, low SoC if
//!      charging). Stale or missing SoC = pass-through (don't gate).
//!   3. Sort eligible candidates by direction-aware SoC (best first).
//!   4. Keep the existing lead unless: it became ineligible, OR a
//!      candidate beats it by >5 % SoC and at least 60 s passed
//!      since the last switch.
//!
//! On a switch: the previous lead's allocation is set to inactive
//! (no virtual_shelly responses). The new lead becomes active on the
//! same tick. The previous inverter takes ~30–60 s to actually shut
//! down via its CT-watchdog — during that window two batteries in
//! the same circuit might briefly run together, but each on its own
//! is bounded by its hardware cap which is < circuit cap. No
//! overload possible.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::time;
use tracing::{debug, info};

use crate::config::{BatteryConfig, Config};
use crate::rpc::EmStatusIncoming;
use crate::state::{
    Allocation, AppState, BatteryTelemetry, CircuitState, DirectionHint, PhaseFactors,
    PhaseWatts,
};

const ALLOCATION_TICK_MS: u64 = 200;
/// SoC eligibility uses a reading at most this old. Older = ignore
/// (fail open — let the inverter's own protections handle it).
const SOC_MAX_AGE: Duration = Duration::from_secs(600);
/// Don't switch the lead more often than this. Round-robin needs to
/// happen on a slow timescale to give the previous inverter time to
/// actually shut down.
const MIN_LEAD_HOLD: Duration = Duration::from_secs(60);
/// Settle time between lead deactivation and new lead activation in
/// the same circuit. The previous battery's CT-watchdog needs this
/// long to actually shut it off; activating a new lead before then
/// would briefly run two batteries in the same circuit and could
/// overload its protective device.
const LEAD_SWITCH_SETTLE: Duration = Duration::from_secs(30);
/// SoC margin a candidate must beat the current lead by before we
/// switch (when the current lead has reached an extreme SoC and we
/// want to hand over to a battery with more headroom in both
/// directions).
const SOC_SWITCH_MARGIN_PCT: f64 = 5.0;
/// Lead is considered "near full" when its SoC is within this many
/// percent of `max_soc_percent`, and "near empty" within this many
/// percent of `min_soc_percent`. At those points we'd prefer a
/// battery with more headroom in both directions.
const SOC_HEADROOM_PCT: f64 = 5.0;
/// Periodic forced rotation interval — even if no SoC trigger fires,
/// rotate the lead this often so members wear evenly over time.
const PERIODIC_ROTATION: Duration = Duration::from_secs(4 * 3600);
/// Minimum SoC delta over the lookback window to count as a real
/// charge/discharge signal rather than measurement noise. Below this
/// the inferred direction is "Idle".
const SOC_DELTA_THRESHOLD_PCT: f64 = 0.5;
/// We need previous SoC at least this old before we trust the delta.
/// Faster than this and we're seeing noise / quantisation.
const SOC_DELTA_MIN_AGE: Duration = Duration::from_secs(60);

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) {
    let mut interval = time::interval(Duration::from_millis(ALLOCATION_TICK_MS));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    {
        let cfg = config.load();
        info!(
            circuits = cfg.circuits.len(),
            batteries = cfg.batteries.len(),
            "dispatcher started (multiplex; max 1 lead per circuit)"
        );
    }

    loop {
        interval.tick().await;
        if state.detection_active.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let cfg = config.load_full();
        let snapshot = state.snapshot.load_full();
        if snapshot.age.is_none() {
            continue;
        }
        recompute(&state, &cfg, &snapshot.status);
    }
}

fn recompute(state: &AppState, config: &Config, snap: &EmStatusIncoming) {
    let now = Instant::now();
    let real_a = snap.a_act_power.unwrap_or(0.0);
    let real_b = snap.b_act_power.unwrap_or(0.0);
    let real_c = snap.c_act_power.unwrap_or(0.0);
    let real_total = real_a + real_b + real_c;

    let telemetry: HashMap<String, BatteryTelemetry> = state.telemetry.read().clone();
    let force_inactive: HashMap<String, Instant> = state.force_inactive_until.read().clone();
    let prev_circuit_state_for_dir = state.circuits.read().clone();

    // Direction inferred per circuit from ΔSoC of its current lead.
    // SoC rising → battery is charging; falling → discharging. We
    // trust this far more than the CT signal, which is bent by the
    // battery's own action.
    let infer_direction = |circuit_id: &str| -> DirectionHint {
        let prev = prev_circuit_state_for_dir.get(circuit_id);
        let lead_id = prev.and_then(|p| p.current_lead.as_ref());
        let Some(lead_id) = lead_id else {
            return DirectionHint::Idle;
        };
        let Some(t) = telemetry.get(lead_id) else {
            return DirectionHint::Idle;
        };
        let (Some(now_soc), Some(prev_soc), Some(prev_at)) =
            (t.soc_percent, t.previous_soc_percent, t.previous_soc_at)
        else {
            return DirectionHint::Idle;
        };
        if now.saturating_duration_since(prev_at) < SOC_DELTA_MIN_AGE {
            return prev.map(|p| p.direction_hint).unwrap_or_default();
        }
        let delta = now_soc - prev_soc;
        if delta > SOC_DELTA_THRESHOLD_PCT {
            DirectionHint::Charging
        } else if delta < -SOC_DELTA_THRESHOLD_PCT {
            DirectionHint::Discharging
        } else {
            DirectionHint::Idle
        }
    };

    // Group batteries by circuit and pick one lead per circuit.
    let mut by_circuit: HashMap<&str, Vec<&BatteryConfig>> = HashMap::new();
    for b in &config.batteries {
        by_circuit.entry(b.circuit.as_str()).or_default().push(b);
    }

    let mut new_circuit_state: HashMap<String, CircuitState> = HashMap::new();
    let mut active_leads: Vec<&BatteryConfig> = Vec::new();
    let prev_circuit_state = prev_circuit_state_for_dir.clone();

    for circuit in &config.circuits {
        let members = by_circuit.get(circuit.id.as_str()).cloned().unwrap_or_default();
        let prev = prev_circuit_state.get(&circuit.id).cloned().unwrap_or_default();
        let direction = infer_direction(&circuit.id);

        // Are we still settling from a previous switch? If so, no
        // active lead this tick; we just decay the transition timer.
        let in_transition = prev
            .transition_until
            .map(|until| now < until)
            .unwrap_or(false);

        let (new_lead, new_transition_until) = if in_transition {
            (None, prev.transition_until)
        } else {
            let candidate = pick_lead(
                &members,
                &telemetry,
                &force_inactive,
                direction,
                now,
                &prev,
            );
            // If the candidate differs from the previous lead AND
            // the previous lead actually existed, we need a settle
            // window so the previous inverter can shut off via its
            // own CT-watchdog before the new one starts.
            match (&prev.current_lead, candidate.map(|b| b.id.as_str())) {
                (Some(prev_id), Some(new_id)) if prev_id != new_id => {
                    (None, Some(now + LEAD_SWITCH_SETTLE))
                }
                _ => (candidate, None),
            }
        };

        if let Some(lead_battery) = new_lead {
            active_leads.push(lead_battery);
        }

        let last_switch_at = match (&prev.current_lead, new_lead.map(|b| b.id.as_str())) {
            (Some(prev_id), Some(new_id)) if prev_id == new_id => prev.last_switch_at,
            _ => Some(now),
        };
        new_circuit_state.insert(
            circuit.id.clone(),
            CircuitState {
                current_lead: new_lead.map(|b| b.id.clone()),
                last_switch_at,
                direction_hint: direction,
                transition_until: new_transition_until,
            },
        );
    }

    // Capacity-weighted distribution: each active lead's share of the
    // CT signal is proportional to its hardware cap in the current
    // direction. Asymmetric caps (Marstek: max_charge=2500, max_discharge=800)
    // are honoured automatically.
    let weights: HashMap<&str, f64> = active_leads
        .iter()
        .map(|b| {
            let w = if real_total >= 0.0 {
                b.max_discharge_w
            } else {
                b.max_charge_w
            };
            (b.id.as_str(), w.max(0.0))
        })
        .collect();
    let total_weight: f64 = weights.values().sum();

    // Build allocations for *every* configured battery, marking the
    // ones that aren't the lead of their circuit as multiplex-inactive.
    let mut next: HashMap<IpAddr, Allocation> = HashMap::new();
    for battery in &config.batteries {
        let is_lead = active_leads.iter().any(|b| b.id == battery.id);
        let factor = if is_lead && total_weight > 0.0 {
            weights.get(battery.id.as_str()).copied().unwrap_or(0.0) / total_weight
        } else {
            0.0
        };
        let phase_a = real_a * factor;
        let phase_b = real_b * factor;
        let phase_c = real_c * factor;
        let allocated_w = phase_a + phase_b + phase_c;

        let note = build_note(is_lead, battery, &telemetry, &force_inactive, now);

        next.insert(
            battery.address,
            Allocation {
                battery_id: battery.id.clone(),
                factors: PhaseFactors { a: factor, b: factor, c: factor },
                phase_w: PhaseWatts { a: phase_a, b: phase_b, c: phase_c },
                allocated_w,
                magnitude_w: phase_a.abs() + phase_b.abs() + phase_c.abs(),
                circuit: battery.circuit.clone(),
                multiplex_inactive: !is_lead,
                note,
            },
        );
    }

    state.energy.write().integrate(snap, ALLOCATION_TICK_MS as f64 / 1000.0);

    debug!(
        real_total,
        n_active = active_leads.len(),
        total_weight,
        "dispatcher tick"
    );
    for lead in &active_leads {
        debug!(circuit = %lead.circuit, lead = %lead.id, "circuit lead");
    }

    *state.circuits.write() = new_circuit_state;
    *state.allocations.write() = next;
}

fn pick_lead<'a>(
    members: &[&'a BatteryConfig],
    telemetry: &HashMap<String, BatteryTelemetry>,
    force_inactive: &HashMap<String, Instant>,
    direction: DirectionHint,
    now: Instant,
    prev_state: &CircuitState,
) -> Option<&'a BatteryConfig> {
    let candidates: Vec<&&BatteryConfig> = members
        .iter()
        .filter(|b| {
            // Test-deactivate honours.
            if let Some(until) = force_inactive.get(&b.id)
                && now < *until
            {
                return false;
            }
            // SoC eligibility for the inferred direction. Direction
            // came from observing this circuit's lead's SoC delta —
            // it's the truth, not an assumption from the noisy CT.
            is_eligible(b, telemetry.get(&b.id), direction)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let soc_of = |id: &str| {
        telemetry
            .get(id)
            .and_then(|t| t.soc_percent)
            .unwrap_or(50.0)
    };

    // Try to keep the previous lead unless we have a reason to switch.
    let prev_lead = prev_state
        .current_lead
        .as_ref()
        .and_then(|id| candidates.iter().find(|b| &b.id == id).copied().copied());

    let Some(prev_lead) = prev_lead else {
        // No previous lead (first tick / lead vanished). Pick by
        // direction if known, otherwise by SoC closest to 50% so we
        // have headroom in either direction.
        let mut sorted = candidates.clone();
        match direction {
            DirectionHint::Discharging => {
                sorted.sort_by(|a, b| {
                    soc_of(&b.id)
                        .partial_cmp(&soc_of(&a.id))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.priority.cmp(&b.priority))
                });
            }
            DirectionHint::Charging => {
                sorted.sort_by(|a, b| {
                    soc_of(&a.id)
                        .partial_cmp(&soc_of(&b.id))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.priority.cmp(&b.priority))
                });
            }
            DirectionHint::Idle => {
                sorted.sort_by(|a, b| {
                    let da = (soc_of(&a.id) - 50.0).abs();
                    let db = (soc_of(&b.id) - 50.0).abs();
                    da.partial_cmp(&db)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.priority.cmp(&b.priority))
                });
            }
        }
        return Some(*sorted[0]);
    };

    // Min-lock between switches keeps round-robin from oscillating.
    if let Some(last_switch) = prev_state.last_switch_at
        && now.saturating_duration_since(last_switch) < MIN_LEAD_HOLD
    {
        return Some(prev_lead);
    }

    let prev_soc = soc_of(&prev_lead.id);

    // SoC-extreme triggered switch: if we're on the wrong side of
    // headroom for the current direction, hand over to a member with
    // more room.
    match direction {
        DirectionHint::Discharging if prev_soc < prev_lead.min_soc_percent + SOC_HEADROOM_PCT => {
            // Need a battery with more discharge headroom.
            let alt = candidates
                .iter()
                .filter(|b| b.id != prev_lead.id)
                .max_by(|a, b| {
                    soc_of(&a.id)
                        .partial_cmp(&soc_of(&b.id))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            if let Some(alt) = alt
                && soc_of(&alt.id) - prev_soc > SOC_SWITCH_MARGIN_PCT
            {
                return Some(**alt);
            }
        }
        DirectionHint::Charging if prev_soc > prev_lead.max_soc_percent - SOC_HEADROOM_PCT => {
            let alt = candidates
                .iter()
                .filter(|b| b.id != prev_lead.id)
                .min_by(|a, b| {
                    soc_of(&a.id)
                        .partial_cmp(&soc_of(&b.id))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            if let Some(alt) = alt
                && prev_soc - soc_of(&alt.id) > SOC_SWITCH_MARGIN_PCT
            {
                return Some(**alt);
            }
        }
        _ => {}
    }

    // Periodic forced rotation for fairness.
    if let Some(last_switch) = prev_state.last_switch_at
        && now.saturating_duration_since(last_switch) > PERIODIC_ROTATION
    {
        let mut sorted: Vec<&&BatteryConfig> = candidates.clone();
        sorted.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
        if let Some(idx) = sorted.iter().position(|b| b.id == prev_lead.id) {
            let next_idx = (idx + 1) % sorted.len();
            if next_idx != idx {
                return Some(*sorted[next_idx]);
            }
        }
    }

    Some(prev_lead)
}

fn is_eligible(
    b: &BatteryConfig,
    t: Option<&BatteryTelemetry>,
    direction: DirectionHint,
) -> bool {
    let Some(t) = t else { return true };
    let Some(soc) = t.soc_percent else { return true };
    let Some(last) = t.last_update else { return true };
    if Instant::now().saturating_duration_since(last) > SOC_MAX_AGE {
        return true;
    }
    match direction {
        DirectionHint::Charging => soc < b.max_soc_percent,
        DirectionHint::Discharging => soc > b.min_soc_percent,
        // Without a confirmed direction we don't gate — the inverter's
        // own protections refuse the impossible direction anyway.
        DirectionHint::Idle => true,
    }
}

fn build_note(
    is_lead: bool,
    battery: &BatteryConfig,
    telemetry: &HashMap<String, BatteryTelemetry>,
    force_inactive: &HashMap<String, Instant>,
    now: Instant,
) -> Option<String> {
    if let Some(until) = force_inactive.get(&battery.id)
        && now < *until
    {
        let remaining = until.saturating_duration_since(now).as_secs();
        return Some(format!("test-deactivated for {remaining}s more"));
    }
    if !is_lead {
        return Some("multiplex: standby".into());
    }
    if let Some(t) = telemetry.get(&battery.id)
        && let Some(soc) = t.soc_percent
    {
        if soc >= battery.max_soc_percent {
            return Some(format!("full (SoC {soc:.0}%)"));
        }
        if soc <= battery.min_soc_percent {
            return Some(format!("empty (SoC {soc:.0}%)"));
        }
    }
    None
}
