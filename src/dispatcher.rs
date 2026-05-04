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

    // The desired battery output (sum across all batteries) is whatever
    // would zero out the grid. Positive grid_w (importing) -> we want
    // batteries to discharge a total of +grid_w. Negative -> charge.
    let target_total = grid_w;

    let mut desired: HashMap<String, f64> = HashMap::new();
    for b in bats.values() {
        desired.insert(b.id.clone(), 0.0);
    }

    let charging = target_total < 0.0;
    let discharging = target_total > 0.0;

    let mut eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| {
            // Skip batteries on a muted circuit -- pulses won't be sent
            // to them anyway.
            let circuit_silent = circuits
                .get(&b.circuit)
                .and_then(|c| c.silent_until)
                .map(|t| t > now)
                .unwrap_or(false);
            if circuit_silent {
                return false;
            }
            if charging {
                if b.max_charge_w <= 0.0 {
                    return false;
                }
                if let Some(soc) = b.soc_pct {
                    if soc >= dcfg.soc_full_pct {
                        return false;
                    }
                }
                true
            } else if discharging {
                if b.max_discharge_w <= 0.0 {
                    return false;
                }
                if let Some(soc) = b.soc_pct {
                    if soc <= dcfg.soc_empty_pct {
                        return false;
                    }
                }
                true
            } else {
                true
            }
        })
        .collect();

    if eligible.is_empty() || target_total.abs() < 1e-3 {
        return desired;
    }

    let weight_of = |b: &BatteryState| -> f64 {
        let dir_cap = if charging {
            b.max_charge_w
        } else {
            b.max_discharge_w
        };
        b.priority_weight * dir_cap.max(1.0)
    };

    let total_w: f64 = eligible.iter().map(|b| weight_of(b)).sum();
    let mut shares: HashMap<String, f64> = HashMap::new();
    for b in &eligible {
        let s = target_total * weight_of(b) / total_w;
        shares.insert(b.id.clone(), s);
    }

    // Battery hardware + saturation clamp. Track unallocated remainder
    // and hand it to siblings (grid balance > SoC balance).
    loop {
        let mut overflow = 0.0;
        let mut clamped_ids: Vec<String> = Vec::new();
        for b in &eligible {
            let s = shares.get(&b.id).copied().unwrap_or(0.0);
            let mut limited = if charging {
                s.max(-b.max_charge_w)
            } else {
                s.min(b.max_discharge_w)
            };
            if let Some(c) = b.saturation_ceiling_w {
                if charging {
                    limited = limited.max(c);
                } else {
                    limited = limited.min(c);
                }
            }
            if (limited - s).abs() > 1e-3 {
                overflow += s - limited;
                shares.insert(b.id.clone(), limited);
                clamped_ids.push(b.id.clone());
            }
        }
        if overflow.abs() < 1e-3 {
            break;
        }
        let receivers: Vec<&BatteryState> = eligible
            .iter()
            .copied()
            .filter(|b| !clamped_ids.contains(&b.id))
            .collect();
        if receivers.is_empty() {
            break;
        }
        let recv_total_w: f64 = receivers.iter().map(|b| weight_of(b)).sum();
        if recv_total_w <= 0.0 {
            break;
        }
        for r in &receivers {
            let extra = overflow * weight_of(r) / recv_total_w;
            *shares.entry(r.id.clone()).or_insert(0.0) += extra;
        }
        eligible.retain(|b| !clamped_ids.contains(&b.id));
    }

    // Per-circuit cap from PLUG measurements. Plug-measured |sum| of all
    // batteries on a circuit must stay below cap_w * circuit_headroom.
    for cs in circuits.values() {
        let members: Vec<&BatteryState> = cs
            .member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .collect();
        if members.is_empty() {
            continue;
        }
        let proposed_sum: f64 = members
            .iter()
            .map(|b| shares.get(&b.id).copied().unwrap_or(0.0))
            .sum();
        let measured_sum: f64 = members
            .iter()
            .map(|b| b.last_plug_w.unwrap_or(0.0))
            .sum();
        let cap = cs.cap_w() * dcfg.circuit_headroom;
        let limit = proposed_sum.abs().max(measured_sum.abs());
        if limit > cap && limit > 0.0 {
            let scale = cap / limit;
            for b in &members {
                if let Some(s) = shares.get_mut(&b.id) {
                    *s *= scale;
                }
            }
        }
    }

    for (id, s) in shares {
        desired.insert(id, s);
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
            // Drop any pending pulses; reset commanded_w to 0 so when the
            // circuit comes back we start from a known state. The Marstek
            // watchdog will have cleared its integrator during the silence.
            if !b.pulse_queue.is_empty() {
                b.pulse_queue.clear();
            }
            b.commanded_w = 0.0;
            continue;
        }

        update_saturation(b, dcfg, now);

        // Sequencing: only queue a new pulse if the previous has settled.
        if !b.pulse_settled(dcfg.hit_tolerance_w) {
            continue;
        }

        let want = desired.get(&b.id).copied().unwrap_or(0.0);
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
        for _ in 0..dcfg.pulse_count {
            b.pulse_queue.push_back(actual_delta);
        }
        debug!(
            battery = %b.id,
            delta = actual_delta,
            commanded_w = b.commanded_w,
            pulses = dcfg.pulse_count,
            "queued pulse"
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
