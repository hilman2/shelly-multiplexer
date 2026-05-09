//! Pulse-based dispatcher (v0.3.0+ delta-only architecture).
//!
//! Every cycle:
//!   1. Update circuit-mute state from plug freshness.
//!   2. Read grid_w from the real Shelly. Apply asymmetric grid_bias_w
//!      to compute a correction_total.
//!   3. Distribute correction_total across batteries by available
//!      headroom (= plug_w against SoC-aware soft bounds), with overflow
//!      redistribution if any battery clamps.
//!   4. Per-circuit cap enforcement on (plug_w + delta) sum.
//!   5. For each battery whose previous pulse has settled (plug moved
//!      or settle_timeout_s elapsed): queue a fresh pulse_count-long
//!      CT burst with the computed delta. Snapshot plug_w_at_pulse_send.
//!
//! There is no virtual integrator, no "commanded" we maintain; the plug
//! is the only ground truth. Saturation falls out for free: when a
//! battery is at hardware/SoC limit, headroom = 0 → 0 weight → 0 share
//! of new correction → siblings absorb the unmet residual.

use std::collections::HashMap;
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
    info!(cycle_ms = cfg0.dispatcher.cycle_ms, "pulse dispatcher started (v0.3 delta-only)");
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
    update_circuit_mute(state, dcfg, now);
    let grid_w = {
        let snap = state.snapshot.load_full();
        snap.status.total_act_power.unwrap_or(0.0)
    };
    let deltas = compute_deltas(state, dcfg, grid_w, now);
    queue_pulses(state, dcfg, &deltas, now);
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
// SoC-aware bounds (live, from current plug + soc)
// ---------------------------------------------------------------------------

fn high_bound(b: &BatteryState, dcfg: &DispatcherConfig) -> f64 {
    let empty = b.effective_soc_empty_pct(dcfg.soc_empty_pct);
    match b.soc_pct {
        Some(soc) if soc <= empty => 0.0,
        _ => b.max_discharge_w,
    }
}

fn low_bound(b: &BatteryState, dcfg: &DispatcherConfig) -> f64 {
    let full = b.effective_soc_full_pct(dcfg.soc_full_pct);
    match b.soc_pct {
        Some(soc) if soc >= full => 0.0,
        _ => -b.max_charge_w,
    }
}

/// Headroom = how much further we can push this battery's plug_w in the
/// requested direction, respecting hardware caps + SoC soft bounds.
fn headroom(b: &BatteryState, dcfg: &DispatcherConfig, need_more_discharge: bool) -> f64 {
    let plug = b.last_plug_w.unwrap_or(0.0);
    let h = if need_more_discharge {
        high_bound(b, dcfg) - plug
    } else {
        plug - low_bound(b, dcfg)
    };
    h.max(0.0)
}

// ---------------------------------------------------------------------------
// Step 2/3: distribute correction across batteries
// ---------------------------------------------------------------------------

fn compute_deltas(
    state: &AppState,
    dcfg: &DispatcherConfig,
    grid_w: f64,
    now: Instant,
) -> HashMap<String, f64> {
    let bats = state.batteries.read();
    let circuits = state.circuits.read();

    let raw = grid_w;
    let correction_total = if raw > 0.0 {
        (raw - dcfg.grid_bias_w).max(0.0)
    } else if raw < 0.0 {
        (raw + dcfg.grid_bias_w).min(0.0)
    } else {
        0.0
    };

    let mut deltas: HashMap<String, f64> = HashMap::new();
    for b in bats.values() {
        deltas.insert(b.id.clone(), 0.0);
    }
    if correction_total.abs() < dcfg.deadband_w {
        return deltas;
    }

    let need_more_discharge = correction_total > 0.0;

    let muted_circuit = |cid: &str| -> bool {
        circuits
            .get(cid)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false)
    };

    let mut eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| {
            if muted_circuit(&b.circuit) {
                return false;
            }
            headroom(b, dcfg, need_more_discharge) > 0.0
        })
        .collect();

    if eligible.is_empty() {
        return deltas;
    }

    let weight_of = |b: &BatteryState| -> f64 {
        b.priority_weight * headroom(b, dcfg, need_more_discharge).max(1.0)
    };

    // Distribute correction with overflow redistribution.
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
            let prev = *deltas.get(&b.id).unwrap();
            let proposed = prev + share;
            // Clamp to remaining headroom (live).
            let h = headroom(b, dcfg, need_more_discharge);
            let clamped = if need_more_discharge {
                proposed.min(h).max(0.0)
            } else {
                proposed.max(-h).min(0.0)
            };
            deltas.insert(b.id.clone(), clamped);
            if (clamped - proposed).abs() > 1e-3 {
                clamped_ids.push(b.id.clone());
            }
        }
        let applied: f64 = bats
            .values()
            .map(|b| deltas.get(&b.id).copied().unwrap_or(0.0))
            .sum();
        remaining = correction_total - applied;
        eligible.retain(|b| !clamped_ids.contains(&b.id));
    }

    // Per-circuit cap on (plug_w + delta) sum. Plug-measured |sum| of all
    // batteries on a circuit, plus any new delta we'd add, must stay
    // below cap × headroom. If exceeded, scale the deltas (only) so the
    // post-pulse sum lands at the cap.
    for cs in circuits.values() {
        let members: Vec<&BatteryState> = cs
            .member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .collect();
        if members.is_empty() {
            continue;
        }
        let measured_sum: f64 = members.iter().map(|b| b.last_plug_w.unwrap_or(0.0)).sum();
        let delta_sum: f64 = members
            .iter()
            .map(|b| deltas.get(&b.id).copied().unwrap_or(0.0))
            .sum();
        let post = measured_sum + delta_sum;
        let cap = cs.cap_w() * dcfg.circuit_headroom;
        if post.abs() > cap && delta_sum.abs() > 1e-3 {
            let target_post = cap.copysign(post);
            let target_delta_sum = target_post - measured_sum;
            let scale = target_delta_sum / delta_sum;
            for b in &members {
                if let Some(d) = deltas.get_mut(&b.id) {
                    *d *= scale;
                }
            }
        }
    }

    deltas
}

// ---------------------------------------------------------------------------
// Step 4: queue pulses
// ---------------------------------------------------------------------------

fn queue_pulses(
    state: &AppState,
    dcfg: &DispatcherConfig,
    deltas: &HashMap<String, f64>,
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
            // Drop any pending pulse — circuit is muted, Marstek's watchdog
            // will clear its target during the silence.
            b.pending_pulse_w = 0.0;
            b.pulse_remaining = 0;
            b.plug_w_at_pulse_send = None;
            continue;
        }

        // Sequencing: only queue a new pulse once the previous has settled.
        // Settled = plug moved by >= deadband_w from snapshot, OR
        // settle_timeout_s elapsed since the cycle ended, OR no prior pulse.
        if !b.pulse_settled(dcfg.deadband_w, dcfg.settle_timeout_s) {
            continue;
        }

        let delta = deltas.get(&b.id).copied().unwrap_or(0.0);
        if delta.abs() < dcfg.deadband_w {
            continue;
        }

        b.pending_pulse_w = delta;
        b.pulse_remaining = dcfg.pulse_count;
        b.plug_w_at_pulse_send = b.last_plug_w;
        b.last_pulse_completed_at = None;
        debug!(
            battery = %b.id,
            delta,
            plug_at_send = ?b.plug_w_at_pulse_send,
            pulses = dcfg.pulse_count,
            "armed pulse"
        );
    }
}
