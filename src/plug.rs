//! Shelly Plug PM Gen3 HTTP poller, one task per battery's dedicated plug.
//!
//! Polls `<plug_url>/rpc/Switch.GetStatus?id=0` at the dispatcher cycle rate
//! and stores the signed power reading in `BatteryState.last_plug_w`.
//!
//! Sign convention: the Shelly Plug reports `apower` as positive when current
//! flows from the line into the load. For our purposes the load is the
//! Marstek Venus E. When the Marstek charges (consuming from grid), `apower`
//! is positive on the plug. When the Marstek discharges, `apower` is
//! negative. Our app convention is the opposite (positive = discharge), so
//! we negate the plug value once at ingestion.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::Deserialize;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct SwitchStatus {
    /// Active power in watts. Sign: positive = flowing into the load.
    apower: Option<f64>,
    /// Relay state (true = closed, current can flow). Cross-referenced
    /// against `BatteryState.plug_relay_state` so the dispatcher's
    /// emergency-cutoff logic knows whether the plug is currently
    /// physically connected.
    output: Option<bool>,
}

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let cfg0 = config.load_full();
    let interval_ms = cfg0.dispatcher.cycle_ms.max(100);
    let timeout_ms = cfg0.real_shelly.request_timeout_ms.clamp(200, 800);
    let stable_w = cfg0.dispatcher.plug_stable_w;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .context("building plug http client")?;

    // Spawn one task per battery — independent failure domain.
    let mut handles = Vec::new();
    for b in &cfg0.batteries {
        let state_c = state.clone();
        let client_c = client.clone();
        let battery_id = b.id.clone();
        let plug_url = b.plug_url.trim_end_matches('/').to_string();
        let interval = interval_ms;
        handles.push(tokio::spawn(async move {
            poll_plug_loop(state_c, client_c, battery_id, plug_url, interval, stable_w).await;
        }));
    }

    if handles.is_empty() {
        // Empty template / no batteries yet — idle forever instead of
        // bailing, so the rest of the add-on (UI, real-shelly poller,
        // dispatcher idling at zero) keeps running.
        info!("no batteries configured — plug poller idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Wait forever — each poll_plug_loop loops infinitely; if one ever
    // ends we'd want to know.
    for h in handles {
        let _ = h.await;
    }
    anyhow::bail!("all plug poll tasks ended")
}

async fn poll_plug_loop(
    state: Arc<AppState>,
    client: reqwest::Client,
    battery_id: String,
    plug_url: String,
    interval_ms: u64,
    stable_w: f64,
) {
    let url = format!("{plug_url}/rpc/Switch.GetStatus?id=0");
    let mut tick = time::interval(Duration::from_millis(interval_ms));
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match fetch_status(&client, &url).await {
            Ok((apower, relay_on)) => {
                // Plug sign -> app sign: invert.
                let signed_w = -apower;
                let now = std::time::Instant::now();
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery_id) {
                    // Track movement: if the new reading differs from the
                    // previous by more than the stable threshold, the plug
                    // is "moving" and `pulse_settled` keeps blocking until
                    // a stable window elapses. First reading also seeds
                    // last_plug_movement_at so the timestamp is meaningful.
                    let moved = match b.last_plug_w {
                        Some(prev) => (signed_w - prev).abs() > stable_w,
                        None => true,
                    };
                    if moved {
                        b.last_plug_movement_at = Some(now);
                    }
                    b.last_plug_w = Some(signed_w);
                    b.last_plug_at = Some(now);
                    b.plug_relay_state = relay_on;
                    // Any previous error is now stale — the plug is reachable
                    // again and the marstek SoC poller has its own clearing
                    // logic, so leaving its message would be misleading.
                    if let Some(e) = &b.last_error {
                        if e.starts_with("plug ") {
                            b.last_error = None;
                        }
                    }
                }
                debug!(battery = %battery_id, apower, signed_w, relay_on = ?relay_on, "plug reading");
            }
            Err(e) => {
                warn!(battery = %battery_id, url = %url, error = %e, "plug poll failed");
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery_id) {
                    b.last_error = Some(format!("plug unreachable: {e}"));
                }
            }
        }
    }
}

async fn fetch_status(
    client: &reqwest::Client,
    url: &str,
) -> Result<(f64, Option<bool>)> {
    let res = client
        .get(url)
        .send()
        .await
        .context("plug request failed")?;
    if !res.status().is_success() {
        anyhow::bail!("plug http {}", res.status());
    }
    let body: SwitchStatus = res.json().await.context("plug json parse")?;
    let apower = body
        .apower
        .ok_or_else(|| anyhow::anyhow!("plug omitted apower"))?;
    Ok((apower, body.output))
}

// ---------------------------------------------------------------------------
// Hard cut-off — toggle the plug's relay via Shelly Gen3 HTTP-RPC.
// ---------------------------------------------------------------------------
//
// This is the last line of defence: if a battery refuses to honour its
// commanded setpoint AND the resulting plug power pushes a circuit over
// its fuse cap, the dispatcher uses `set_relay(false)` to physically
// disconnect that battery. Re-enabling happens automatically after the
// recovery window or manually via the admin UI.
//
// Shelly Gen3 HTTP-RPC sometimes refuses query-string form for write
// methods, so we POST the JSON-RPC body. Both `Switch.Set` (modern) and
// the GET fallback work on Gen3 firmware ≥ 1.4.
pub async fn set_relay(plug_url: &str, on: bool) -> Result<()> {
    let url = format!("{}/rpc/Switch.Set", plug_url.trim_end_matches('/'));
    let body = serde_json::json!({ "id": 0, "on": on });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building plug relay client")?;
    let res = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("plug Switch.Set request failed")?;
    if !res.status().is_success() {
        anyhow::bail!("plug Switch.Set http {}", res.status());
    }
    Ok(())
}
