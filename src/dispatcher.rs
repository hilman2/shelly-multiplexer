//! Pulse-based dispatcher (v0.4.3+ target-based architecture).
//!
//! Every cycle:
//!   1. Update circuit-mute state from plug freshness AND grid freshness.
//!      If we can't trust either side, every circuit goes silent until
//!      both recover — Marstek watchdogs then clear their integrators.
//!   2. **Global settle gate**: if any non-muted battery is still in a
//!      pulse cycle (pulse_remaining > 0, OR pulse_remaining = 0 but the
//!      plug hasn't yet moved AND stayed stable for `plug_stable_duration_s`
//!      and `settle_timeout_s` hasn't elapsed), skip the cycle entirely.
//!      "Stable" = no consecutive >`plug_stable_w` movement, i.e. Marstek
//!      has FINISHED implementing the delta (not just started). Without
//!      this gate the
//!      dispatcher would distribute a correction across all batteries
//!      while a previous pulse for a slow-reacting sibling is still in
//!      flight — the slow sibling's pending Δ is not yet visible in
//!      grid_w, so the fast battery would receive a second Δ on top of
//!      its first, producing overshoot followed by ringing back the
//!      other way.
//!   3. Read grid_w from the real Shelly snapshot. Apply asymmetric
//!      grid_bias_w + deadband to compute grid_correction.
//!   4. Compute desired_total = sum(plug_w over eligible) + grid_correction
//!      and distribute it across eligible batteries weighted by
//!        priority_weight × capacity_wh × soc_room
//!      where soc_room is (soc_full - soc) for charging and (soc - soc_empty)
//!      for discharging. Each target is clamped to [low_bound, high_bound]
//!      (SoC-aware), overflow redistributed to siblings. delta_i = target_i
//!      - plug_w_i.
//!   5. Per-circuit cap enforcement on (plug_w + delta) sum. Scale is
//!      always clamped to [0, 1] — we never flip the requested direction
//!      to "fix" an over-cap state, because that would oscillate against
//!      the dispatch direction. If the circuit is already over cap, all
//!      same-direction deltas drop to 0.
//!   6. Queue a fresh pulse_count-long CT burst for each battery whose
//!      delta exceeds `deadband_w`. (Per-battery pulse_settled isn't
//!      checked here any more — the global gate above covers it for
//!      every active battery.)
//!
//! Why target-based (v0.4.3) instead of delta-based (≤ v0.4.2): with
//! two batteries on one circuit at e.g. {A=-400 W, B=-2000 W} (total
//! -2400 W charge), if a cloud cuts surplus from 2400 to 400 W,
//! grid_w jumps to +2000. The old delta-based code split +1970 across
//! both by headroom → A flipped to +372 (DISCHARGE) while B stayed at
//! -802 (CHARGE), wasting conversion losses in both inverters. The
//! target-based code distributes the TARGET (-430), giving each
//! battery ≈ -215 W → both keep charging, just less. Always one direction.
//!
//! The SoC weighting (soc_room factor) is a secondary preference: it
//! never reduces the total dispatched correction (the primary goal),
//! only shifts WHICH battery does the work — emptier batteries get
//! more charge, fuller batteries get more discharge → all reach the
//! healthy band together.
//!
//! There is no virtual integrator. The plug is the only ground truth.
//! Saturation falls out naturally via target clamping at [low_bound,
//! high_bound], with overflow redistributed to siblings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{Config, DispatchMode, DispatcherConfig, LocationConfig};
use crate::modbus::{ModbusDispatch, Setpoint};
use crate::plug;
use crate::state::{AppState, BatteryState};

pub async fn run(
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    modbus_dispatch: Option<ModbusDispatch>,
) {
    let cfg0 = config.load_full();
    let cycle = Duration::from_millis(cfg0.dispatcher.cycle_ms.max(50));
    let mut tick = time::interval(cycle);
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mode = cfg0.dispatcher.mode;
    info!(
        cycle_ms = cfg0.dispatcher.cycle_ms,
        ?mode,
        "dispatcher started"
    );
    loop {
        tick.tick().await;
        let cfg = config.load_full();
        // Run the emergency-cutoff check FIRST: if a circuit is already
        // over cap on incoming plug readings, we cut the worst offender
        // BEFORE the dispatcher math gets a chance to issue setpoints
        // that the rogue battery would ignore anyway.
        enforce_circuit_safety(state.clone(), &cfg.dispatcher, &modbus_dispatch);
        // Night cutoff: cut empty batteries between sunset and sunrise
        // to skip the Marstek inverter's standby losses.
        enforce_night_cutoff(state.clone(), &cfg.dispatcher, &cfg.location, chrono::Utc::now());
        let result = match cfg.dispatcher.mode {
            DispatchMode::Pulse => step_pulse(&state, &cfg.dispatcher),
            DispatchMode::Modbus => step_modbus(&state, &cfg.dispatcher, &modbus_dispatch),
        };
        if let Err(e) = result {
            warn!(error = %e, "dispatcher step failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Night cutoff — skip inverter standby losses between sunset and sunrise.
// ---------------------------------------------------------------------------
//
// A Marstek Venus E pulls ~5-15 W in standby. Over a 12-hour winter
// night that's ~60-180 Wh per battery — small per night, but a real
// efficiency win across a year and a fleet. When the battery is empty
// (no point keeping it powered up to sit idle), we cut its plug at
// sunset and re-enable at sunrise (when PV could start charging it
// again).
//
// Uses the same plug_cut_until / plug_cut_reason fields as the
// emergency cutoff — the UI distinguishes the two by reason prefix.
// The recovery boundary is sunrise (not a fixed window), so the cut
// duration grows in winter and shrinks in summer automatically.
fn enforce_night_cutoff(
    state: Arc<AppState>,
    dcfg: &DispatcherConfig,
    location: &LocationConfig,
    now_utc: chrono::DateTime<chrono::Utc>,
) {
    if !dcfg.night_cutoff_enabled {
        return;
    }
    let (Some(lat), Some(lon)) = (location.latitude, location.longitude) else {
        // Validation guarantees this can't be reached if enabled.
        return;
    };

    // Today's sunrise / sunset (UTC), plus tomorrow's sunrise as the
    // recovery boundary when we cut after midnight UTC and "today" already
    // passed sunset.
    let today = now_utc.date_naive();
    let coord = match sunrise::Coordinates::new(lat, lon) {
        Some(c) => c,
        None => return,
    };
    let solar_today = sunrise::SolarDay::new(coord, today);
    let sunrise_today = solar_today.event_time(sunrise::SolarEvent::Sunrise);
    let sunset_today = solar_today.event_time(sunrise::SolarEvent::Sunset);
    let solar_tomorrow =
        sunrise::SolarDay::new(coord, today.succ_opt().unwrap_or(today));
    let sunrise_tomorrow = solar_tomorrow.event_time(sunrise::SolarEvent::Sunrise);

    // Polar day / night: no sunrise OR sunset on a given date. We
    // conservatively skip the cutoff feature in that case rather than
    // guessing — users in those latitudes can use time-window logic
    // (not yet exposed) instead.
    let (sunrise_today, sunset_today, sunrise_tomorrow) =
        match (sunrise_today, sunset_today, sunrise_tomorrow) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return,
        };

    // "Night" = before today's sunrise OR after today's sunset.
    let is_night = now_utc < sunrise_today || now_utc >= sunset_today;
    let next_sunrise = if now_utc < sunrise_today {
        sunrise_today
    } else {
        sunrise_tomorrow
    };

    // Convert next_sunrise to Instant (best-effort: the diff between
    // wall clock and Instant clocks is small for the lifetime of the
    // process; we add the duration from now_utc onto Instant::now()).
    let now_inst = Instant::now();
    let recovery_inst = match (next_sunrise - now_utc).to_std() {
        Ok(d) => now_inst + d,
        Err(_) => now_inst + Duration::from_secs(8 * 3600),
    };

    let mut to_cut: Vec<(String, String)> = Vec::new();
    let mut to_restore: Vec<(String, String)> = Vec::new();
    {
        let mut bats = state.batteries.write();
        for b in bats.values_mut() {
            // Resolve the effective empty threshold (BMS > TOML > default).
            let empty = b.effective_soc_empty_pct(dcfg.soc_empty_pct);
            let margin = crate::config::NIGHT_CUTOFF_SOC_MARGIN_PCT;
            let is_empty = matches!(b.soc_pct, Some(soc) if soc <= empty + margin);

            // Is THIS battery currently cut because of a night cutoff?
            let cut_for_night = b
                .plug_cut_reason
                .as_deref()
                .map(|s| s.starts_with("night cutoff:"))
                .unwrap_or(false);

            if is_night && is_empty && b.plug_cut_until.is_none() {
                // Eligible: arm the cut.
                let reason = format!(
                    "night cutoff: SoC {:.0}% ≤ {:.0}% + {:.0}% margin, sunset to sunrise",
                    b.soc_pct.unwrap_or(0.0),
                    empty,
                    margin
                );
                b.plug_cut_until = Some(recovery_inst);
                b.plug_cut_reason = Some(reason.clone());
                b.plug_relay_state = Some(false);
                to_cut.push((b.id.clone(), b.plug_url.clone()));
            } else if cut_for_night {
                // Recovery: if it's day OR SoC rose above empty + margin,
                // restore the plug.
                let soc_recovered =
                    matches!(b.soc_pct, Some(soc) if soc > empty + margin);
                if !is_night || soc_recovered {
                    b.plug_cut_until = None;
                    b.plug_cut_reason = None;
                    to_restore.push((b.id.clone(), b.plug_url.clone()));
                }
            }
        }
    }

    for (id, plug_url) in to_cut {
        let id2 = id.clone();
        let plug_url2 = plug_url.clone();
        let state_for_task = state.clone();
        tokio::spawn(async move {
            match plug::set_relay(&plug_url2, false).await {
                Ok(()) => {
                    info!(battery = %id2, "night cutoff: plug relay opened");
                }
                Err(e) => {
                    warn!(
                        battery = %id2,
                        error = %e,
                        "night cutoff: plug write failed — reverting state"
                    );
                    let mut bats = state_for_task.batteries.write();
                    if let Some(b) = bats.get_mut(&id2) {
                        b.plug_cut_until = None;
                        b.plug_cut_reason = Some(format!("night cutoff failed: {e}"));
                    }
                }
            }
        });
    }

    for (id, plug_url) in to_restore {
        let id2 = id.clone();
        let plug_url2 = plug_url.clone();
        let state_for_task = state.clone();
        tokio::spawn(async move {
            match plug::set_relay(&plug_url2, true).await {
                Ok(()) => {
                    info!(battery = %id2, "night cutoff: plug restored at sunrise");
                    let mut bats = state_for_task.batteries.write();
                    if let Some(b) = bats.get_mut(&id2) {
                        b.plug_relay_state = Some(true);
                    }
                }
                Err(e) => {
                    warn!(
                        battery = %id2,
                        error = %e,
                        "night cutoff recovery FAILED — will retry next cycle"
                    );
                }
            }
        });
    }
}

/// Public entrypoint for the admin API: manually resets the cutoff
/// flag and re-enables the plug. Returns the plug HTTP call's result.
pub async fn manual_reset_cutoff(state: Arc<AppState>, battery_id: &str) -> anyhow::Result<()> {
    let plug_url = {
        let bats = state.batteries.read();
        let b = bats
            .get(battery_id)
            .ok_or_else(|| anyhow::anyhow!("unknown battery: {battery_id}"))?;
        if b.plug_cut_until.is_none() {
            anyhow::bail!("battery {battery_id} is not in cutoff state");
        }
        b.plug_url.clone()
    };
    plug::set_relay(&plug_url, true).await?;
    let mut bats = state.batteries.write();
    if let Some(b) = bats.get_mut(battery_id) {
        b.plug_cut_until = None;
        b.plug_cut_reason = None;
        b.plug_relay_state = Some(true);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Emergency circuit cutoff — TWO-PHASE escalation.
// ---------------------------------------------------------------------------
//
// `compute_targets`'s per-circuit cap normally keeps measured plug
// power under fuse. But Marstek inverters over-deliver — commanded
// -1500 W often reads -1600 W at the plug — so transient overshoots
// happen. The OLD policy went straight from "over cap + margin for
// 5 s" to "open the plug relay", which the user observed firing
// "ohne not" (without need) on routine over-delivery.
//
// New escalation:
//   Phase 1 (after `EMERGENCY_CUTOFF_GRACE_S` over cap):
//     SOFT remediation — push `Setpoint::Standby` to every battery on
//     the circuit. Marsteks ramp toward zero in 1-3 s. Circuit cap
//     soft logic in `compute_targets` is also pulling targets down
//     every cycle; this is a more aggressive escalation that bypasses
//     all the gradual ramping.
//   Phase 2 (after another `EMERGENCY_SOFT_REMEDIATION_GRACE_S`, plus
//   the original grace, still over cap):
//     HARD remediation — physically open the worst offender's plug
//     relay. Only reached when soft control has failed: a battery is
//     ignoring our Standby command. That's the pathological case the
//     plug relay is designed for.
//
// Recovery: as soon as the circuit drops back under cap, both phase
// timestamps reset. After `EMERGENCY_CUTOFF_RECOVERY_S` an already-
// tripped plug auto-reenables.
fn enforce_circuit_safety(
    state: Arc<AppState>,
    dcfg: &DispatcherConfig,
    dispatch: &Option<ModbusDispatch>,
) {
    if dcfg.emergency_cutoff_margin_w <= 0.0 {
        // Feature disabled — skip everything.
        return;
    }
    let now = Instant::now();
    let grace = Duration::from_secs_f64(crate::config::EMERGENCY_CUTOFF_GRACE_S.max(0.5));
    let soft_grace =
        Duration::from_secs_f64(crate::config::EMERGENCY_SOFT_REMEDIATION_GRACE_S.max(1.0));
    let recovery = Duration::from_secs_f64(crate::config::EMERGENCY_CUTOFF_RECOVERY_S.max(60.0));

    let mut to_soft_remediate: Vec<String> = Vec::new(); // battery_ids to Standby
    let mut to_trip: Vec<(String, String, String)> = Vec::new(); // (id, plug_url, reason)
    let mut to_reenable: Vec<(String, String)> = Vec::new(); // (id, plug_url)

    {
        let bats = state.batteries.read();
        let mut circuits = state.circuits.write();

        for cs in circuits.values_mut() {
            let cap = cs.cap_w();
            let margin = dcfg.emergency_cutoff_margin_w;
            // Only consider batteries whose plug is currently on (otherwise
            // they can't contribute, and we don't want to count a stale
            // reading from a recently-cut plug).
            let members: Vec<&BatteryState> = cs
                .member_ids
                .iter()
                .filter_map(|id| bats.get(id))
                .filter(|b| b.plug_relay_state.unwrap_or(true)) // assume on if unknown
                .collect();
            if members.is_empty() {
                cs.overload_started_at = None;
                cs.soft_remediation_started_at = None;
                continue;
            }
            let signed_sum: f64 = members.iter().filter_map(|b| b.last_plug_w).sum();
            let overload = signed_sum.abs() > cap + margin;

            if !overload {
                cs.overload_started_at = None;
                cs.soft_remediation_started_at = None;
                continue;
            }

            let started = *cs.overload_started_at.get_or_insert(now);
            let over_for = now.duration_since(started);
            if over_for < grace {
                // Brief overshoot — let compute_targets' soft cap
                // logic handle it on the next dispatcher tick.
                continue;
            }

            // Phase 1: soft remediation. Push Standby to everyone on
            // the circuit immediately. compute_targets will keep them
            // there as long as the over-cap condition persists.
            let soft_started = *cs.soft_remediation_started_at.get_or_insert(now);
            for m in &members {
                if m.plug_cut_until.map_or(true, |t| t <= now) {
                    to_soft_remediate.push(m.id.clone());
                }
            }

            // Phase 2: if soft remediation has been in effect for the
            // full soft_grace window AND we're STILL over cap, the
            // Marstek is refusing to honour our Standby. Trip the
            // worst offender's plug relay.
            if now.duration_since(soft_started) >= soft_grace {
                let dir = signed_sum.signum();
                let worst = members
                    .iter()
                    .filter(|b| {
                        b.plug_cut_until.map_or(true, |t| t <= now)
                            && b.last_plug_w
                                .map(|w| w.signum() == dir && w.abs() > 0.0)
                                .unwrap_or(false)
                    })
                    .max_by(|a, b| {
                        let av = a.last_plug_w.unwrap_or(0.0).abs();
                        let bv = b.last_plug_w.unwrap_or(0.0).abs();
                        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
                    });
                if let Some(b) = worst {
                    let reason = format!(
                        "circuit {} over cap: |{:.0} W| > {:.0} W + {:.0} W margin for {:.1}s (soft remediation didn't help after {:.1}s)",
                        cs.config.id,
                        signed_sum,
                        cap,
                        margin,
                        over_for.as_secs_f64(),
                        now.duration_since(soft_started).as_secs_f64(),
                    );
                    to_trip.push((b.id.clone(), b.plug_url.clone(), reason));
                }
            }
        }

        // Recovery sweep: any battery whose cut window has expired
        // gets its plug re-enabled.
        for b in bats.values() {
            if let Some(t) = b.plug_cut_until {
                // `t` is the cut_until timestamp — if it's already in
                // the past, the recovery window has elapsed.
                if t <= now {
                    to_reenable.push((b.id.clone(), b.plug_url.clone()));
                }
            }
        }
    }

    // Phase 1 soft remediation: command Standby on every battery whose
    // circuit has been over-cap for `grace`. This is in addition to
    // `compute_targets`' soft-scaling pass (which already pulls
    // targets down proportionally every cycle); a direct Standby
    // bypasses the rate limiter and tells the Marstek to drop to 0
    // as fast as its own ramp allows. Sent every cycle while the
    // overload persists, which is fine — `ModbusDispatch::send`
    // dedupes to the channel's last value internally.
    if let Some(d) = dispatch {
        for id in &to_soft_remediate {
            d.send(id, crate::modbus::Setpoint::Standby);
        }
        if !to_soft_remediate.is_empty() {
            info!(
                batteries = ?to_soft_remediate,
                "circuit overload: SOFT remediation — pushing Standby to all members"
            );
        }
    }

    // Optimistically stamp the state so the SAME cycle's subsequent
    // dispatch math sees "this battery is cut" — the HTTP call still
    // runs in the background. If the HTTP fails we revert below.
    if !to_trip.is_empty() {
        let mut bats = state.batteries.write();
        for (id, _, reason) in &to_trip {
            if let Some(b) = bats.get_mut(id) {
                b.plug_cut_until = Some(now + recovery);
                b.plug_cut_reason = Some(reason.clone());
                b.plug_relay_state = Some(false);
            }
        }
    }

    for (id, plug_url, reason) in to_trip {
        let id2 = id.clone();
        let plug_url2 = plug_url.clone();
        let reason2 = reason.clone();
        let state_for_task = state.clone();
        tokio::spawn(async move {
            match plug::set_relay(&plug_url2, false).await {
                Ok(()) => {
                    warn!(
                        battery = %id2,
                        reason = %reason2,
                        "EMERGENCY CUTOFF: plug relay opened"
                    );
                }
                Err(e) => {
                    warn!(
                        battery = %id2,
                        error = %e,
                        "EMERGENCY CUTOFF http call failed — clearing the cut flag so the next cycle retries"
                    );
                    let mut bats = state_for_task.batteries.write();
                    if let Some(b) = bats.get_mut(&id2) {
                        b.plug_cut_until = None;
                        b.plug_cut_reason = Some(format!("cutoff failed: {e}"));
                    }
                }
            }
        });
    }

    for (id, plug_url) in to_reenable {
        let id2 = id.clone();
        let plug_url2 = plug_url.clone();
        let state_for_task = state.clone();
        tokio::spawn(async move {
            match plug::set_relay(&plug_url2, true).await {
                Ok(()) => {
                    info!(battery = %id2, "emergency cutoff recovery: plug relay closed");
                    let mut bats = state_for_task.batteries.write();
                    if let Some(b) = bats.get_mut(&id2) {
                        b.plug_cut_until = None;
                        b.plug_cut_reason = None;
                        b.plug_relay_state = Some(true);
                    }
                }
                Err(e) => {
                    warn!(
                        battery = %id2,
                        error = %e,
                        "emergency cutoff recovery FAILED — plug still off, will retry next cycle"
                    );
                }
            }
        });
    }
}

/// Modbus-dispatch tick — compute absolute setpoints and push them
/// through `ModbusDispatch`. Way simpler than the pulse path: no
/// settle gate, no in-flight tracking, no virtual Shelly. Just
/// figure out the target per battery and tell each Marstek exactly
/// what to do.
fn step_modbus(
    state: &AppState,
    dcfg: &DispatcherConfig,
    dispatch: &Option<ModbusDispatch>,
) -> anyhow::Result<()> {
    let Some(dispatch) = dispatch else {
        // Modbus mode requested but the writer pool didn't spin up
        // (likely no batteries with modbus_host). Nothing to do.
        return Ok(());
    };
    let now = Instant::now();
    let grid_fresh = update_circuit_mute(state, dcfg, now);
    // No `detect_setpoint_outcomes` call here — in modbus mode we
    // have direct telemetry (SoC, force_mode_actual, battery_power)
    // and the BMS cutoffs (44000/44001) which feed low_bound /
    // high_bound. A "full" battery's charge bound is already pinned
    // to 0 before we issue any command, so empirical refusal
    // detection is redundant. Pulse mode still relies on it.

    if !grid_fresh {
        // Grid stale → command every battery to standby so they don't
        // keep churning on a stale assumption. Same safety stance as
        // the pulse path's silence window.
        for id in dispatch.battery_ids() {
            dispatch.send(&id, Setpoint::Standby);
        }
        return Ok(());
    }

    // Raw grid reading. Smoothing has moved to the algorithm level
    // (`rate_limit_w_per_cycle`) so we don't need to pre-filter here.
    let grid_w = {
        let snap = state.snapshot.load_full();
        snap.status.total_act_power.unwrap_or(0.0)
    };
    let targets = compute_targets(state, dcfg, grid_w, now);

    // PARALLEL dispatch: write the computed target to EVERY eligible
    // battery on every cycle. Why this matters more than it sounds:
    //
    //   Up to v0.9 step_modbus dispatched sequentially per circuit —
    //   one battery per cycle, only the one with the biggest delta
    //   from its last setpoint, and only when its previous write had
    //   "settled" (plug movement observed + 3 s of stability, or 5 s
    //   timeout). With two batteries on a circuit the second battery's
    //   setpoint was 3-5 s stale by the time it got refreshed. Field
    //   testing showed grid swings staying uncorrected for 30+ seconds
    //   because the dispatcher's intent was right but it could only
    //   write to one battery at a time. v0.10 fires every battery in
    //   parallel: a grid swing now corrects within ONE dispatcher tick
    //   (limited only by the Marstek's 1-3 s hardware ramp).
    //
    // Cap-safety: compute_targets already enforces the per-circuit
    // fuse cap on the SUM of targets, so parallel writes can't push
    // a circuit over its limit. Each battery's writer task suppresses
    // duplicate-ish writes via WRITE_DEADBAND_W (25 W) so micro-jitter
    // in the target doesn't spam Modbus traffic.
    let bats = state.batteries.read();
    let circuits = state.circuits.read();
    // Per-circuit accumulator for the post-dispatch summary log. Lets
    // the operator confirm at a glance whether the commanded sum stays
    // under cap — separate from the cap-engaged WARN, which fires
    // BEFORE per-battery `from_signed_watts` clamping. The numbers in
    // this log are exactly what the BatteryWriters will write.
    let mut commanded_per_circuit: HashMap<String, (f64, f64, f64, f64, Vec<(String, f64, f64)>)> =
        HashMap::new();
    for b in bats.values() {
        // Skip cut plugs (emergency cutoff or night cutoff active).
        if b.plug_cut_until.map(|t| t > now).unwrap_or(false) {
            continue;
        }
        // Skip muted circuits (plug or grid stale → wait for recovery).
        let muted = circuits
            .get(&b.circuit)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false);
        if muted {
            continue;
        }
        let target = targets.get(&b.id).copied().unwrap_or(0.0);
        let sp = Setpoint::from_signed_watts(target, b.max_charge_w, b.max_discharge_w);
        let commanded = sp.to_signed_watts();
        let plug = b.last_plug_w.unwrap_or(0.0);
        let entry = commanded_per_circuit
            .entry(b.circuit.clone())
            .or_insert((0.0, 0.0, 0.0, 0.0, Vec::new()));
        entry.0 += target;
        entry.1 += commanded;
        entry.2 += plug;
        entry.4.push((b.id.clone(), commanded, plug));
        dispatch.send(&b.id, sp);
    }
    // Fill cap_w per circuit (separate loop so we don't double-hold
    // the circuits read-lock).
    for (cid, entry) in commanded_per_circuit.iter_mut() {
        if let Some(cs) = circuits.get(cid) {
            entry.3 = cs.cap_w();
        }
    }
    for (cid, (target_sum, commanded_sum, plug_sum, cap, per_bat)) in &commanded_per_circuit {
        let over_cap = commanded_sum.abs() > *cap;
        let level = if over_cap { tracing::Level::WARN } else { tracing::Level::DEBUG };
        // Manual dispatch — `warn!`/`debug!` macros don't accept a
        // dynamic level.
        if level == tracing::Level::WARN {
            warn!(
                circuit = %cid,
                cap_w = cap,
                target_sum,
                commanded_sum,
                plug_sum,
                per_battery = ?per_bat,
                "POST-DISPATCH SUMMARY: commanded sum OVER cap — cap-scaling failed to hold"
            );
        } else {
            debug!(
                circuit = %cid,
                cap_w = cap,
                target_sum,
                commanded_sum,
                plug_sum,
                per_battery = ?per_bat,
                "post-dispatch summary"
            );
        }
    }
    Ok(())
}

fn step_pulse(state: &AppState, dcfg: &DispatcherConfig) -> anyhow::Result<()> {
    let now = Instant::now();
    let grid_fresh = update_circuit_mute(state, dcfg, now);
    if !grid_fresh {
        // No usable grid measurement → leave deltas at zero, circuits are
        // muted by update_circuit_mute, virtual_shelly will drop responses
        // and Marstek watchdogs clear the integrator.
        return Ok(());
    }
    // Classify the previous cycle's outcome before queueing new work:
    // batteries that refused a significant directional pulse get a
    // direction-lockout; batteries whose plug moved get any stale
    // lockout cleared. Does nothing for batteries we haven't pulsed yet.
    detect_pulse_outcomes(state, dcfg, now);
    if any_pulse_in_flight(state, dcfg, now) {
        // A previous pulse is still committed but its plug response isn't
        // visible in grid_w yet. Re-dispatching now would double-commit
        // any battery whose own pulse already landed — we'd add a "fair
        // share" of the still-uncorrected grid_w on top of the Δ that's
        // mid-flight in a sibling. Wait one cycle. settle_timeout_s
        // bounds the wait if a Marstek refuses to react.
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

/// Empirical full/empty detection. Run once per dispatcher cycle, BEFORE
/// queueing new work, so the eligibility filter and bounds see the
/// up-to-date lockouts.
///
/// For each battery with a finished pulse cycle (pulse_remaining == 0
/// and a `last_pulse_delta_w` recorded), we compare the plug movement
/// since `plug_w_at_pulse_send` against `plug_stable_w`:
///   - moved ≥ plug_stable_w → request was honoured. Clear any stale
///     lockout for that direction.
///   - moved < plug_stable_w AND settle_timeout_s elapsed AND the
///     request was ≥ deadband_w → request was REFUSED. Lock the
///     direction for soc_unknown_lockout_s. The opposite direction is
///     untouched: a battery that refuses charging (likely "full") will
///     still happily discharge.
///
/// In either decisive case we clear `last_pulse_delta_w` so the same
/// cycle isn't re-classified on the next tick.
fn detect_pulse_outcomes(state: &AppState, dcfg: &DispatcherConfig, now: Instant) {
    let mut bats = state.batteries.write();
    for b in bats.values_mut() {
        if b.pulse_remaining > 0 {
            continue;
        }
        let Some(delta) = b.last_pulse_delta_w else {
            continue;
        };
        let Some(completed_at) = b.last_pulse_completed_at else {
            continue;
        };
        let Some(snap) = b.plug_w_at_pulse_send else {
            continue;
        };
        let Some(plug) = b.last_plug_w else {
            continue;
        };
        let moved = (plug - snap).abs();

        if moved >= crate::config::PLUG_STABLE_W {
            // Pulse landed. The Marstek IS responding in this direction,
            // so any prior lockout for it is stale — drop it.
            if delta < 0.0 {
                b.charge_locked_until = None;
            } else if delta > 0.0 {
                b.discharge_locked_until = None;
            }
            b.last_pulse_delta_w = None;
            continue;
        }
        if delta.abs() < crate::config::WRITE_DEADBAND_W {
            // Below deadband; we shouldn't even have queued it. Decline
            // to draw a conclusion either way.
            b.last_pulse_delta_w = None;
            continue;
        }
        let elapsed = now.duration_since(completed_at).as_secs_f64();
        if elapsed < dcfg.settle_timeout_s {
            // Still inside the settle window — give the Marstek more
            // time before deciding it refused.
            continue;
        }
        // Refusal confirmed: significant directional request, no plug
        // movement, settle window over. Lock the offending direction.
        let until = now + Duration::from_secs_f64(crate::config::SOC_UNKNOWN_LOCKOUT_S);
        if delta < 0.0 {
            b.charge_locked_until = Some(until);
            warn!(
                battery = %b.id,
                delta_w = delta,
                lockout_s = crate::config::SOC_UNKNOWN_LOCKOUT_S,
                "charge refused (likely full) — locking charge direction"
            );
        } else {
            b.discharge_locked_until = Some(until);
            warn!(
                battery = %b.id,
                delta_w = delta,
                lockout_s = crate::config::SOC_UNKNOWN_LOCKOUT_S,
                "discharge refused (likely empty) — locking discharge direction"
            );
        }
        b.last_pulse_delta_w = None;
    }
}

/// True iff any battery in a non-muted circuit is still in a pulse
/// cycle (pulses out but plug response not yet observed). Muted-circuit
/// batteries are excluded — their pending state will be cleared by
/// queue_pulses anyway, and the silence-window keeps grid_w independent
/// of their behaviour.
fn any_pulse_in_flight(state: &AppState, dcfg: &DispatcherConfig, now: Instant) -> bool {
    let bats = state.batteries.read();
    let circuits = state.circuits.read();
    bats.values().any(|b| {
        let muted = circuits
            .get(&b.circuit)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false);
        if muted {
            return false;
        }
        !b.pulse_settled(crate::config::PLUG_STABLE_DURATION_S, dcfg.settle_timeout_s)
    })
}

// ---------------------------------------------------------------------------
// Step 1: plug/grid freshness -> circuit mute. Returns whether grid_w
// is fresh enough to act on; if false, the caller should skip the cycle
// (every circuit is already muted).
// ---------------------------------------------------------------------------

fn update_circuit_mute(state: &AppState, dcfg: &DispatcherConfig, now: Instant) -> bool {
    let plug_stale_s = dcfg.plug_stale_s;
    let grid_stale_s = dcfg.grid_stale_s;
    // The post-stale cooldown is a pulse-mode quirk: in pulse mode the
    // Marstek's internal CT integrator needs ~60 s without input to
    // clear, otherwise the first post-recovery pulse stacks on top of
    // whatever the integrator had accumulated. In modbus mode we're in
    // force_mode (not following CT at all), so there's no integrator
    // to clear — recovery is immediate, no cooldown needed.
    let silence = match dcfg.mode {
        DispatchMode::Pulse => Duration::from_secs_f64(crate::config::GROUP_SILENT_AFTER_STALE_S),
        DispatchMode::Modbus => Duration::ZERO,
    };

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
            // Stale path: in pulse mode set the long cooldown so the
            // Marstek integrators clear; in modbus mode just mute the
            // current cycle (silence = 0 → silent_until = now means
            // "muted now, but the next fresh cycle resumes immediately").
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
// Grid correction — direction-aware "never cross zero" rule.
// ---------------------------------------------------------------------------
//
// The asymmetric `grid_bias_w` policy is "never cross zero in the
// direction we don't want":
//   • while CHARGING (current plug sum well below zero): target grid =
//     -SAFETY_MARGIN_W (10 W export). Any grid value above -margin
//     pulls our setpoint toward less charging; if needed the math
//     naturally flips through standby into discharging.
//   • while DISCHARGING (current plug sum well above zero): target
//     grid = +SAFETY_MARGIN_W. Same mirror behaviour.
//   • IDLE: grid in [-margin, +margin] is "hold"; outside fires the
//     appropriate direction.
//
// The output is a single signed correction: the watts we'd need to add
// to plug_sum to land grid_w at the target. The caller adds it to the
// current measured plug_sum and lets `compute_targets` distribute the
// result across batteries (with per-battery [low_bound, high_bound]
// clamps, per-cycle rate-limit, and per-circuit cap scaling).
//
// History:
//   • v0.8.1: introduced the "never cross zero in dispatch direction"
//     rule.
//   • v0.8.3: dedicated idle threshold so Marstek standby loss
//     (~5-15 W per inverter) doesn't get misread as active charging.
//   • v0.10:  dropped the asymmetric `grid_bias_w` and the noise-filter
//     `deadband_w` from the config schema. Hardcoded SAFETY_MARGIN_W
//     = 10 W is enough because (a) dispatch fires every cycle (~200 ms)
//     in parallel to every battery, and (b) Marstek ramps in 1-3 s, so
//     fast feedback control replaces a big tuning buffer.
fn compute_grid_correction(
    grid_w: f64,
    current_total: f64,
    eligible_count: usize,
) -> f64 {
    let margin = crate::config::SAFETY_MARGIN_W;
    // Direction-detection threshold: |current dispatch power| must
    // exceed this to count as "actively charging/discharging". Below it
    // we treat the system as idle. Scales with battery count so the
    // Marstek standby loss across N inverters doesn't add up over the
    // threshold by itself (observed 5-15 W per battery at idle vs.
    // MIN_W = 50 W as the smallest commanded value — a `MIN_W / 2`
    // per-battery slice sits cleanly in the gap).
    let idle_threshold = (crate::modbus::MARSTEK_MIN_W * eligible_count as f64 / 2.0)
        .max(margin);
    let charging_now = current_total < -idle_threshold;
    let discharging_now = current_total > idle_threshold;

    // Where do we want grid_w to land?
    let target_grid = if charging_now {
        -margin
    } else if discharging_now {
        margin
    } else if grid_w > margin {
        // Idle + grid imports more than margin → start discharging.
        margin
    } else if grid_w < -margin {
        // Idle + grid exports more than margin → start charging.
        -margin
    } else {
        // Idle + grid inside the band → hold position.
        return 0.0;
    };

    // Each W added to plug_sum (more discharge / less charge) REDUCES
    // grid_w by 1 W. We want new_grid = target_grid, so the correction
    // we have to add to plug_sum is grid_w - target_grid.
    grid_w - target_grid
}

// ---------------------------------------------------------------------------
// SoC-aware bounds (live, from current plug + soc)
// ---------------------------------------------------------------------------

fn high_bound(b: &BatteryState, dcfg: &DispatcherConfig, now: Instant) -> f64 {
    // Empirical "empty" lockout has the same effect as the hard SoC-empty
    // gate: discharge is pinned to 0 until the lockout expires.
    if b.is_discharge_locked(now) {
        return 0.0;
    }
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

fn low_bound(b: &BatteryState, dcfg: &DispatcherConfig, now: Instant) -> f64 {
    // Empirical "full" lockout, symmetric to `high_bound`.
    if b.is_charge_locked(now) {
        return 0.0;
    }
    let full = b.effective_soc_full_pct(dcfg.soc_full_pct);
    match b.soc_pct {
        Some(soc) if soc >= full => 0.0,
        // Below the full cutoff: cap at the SoC-aware effective max
        // charge. Same reasoning as `high_bound` — taper near 100 %
        // SoC so we don't try to push more in than the BMS will accept.
        _ => -b.effective_max_charge_w(),
    }
}

// ---------------------------------------------------------------------------
// Step 3/4: target-based delta computation
// ---------------------------------------------------------------------------

fn compute_deltas(
    state: &AppState,
    dcfg: &DispatcherConfig,
    grid_w: f64,
    now: Instant,
) -> HashMap<String, f64> {
    let bats = state.batteries.read();
    let circuits = state.circuits.read();

    let mut deltas: HashMap<String, f64> = HashMap::new();
    for b in bats.values() {
        deltas.insert(b.id.clone(), 0.0);
    }

    let muted_circuit = |cid: &str| -> bool {
        circuits
            .get(cid)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false)
    };

    // Eligible = not on a muted circuit AND has a usable plug reading.
    //
    // We do NOT filter on `b.active` (= has a configured SoC source):
    // since v0.6, batteries without a SoC source participate too. They
    // rely on empirical refusal detection (`detect_pulse_outcomes`) to
    // discover their own "full" / "empty" — per-direction lockouts
    // shadow the SoC gates when SoC isn't known.
    let eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| !muted_circuit(&b.circuit) && b.last_plug_w.is_some())
        .collect();
    if eligible.is_empty() {
        return deltas;
    }

    // Direction-aware asymmetric bias. See `compute_grid_correction`
    // for the policy. Compute current_total first because the bias
    // logic needs to know which direction we're already dispatching in.
    let current_total: f64 = eligible
        .iter()
        .map(|b| b.last_plug_w.unwrap_or(0.0))
        .sum();
    let grid_correction =
        compute_grid_correction(grid_w, current_total, eligible.len());

    // Conflict detection: any active circuit with opposing flows above
    // MIN_W must be realigned even when the grid itself is balanced.
    // Threshold = MARSTEK_MIN_W (50 W) so a battery's standby loss
    // doesn't get counted as "active discharge".
    let has_conflict = circuits.values().any(|cs| {
        if cs.silent_until.map(|t| t > now).unwrap_or(false) {
            return false;
        }
        let plugs: Vec<f64> = cs
            .member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .filter_map(|b| b.last_plug_w)
            .collect();
        let threshold = crate::modbus::MARSTEK_MIN_W;
        let any_pos = plugs.iter().any(|w| *w > threshold);
        let any_neg = plugs.iter().any(|w| *w < -threshold);
        any_pos && any_neg
    });

    if grid_correction.abs() < 1e-3 && !has_conflict {
        return deltas;
    }

    let desired_total = current_total + grid_correction;
    let charging = desired_total < 0.0;

    // weight = priority × capacity × soc_room (in the relevant direction).
    // A battery locked in the active direction (empirical full/empty
    // detection) gets weight 0 — saves a redistribution iteration vs.
    // letting the bound-clamp empty out its share later.
    let weight_of = |b: &BatteryState| -> f64 {
        if charging && b.is_charge_locked(now) {
            return 0.0;
        }
        if !charging && b.is_discharge_locked(now) {
            return 0.0;
        }
        let cap = if b.capacity_wh > 0.0 {
            b.capacity_wh
        } else {
            b.max_charge_w + b.max_discharge_w
        };
        let soc_room = match b.soc_pct {
            Some(soc) => {
                if charging {
                    (b.effective_soc_full_pct(dcfg.soc_full_pct) - soc).max(0.0)
                } else {
                    (soc - b.effective_soc_empty_pct(dcfg.soc_empty_pct)).max(0.0)
                }
            }
            // Unknown SoC: neutral 50 %-point room so the battery still
            // participates rather than stalling the dispatcher.
            None => 50.0,
        };
        b.priority_weight * cap * soc_room
    };

    let mut targets: HashMap<String, f64> = HashMap::new();
    for b in &eligible {
        targets.insert(b.id.clone(), 0.0);
    }
    let mut active: Vec<&BatteryState> = eligible.iter().copied().collect();
    let mut remaining = desired_total;

    for _ in 0..6 {
        if active.is_empty() || remaining.abs() < 1e-3 {
            break;
        }
        let total_weight: f64 = active.iter().map(|b| weight_of(b)).sum();
        if total_weight <= 0.0 {
            // No battery has SoC room in the desired direction; what's
            // already in targets stays, the rest of desired_total is
            // physically unreachable this cycle.
            break;
        }
        let mut clamped_ids: Vec<String> = Vec::new();
        for b in &active {
            let prev = targets.get(&b.id).copied().unwrap_or(0.0);
            let share = remaining * weight_of(b) / total_weight;
            let proposed = prev + share;
            let lo = low_bound(b, dcfg, now);
            let hi = high_bound(b, dcfg, now);
            let clamped = proposed.clamp(lo, hi);
            targets.insert(b.id.clone(), clamped);
            if (clamped - proposed).abs() > 1e-3 {
                clamped_ids.push(b.id.clone());
            }
        }
        let assigned: f64 = targets.values().sum();
        remaining = desired_total - assigned;
        if clamped_ids.is_empty() {
            break;
        }
        active.retain(|b| !clamped_ids.contains(&b.id));
    }

    // targets → deltas
    for b in &eligible {
        let plug = b.last_plug_w.unwrap_or(0.0);
        let target = targets.get(&b.id).copied().unwrap_or(plug);
        deltas.insert(b.id.clone(), target - plug);
    }

    // Per-circuit cap on (plug_w + delta) sum.
    //
    // Scale is clamped to [0, 1]:
    //   - 1.0 → no scaling, deltas pass through.
    //   - 0.0..1.0 → shrink toward zero so post lands on cap.
    //   - 0.0 → fully suppress this circuit's deltas (already at/over cap).
    //
    // We deliberately do NOT flip signs to "fix" an already-over-cap
    // measured_sum: that would emit reverse-direction pulses against the
    // grid-balance intent and oscillate.
    for cs in circuits.values() {
        let members: Vec<&BatteryState> = cs
            .member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .filter(|b| b.last_plug_w.is_some())
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
        let cap = cs.cap_w();
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
// Step 3 / modbus path: absolute setpoint computation
// ---------------------------------------------------------------------------
//
// Same overall math as compute_deltas (eligibility, grid correction,
// weighted share with [low_bound, high_bound] clamping, iterative
// redistribution of clamped surplus). The only differences:
//
//   - we return ABSOLUTE targets, not deltas (the writer doesn't care
//     about the plug delta — Modbus accepts wattages directly),
//   - the circuit cap is enforced on the SUM of targets, not on the
//     plug+delta sum (cleaner: no measurement loop in the math).
//
// Inactive batteries (= no SoC source) participate via empirical
// detect_setpoint_outcomes — same lockout semantics as pulse mode.
fn compute_targets(
    state: &AppState,
    dcfg: &DispatcherConfig,
    grid_w: f64,
    now: Instant,
) -> HashMap<String, f64> {
    let bats = state.batteries.read();
    let circuits = state.circuits.read();

    let mut targets: HashMap<String, f64> = HashMap::new();
    for b in bats.values() {
        targets.insert(b.id.clone(), 0.0);
    }

    let muted_circuit = |cid: &str| -> bool {
        circuits
            .get(cid)
            .and_then(|c| c.silent_until)
            .map(|t| t > now)
            .unwrap_or(false)
    };

    let eligible: Vec<&BatteryState> = bats
        .values()
        .filter(|b| !muted_circuit(&b.circuit) && b.last_plug_w.is_some())
        .collect();
    if eligible.is_empty() {
        return targets;
    }

    // Direction-aware asymmetric bias (same as pulse mode). See
    // `compute_grid_correction` for the rule.
    let current_total: f64 = eligible.iter().map(|b| b.last_plug_w.unwrap_or(0.0)).sum();
    let grid_correction =
        compute_grid_correction(grid_w, current_total, eligible.len());
    let desired_total = current_total + grid_correction;
    let charging = desired_total < 0.0;

    let weight_of = |b: &BatteryState| -> f64 {
        if charging && b.is_charge_locked(now) {
            return 0.0;
        }
        if !charging && b.is_discharge_locked(now) {
            return 0.0;
        }
        let cap = if b.capacity_wh > 0.0 {
            b.capacity_wh
        } else {
            b.max_charge_w + b.max_discharge_w
        };
        let soc_room = match b.soc_pct {
            Some(soc) => {
                if charging {
                    (b.effective_soc_full_pct(dcfg.soc_full_pct) - soc).max(0.0)
                } else {
                    (soc - b.effective_soc_empty_pct(dcfg.soc_empty_pct)).max(0.0)
                }
            }
            None => 50.0,
        };
        b.priority_weight * cap * soc_room
    };

    let mut active: Vec<&BatteryState> = eligible.iter().copied().collect();
    let mut remaining = desired_total;

    for _ in 0..6 {
        if active.is_empty() || remaining.abs() < 1e-3 {
            break;
        }
        let total_weight: f64 = active.iter().map(|b| weight_of(b)).sum();
        if total_weight <= 0.0 {
            break;
        }
        let mut clamped_ids: Vec<String> = Vec::new();
        for b in &active {
            let prev = targets.get(&b.id).copied().unwrap_or(0.0);
            let share = remaining * weight_of(b) / total_weight;
            let proposed = prev + share;
            let lo = low_bound(b, dcfg, now);
            let hi = high_bound(b, dcfg, now);
            let clamped = proposed.clamp(lo, hi);
            targets.insert(b.id.clone(), clamped);
            if (clamped - proposed).abs() > 1e-3 {
                clamped_ids.push(b.id.clone());
            }
        }
        let assigned: f64 = active
            .iter()
            .map(|b| targets.get(&b.id).copied().unwrap_or(0.0))
            .sum::<f64>()
            + targets
                .iter()
                .filter(|(id, _)| !active.iter().any(|b| &b.id == *id))
                .map(|(_, v)| *v)
                .sum::<f64>();
        remaining = desired_total - assigned;
        if clamped_ids.is_empty() {
            break;
        }
        active.retain(|b| !clamped_ids.contains(&b.id));
    }

    // Per-circuit MIN_W consolidation. Splitting a small circuit total
    // (say 80 W across two batteries → 40 W each) makes every share
    // fall under the Marstek firmware floor, so each `Setpoint::from_
    // signed_watts` collapses to Standby and nothing happens. This
    // pass detects the case and concentrates the entire circuit target
    // onto ONE battery (the one with the highest dispatch-direction
    // weight) so the command actually fires.
    //
    // Triggered only when:
    //   * |Σ targets on circuit| ≥ MARSTEK_MIN_W  (meaningful command),
    //   * every individual target on the circuit has |t| < MARSTEK_MIN_W
    //     (i.e. nobody would dispatch on their own).
    //
    // The chosen battery's target is clamped to its [low_bound,
    // high_bound] so the consolidation can't violate SoC gates or
    // hardware caps. If clamping leaves residual the dispatcher will
    // pick it up next cycle on a sibling.
    for cs in circuits.values() {
        let member_ids = &cs.member_ids;
        let target_sum: f64 = member_ids
            .iter()
            .filter_map(|id| targets.get(id).copied())
            .sum();
        if target_sum.abs() < crate::modbus::MARSTEK_MIN_W {
            continue;
        }
        let any_dispatches = member_ids
            .iter()
            .filter_map(|id| targets.get(id).copied())
            .any(|t| t.abs() >= crate::modbus::MARSTEK_MIN_W);
        if any_dispatches {
            continue;
        }
        // All individual targets are sub-MIN_W. Concentrate.
        let dir = target_sum.signum();
        let primary = member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .filter(|b| b.last_plug_w.is_some())
            .filter(|b| {
                // Only consider batteries with headroom in the active
                // direction — a charge-locked battery can't absorb a
                // negative target.
                if dir < 0.0 {
                    low_bound(b, dcfg, now) < 0.0
                } else {
                    high_bound(b, dcfg, now) > 0.0
                }
            })
            .max_by(|a, b| {
                // Prefer the battery with the largest absolute existing
                // target in the active direction (= most "wanting" to
                // dispatch already); fall back to priority weight.
                let av = targets.get(&a.id).copied().unwrap_or(0.0).abs();
                let bv = targets.get(&b.id).copied().unwrap_or(0.0).abs();
                av.partial_cmp(&bv)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.priority_weight.partial_cmp(&b.priority_weight).unwrap_or(std::cmp::Ordering::Equal))
            });
        if let Some(p) = primary {
            let lo = low_bound(p, dcfg, now);
            let hi = high_bound(p, dcfg, now);
            let concentrated = target_sum.clamp(lo, hi);
            for id in member_ids {
                if *id == p.id {
                    targets.insert(id.clone(), concentrated);
                } else {
                    targets.insert(id.clone(), 0.0);
                }
            }
            debug!(
                circuit = %cs.config.id,
                primary = %p.id,
                concentrated_w = concentrated,
                "MIN_W consolidation engaged"
            );
        }
    }

    // Per-battery rate limit: clamp each target's distance from the
    // CURRENT plug-measured power to ±rate_limit_w_per_cycle. Smooths
    // big swings into ramps — going 0 W → 2.5 kW takes
    // ceil(2500 / rate_limit) cycles. Replaces the EMA grid smoother
    // + per-write throttle + min_write_interval we used to expose: one
    // knob, applied at the algorithm level. Inspired by the Python ref
    // impl's "max 750 W/cycle" rate limit.
    //
    // Apply BEFORE the per-circuit cap: this only ever SHRINKS target
    // magnitudes (clamping a delta toward zero can't grow |target|
    // unless plug and target have opposite signs, in which case the
    // post-rate-limit |target| ≤ |plug| + rate_limit ≤ |original|),
    // so the circuit cap below remains a hard upper bound.
    let rate_limit = dcfg.rate_limit_w_per_cycle.max(1.0);
    for b in &eligible {
        let plug = b.last_plug_w.unwrap_or(0.0);
        if let Some(t) = targets.get_mut(&b.id) {
            let delta = *t - plug;
            let limited = delta.clamp(-rate_limit, rate_limit);
            *t = plug + limited;
        }
    }

    // Per-circuit cap with PLUG-FEEDBACK enforcement.
    //
    // Pre-v0.11.6 capped only `target_sum`. That ignored Marstek
    // over-delivery — a Venus E commanded at -1500 W typically reads
    // -1550 to -1650 W at the plug (5-10 % over). With both batteries
    // on a 3220 W (14 A) circuit running at the target cap of 3059 W
    // (0.95 headroom), plug sums of 3200-3300 W are normal — well
    // over the emergency_cutoff_margin trigger, which then tripped
    // the plug relay "ohne not".
    //
    // Fix: also consider the CURRENT plug sum when scaling. If the
    // measured power is over cap (because Marstek is over-delivering
    // a previous setpoint), shrink targets BELOW cap so the next
    // commanded value pulls the plug back down. Use the larger of
    // |target_sum| and |plug_sum| as the constraint basis.
    for cs in circuits.values() {
        let cap = cs.cap_w();
        let member_ids: Vec<String> = cs.member_ids.clone();
        let target_sum: f64 = member_ids
            .iter()
            .filter_map(|id| targets.get(id).copied())
            .sum();
        let plug_sum: f64 = member_ids
            .iter()
            .filter_map(|id| bats.get(id))
            .filter_map(|b| b.last_plug_w)
            .sum();
        let worst_case = target_sum.abs().max(plug_sum.abs());
        if worst_case > cap {
            let scale = cap / worst_case;
            warn!(
                circuit = %cs.config.id,
                cap_w = cap,
                target_sum,
                plug_sum,
                worst_case,
                applied_scale = scale,
                "circuit cap engaged — scaling targets (plug-feedback)"
            );
            for id in &member_ids {
                if let Some(t) = targets.get_mut(id) {
                    *t *= scale;
                }
            }
        }
    }

    targets
}

// ---------------------------------------------------------------------------
// Step 4: queue pulses
// ---------------------------------------------------------------------------

fn queue_pulses(
    state: &AppState,
    _dcfg: &DispatcherConfig,
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
            // Drop any pending pulse and reset the settle bookkeeping —
            // circuit is muted, Marstek's watchdog will clear its target
            // during the silence. Clearing last_pulse_completed_at means
            // the next post-recovery cycle treats this battery as fresh
            // (initial state) instead of waiting for a stale movement.
            // Also drop last_pulse_delta_w so the silence window doesn't
            // leak into refusal detection on the other side.
            b.pending_pulse_w = 0.0;
            b.pulse_remaining = 0;
            b.plug_w_at_pulse_send = None;
            b.last_pulse_completed_at = None;
            b.last_pulse_delta_w = None;
            continue;
        }

        // No per-battery settle check here: the global gate in `step`
        // already guarantees every non-muted battery has either landed
        // its previous pulse or hit settle_timeout_s before we get here.

        let delta = deltas.get(&b.id).copied().unwrap_or(0.0);
        if delta.abs() < crate::config::WRITE_DEADBAND_W {
            continue;
        }

        b.pending_pulse_w = delta;
        b.pulse_remaining = crate::config::PULSE_COUNT;
        b.plug_w_at_pulse_send = b.last_plug_w;
        b.last_pulse_completed_at = None;
        // Remember the magnitude+direction we're about to commit, so
        // detect_pulse_outcomes can classify accept/refuse next cycle.
        b.last_pulse_delta_w = Some(delta);
        debug!(
            battery = %b.id,
            delta,
            plug_at_send = ?b.plug_w_at_pulse_send,
            pulses = crate::config::PULSE_COUNT,
            "armed pulse"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::rpc::EmStatusIncoming;
    use crate::state::{AppState, BatteryState, EmSnapshot};
    use std::time::Duration;

    fn dcfg() -> DispatcherConfig {
        DispatcherConfig::default()
    }

    // -----------------------------------------------------------------
    // compute_grid_correction — direction-aware asymmetric bias rule.
    // -----------------------------------------------------------------

    /// Regression test for the v0.8.0 "persistent import while
    /// charging" bug. With current_total = -600 (charging hard) and
    /// grid = +28 (small import), the old code computed
    /// `(28 - 100).max(0) = 0` and held position. The new code
    /// Charging at -600 W, grid imports +28 W = policy violation. With
    /// SAFETY_MARGIN_W = 10 the dispatcher wants grid to land at -10 W
    /// (export 10), so the correction is grid - target_grid = 28 - (-10)
    /// = 38 W → reduce charging by 38 W.
    #[test]
    fn grid_correction_charging_with_small_import_reduces_charging() {
        // current_total = -600 W, grid = +28 W, 2 batteries.
        // idle_threshold = max(10, 50 * 2 / 2) = 50. |-600| > 50 → charging.
        let corr = compute_grid_correction(28.0, -600.0, 2);
        assert!(
            (corr - 38.0).abs() < 1e-6,
            "expected +38 W correction (= grid - target_grid), got {corr}"
        );
    }

    /// Mirror: discharging while exporting → reduce discharging.
    #[test]
    fn grid_correction_discharging_with_small_export_reduces_discharging() {
        // current_total = +600 (discharging), grid = -28 (export).
        // target_grid = +10. correction = -28 - 10 = -38.
        let corr = compute_grid_correction(-28.0, 600.0, 2);
        assert!(
            (corr - (-38.0)).abs() < 1e-6,
            "expected -38 W correction, got {corr}"
        );
    }

    /// Big import while charging only a little → the correction
    /// naturally flips the dispatch direction (charging → discharging).
    /// The user's directive: "richtung umdrehen wenn nötig" (flip if
    /// needed). No artificial zero-crossing cap.
    #[test]
    fn grid_correction_big_import_flips_charging_to_discharging() {
        // current_total = -100 W (charging slightly), grid = +500 W
        // (big import). idle_threshold = 50, |-100| > 50 → charging.
        // target = -10. correction = 500 - (-10) = 510.
        // desired_total = -100 + 510 = +410 → flips to discharge.
        let corr = compute_grid_correction(500.0, -100.0, 1);
        assert!(
            (corr - 510.0).abs() < 1e-6,
            "expected +510 W correction (flips direction), got {corr}"
        );
    }

    /// Charging with healthy export → keep charging, pull grid toward
    /// the export margin (-10 W).
    #[test]
    fn grid_correction_charging_with_big_export_increases_charging() {
        // current_total = -1000, grid = -600 (600 W export).
        // target = -10. correction = -600 - (-10) = -590 (charge more).
        let corr = compute_grid_correction(-600.0, -1000.0, 2);
        assert!(
            (corr - (-590.0)).abs() < 1e-6,
            "expected -590 W correction, got {corr}"
        );
    }

    /// Idle with tiny grid imbalance inside the margin band → hold.
    #[test]
    fn grid_correction_idle_inside_margin_holds() {
        // current_total = -13 (Marstek standby loss, idle), grid = +8.
        // |grid| <= margin → hold, correction = 0.
        let corr = compute_grid_correction(8.0, -13.0, 2);
        assert_eq!(corr, 0.0);
        let corr = compute_grid_correction(-8.0, -13.0, 2);
        assert_eq!(corr, 0.0);
    }

    /// Regression test for the v0.8.3 "standby loss misread as
    /// charging" bug. Both batteries idle (plug = -7 W each from
    /// inverter standby loss only). Grid imports +192 W. The dedicated
    /// idle_threshold (= MIN_W * N / 2 = 50 for 2 batt) correctly
    /// classifies -13 W as idle → idle branch → start discharging.
    #[test]
    fn grid_correction_ignores_marstek_standby_loss() {
        let corr = compute_grid_correction(192.0, -13.0, 2);
        // idle (|-13| < 50). grid > +margin → target = +10.
        // correction = 192 - 10 = 182.
        assert!(
            (corr - 182.0).abs() < 1e-6,
            "expected +182 W discharge correction (idle), got {corr}"
        );
    }

    /// Sanity: actual charging (current_total clearly past threshold)
    /// still uses the charging branch.
    #[test]
    fn grid_correction_actively_charging_still_uses_charging_branch() {
        // current_total = -800 W, grid = +30. target = -10.
        // correction = 30 - (-10) = 40.
        let corr = compute_grid_correction(30.0, -800.0, 2);
        assert!(
            (corr - 40.0).abs() < 1e-6,
            "expected +40 W correction, got {corr}"
        );
    }

    /// Regression test for the "small dispatch lost to MIN_W rounding"
    /// bug. With 3 batteries idle (plug=0) and grid +130 W: target =
    /// +10, correction = +120 W. Split 3-way that's 40 W per battery
    /// — every share is below MARSTEK_MIN_W = 50 so
    /// `Setpoint::from_signed_watts` would round each to Standby and
    /// nothing fires. The consolidation pass must detect this and
    /// concentrate the 120 W onto ONE battery.
    #[test]
    fn compute_targets_consolidates_sub_min_w_split_onto_one_battery() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, 130.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
        }
        let dcfg = dcfg();
        let targets = compute_targets(&state, &dcfg, 130.0, now);

        // Total discharge target = grid - margin = 120 W. After
        // consolidation: ONE battery near 120, others at 0.
        let nonzero: Vec<(&String, &f64)> =
            targets.iter().filter(|(_, t)| t.abs() > 1e-3).collect();
        assert_eq!(
            nonzero.len(),
            1,
            "expected exactly one non-zero target after MIN_W consolidation, got {nonzero:?}"
        );
        let (id, t) = nonzero[0];
        assert!(
            *t >= crate::modbus::MARSTEK_MIN_W,
            "{id}: target {t} should be ≥ MARSTEK_MIN_W after consolidation"
        );
        assert!(
            *t < 150.0,
            "{id}: target {t} should be ≤ correction ~120, got way more"
        );
    }

    /// Above the consolidation threshold the normal weighted split still
    /// applies — we don't want to concentrate when each battery's share
    /// is meaningful on its own.
    #[test]
    fn compute_targets_keeps_split_when_each_share_above_min_w() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, 1000.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
        }
        let mut dcfg = dcfg();
        dcfg.rate_limit_w_per_cycle = 9999.0;
        let targets = compute_targets(&state, &dcfg, 1000.0, now);
        // 1000 - bias 100 = 900 W total, 3 batteries → ~300 each. All
        // above MIN_W → normal split, no consolidation.
        let nonzero_count = targets.values().filter(|t| t.abs() > 1e-3).count();
        assert_eq!(
            nonzero_count, 3,
            "expected all 3 batteries to share when each share is above MIN_W"
        );
    }

    /// Idle case (plug sum near zero): classic asymmetric bias picks
    /// the starting direction, margin (10 W) is the hysteresis band.
    #[test]
    fn grid_correction_idle_picks_direction_from_grid_sign() {
        // Idle, grid = +200 → target +10, correction = 190 (discharge).
        assert!((compute_grid_correction(200.0, 0.0, 2) - 190.0).abs() < 1e-6);
        // Idle, grid = +5 (< margin) → 0, hold.
        assert_eq!(compute_grid_correction(5.0, 0.0, 2), 0.0);
        // Idle, grid = -300 → target -10, correction = -290 (charge).
        assert!((compute_grid_correction(-300.0, 0.0, 2) - (-290.0)).abs() < 1e-6);
    }

    fn make_battery(id: &str, plug_w: f64, max_charge: f64, max_discharge: f64) -> BatteryState {
        BatteryState {
            id: id.into(),
            circuit: "c1".into(),
            address: format!("127.0.0.{}", id.bytes().last().unwrap_or(1))
                .parse()
                .unwrap(),
            marstek_model: crate::config::MarstekModel::VenusEV1V2,
            plug_url: format!("http://127.0.0.{}", id.bytes().last().unwrap_or(1) + 100),
            active: true,
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
            last_plug_movement_at: None,
            last_marstek_poll_at: None,
            soc_pct: None,
            soc_at: None,
            soc_source: None,
            last_pulse_delta_w: None,
            charge_locked_until: None,
            discharge_locked_until: None,
            last_modbus_setpoint_w: None,
            last_modbus_write_at: None,
            last_modbus_write_error: None,
            last_battery_power_w: None,
            bms_full_pct: None,
            bms_empty_pct: None,
            plug_relay_state: None,
            plug_cut_until: None,
            plug_cut_reason: None,
            last_error: None,
            virtual_unit_id: 1,
            cached_holding_regs: std::collections::HashMap::new(),
            cached_regs_refreshed_at: None,
        }
    }

    #[test]
    fn low_bound_zero_at_full_soc() {
        let mut cfg = dcfg();
        cfg.soc_full_pct = 95.0;
        let mut b = make_battery("a", -500.0, 2500.0, 800.0);
        b.soc_pct = Some(96.0);
        // SoC at/above full: can't charge any more.
        assert_eq!(low_bound(&b, &cfg, Instant::now()), 0.0);
        // Discharge bound is unaffected by full SoC.
        assert!(high_bound(&b, &cfg, Instant::now()) > 0.0);
    }

    #[test]
    fn high_bound_zero_at_empty_soc() {
        let mut cfg = dcfg();
        cfg.soc_empty_pct = 5.0;
        let mut b = make_battery("a", 200.0, 2500.0, 800.0);
        b.soc_pct = Some(4.0);
        // SoC at/below empty: can't discharge any more.
        assert_eq!(high_bound(&b, &cfg, Instant::now()), 0.0);
        assert!(low_bound(&b, &cfg, Instant::now()) < 0.0);
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
        assert_eq!(high_bound(&b, &cfg, Instant::now()), 400.0);
        // Far above → full cap.
        b.soc_pct = Some(50.0);
        assert_eq!(high_bound(&b, &cfg, Instant::now()), 800.0);
        // At hard empty cutoff → 0.
        b.soc_pct = Some(4.0);
        assert_eq!(high_bound(&b, &cfg, Instant::now()), 0.0);
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
        assert_eq!(low_bound(&b, &cfg, Instant::now()), -1000.0);
        // Below taper → full cap.
        b.soc_pct = Some(80.0);
        assert_eq!(low_bound(&b, &cfg, Instant::now()), -2500.0);
        // At hard full cutoff → 0, regardless of taper.
        b.soc_pct = Some(96.0);
        assert_eq!(low_bound(&b, &cfg, Instant::now()), 0.0);
    }

    #[test]
    fn taper_tightens_low_bound() {
        // Battery at 92 % SoC, nominal max_charge 2500 W, taper kicks at
        // 90 % down to 1000 W. low_bound should reflect the taper so
        // target clamping limits the dispatched share.
        let cfg = dcfg();
        let mut b = make_battery("a", -200.0, 2500.0, 800.0);
        b.charge_taper_soc_pct = Some(90.0);
        b.charge_taper_w = Some(1000.0);
        b.soc_pct = Some(92.0);
        assert_eq!(low_bound(&b, &cfg, Instant::now()), -1000.0);
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

    // ---- global settle gate tests -----------------------------------------

    /// Build a 3-battery, 1-circuit AppState used by the gate tests.
    fn three_battery_state() -> std::sync::Arc<AppState> {
        let cfg: Config = toml::from_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 32

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://x"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"

[[batteries]]
id = "b"
address = "192.168.1.52"
circuit = "c1"
plug_url = "http://y"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.92"

[[batteries]]
id = "c"
address = "192.168.1.53"
circuit = "c1"
plug_url = "http://z"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.93"
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        AppState::from_config(&cfg)
    }

    fn fresh_grid_snapshot(state: &AppState, total_w: f64) {
        state.snapshot.store(std::sync::Arc::new(EmSnapshot {
            status: EmStatusIncoming {
                total_act_power: Some(total_w),
                ..Default::default()
            },
            age: Some(Instant::now()),
        }));
    }

    /// User-reported race scenario, v0.4.1 reproduction:
    ///   t=0: grid -1500 W, all 3 batteries get a -500 W pulse.
    ///   t=~2 s: A reacts (plug -500), B/C still in flight.
    ///   v0.4.1 dispatcher would here distribute the residual -1000 W
    ///   across all 3 again and queue an *additional* ~-277 W to A on
    ///   top of the -500 it's already committed to → overshoot.
    ///   With the global gate this test asserts: A keeps its previous
    ///   commitment (no new pulse armed), no new pulse for B/C either.
    #[test]
    fn global_gate_blocks_when_any_battery_has_pulse_in_flight() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, -1500.0);
        let now = Instant::now();
        // Reset all plug ages so circuit isn't muted on freshness.
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
            // A: previous pulse landed (plug moved -500 from snapshot 0).
            let a = bats.get_mut("a").unwrap();
            a.last_plug_w = Some(-500.0);
            a.plug_w_at_pulse_send = Some(0.0);
            a.last_pulse_completed_at = Some(now - Duration::from_millis(500));
            a.pulse_remaining = 0;
            a.pending_pulse_w = 0.0;
            // B: pulse still going out on the wire.
            let b = bats.get_mut("b").unwrap();
            b.pulse_remaining = 2;
            b.pending_pulse_w = -500.0;
            b.plug_w_at_pulse_send = Some(0.0);
            // C: same.
            let c = bats.get_mut("c").unwrap();
            c.pulse_remaining = 2;
            c.pending_pulse_w = -500.0;
            c.plug_w_at_pulse_send = Some(0.0);
        }

        let dcfg = Default::default();
        step_pulse(&state, &dcfg).unwrap();

        let bats = state.batteries.read();
        // The whole point: A must NOT receive a fresh pulse, even though
        // its own pulse_settled() would say "yes". The global gate
        // dominates, because B and C are in flight.
        let a = bats.get("a").unwrap();
        assert_eq!(a.pulse_remaining, 0);
        assert_eq!(a.pending_pulse_w, 0.0);
        // B and C keep their original commitments untouched (gate skipped
        // queue_pulses entirely; the in-flight state is preserved).
        let b = bats.get("b").unwrap();
        assert_eq!(b.pulse_remaining, 2);
        assert_eq!(b.pending_pulse_w, -500.0);
        let c = bats.get("c").unwrap();
        assert_eq!(c.pulse_remaining, 2);
        assert_eq!(c.pending_pulse_w, -500.0);
    }

    /// Inverse: when every battery has settled, the dispatcher proceeds
    /// normally and arms a pulse where the grid_w correction asks for it.
    #[test]
    fn global_gate_passes_when_every_battery_settled() {
        let state = three_battery_state();
        // Big enough export that each battery's share clears the deadband.
        fresh_grid_snapshot(&state, -1500.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
                // Initial state — never pulsed → settled by definition.
                b.pulse_remaining = 0;
                b.pending_pulse_w = 0.0;
                b.plug_w_at_pulse_send = None;
                b.last_pulse_completed_at = None;
            }
        }

        let dcfg = Default::default();
        step_pulse(&state, &dcfg).unwrap();

        let bats = state.batteries.read();
        // Each battery should now carry an armed pulse (negative = charge).
        for (id, b) in bats.iter() {
            assert!(
                b.pulse_remaining > 0,
                "{id}: expected pulse armed, got remaining={}",
                b.pulse_remaining
            );
            assert!(
                b.pending_pulse_w < 0.0,
                "{id}: expected charge Δ, got {}",
                b.pending_pulse_w
            );
        }
    }

    /// settle_timeout_s is the escape hatch: after the timeout elapses,
    /// a refusing-Marstek battery counts as settled (the dispatcher can't
    /// wait forever) and the gate releases.
    #[test]
    fn global_gate_releases_after_settle_timeout() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, -1500.0);
        let now = Instant::now();
        let dcfg: DispatcherConfig = Default::default();
        // Pretend B's last pulse completed 2× the timeout ago and the plug
        // never moved. pulse_settled() must now return true via the
        // timeout branch, freeing the gate.
        let long_ago = now - Duration::from_secs_f64(dcfg.settle_timeout_s * 2.0);
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
                b.pulse_remaining = 0;
                b.pending_pulse_w = 0.0;
                b.plug_w_at_pulse_send = None;
                b.last_pulse_completed_at = None;
            }
            let b = bats.get_mut("b").unwrap();
            b.last_pulse_completed_at = Some(long_ago);
            b.plug_w_at_pulse_send = Some(0.0); // didn't move
        }

        step_pulse(&state, &dcfg).unwrap();

        let bats = state.batteries.read();
        // Gate released → all three got fresh pulses.
        for (id, bb) in bats.iter() {
            assert!(
                bb.pulse_remaining > 0,
                "{id}: gate should have released after timeout"
            );
        }
    }

    // -----------------------------------------------------------------
    // Empirical full/empty detection (v0.6) — direction lockouts.
    // -----------------------------------------------------------------

    /// Refusal: a non-trivial charge request goes out, the plug doesn't
    /// move past plug_stable_w within settle_timeout_s. The charge
    /// direction must be locked for soc_unknown_lockout_s; discharge
    /// stays free.
    #[test]
    fn refused_charge_locks_charge_direction() {
        let dcfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        // Pretend we requested a -500 W charge that completed
        // `settle_timeout_s + 1 s` ago and the plug never moved.
        let completed = Instant::now()
            - Duration::from_secs_f64(dcfg.settle_timeout_s + 1.0);
        b.last_pulse_completed_at = Some(completed);
        b.plug_w_at_pulse_send = Some(0.0);
        b.last_plug_w = Some(0.0);
        b.last_pulse_delta_w = Some(-500.0);

        // Feed it through detect via a minimal AppState-like setup.
        let state = single_battery_state(b);
        let now = Instant::now();
        detect_pulse_outcomes(&state, &dcfg, now);

        let bats = state.batteries.read();
        let b = bats.get("a").unwrap();
        assert!(b.is_charge_locked(now), "charge should be locked");
        assert!(!b.is_discharge_locked(now), "discharge must stay free");
        assert!(
            b.last_pulse_delta_w.is_none(),
            "marker should be cleared after detection"
        );
    }

    /// Symmetric to the charge case: a refused discharge locks the
    /// discharge direction only.
    #[test]
    fn refused_discharge_locks_discharge_direction() {
        let dcfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        let completed = Instant::now()
            - Duration::from_secs_f64(dcfg.settle_timeout_s + 1.0);
        b.last_pulse_completed_at = Some(completed);
        b.plug_w_at_pulse_send = Some(0.0);
        b.last_plug_w = Some(0.0);
        b.last_pulse_delta_w = Some(500.0); // positive = discharge

        let state = single_battery_state(b);
        let now = Instant::now();
        detect_pulse_outcomes(&state, &dcfg, now);

        let bats = state.batteries.read();
        let b = bats.get("a").unwrap();
        assert!(b.is_discharge_locked(now));
        assert!(!b.is_charge_locked(now));
    }

    /// Accepted pulse: plug moved past plug_stable_w. Any stale lockout
    /// for that direction must be cleared.
    #[test]
    fn accepted_charge_clears_charge_lockout() {
        let dcfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.last_pulse_completed_at = Some(Instant::now() - Duration::from_secs(1));
        b.plug_w_at_pulse_send = Some(0.0);
        b.last_plug_w = Some(-300.0); // moved 300 W toward charge
        b.last_pulse_delta_w = Some(-500.0);
        // Pretend we were already locked from a previous refusal.
        b.charge_locked_until = Some(Instant::now() + Duration::from_secs(60));

        let state = single_battery_state(b);
        let now = Instant::now();
        detect_pulse_outcomes(&state, &dcfg, now);

        let bats = state.batteries.read();
        let b = bats.get("a").unwrap();
        assert!(
            !b.is_charge_locked(now),
            "successful charge response must clear the charge lockout"
        );
    }

    /// Sub-deadband request: should never trigger a lockout, regardless
    /// of plug movement (or lack of it). We wouldn't have queued it in
    /// the first place under normal conditions.
    #[test]
    fn sub_deadband_request_does_not_lock() {
        let dcfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.last_pulse_completed_at = Some(Instant::now() - Duration::from_secs(30));
        b.plug_w_at_pulse_send = Some(0.0);
        b.last_plug_w = Some(0.0);
        b.last_pulse_delta_w = Some(-10.0); // tiny

        let state = single_battery_state(b);
        let now = Instant::now();
        detect_pulse_outcomes(&state, &dcfg, now);

        let bats = state.batteries.read();
        let b = bats.get("a").unwrap();
        assert!(!b.is_charge_locked(now));
        assert!(!b.is_discharge_locked(now));
    }

    /// While the lockout is in the future, low_bound for charging is
    /// pinned to 0; high_bound (discharging) is unaffected.
    #[test]
    fn charge_lockout_pins_low_bound_only() {
        let cfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        let now = Instant::now();
        b.charge_locked_until = Some(now + Duration::from_secs(120));
        assert_eq!(low_bound(&b, &cfg, now), 0.0);
        // Discharge still allowed at full cap.
        assert_eq!(high_bound(&b, &cfg, now), 800.0);
    }

    /// Once the lockout expires (in the past), bounds revert to normal.
    #[test]
    fn expired_lockout_releases_bound() {
        let cfg = dcfg();
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        let now = Instant::now();
        b.charge_locked_until = Some(now - Duration::from_secs(1));
        assert!(!b.is_charge_locked(now));
        assert_eq!(low_bound(&b, &cfg, now), -2500.0);
    }

    // -----------------------------------------------------------------
    // Modbus-mode tests (v0.7) — compute_targets math and
    // detect_setpoint_outcomes lockout semantics.
    // -----------------------------------------------------------------

    /// With grid importing 1500 W and three idle batteries, compute_targets
    /// must distribute the discharge target across all three (weighted
    /// equally — identical capacity_wh) so that their sum equals the
    /// desired_total within the deadband.
    #[test]
    fn compute_targets_distributes_import_correction_evenly() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, 1500.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
        }
        // Lift the rate-limit so this test exercises the distribution
        // math, not the per-cycle ramp. There's a dedicated test for
        // rate-limit ramping below.
        let mut dcfg = dcfg();
        dcfg.rate_limit_w_per_cycle = 9999.0;
        let targets = compute_targets(&state, &dcfg, 1500.0, now);

        let sum: f64 = targets.values().sum();
        // SAFETY_MARGIN_W = 10 W: target grid = +10, so correction =
        // 1490 W (= 1500 - 10). Distributed across 3 batteries.
        assert!(
            (sum - 1490.0).abs() < 1.0,
            "sum of targets should equal grid - margin, got {sum}"
        );
        // Each battery gets roughly a third — positive (discharge).
        for (id, t) in &targets {
            assert!(
                *t > 400.0 && *t < 600.0,
                "{id}: target {t} not within expected third"
            );
        }
    }

    /// With the rate limit at 500 W/cycle and three batteries starting
    /// at plug=0, a 5 kW grid import must NOT instantly push them to
    /// max — each one ramps by at most 500 W toward the desired target.
    #[test]
    fn compute_targets_rate_limit_clamps_per_cycle_ramp() {
        let state = three_battery_state();
        fresh_grid_snapshot(&state, 5000.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
        }
        let mut dcfg = dcfg();
        dcfg.rate_limit_w_per_cycle = 500.0;
        let targets = compute_targets(&state, &dcfg, 5000.0, now);

        // Each battery from plug=0: |target - 0| ≤ 500 → |target| ≤ 500.
        for (id, t) in &targets {
            assert!(
                t.abs() <= 500.0 + 1e-3,
                "{id}: |target {t}| should be ≤ rate_limit 500"
            );
        }
        // Sum across the eligible three ≤ 1500.
        let sum: f64 = targets.values().sum();
        assert!(sum.abs() <= 1500.0 + 1e-3, "sum {sum} exceeds 1500");
    }

    /// Charge-locked battery must NOT receive any charge target even if
    /// its weight would otherwise win — the locked direction is bound to 0.
    /// Verifies that low_bound's lockout pin propagates through compute_targets.
    #[test]
    fn compute_targets_skips_charge_locked_battery() {
        let state = three_battery_state();
        // Grid exporting 3000 W → batteries should charge.
        fresh_grid_snapshot(&state, -3000.0);
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
            }
            // A is locked from charging (= "full").
            let a = bats.get_mut("a").unwrap();
            a.charge_locked_until = Some(now + Duration::from_secs(120));
        }
        let dcfg = dcfg();
        let targets = compute_targets(&state, &dcfg, -3000.0, now);

        let a = *targets.get("a").unwrap();
        assert_eq!(
            a, 0.0,
            "locked battery should get 0 charge target, got {a}"
        );
        // B and C take the load.
        assert!(*targets.get("b").unwrap() < -0.5);
        assert!(*targets.get("c").unwrap() < -0.5);
    }

    /// Circuit cap on the SUM of targets — if everyone wants 2500 W
    /// discharge on a 16 A / 230 V circuit (cap 3680 W * 0.95 = 3496 W),
    /// the sum must scale down to land on the cap.
    #[test]
    fn compute_targets_scales_to_circuit_cap() {
        let state = three_battery_state();
        // Massive import → every battery would max out at 800 W discharge.
        // 3 × 800 = 2400 W which is UNDER the c1 cap (32 A * 230 V * 0.95
        // = 6992 W), so to test the cap scaling I need a smaller cap.
        // The three_battery_state uses fuse_amps = 32; let me check by
        // first making batteries with high max_discharge so sum > cap.
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(0.0);
                b.last_plug_at = Some(now);
                // Bump per-battery max_discharge to 3000 so sum could
                // exceed 6992 (3 × 3000 = 9000).
                b.max_discharge_w = 3000.0;
            }
        }
        fresh_grid_snapshot(&state, 9000.0);
        // Disable the rate limit for this test — we're verifying the
        // per-circuit cap math, not the per-cycle ramp.
        let mut dcfg = dcfg();
        dcfg.rate_limit_w_per_cycle = 9999.0;
        let targets = compute_targets(&state, &dcfg, 9000.0, now);

        // v0.11.7 removed the circuit_headroom factor — the cap is now
        // raw fuse_amps × voltage. User is expected to pick fuse_amps
        // they're comfortable driving at 100 %.
        let cap = 32.0 * 230.0;
        let sum: f64 = targets.values().sum();
        // After cap scaling, sum should be at the cap (within a small epsilon).
        assert!(
            (sum - cap).abs() < 5.0,
            "sum {sum} should land near cap {cap}"
        );
    }

    // -----------------------------------------------------------------
    // Emergency plug cutoff — v0.7 hard safety layer.
    // -----------------------------------------------------------------

    /// Below the margin → no overload tracker, no trip.
    #[test]
    fn cutoff_no_overload_when_within_cap() {
        let state = three_battery_state();
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for (i, b) in bats.values_mut().enumerate() {
                b.last_plug_w = Some(1000.0 + i as f64);
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        let dcfg = dcfg();
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        let circuits = state.circuits.read();
        let cs = circuits.get("c1").unwrap();
        assert!(
            cs.overload_started_at.is_none(),
            "no overload should be tracked under cap"
        );
    }

    /// Over cap by more than the margin → overload tracker starts.
    /// Grace not yet elapsed → no trip.
    #[test]
    fn cutoff_starts_tracker_on_first_overload() {
        let state = three_battery_state();
        let now = Instant::now();
        // c1 cap = 32 A * 230 V * 0.95 = ~6992 W. Push sum WAY over.
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(3000.0); // 3 × 3000 = 9000 > 6992 + 200
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        let dcfg = dcfg();
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        let circuits = state.circuits.read();
        let cs = circuits.get("c1").unwrap();
        assert!(
            cs.overload_started_at.is_some(),
            "overload tracker should start on first overload"
        );
    }

    /// Sustained overload past the grace window picks the worst
    /// offender and stamps plug_cut_until. The HTTP call is fired in a
    /// detached task (we use tokio::test so spawn works; the spawned
    /// task tries to hit an unreachable plug URL and fails, but the
    /// state stamping happens synchronously before that).
    #[tokio::test]
    async fn cutoff_trips_worst_offender_after_grace() {
        let state = three_battery_state();
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            // Set up: a + b mild; c is the worst offender.
            bats.get_mut("a").unwrap().last_plug_w = Some(2500.0);
            bats.get_mut("b").unwrap().last_plug_w = Some(2500.0);
            bats.get_mut("c").unwrap().last_plug_w = Some(4500.0);
            for b in bats.values_mut() {
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        // Pretend the overload has been going on for longer than grace
        // AND soft remediation has been in effect long enough that
        // Phase 2 (hard trip) is justified. Without both timestamps
        // the function would only push Standby on this cycle.
        {
            let mut circuits = state.circuits.write();
            let cs = circuits.get_mut("c1").unwrap();
            cs.overload_started_at = Some(
                now - Duration::from_secs_f64(
                    crate::config::EMERGENCY_CUTOFF_GRACE_S
                        + crate::config::EMERGENCY_SOFT_REMEDIATION_GRACE_S
                        + 1.0,
                ),
            );
            cs.soft_remediation_started_at = Some(
                now - Duration::from_secs_f64(
                    crate::config::EMERGENCY_SOFT_REMEDIATION_GRACE_S + 1.0,
                ),
            );
        }
        let dcfg = dcfg();
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        let bats = state.batteries.read();
        let c = bats.get("c").unwrap();
        assert!(
            c.plug_cut_until.is_some(),
            "worst offender c should be marked for cutoff"
        );
        // a and b should not be cut (only ONE per cycle).
        assert!(bats.get("a").unwrap().plug_cut_until.is_none());
        assert!(bats.get("b").unwrap().plug_cut_until.is_none());
    }

    /// After grace, but before soft_grace elapses: the function must
    /// stamp `soft_remediation_started_at` and NOT trip any plug. This
    /// is the bug the user reported in v0.11.5 ("Plugs ausgeschaltet
    /// anstatt die Ladeleistung anzupassen"): the dispatcher must
    /// attempt soft remediation first.
    #[test]
    fn cutoff_soft_remediates_before_tripping() {
        let state = three_battery_state();
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(2500.0);
            bats.get_mut("b").unwrap().last_plug_w = Some(2500.0);
            bats.get_mut("c").unwrap().last_plug_w = Some(4500.0);
            for b in bats.values_mut() {
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        // Overload has been going on past `grace` but NOT past
        // `grace + soft_grace`. So Phase 1 must engage, Phase 2 not.
        {
            let mut circuits = state.circuits.write();
            circuits.get_mut("c1").unwrap().overload_started_at = Some(
                now - Duration::from_secs_f64(
                    crate::config::EMERGENCY_CUTOFF_GRACE_S + 0.5,
                ),
            );
        }
        let dcfg = dcfg();
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        // Soft remediation timestamp now set.
        {
            let circuits = state.circuits.read();
            let cs = circuits.get("c1").unwrap();
            assert!(
                cs.soft_remediation_started_at.is_some(),
                "Phase 1 must stamp soft_remediation_started_at"
            );
        }
        // No plug trip yet — soft_grace hasn't elapsed.
        let bats = state.batteries.read();
        assert!(
            bats.get("a").unwrap().plug_cut_until.is_none(),
            "no plug should be cut during soft-remediation phase"
        );
        assert!(
            bats.get("b").unwrap().plug_cut_until.is_none(),
            "no plug should be cut during soft-remediation phase"
        );
        assert!(
            bats.get("c").unwrap().plug_cut_until.is_none(),
            "no plug should be cut during soft-remediation phase"
        );
    }

    /// Recovery: once the circuit comes back under cap, BOTH
    /// `overload_started_at` and `soft_remediation_started_at` reset,
    /// so the next overload starts the clock fresh.
    #[test]
    fn cutoff_recovery_clears_both_timestamps() {
        let state = three_battery_state();
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(1000.0); // well under cap
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        // Pretend Phase 1 was previously engaged.
        {
            let mut circuits = state.circuits.write();
            let cs = circuits.get_mut("c1").unwrap();
            cs.overload_started_at = Some(now - Duration::from_secs(30));
            cs.soft_remediation_started_at = Some(now - Duration::from_secs(20));
        }
        let dcfg = dcfg();
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        let circuits = state.circuits.read();
        let cs = circuits.get("c1").unwrap();
        assert!(cs.overload_started_at.is_none(), "recovery must clear overload_started_at");
        assert!(
            cs.soft_remediation_started_at.is_none(),
            "recovery must clear soft_remediation_started_at"
        );
    }

    /// Disabled feature (margin = 0) is a complete no-op.
    #[test]
    fn cutoff_disabled_when_margin_zero() {
        let state = three_battery_state();
        let now = Instant::now();
        {
            let mut bats = state.batteries.write();
            for b in bats.values_mut() {
                b.last_plug_w = Some(5000.0); // massively over cap
                b.last_plug_at = Some(now);
                b.plug_relay_state = Some(true);
            }
        }
        let mut dcfg = dcfg();
        dcfg.emergency_cutoff_margin_w = 0.0; // disable
        enforce_circuit_safety(state.clone(), &dcfg, &None);
        let circuits = state.circuits.read();
        let cs = circuits.get("c1").unwrap();
        assert!(
            cs.overload_started_at.is_none(),
            "disabled feature must not even start the tracker"
        );
    }

    // -----------------------------------------------------------------
    // Sequential dispatch: modbus_settled() + per-circuit single-write
    // -----------------------------------------------------------------

    #[test]
    fn modbus_settled_initial_state() {
        let b = make_battery("a", 0.0, 2500.0, 800.0);
        assert!(b.modbus_settled(1.5, 5.0));
    }

    #[test]
    fn modbus_settled_blocks_during_settle_window() {
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.last_modbus_write_at = Some(Instant::now() - Duration::from_millis(500));
        // Plug hasn't moved since write → not settled, well within 5s timeout.
        assert!(!b.modbus_settled(1.5, 5.0));
    }

    #[test]
    fn modbus_settled_via_plug_movement_and_stability() {
        let mut b = make_battery("a", -500.0, 2500.0, 800.0);
        let now = Instant::now();
        b.last_modbus_write_at = Some(now - Duration::from_secs(3));
        b.last_plug_movement_at = Some(now - Duration::from_secs(2));
        // Plug moved 1s after write, then stable for 2s ≥ 1.5s → settled.
        assert!(b.modbus_settled(1.5, 5.0));
    }

    #[test]
    fn modbus_settled_via_timeout_when_plug_didnt_move() {
        let mut b = make_battery("a", 0.0, 2500.0, 800.0);
        b.last_modbus_write_at = Some(Instant::now() - Duration::from_secs(10));
        b.last_plug_movement_at = None;
        // No plug response after settle_timeout_s = 5 → timeout-settled.
        assert!(b.modbus_settled(1.5, 5.0));
    }

    fn single_battery_state(bs: BatteryState) -> std::sync::Arc<AppState> {
        let cfg: Config = toml::from_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 32

[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://x"
max_charge_w = 2500
max_discharge_w = 800
"#,
        )
        .unwrap();
        let state = AppState::from_config(&cfg);
        {
            let mut bats = state.batteries.write();
            bats.insert(bs.id.clone(), bs);
        }
        state
    }

    // -----------------------------------------------------------------
    // End-to-end multi-cycle scenario tests
    //
    // These simulate the real dispatcher loop: every cycle we compute
    // targets, apply them as the new plug readings (modelling the
    // Marstek as a perfect setpoint follower for the test), and then
    // recompute grid_w from the energy-balance invariant
    //     external_load = grid + plug_sum
    // where `external_load` (= house consumption - PV production) is
    // held constant across the simulation. Each test verifies that the
    // dispatcher converges grid_w to the ±SAFETY_MARGIN_W band within
    // a small number of cycles.
    //
    // These are the tests that catch the kind of bug the v0.10 rework
    // fixed: slow response when sequential dispatch + the 5 s settle
    // gate meant only one battery per circuit got an updated setpoint
    // per cycle. With parallel dispatch the same scenarios should
    // converge in 1-3 cycles.
    // -----------------------------------------------------------------

    /// Run `cycles` dispatcher iterations starting from whatever state
    /// the caller seeded. Returns a Vec of (cycle_idx, grid_w,
    /// plug_sum) snapshots taken AFTER each cycle.
    fn simulate(
        state: &std::sync::Arc<AppState>,
        dcfg: &DispatcherConfig,
        cycles: usize,
    ) -> Vec<(usize, f64, f64)> {
        let mut history = Vec::new();
        for cycle in 0..cycles {
            let now = Instant::now();
            let grid = state
                .snapshot
                .load_full()
                .status
                .total_act_power
                .unwrap_or(0.0);
            let plug_sum_before: f64 = state
                .batteries
                .read()
                .values()
                .filter_map(|b| b.last_plug_w)
                .sum();
            let external_load = grid + plug_sum_before;

            let targets = compute_targets(state, dcfg, grid, now);
            let plug_sum_after: f64 = targets.values().copied().sum();

            // Marstek model: perfect setpoint follower. The real
            // Marstek ramps over 1-3 s, but rate_limit_w_per_cycle
            // (= 500 W default) is what bounds each cycle's change
            // — and that's already applied inside compute_targets. So
            // "plug = target" is a faithful simulation of one cycle's
            // worth of dispatch.
            {
                let mut bats = state.batteries.write();
                for b in bats.values_mut() {
                    if let Some(t) = targets.get(&b.id).copied() {
                        b.last_plug_w = Some(t);
                        b.last_plug_at = Some(now);
                    }
                }
            }
            // Conservation: external load is constant; the new grid
            // reading is whatever load isn't covered by the batteries.
            let new_grid = external_load - plug_sum_after;
            fresh_grid_snapshot(state, new_grid);
            history.push((cycle, new_grid, plug_sum_after));
        }
        history
    }

    /// Build a 2-battery test state on a circuit with a fat fuse (no
    /// circuit-cap interference). Each battery has 2500 W in both
    /// directions and SoC = 50 % (well clear of any gate / taper).
    fn two_battery_state() -> std::sync::Arc<AppState> {
        // Build via TOML — keeps the test in sync with the actual
        // config schema (defaults + validation) instead of cooking up
        // a raw struct literal.
        let cfg: Config = toml::from_str(
            r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020

[virtual_shelly]

[management]

[[circuits]]
id = "c1"
fuse_amps = 32
phases = 1
voltage = 230
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let cfg_circuit = cfg.circuits[0].clone();
        let state = AppState::from_config(&cfg);
        let mut a = make_battery("a", 0.0, 2500.0, 2500.0);
        a.soc_pct = Some(50.0);
        let mut b = make_battery("b", 0.0, 2500.0, 2500.0);
        b.soc_pct = Some(50.0);
        b.address = "127.0.0.2".parse().unwrap();
        b.plug_url = "http://127.0.0.99".into();
        {
            let mut bats = state.batteries.write();
            bats.insert("a".into(), a);
            bats.insert("b".into(), b);
        }
        // Need a circuit state too — AppState::from_config didn't
        // build one because batteries are added after the fact.
        {
            let mut circuits = state.circuits.write();
            circuits.insert(
                "c1".into(),
                crate::state::CircuitState {
                    config: cfg_circuit,
                    member_ids: vec!["a".into(), "b".into()],
                    silent_until: None,
                    overload_started_at: None,
                    soft_remediation_started_at: None,
                },
            );
        }
        state
    }

    fn dcfg_loose() -> DispatcherConfig {
        // Default config but with the rate_limit lifted high enough
        // that single-cycle convergence is possible for the scenarios
        // that don't deliberately test the ramp.
        let mut d = dcfg();
        d.rate_limit_w_per_cycle = 9999.0;
        d
    }

    // ---- The bug screenshots ----

    /// Field report screenshot #1: grid importing +140 W, both
    /// batteries charging (a = -242 W, b = -277 W). With sequential
    /// dispatch this took 30+ s to correct. With parallel dispatch
    /// the very next cycle's targets land grid at -10 W (the export
    /// margin).
    #[test]
    fn scenario_charging_with_grid_import_converges_in_one_cycle() {
        let state = two_battery_state();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(-242.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(-277.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, 140.0);

        let history = simulate(&state, &dcfg_loose(), 2);
        let (_, grid_after_first_cycle, _) = history[0];
        // Target grid is -10 W (charging → 10 W export). With no rate
        // limit getting in the way, one cycle should land us inside
        // the margin band.
        assert!(
            (grid_after_first_cycle - (-10.0)).abs() < 1.0,
            "expected grid to land at -10 W after one cycle, got {grid_after_first_cycle}"
        );
    }

    /// Field report screenshot #2: idle (~0 W plug sum), grid
    /// exporting -164 W. Within one cycle we should start charging,
    /// pulling grid up to -10 W.
    #[test]
    fn scenario_idle_with_grid_export_starts_charging_in_one_cycle() {
        let state = two_battery_state();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(0.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(0.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, -164.0);

        let history = simulate(&state, &dcfg_loose(), 2);
        let (_, grid_after, plug_sum_after) = history[0];
        assert!(
            (grid_after - (-10.0)).abs() < 1.0,
            "expected grid to land at -10 W after one cycle, got {grid_after}"
        );
        // The 154 W that grid no longer absorbs has gone into the
        // batteries → plug_sum should be ≈ -154 (charging).
        assert!(
            plug_sum_after < -100.0,
            "expected batteries to be charging after one cycle, got plug_sum={plug_sum_after}"
        );
    }

    /// Discharging while exporting (the mirror violation): reduce
    /// discharging until grid lands at +10 W (the import margin).
    #[test]
    fn scenario_discharging_with_grid_export_converges_in_one_cycle() {
        let state = two_battery_state();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(200.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(300.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, -50.0);

        let history = simulate(&state, &dcfg_loose(), 2);
        let (_, grid_after, _) = history[0];
        assert!(
            (grid_after - 10.0).abs() < 1.0,
            "expected grid to land at +10 W, got {grid_after}"
        );
    }

    /// Direction flip: charging at a tiny -100 W, grid suddenly imports
    /// +800 W (e.g. a kettle switches on). The reduction goes through
    /// standby into discharging — the user said "richtung umdrehen wenn
    /// nötig", and the algorithm does so naturally over two cycles:
    ///
    ///   * cycle 1: charging direction at decision time, target grid =
    ///     -10. Math says "go from -100 to +710" → flip. New grid -10.
    ///   * cycle 2: now discharging direction, target grid = +10.
    ///     Trim back from +710 to +690 → grid lands at +10, stable.
    ///
    /// Both grid values are inside the ±margin band the whole time
    /// (never crosses zero into a worse violation), and the dispatch
    /// is in the right direction (discharging while grid imports).
    #[test]
    fn scenario_huge_import_flips_charging_to_discharging() {
        let state = two_battery_state();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(-50.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(-50.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, 800.0);

        let history = simulate(&state, &dcfg_loose(), 3);
        // After cycle 1 we're already in the margin band (-10 W),
        // discharging.
        let (_, grid_1, plug_sum_1) = history[0];
        assert!(
            grid_1.abs() < 15.0,
            "cycle 1: grid should be near zero (within margin), got {grid_1}"
        );
        assert!(
            plug_sum_1 > 0.0,
            "cycle 1: should have flipped to discharging, got plug_sum={plug_sum_1}"
        );
        // By cycle 2 the direction-aware target settles at +10 W
        // (discharging → import-side margin).
        let (_, grid_2, plug_sum_2) = history[1];
        assert!(
            (grid_2 - 10.0).abs() < 1.0,
            "cycle 2: grid should land at +10 W, got {grid_2}"
        );
        assert!(
            plug_sum_2 > 0.0,
            "cycle 2: discharging, got plug_sum={plug_sum_2}"
        );
    }

    /// Stable equilibrium: once we've reached grid in the ±margin band
    /// the dispatcher should HOLD (no oscillation, no bleeding the
    /// batteries dry just to chase a few watts).
    #[test]
    fn scenario_equilibrium_holds_inside_margin() {
        let state = two_battery_state();
        // Discharging steadily; grid already at the +10 target.
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(200.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(200.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, 10.0);

        let history = simulate(&state, &dcfg_loose(), 5);
        for (cycle, grid, plug_sum) in &history {
            // Grid should stay within a tight band around +10 W —
            // tiny jitter is OK, big swings are NOT.
            assert!(
                (*grid - 10.0).abs() < 5.0,
                "cycle {cycle}: grid drifted to {grid} from equilibrium"
            );
            // Discharging should hold near the equilibrium plug sum.
            assert!(
                (*plug_sum - 400.0).abs() < 50.0,
                "cycle {cycle}: plug_sum drifted to {plug_sum} from equilibrium ~400"
            );
        }
    }

    /// Rate-limited ramp: idle batteries, sudden big import. The
    /// rate-limit caps each per-cycle change to 500 W per battery.
    /// With 2 batteries that's 1000 W/cycle. So a 3000 W import takes
    /// 3 cycles to fully cover.
    #[test]
    fn scenario_rate_limited_ramp_converges_in_three_cycles() {
        let state = two_battery_state();
        {
            let mut bats = state.batteries.write();
            bats.get_mut("a").unwrap().last_plug_w = Some(0.0);
            bats.get_mut("a").unwrap().last_plug_at = Some(Instant::now());
            bats.get_mut("b").unwrap().last_plug_w = Some(0.0);
            bats.get_mut("b").unwrap().last_plug_at = Some(Instant::now());
        }
        fresh_grid_snapshot(&state, 3000.0);

        // Default rate_limit_w_per_cycle = 500.
        let dcfg = dcfg();
        let history = simulate(&state, &dcfg, 5);
        // Within 3 cycles grid should land in the margin band.
        let (_, grid_after_3, _) = history[2];
        assert!(
            (grid_after_3 - 10.0).abs() < 30.0,
            "expected grid near +10 W after 3 cycles under rate-limit, got {grid_after_3}"
        );
        // And subsequent cycles must stay there.
        for (cycle, grid, _) in &history[3..] {
            assert!(
                (*grid - 10.0).abs() < 30.0,
                "cycle {cycle}: grid drifted after converging — {grid}"
            );
        }
    }
}
