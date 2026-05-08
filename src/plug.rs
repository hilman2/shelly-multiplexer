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
}

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let cfg0 = config.load_full();
    let interval_ms = cfg0.dispatcher.cycle_ms.max(100);
    let timeout_ms = cfg0.real_shelly.request_timeout_ms.clamp(200, 800);
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
            poll_plug_loop(state_c, client_c, battery_id, plug_url, interval).await;
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
) {
    let url = format!("{plug_url}/rpc/Switch.GetStatus?id=0");
    let mut tick = time::interval(Duration::from_millis(interval_ms));
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match fetch_apower(&client, &url).await {
            Ok(apower) => {
                // Plug sign -> app sign: invert.
                let signed_w = -apower;
                let now = std::time::Instant::now();
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery_id) {
                    b.last_plug_w = Some(signed_w);
                    b.last_plug_at = Some(now);
                    // Any previous error is now stale — the plug is reachable
                    // again and the marstek SoC poller has its own clearing
                    // logic, so leaving its message would be misleading.
                    if let Some(e) = &b.last_error {
                        if e.starts_with("plug ") {
                            b.last_error = None;
                        }
                    }
                }
                debug!(battery = %battery_id, apower, signed_w, "plug reading");
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

async fn fetch_apower(client: &reqwest::Client, url: &str) -> Result<f64> {
    let res = client
        .get(url)
        .send()
        .await
        .context("plug request failed")?;
    if !res.status().is_success() {
        anyhow::bail!("plug http {}", res.status());
    }
    let body: SwitchStatus = res.json().await.context("plug json parse")?;
    body.apower.ok_or_else(|| anyhow::anyhow!("plug omitted apower"))
}
