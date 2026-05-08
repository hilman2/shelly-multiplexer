//! Pulse-based dispatcher.
//!
//! Cycle (default 200 ms):
//!   1. Check plug freshness per battery. If any plug in a circuit is stale,
//!      mute the entire circuit for `group_silent_after_stale_s` (60 s).
//!   2. Read grid_w from real Shelly. Compute desired_w per battery
//!      using capacity- and SoC-weighted distribution under group caps
//!      derived from the plug measurements.
//!   3. Detect saturation (commanded_w not reachable per plug). Reduce
//!      affected battery's commanded_w to its observed ceiling and
//!      redispatch the slack to siblings.
//!   4. For each battery, compute delta = desired - commanded. If
//!      |delta| > deadband AND the previous pulse has settled, queue
//!      `pulse_count` pulses of value `delta` and update commanded_w.
//!
//! Priority: avoiding grid import/export wins over balanced SoC. If a small
//! battery is the only one with charge headroom, it takes the full surplus
//! (up to its hardware max) to keep the grid at zero.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{Config, DispatcherConfig};
use crate::state::{AppState, BatteryState};

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) {
    let cfg0 = config.load_full();
    let cycle = Duration::from_millis(cfg0.dispatcher.cycle_ms.max(50));
    let mut tick = time::interval(cycle);
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    info!(cycle_ms = cfg0.dispatcher.cycle_ms, "pulse dispatcher started");
    loop {
        tick.tick().await;
        let cfg = config.load_full();
        if let Err(e) = step(&state, &cfg.dispatcher) {
            warn!(error = %e, "dispatcher step failed");
        }
    }
}

fn step(state: &AppState, dcfg: &DispatcherConfig) -> anyhow::Result<()> {
    let now = Instant::now();

    // Step 1: plug freshness -> circuit mute decisions.
    update_circuit_mute(state, dcfg, now);

    // Step 2: snapshot grid power. If we never got a real-shelly reading
    // yet, treat as 0 -> batteries should idle.
    let grid_w = {
        let snap = state.snapshot.load_full();
        snap.status.total_act_power.unwrap_or(0.0)
    };

    // Step 3: compute desired_w per battery.
    let desired_per_battery = compute_desired(state, dcfg, grid_w, now);

    // Step 4: queue pulses for whoever is settled and has a delta beyond
    // the deadband. This is also where we update the virtual integrator.
    queue_pulses(state, dcfg, &desired_per_battery, now);

    Ok(())
}

// ---------------------------------------------------------------------------
// Step 1: plug freshness -> circuit mute
// ---------------------------------------------------------------------------

fn update_circuit_mute(state: &AppState, dcfg: &DispatcherConfig, now: Instant) {
    let stale_s = dcfg.plug_stale_s;
    let silence = Duration::from_secs_f64(dcfg.group_silent_after_stale_s);

    let bats = state.batteries.read();
    let mut circuits = state.circuits.write();
    for (cid, cs) in circuits.iter_mut() {
        let any_stale = cs.member_ids.iter().any(|bid| {
            bats.get(bid)
                .map(|b| !b.is_plug_fresh(now, stale_s))
                .unwrap_or(true)
        });
        if any_stale {
            // Reset the silence window to NOW + silence each tick the plug
            // is still stale, so it only clears once a stale-free
            // observation has been made.
            let target = now + silence;
            cs.silent_until = match cs.silent_until {
                Some(prev) if prev > target => Some(prev),
                _ => Some(target),
            };
        } else if let Some(until) = cs.silent_until {
            if until <= now {
                cs.silent_until = None;
                debug!(circuit = %cid, "circuit silence cleared");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 2/3: compute desired_w per battery
// ---------------------------------------------------------------------------

fn compute_desired(
    state: &AppState,
    dcfg: &DispatcherConfig,
    grid_w: f64,
    now: Instant,
) -> std::collections::HashMap<String, f64> {
    use std::collections::HashMap;

    let bats = state.batteries.read();
    let circuits = state.circuits.read();

    // SALDIERENDE control loop:
    // The Shelly grid reading is what's left AFTER the batteries already
    // acted on the grid. So `grid_w` is the *residual* error we need to
    // compensate for, NOT the absolute target. To bring grid to zero,
    // total battery output must change by `grid_w`, on top of where it is.
    //
    // Convention: positive grid_w = importing => batteries should
    // discharge MORE (commanded += positive). Negative grid_w = exporting
    // => charge MORE (commanded += negative).
    //
    // Asymmetric bias: shrink the correction by `grid_bias_w` toward zero
    // and clamp so it never crosses zero. Effect: when discharging we
    // leave a small import margin (cheap insurance against accidentally
    // pushing into export), and when charging we leave a small export
    // margin (no accidental grid draw to top up the battery).
    let raw = grid_w;
    let correction_total = if raw > 0.0 {
        (raw - dcfg.grid_bias_w).max(0.0)
    } else if raw < 0.0 {
        (raw + dcfg.grid_bias_w).min(0.0)
    } else {
        0.0
    };

    // Default: keep every battery exactly where it is.
    let mut desired: HashMap<String, f64> = HashMap::new();
    for b in bats.values() {
        desired.insert(b.id.clone(), b.commanded_w);
    }

    if correction_total.abs() < dcfg.deadband_w {
        return desired;
    }

    let need_more_discharge = correction_total > 0.0;

    let muted_circuit = |cid: &str| -> bool {
        circuits
            .get(cid)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false)
    };

    // Eligibility: skip muted circuits and batteries that physically can't
    // absorb correction in the needed direction.
    let mut eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| {
            if muted_circuit(&b.circuit) {
                return false;
            }
            if need_more_discharge {
                if b.commanded_w >= b.max_discharge_w - 1.0 {
                    return false;
                }
                if let Some(soc) = b.soc_pct {
                    if soc <= dcfg.soc_empty_pct && b.commanded_w <= 0.0 {
                        return false;
                    }
                }
            } else {
                if b.commanded_w <= -b.max_charge_w + 1.0 {
                    return false;
                }
                if let Some(soc) = b.soc_pct {
                    if soc >= dcfg.soc_full_pct && b.commanded_w >= 0.0 {
                        return false;
                    }
                }
            }
            true
        })
        .collect();

    if eligible.is_empty() {
        return desired;
    }

    // Weight by priority × directional headroom (how much room each
    // battery still has to absorb correction in the needed direction).
    let headroom_of = |b: &BatteryState| -> f64 {
        let h = if need_more_discharge {
            b.max_discharge_w - b.commanded_w
        } else {
            b.max_charge_w + b.commanded_w
        };
        h.max(0.0)
    };
    let weight_of = |b: &BatteryState| -> f64 { b.priority_weight * headroom_of(b).max(1.0) };

    // Distribute correction with overflow redistribution: if a battery
    // clamps before absorbing its full share, the leftover goes to
    // siblings with remaining headroom (grid balance > SoC balance).
    let mut remaining = correction_total;
    for _ in 0..6 {
        if remaining.abs() < 1e-3 || eligible.is_empty() {
            break;
        }
        let total_w: f64 = eligible.iter().map(|b| weight_of(b)).sum();
        if total_w <= 0.0 {
            break;
        }
        let mut clamped_ids: Vec<String> = Vec::new();
        for b in &eligible {
            let share = remaining * weight_of(b) / total_w;
            let prev = *desired.get(&b.id).unwrap();
            let proposed = prev + share;
            let mut clamped = proposed.max(-b.max_charge_w).min(b.max_discharge_w);
            if let Some(c) = b.saturation_ceiling_w {
                if need_more_discharge {
                    clamped = clamped.min(c);
                } else {
                    clamped = clamped.max(c);
                }
            }
            desired.insert(b.id.clone(), clamped);
            if (clamped - proposed).abs() > 1e-3 {
                clamped_ids.push(b.id.clone());
            }
        }
        let applied: f64 = bats
            .values()
            .map(|b| desired.get(&b.id).copied().unwrap_or(b.commanded_w) - b.commanded_w)
            .sum();
        remaining = correction_total - applied;
        eligible.retain(|b| !clamped_ids.contains(&b.id));
    }

    // Per-circuit cap enforcement on the desired values. Plug-measured
    // |sum| of all batteries on a circuit must stay below cap × headroom;
    // we use whichever of (desired_sum, measured_sum) is bigger.
    for cs in circuits.values() {
        let members: Vec<&BatteryState> = cs
            .member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .collect();
        if members.is_empty() {
            continue;
        }
        let desired_sum: f64 = members
            .iter()
            .map(|b| desired.get(&b.id).copied().unwrap_or(b.commanded_w))
            .sum();
        let measured_sum: f64 = members.iter().map(|b| b.last_plug_w.unwrap_or(0.0)).sum();
        let cap = cs.cap_w() * dcfg.circuit_headroom;
        let limit = desired_sum.abs().max(measured_sum.abs());
        if limit > cap && limit > 0.0 {
            let scale = cap / limit;
            for b in &members {
                if let Some(d) = desired.get_mut(&b.id) {
                    *d *= scale;
                }
            }
        }
    }

    desired
}

// ---------------------------------------------------------------------------
// Step 4: pulse queueing
// ---------------------------------------------------------------------------

fn queue_pulses(
    state: &AppState,
    dcfg: &DispatcherConfig,
    desired: &std::collections::HashMap<String, f64>,
    now: Instant,
) {
    let mut bats = state.batteries.write();
    let circuits = state.circuits.read();

    for b in bats.values_mut() {
        let circuit_silent = circuits
            .get(&b.circuit)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false);
        if circuit_silent {
            // Drop any pending pulse; reset commanded_w to 0 so when the
            // circuit comes back we start from a known state. The Marstek
            // watchdog will have cleared its integrator during the silence.
            b.pending_pulse_w = 0.0;
            b.pulse_remaining = 0;
            b.commanded_w = 0.0;
            continue;
        }

        update_saturation(b, dcfg, now);

        // Sequencing: only queue a new pulse if the previous has settled.
        if !b.pulse_settled(dcfg.hit_tolerance_w) {
            continue;
        }

        let want = *desired.get(&b.id).unwrap_or(&b.commanded_w);
        let delta = want - b.commanded_w;
        if delta.abs() < dcfg.deadband_w {
            continue;
        }

        let new_commanded = (b.commanded_w + delta)
            .max(-b.max_charge_w)
            .min(b.max_discharge_w);
        let actual_delta = new_commanded - b.commanded_w;
        if actual_delta.abs() < dcfg.deadband_w {
            continue;
        }

        b.commanded_w = new_commanded;
        // Replace any stale value (none should be there given pulse_settled)
        // with the fresh delta and arm the per-poll counter.
        b.pending_pulse_w = actual_delta;
        b.pulse_remaining = dcfg.pulse_count;
        debug!(
            battery = %b.id,
            delta = actual_delta,
            commanded_w = b.commanded_w,
            pulses = dcfg.pulse_count,
            "armed pulse"
        );
    }
}

fn update_saturation(b: &mut BatteryState, dcfg: &DispatcherConfig, now: Instant) {
    let Some(plug) = b.last_plug_w else {
        return;
    };
    let gap = b.commanded_w - plug;
    let same_sign =
        (b.commanded_w >= 0.0 && plug >= 0.0) || (b.commanded_w < 0.0 && plug < 0.0);
    let saturated_now = same_sign && gap.abs() > dcfg.saturation_gap_w;

    if saturated_now {
        match b.saturation_since {
            None => b.saturation_since = Some(now),
            Some(t) => {
                if now.duration_since(t).as_secs_f64() >= dcfg.saturation_window_s
                    && (!b.saturated || b.saturation_ceiling_w.is_none())
                {
                    b.saturated = true;
                    b.saturation_ceiling_w = Some(plug);
                    // Reduce our virtual integrator to physical reality so
                    // the next dispatch round redistributes the slack.
                    if (b.commanded_w - plug).abs() > dcfg.deadband_w {
                        b.commanded_w = plug;
                    }
                    info!(
                        battery = %b.id,
                        ceiling_w = plug,
                        "battery saturated"
                    );
                }
            }
        }
    } else {
        if b.saturated {
            info!(battery = %b.id, "battery saturation cleared");
        }
        b.saturation_since = None;
        b.saturated = false;
        b.saturation_ceiling_w = None;
    }
}
