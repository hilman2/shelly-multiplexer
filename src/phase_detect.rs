//! Active phase detection. Pauses the dispatcher, drives one battery
//! at a time at full charge then full discharge, and watches which CT
//! phase reacts. The phase with the strongest *consistent* swing
//! (positive on charge, negative on discharge — or the inverse, since
//! the sign convention depends on which side of the wire the battery
//! injects on) is recorded as the physical phase, with a confidence
//! score derived from how much it stands out from the other phases.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use chrono::SecondsFormat;
use parking_lot::RwLock;
use tokio::time;
use tracing::{info, warn};

use crate::config::{Config, DetectedPhase, PhaseAssignment};
use crate::state::{Allocation, AppState, PhaseFactors, PhaseWatts};

/// Time after a setpoint change before we sample the CT. Battery
/// inverters react with seconds of lag; this is long enough to ride
/// past the transient.
const SETTLE_SECS: u64 = 12;
/// Total per-battery probe time = baseline + charge + idle + discharge + idle.
/// The user sees "~50 s per battery" in the GUI countdown.
const BASELINE_SECS: u64 = 6;
const IDLE_SECS: u64 = 4;

/// Live state of an in-progress detection run. The HTTP API reads it
/// to drive the GUI progress display.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DetectionStatus {
    pub running: bool,
    pub current_battery: Option<String>,
    pub phase: DetectionPhase,
    pub started_at_ms_ago: Option<u128>,
    pub message: Option<String>,
    pub results: HashMap<String, DetectedPhase>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionPhase {
    #[default]
    Idle,
    Baseline,
    Charging,
    Discharging,
    Finalizing,
    Done,
}

pub type SharedStatus = Arc<RwLock<DetectionStatus>>;

/// Run detection for every battery in the config, sequentially. The
/// returned map can be merged back into Config and persisted by the
/// caller. While this runs, the dispatcher must skip its loop (it
/// checks `state.detection_active`).
pub async fn run_all(
    state: &AppState,
    config_swap: &Arc<ArcSwap<Config>>,
    status: SharedStatus,
) -> Result<HashMap<String, DetectedPhase>> {
    let cfg = config_swap.load_full();
    let battery_ids: Vec<String> = cfg.batteries.iter().map(|b| b.id.clone()).collect();
    let battery_addrs: HashMap<String, IpAddr> = cfg
        .batteries
        .iter()
        .map(|b| (b.id.clone(), b.address))
        .collect();
    let battery_max_charge: HashMap<String, f64> = cfg
        .batteries
        .iter()
        .map(|b| (b.id.clone(), b.max_charge_w))
        .collect();
    let battery_max_discharge: HashMap<String, f64> = cfg
        .batteries
        .iter()
        .map(|b| (b.id.clone(), b.max_discharge_w))
        .collect();

    {
        let mut s = status.write();
        s.running = true;
        s.started_at_ms_ago = Some(0);
        s.message = Some(format!("Probing {} batteries…", battery_ids.len()));
        s.results.clear();
        s.last_error = None;
    }
    let started = Instant::now();

    state.detection_active.store(true, std::sync::atomic::Ordering::SeqCst);
    let _guard = scopeguard::guard((), |_| {
        state.detection_active.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    let mut results: HashMap<String, DetectedPhase> = HashMap::new();
    for id in &battery_ids {
        let Some(addr) = battery_addrs.get(id).copied() else { continue };
        let max_c = battery_max_charge.get(id).copied().unwrap_or(0.0);
        let max_d = battery_max_discharge.get(id).copied().unwrap_or(0.0);
        if max_c <= 0.0 && max_d <= 0.0 {
            warn!(battery = %id, "skipping detection: max_charge_w = max_discharge_w = 0");
            continue;
        }

        // Park *all* batteries first so their inverters idle.
        park_all(state, &battery_addrs);
        update_status(&status, |s| {
            s.current_battery = Some(id.clone());
            s.phase = DetectionPhase::Baseline;
            s.message = Some(format!("Battery {id}: parking everyone, baseline…"));
            s.started_at_ms_ago = Some(started.elapsed().as_millis());
        });
        time::sleep(Duration::from_secs(BASELINE_SECS)).await;
        let baseline = sample_phases(state)?;

        // Charge probe.
        update_status(&status, |s| {
            s.phase = DetectionPhase::Charging;
            s.message = Some(format!("Battery {id}: charge probe ({SETTLE_SECS}s)…"));
        });
        drive_battery(state, addr, -max_c.max(500.0));
        time::sleep(Duration::from_secs(SETTLE_SECS)).await;
        let charge_snap = sample_phases(state)?;

        // Idle between probes so the inverter relaxes back.
        park_all(state, &battery_addrs);
        time::sleep(Duration::from_secs(IDLE_SECS)).await;

        // Discharge probe.
        update_status(&status, |s| {
            s.phase = DetectionPhase::Discharging;
            s.message = Some(format!("Battery {id}: discharge probe ({SETTLE_SECS}s)…"));
        });
        drive_battery(state, addr, max_d.max(500.0));
        time::sleep(Duration::from_secs(SETTLE_SECS)).await;
        let discharge_snap = sample_phases(state)?;

        // Park again before next battery.
        park_all(state, &battery_addrs);

        // Per-phase signed delta from baseline. Charge should pull
        // import UP on that phase (positive delta), discharge should
        // pull it DOWN (negative delta). The phase where charge−discharge
        // is the largest is the physical one; sign is informational.
        let delta_a = (charge_snap.0 - baseline.0) - (discharge_snap.0 - baseline.0);
        let delta_b = (charge_snap.1 - baseline.1) - (discharge_snap.1 - baseline.1);
        let delta_c = (charge_snap.2 - baseline.2) - (discharge_snap.2 - baseline.2);

        let scores = [
            (PhaseAssignment::A, delta_a.abs()),
            (PhaseAssignment::B, delta_b.abs()),
            (PhaseAssignment::C, delta_c.abs()),
        ];
        let total: f64 = scores.iter().map(|(_, s)| *s).sum();
        let (winner_phase, winner_score) = scores
            .iter()
            .copied()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();
        let confidence = if total > 1.0 {
            // 0.0 = uniform across phases; 1.0 = winner takes everything.
            ((winner_score / total) - 1.0 / 3.0) / (2.0 / 3.0)
        } else {
            // Below 1 W of total reaction → no usable signal.
            0.0
        }
        .clamp(0.0, 1.0);

        let detected = DetectedPhase {
            phase: winner_phase,
            confidence,
            detected_at: chrono_utc_now(),
            delta_a_w: delta_a,
            delta_b_w: delta_b,
            delta_c_w: delta_c,
        };
        info!(
            battery = %id,
            phase = ?detected.phase,
            confidence,
            delta_a, delta_b, delta_c,
            "phase detection result"
        );
        results.insert(id.clone(), detected.clone());
        update_status(&status, |s| {
            s.results.insert(id.clone(), detected.clone());
        });
    }

    update_status(&status, |s| {
        s.running = false;
        s.current_battery = None;
        s.phase = DetectionPhase::Done;
        s.message = Some(format!("Done. Probed {} batteries.", results.len()));
    });
    park_all(state, &battery_addrs);

    Ok(results)
}

fn update_status(status: &SharedStatus, f: impl FnOnce(&mut DetectionStatus)) {
    let mut s = status.write();
    f(&mut s);
}

/// Set every battery's allocation to zero so the inverters idle.
fn park_all(state: &AppState, addrs: &HashMap<String, IpAddr>) {
    let mut allocs = state.allocations.write();
    for (battery_id, addr) in addrs {
        allocs.insert(
            *addr,
            Allocation {
                battery_id: battery_id.clone(),
                factors: PhaseFactors { a: 0.0, b: 0.0, c: 0.0 },
                phase_w: PhaseWatts { a: 0.0, b: 0.0, c: 0.0 },
                allocated_w: 0.0,
                magnitude_w: 0.0,
                group: None,
                note: Some("phase-detect: parked".into()),
            },
        );
    }
}

/// Drive a single battery to a target output (positive = discharge).
/// All other batteries are left as the dispatcher / park_all set them.
fn drive_battery(state: &AppState, addr: IpAddr, target_w: f64) {
    let mut allocs = state.allocations.write();
    if let Some(alloc) = allocs.get_mut(&addr) {
        alloc.phase_w = PhaseWatts { a: target_w, b: 0.0, c: 0.0 };
        alloc.allocated_w = target_w;
        alloc.magnitude_w = target_w.abs();
        alloc.note = Some(if target_w > 0.0 {
            "phase-detect: discharging".into()
        } else {
            "phase-detect: charging".into()
        });
    }
}

/// Snapshot of (phase A, phase B, phase C) act_power.
fn sample_phases(state: &AppState) -> Result<(f64, f64, f64)> {
    let snap = state.snapshot.load_full();
    if snap.age.is_none() {
        return Err(anyhow!("no real-shelly data yet"));
    }
    let s = &snap.status;
    Ok((
        s.a_act_power.unwrap_or(0.0),
        s.b_act_power.unwrap_or(0.0),
        s.c_act_power.unwrap_or(0.0),
    ))
}

fn chrono_utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Persist detected phases back into the TOML on disk.
pub fn persist_results(
    config_swap: &Arc<ArcSwap<Config>>,
    config_path: &std::path::Path,
    results: &HashMap<String, DetectedPhase>,
) -> Result<()> {
    let mut new_cfg = (**config_swap.load()).clone();
    for b in new_cfg.batteries.iter_mut() {
        if let Some(d) = results.get(&b.id) {
            b.detected_phase = Some(d.clone());
        }
    }
    let toml = toml::to_string_pretty(&new_cfg).context("serialise updated config")?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = config_path.with_extension("toml.tmp");
    info!(
        path = %config_path.display(),
        results = results.len(),
        bytes = toml.len(),
        "persisting phase-detection results"
    );
    std::fs::write(&tmp, toml).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("rename to {}", config_path.display()))?;
    config_swap.store(Arc::new(new_cfg));
    info!(path = %config_path.display(), "phase-detection results persisted");
    Ok(())
}
