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

    // SoC-aware soft bounds on commanded_w. An empty battery must never
    // be DRIVEN into discharge (commanded > 0); a full one must never be
    // driven into charge (commanded < 0). But it MUST be possible to
    // unwind a previous command in the opposite direction (e.g. roll a
    // -81 W charge command back to 0 when SoC is now 1 %).
    let high_bound = |b: &BatteryState| -> f64 {
        let empty = b.effective_soc_empty_pct(dcfg.soc_empty_pct);
        match b.soc_pct {
            Some(soc) if soc <= empty => 0.0,
            _ => b.max_discharge_w,
        }
    };
    let low_bound = |b: &BatteryState| -> f64 {
        let full = b.effective_soc_full_pct(dcfg.soc_full_pct);
        match b.soc_pct {
            Some(soc) if soc >= full => 0.0,
            _ => -b.max_charge_w,
        }
    };

    // Eligibility: skip only muted circuits and batteries with literally
    // zero room to move in the needed direction. SoC limits no longer
    // exclude here — they shape the per-battery clamp below.
    let mut eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| {
            if muted_circuit(&b.circuit) {
                return false;
            }
            if need_more_discharge {
                let h = high_bound(b);
                if b.commanded_w >= h - 1.0 {
                    return false;
                }
            } else {
                let l = low_bound(b);
                if b.commanded_w <= l + 1.0 {
                    return false;
                }
            }
            true
        })
        .collect();

    if eligible.is_empty() {
        return desired;
    }

    // Headroom = how much further each battery's commanded can move in
    // the needed direction, respecting both hardware caps AND the
    // SoC-aware soft bounds.
    let headroom_of = |b: &BatteryState| -> f64 {
        let h = if need_more_discharge {
            high_bound(b) - b.commanded_w
        } else {
            b.commanded_w - low_bound(b)
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
            // Hardware clamp first.
            let mut clamped = proposed.max(-b.max_charge_w).min(b.max_discharge_w);
            // SoC-aware soft bounds. If we're outside the bounds (e.g.
            // commanded was already negative when SoC dropped below
            // empty) we ALLOW the proposed value if it moves toward 0;
            // we just don't permit going PAST the bound away from 0.
            let hb = high_bound(b);
            let lb = low_bound(b);
            if clamped > hb && hb >= prev {
                clamped = hb.max(prev);
            }
            if clamped < lb && lb <= prev {
                clamped = lb.min(prev);
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

        resync_if_drifted(b, dcfg, now);

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

/// Resync our virtual integrator to the plug reading when the gap stays
/// large for too long. This is NOT real "saturation" detection — true
/// BMS saturation only happens at the SoC extremes (typically >98 %
/// charging, <floor discharging) and is already handled by the SoC-aware
/// soft bounds in compute_desired. What this function catches is
/// communication drift: lost UDP pulses, the HACS Marstek plugin sending
/// its own CT signal that overrides ours, or transient BMS refusals.
/// In any of those cases the right move is to trust the plug as ground
/// truth, set commanded := plug, and let the next dispatch tick queue
/// a fresh corrective pulse from there.
fn resync_if_drifted(b: &mut BatteryState, dcfg: &DispatcherConfig, now: Instant) {
    let Some(plug) = b.last_plug_w else {
        return;
    };
    let gap = b.commanded_w - plug;
    if gap.abs() <= dcfg.saturation_gap_w {
        b.saturation_since = None;
        b.saturated = false;
        b.saturation_ceiling_w = None;
        return;
    }
    match b.saturation_since {
        None => b.saturation_since = Some(now),
        Some(t) => {
            if now.duration_since(t).as_secs_f64() >= dcfg.saturation_window_s {
                info!(
                    battery = %b.id,
                    commanded_w = b.commanded_w,
                    plug_w = plug,
                    "model/reality drift — resyncing commanded to plug"
                );
                b.commanded_w = plug;
                b.saturation_since = None;
                b.saturated = false;
                b.saturation_ceiling_w = None;
            }
        }
    }
}
