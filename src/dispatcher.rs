//! Pulse-based dispatcher (v0.3.0+ delta-only architecture).
//!
//! Every cycle:
//!   1. Update circuit-mute state from plug freshness AND grid freshness.
//!      If we can't trust either side, every circuit goes silent until
//!      both recover — Marstek watchdogs then clear their integrators.
//!   2. Read grid_w from the real Shelly snapshot. Apply asymmetric
//!      grid_bias_w to compute a correction_total.
//!   3. Distribute correction_total across batteries by available
//!      headroom (= plug_w against SoC-aware soft bounds), with overflow
//!      redistribution if any battery clamps.
//!   4. Per-circuit cap enforcement on (plug_w + delta) sum. Scale is
//!      always clamped to [0, 1] — we never flip the requested direction
//!      to "fix" an over-cap state, because that would oscillate against
//!      the dispatch direction. If the circuit is already over cap, all
//!      same-direction deltas drop to 0.
//!   5. For each battery whose previous pulse has settled (plug moved
//!      by hit_tolerance_w or settle_timeout_s elapsed): queue a fresh
//!      pulse_count-long CT burst with the computed delta.
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
    let grid_fresh = update_circuit_mute(state, dcfg, now);
    if !grid_fresh {
        // No usable grid measurement → leave deltas at zero, circuits are
        // muted by update_circuit_mute, virtual_shelly will drop responses
        // and Marstek watchdogs clear the integrator.
        return Ok(());
    }
    let grid_w = {
        let snap = state.snapshot.load_full();
        snap.status.total_act_power.unwrap_or(0.0)
    };
    let deltas = compute_deltas(state, dcfg, grid_w, now);
    queue_pulses(state, dcfg, &deltas, now);
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 1: plug/grid freshness -> circuit mute. Returns whether grid_w
// is fresh enough to act on; if false, the caller should skip the cycle
// (every circuit is already muted).
// ---------------------------------------------------------------------------

fn update_circuit_mute(state: &AppState, dcfg: &DispatcherConfig, now: Instant) -> bool {
    let plug_stale_s = dcfg.plug_stale_s;
    let grid_stale_s = dcfg.grid_stale_s;
    let silence = Duration::from_secs_f64(dcfg.group_silent_after_stale_s);

    let grid_fresh = match state.snapshot.load_full().age {
        Some(t) => now.duration_since(t).as_secs_f64() <= grid_stale_s,
        None => false,
    };
    if !grid_fresh {
        warn_throttled_grid_stale(state, now);
    }

    let bats = state.batteries.read();
    let mut circuits = state.circuits.write();
    for (cid, cs) in circuits.iter_mut() {
        let any_plug_stale = cs.member_ids.iter().any(|bid| {
            bats.get(bid)
                .map(|b| !b.is_plug_fresh(now, plug_stale_s))
                .unwrap_or(true)
        });
        let needs_silence = any_plug_stale || !grid_fresh;
        if needs_silence {
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
    grid_fresh
}

fn warn_throttled_grid_stale(state: &AppState, now: Instant) {
    // Throttle the warning to once every ~30 s so we don't spam the log
    // when the real Shelly is offline for hours.
    let mut last = state.last_grid_stale_warn.lock();
    let say = match *last {
        Some(t) => now.duration_since(t) >= Duration::from_secs(30),
        None => true,
    };
    if say {
        let age_s = state
            .snapshot
            .load_full()
            .age
            .map(|t| now.duration_since(t).as_secs_f64());
        warn!(
            grid_age_s = ?age_s,
            "grid measurement stale — muting all circuits, dispatcher idle"
        );
        *last = Some(now);
    }
}

// ---------------------------------------------------------------------------
// SoC-aware bounds (live, from current plug + soc)
// ---------------------------------------------------------------------------

fn high_bound(b: &BatteryState, dcfg: &DispatcherConfig) -> f64 {
    let empty = b.effective_soc_empty_pct(dcfg.soc_empty_pct);
    match b.soc_pct {
        Some(soc) if soc <= empty => 0.0,
        // Above the empty cutoff: cap at the SoC-aware effective max
        // discharge. The taper kicks in below `discharge_taper_soc_pct`
        // (still above the empty cutoff) and reduces the effective cap
        // before the BMS does — keeps `headroom()` honest, prevents
        // integrator overcommit when the battery can't sustain full output.
        _ => b.effective_max_discharge_w(),
    }
}

fn low_bound(b: &BatteryState, dcfg: &DispatcherConfig) -> f64 {
    let full = b.effective_soc_full_pct(dcfg.soc_full_pct);
    match b.soc_pct {
        Some(soc) if soc >= full => 0.0,
        // Below the full cutoff: cap at the SoC-aware effective max
        // charge. Same reasoning as `high_bound` — taper near 100 %
        // SoC so we don't try to push more in than the BMS will accept.
        _ => -b.effective_max_charge_w(),
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

    // weight = priority × headroom. headroom is already > 0 by construction
    // above, so no .max(1.0) safety dance is needed.
    let weight_of = |b: &BatteryState| -> f64 {
        b.priority_weight * headroom(b, dcfg, need_more_discharge)
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
    // below cap × headroom.
    //
    // Scale is clamped to [0, 1]:
    //   - 1.0 → no scaling, deltas pass through.
    //   - 0.0..1.0 → shrink toward zero so post lands on cap.
    //   - 0.0 → fully suppress this circuit's deltas (already at/over cap).
    //
    // We deliberately do NOT flip signs to "fix" an already-over-cap
    // measured_sum: that would emit reverse-direction pulses against the
    // grid-balance intent and oscillate. Cap protection is one-way — it
    // only prevents making things worse. If a circuit is genuinely
    // over-cap, the operator must downsize loads; the dispatcher will
    // refuse to push further but won't fight the user's setpoint.
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
            let raw_scale = target_delta_sum / delta_sum;
            let scale = raw_scale.clamp(0.0, 1.0);
            warn!(
                circuit = %cs.config.id,
                cap_w = cap,
                measured_sum,
                delta_sum,
                raw_scale,
                applied_scale = scale,
                "circuit cap engaged — scaling deltas"
            );
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
        // Settled = plug moved by ≥ hit_tolerance_w from snapshot, OR
        // settle_timeout_s elapsed since the cycle ended, OR no prior pulse.
        // hit_tolerance_w is the noise floor; deadband_w is the minimum
        // pulse magnitude — so a real-pulse landing always crosses
        // hit_tolerance_w.
        if !b.pulse_settled(dcfg.hit_tolerance_w, dcfg.settle_timeout_s) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BatteryState;

    fn dcfg() -> DispatcherConfig {
        DispatcherConfig::default()
    }

    fn make_battery(id: &str, plug_w: f64, max_charge: f64, max_discharge: f64) -> BatteryState {
        BatteryState {
            id: id.into(),
            circuit: "c1".into(),
            address: format!("127.0.0.{}", id.bytes().last().unwrap_or(1))
                .parse()
                .unwrap(),
            max_charge_w: max_charge,
            max_discharge_w: max_discharge,
            capacity_wh: max_charge + max_discharge,
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
            last_plug_w: Some(plug_w),
            last_plug_at: Some(Instant::now()),
            last_marstek_poll_at: None,
            soc_pct: None,
            soc_at: None,
            soc_source: None,
            last_error: None,
        }
    }

    #[test]
    fn headroom_discharge_uses_distance_to_max_discharge() {
        let cfg = dcfg();
        // Currently discharging at 200 W, max 800 W → 600 W of headroom up.
        let b = make_battery("a", 200.0, 2500.0, 800.0);
        assert!((headroom(&b, &cfg, true) - 600.0).abs() < 1e-6);
    }

    #[test]
    fn headroom_charge_uses_distance_to_max_charge() {
        let cfg = dcfg();
        // Currently charging at -1000 W, max charge 2500 W → 1500 W headroom down.
        let b = make_battery("a", -1000.0, 2500.0, 800.0);
        assert!((headroom(&b, &cfg, false) - 1500.0).abs() < 1e-6);
    }

    #[test]
    fn headroom_clamped_to_zero_when_at_or_past_bound() {
        let cfg = dcfg();
        let b = make_battery("a", 800.0, 2500.0, 800.0); // at max discharge
        assert_eq!(headroom(&b, &cfg, true), 0.0);
        let b = make_battery("a", 1000.0, 2500.0, 800.0); // somehow over (plug noise)
        assert_eq!(headroom(&b, &cfg, true), 0.0);
    }

    #[test]
    fn headroom_zero_at_full_soc_when_charging() {
        let mut cfg = dcfg();
        cfg.soc_full_pct = 95.0;
        let mut b = make_battery("a", -500.0, 2500.0, 800.0);
        b.soc_pct = Some(96.0);
        // Charging would push SoC higher, but battery is already "full".
        assert_eq!(headroom(&b, &cfg, false), 0.0);
        // Discharge headroom is unaffected by full SoC.
        assert!(headroom(&b, &cfg, true) > 0.0);
    }

    #[test]
    fn headroom_zero_at_empty_soc_when_discharging() {
        let mut cfg = dcfg();
        cfg.soc_empty_pct = 5.0;
        let mut b = make_battery("a", 200.0, 2500.0, 800.0);
        b.soc_pct = Some(4.0);
        assert_eq!(headroom(&b, &cfg, true), 0.0);
        assert!(headroom(&b, &cfg, false) > 0.0);
    }

    #[test]
    fn cap_scale_clamps_to_zero_when_already_over_cap() {
        // This is the regression test for the v0.4 sign-flip fix:
        // measured_sum already past cap, delta_sum positive → raw_scale
        // would be negative; we MUST clamp to 0 (not flip).
        let measured_sum: f64 = 2500.0;
        let delta_sum: f64 = 200.0;
        let post = measured_sum + delta_sum; // 2700
        let cap: f64 = 2400.0;
        assert!(post.abs() > cap);
        let target_post = cap.copysign(post);
        let target_delta_sum = target_post - measured_sum; // -100
        let raw_scale = target_delta_sum / delta_sum; // -0.5
        let scale = raw_scale.clamp(0.0, 1.0);
        assert_eq!(scale, 0.0);
    }

    #[test]
    fn cap_scale_clamps_to_one_for_safe_room() {
        // measured 1000, delta 100 → post 1100 < cap 2400 → no scaling
        // engaged at all. We don't scale below cap.
        let measured_sum: f64 = 1000.0;
        let delta_sum: f64 = 100.0;
        let post = measured_sum + delta_sum;
        let cap: f64 = 2400.0;
        assert!(post.abs() <= cap);
    }

    #[test]
    fn cap_scale_proportional_when_partially_over() {
        // measured 1500, delta 2000 → post 3500, cap 2400 → scale to fit.
        let measured_sum: f64 = 1500.0;
        let delta_sum: f64 = 2000.0;
        let post = measured_sum + delta_sum;
        let cap: f64 = 2400.0;
        assert!(post.abs() > cap);
        let target_post = cap.copysign(post);
        let target_delta_sum = target_post - measured_sum;
        let raw_scale = target_delta_sum / delta_sum;
        let scale = raw_scale.clamp(0.0, 1.0);
        // 900 / 2000 = 0.45
        assert!((scale - 0.45).abs() < 1e-6);
    }

    #[test]
    fn high_bound_uses_taper_when_near_empty() {
        let cfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.discharge_taper_soc_pct = Some(15.0);
        b.discharge_taper_w = Some(400.0);
        b.soc_pct = Some(12.0);
        // SoC 12 % is below taper threshold 15 % → cap at 400 W.
        assert_eq!(high_bound(&b, &cfg), 400.0);
        // Far above → full cap.
        b.soc_pct = Some(50.0);
        assert_eq!(high_bound(&b, &cfg), 800.0);
        // At hard empty cutoff → 0.
        b.soc_pct = Some(4.0);
        assert_eq!(high_bound(&b, &cfg), 0.0);
    }

    #[test]
    fn low_bound_uses_taper_when_near_full() {
        // Default `soc_full_pct` is 95, so the taper must sit strictly below
        // that to ever fire (validate() enforces this in production).
        let cfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.charge_taper_soc_pct = Some(90.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(92.0);
        // SoC 92 % is at/above taper 90 % but below hard full 95 % → -1000 W.
        assert_eq!(low_bound(&b, &cfg), -1000.0);
        // Below taper → full cap.
        b.soc_pct = Some(80.0);
        assert_eq!(low_bound(&b, &cfg), -2500.0);
        // At hard full cutoff → 0, regardless of taper.
        b.soc_pct = Some(96.0);
        assert_eq!(low_bound(&b, &cfg), 0.0);
    }

    #[test]
    fn taper_reduces_headroom_so_dispatcher_redistributes() {
        // Battery at 92 % SoC, nominal max_charge 2500 W, taper kicks at
        // 90 % to 1000 W. Currently charging at -200 W. Headroom for
        // "more charge" should reflect the taper, not the hardware cap.
        let cfg = dcfg();
        let mut b = make_battery("a", -200.0, 2500.0, 800.0);
        b.charge_taper_soc_pct = Some(90.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(92.0);
        // headroom(charge direction) = plug - low_bound = -200 - (-1000) = 800.
        // Without taper it would be -200 - (-2500) = 2300.
        assert_eq!(headroom(&b, &cfg, false), 800.0);
    }

    #[test]
    fn cap_scale_negative_side_clamps_correctly() {
        // measured -2500 (charge over cap), delta -200 (more charge) →
        // post -2700, cap 2400 → raw_scale would push toward sign flip.
        let measured_sum: f64 = -2500.0;
        let delta_sum: f64 = -200.0;
        let post = measured_sum + delta_sum;
        let cap: f64 = 2400.0;
        assert!(post.abs() > cap);
        let target_post = cap.copysign(post); // -2400
        let target_delta_sum = target_post - measured_sum; // +100
        let raw_scale = target_delta_sum / delta_sum; // -0.5
        let scale = raw_scale.clamp(0.0, 1.0);
        assert_eq!(scale, 0.0);
    }
}
