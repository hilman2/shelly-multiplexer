//! Power allocation logic — saldierender Passthrough.
//!
//! All supported inverters (Marstek Venus E, Hoymiles HM/HMS, …) run
//! their own internal control loop in CT mode: they read the per-phase
//! and total active power reported by the meter and adjust their output
//! to drive the **total** towards zero.
//!
//! Our job is therefore minimal: forward the real Shelly's per-phase
//! readings to each battery, scaled by a per-battery factor that
//! determines how much of the total grid imbalance that battery should
//! cover. With one battery the factor is 1.0 (forward the CT readings
//! unchanged). With multiple batteries the factor is each battery's
//! weighted share of the total (sums to 1.0), so the inverters split
//! the work.
//!
//! No PI controller, no smoothing, no battery-output reconstruction:
//! cascading our control loop on top of the inverter's own loop is
//! what made the previous designs unstable. The inverter is the
//! controller; we're just the meter.
//!
//! Sign convention (Shelly): positive `act_power` = grid import (battery
//! should discharge to cover it), negative `act_power` = grid export
//! (battery should charge from surplus).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::time;
use tracing::{debug, info};

use crate::config::{
    AllocationStrategy, BatteryConfig, Config, GroupConfig,
};
use crate::rpc::EmStatusIncoming;
use crate::state::{Allocation, AppState, BatteryTelemetry, PhaseFactors, PhaseWatts};

const ALLOCATION_TICK_MS: u64 = 200;
/// SoC-based eligibility (`min_soc_percent` / `max_soc_percent`) only
/// applies if the SoC reading is fresher than this. After 10 minutes
/// without an update the SoC could be wildly out of date, so we fail
/// open and let the inverter's own protections take over.
const SOC_MAX_AGE: Duration = Duration::from_secs(600);

/// Reduced factor for batteries at their SoC limit. We don't drop them
/// to zero because then the inverter sees no per-phase data at all and
/// some firmwares interpret "no signal" as "hold previous output" —
/// causing the unit to drift in the wrong direction. A 10% pass-through
/// keeps the internal control loop fed without commanding meaningful
/// action; the inverter's own SoC protection prevents it from
/// over-charging or over-discharging in any case.
const SOC_LIMIT_FACTOR: f64 = 0.1;

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) {
    let mut interval = time::interval(Duration::from_millis(ALLOCATION_TICK_MS));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_tick = Instant::now();

    {
        let cfg = config.load();
        info!(
            strategy = ?cfg.dispatcher.strategy,
            batteries = cfg.batteries.len(),
            groups = cfg.groups.len(),
            "dispatcher started (passthrough × factor)"
        );
    }

    loop {
        interval.tick().await;
        let cfg = config.load_full();
        let now = Instant::now();
        let dt_s = now.saturating_duration_since(last_tick).as_secs_f64();
        last_tick = now;

        let snapshot = state.snapshot.load_full();
        if snapshot.age.is_none() {
            continue;
        }

        recompute(&state, &cfg, &snapshot.status, dt_s);
    }
}

fn recompute(state: &AppState, config: &Config, snap: &EmStatusIncoming, dt_s: f64) {
    let real_a = snap.a_act_power.unwrap_or(0.0);
    let real_b = snap.b_act_power.unwrap_or(0.0);
    let real_c = snap.c_act_power.unwrap_or(0.0);
    let real_total = real_a + real_b + real_c;

    let telemetry: HashMap<String, BatteryTelemetry> = state.telemetry.read().clone();

    // Direction follows the raw grid sign.
    let charging = real_total < 0.0;

    // Strategy weights computed across *all* configured batteries. We
    // then split them into "active" (fully eligible for the current
    // direction) and "limited" (at their SoC limit). Active batteries
    // share the full signal among themselves; limited batteries get a
    // small constant fraction so their inverter loop stays informed.
    let all_batteries: Vec<&BatteryConfig> = config.batteries.iter().collect();
    let strategy_weights = compute_weights(
        &all_batteries,
        &telemetry,
        config.dispatcher.strategy,
        charging,
    );
    let active_weight_sum: f64 = all_batteries
        .iter()
        .filter(|b| is_eligible(b, telemetry.get(&b.id), charging))
        .map(|b| strategy_weights.get(&b.id).copied().unwrap_or(0.0))
        .sum();

    let mut factors: HashMap<String, f64> = HashMap::new();
    for b in &all_batteries {
        let factor = if is_eligible(b, telemetry.get(&b.id), charging) {
            if active_weight_sum > 0.0 {
                strategy_weights.get(&b.id).copied().unwrap_or(0.0) / active_weight_sum
            } else {
                0.0
            }
        } else {
            // Battery at SoC limit for this direction: pass through a
            // small fraction of the CT signal so the inverter's loop
            // stays responsive instead of holding a stale command.
            SOC_LIMIT_FACTOR
        };
        factors.insert(b.id.clone(), factor);
    }

    // Apply per-group caps: if the projected combined output of a
    // group (Σ factor_i × |real_total|) exceeds the group's protection
    // cap, scale that group's factors down. Saldierende inverters
    // command full discharge of their share, so the appropriate
    // worst-case is the signal magnitude itself.
    let groups: HashMap<&str, &GroupConfig> =
        config.groups.iter().map(|g| (g.id.as_str(), g)).collect();
    apply_group_factor_caps(&mut factors, &all_batteries, &groups, real_total.abs());

    // Global safety cap on the **signal** the inverters see. Limits
    // the worst-case combined commanded compensation.
    let safety_cap = state.safety.read().effective_cap_w;
    let signal_scale = if safety_cap > 0.0 && real_total.abs() > safety_cap {
        safety_cap / real_total.abs()
    } else {
        1.0
    };

    let mut next: HashMap<IpAddr, Allocation> = HashMap::new();
    for battery in &config.batteries {
        let raw_factor = factors.get(&battery.id).copied().unwrap_or(0.0) * signal_scale;

        // Hard per-battery cap: never let the signal we feed the
        // inverter ask for more than max_discharge_w (when discharging)
        // or max_charge_w (when charging). The inverter's own internal
        // limits should match these, but this is the line of defence
        // that protects the wiring even if the inverter's config drifts.
        let raw_signal_total = real_total * raw_factor;
        let cable_max = if raw_signal_total >= 0.0 {
            battery.max_discharge_w
        } else {
            battery.max_charge_w
        };
        let cap_scale = if raw_signal_total.abs() > cable_max && cable_max > 0.0 {
            cable_max / raw_signal_total.abs()
        } else {
            1.0
        };
        let factor = raw_factor * cap_scale;

        // Pass the real per-phase CT readings through, scaled by the
        // battery's share. The inverter saldiert intern and drives its
        // own output to compensate.
        let phase_a = real_a * factor;
        let phase_b = real_b * factor;
        let phase_c = real_c * factor;
        let allocated_w = real_total * factor;

        // Note when the battery is at its SoC limit and only receiving
        // the SOC_LIMIT_FACTOR pass-through.
        let note = if !is_eligible(battery, telemetry.get(&battery.id), charging) {
            let soc = telemetry.get(&battery.id).and_then(|t| t.soc_percent);
            match (charging, soc) {
                (true, Some(s)) => Some(format!(
                    "SoC {s:.0}% ≥ {max:.0}% — charge limited (signal × {pct:.0}%)",
                    max = battery.max_soc_percent,
                    pct = SOC_LIMIT_FACTOR * 100.0
                )),
                (false, Some(s)) => Some(format!(
                    "SoC {s:.0}% ≤ {min:.0}% — discharge limited (signal × {pct:.0}%)",
                    min = battery.min_soc_percent,
                    pct = SOC_LIMIT_FACTOR * 100.0
                )),
                _ => Some(format!(
                    "at SoC limit (signal × {pct:.0}%)",
                    pct = SOC_LIMIT_FACTOR * 100.0
                )),
            }
        } else {
            None
        };

        next.insert(
            battery.address,
            Allocation {
                battery_id: battery.id.clone(),
                factors: PhaseFactors { a: factor, b: factor, c: factor },
                phase_w: PhaseWatts { a: phase_a, b: phase_b, c: phase_c },
                allocated_w,
                magnitude_w: allocated_w.abs(),
                group: battery.group.clone(),
                note,
            },
        );
    }

    // Energy counters track real grid throughput, not synthetic per-battery values.
    state.energy.write().integrate(snap, dt_s);

    debug!(
        real_a, real_b, real_c, real_total, signal_scale,
        "dispatcher passthrough"
    );
    for (ip, a) in &next {
        debug!(
            battery = %a.battery_id,
            ip = %ip,
            factor_a = a.factors.a,
            l1 = a.phase_w.a,
            l2 = a.phase_w.b,
            l3 = a.phase_w.c,
            sees = a.allocated_w,
            "alloc"
        );
    }

    *state.allocations.write() = next;
}

/// Scale down per-group factors if their projected combined commanded
/// signal (Σ factor × |real_total|) would exceed the group's
/// protection cap. The cap protects the shared MCB / RCD on a group's
/// fuse circuit (`group.cap_w() = phases × fuse_amps × volts`).
fn apply_group_factor_caps(
    factors: &mut HashMap<String, f64>,
    candidates: &[&BatteryConfig],
    groups: &HashMap<&str, &GroupConfig>,
    real_total_abs: f64,
) {
    if real_total_abs <= 0.0 {
        return;
    }
    let mut by_group: HashMap<&str, Vec<&BatteryConfig>> = HashMap::new();
    for b in candidates {
        if let Some(g) = b.group.as_deref() {
            by_group.entry(g).or_default().push(b);
        }
    }
    for (group_id, members) in by_group {
        let Some(group) = groups.get(group_id) else { continue };
        let cap = group.cap_w();
        if cap <= 0.0 {
            continue;
        }
        let factor_sum: f64 = members
            .iter()
            .map(|b| factors.get(b.id.as_str()).copied().unwrap_or(0.0))
            .sum();
        let projected = factor_sum * real_total_abs;
        if projected > cap {
            let scale = cap / projected;
            for b in &members {
                if let Some(f) = factors.get_mut(b.id.as_str()) {
                    *f *= scale;
                }
            }
            debug!(group = group_id, scale, "group cap engaged on factors");
        }
    }
}

/// True if the battery may participate in the requested direction
/// (charging if `charging`, otherwise discharging). Same fail-open
/// behaviour as before: missing or stale telemetry leaves the battery
/// in the pool.
fn is_eligible(b: &BatteryConfig, t: Option<&BatteryTelemetry>, charging: bool) -> bool {
    let Some(t) = t else { return true };
    let Some(soc) = t.soc_percent else { return true };
    let Some(last) = t.last_update else { return true };
    if Instant::now().saturating_duration_since(last) > SOC_MAX_AGE {
        return true;
    }
    if charging && soc >= b.max_soc_percent {
        return false;
    }
    if !charging && soc <= b.min_soc_percent {
        return false;
    }
    true
}

fn compute_weights(
    batteries: &[&BatteryConfig],
    telemetry: &HashMap<String, BatteryTelemetry>,
    strategy: AllocationStrategy,
    charging: bool,
) -> HashMap<String, f64> {
    match strategy {
        AllocationStrategy::Equal => batteries.iter().map(|b| (b.id.clone(), 1.0)).collect(),
        AllocationStrategy::ByCapacity => batteries
            .iter()
            .map(|b| {
                let cap = if charging { b.max_charge_w } else { b.max_discharge_w };
                (b.id.clone(), cap.max(0.0))
            })
            .collect(),
        AllocationStrategy::BySoc => batteries
            .iter()
            .map(|b| {
                let soc = telemetry
                    .get(&b.id)
                    .and_then(|t| t.soc_percent)
                    .map(|p| (p / 100.0).clamp(0.0, 1.0))
                    .unwrap_or(0.5);
                let w = if charging {
                    (1.0 - soc).max(0.05)
                } else {
                    soc.max(0.05)
                };
                (b.id.clone(), w)
            })
            .collect(),
        AllocationStrategy::Priority => {
            let min_prio = batteries.iter().map(|b| b.priority).min().unwrap_or(0);
            batteries
                .iter()
                .map(|b| {
                    let w = if b.priority == min_prio { 1.0 } else { 0.0 };
                    (b.id.clone(), w)
                })
                .collect()
        }
    }
}
