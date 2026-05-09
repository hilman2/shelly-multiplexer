//! Home Assistant Core API client — SoC reader.
//!
//! When `home_assistant.enabled = true` and a battery has `soc_entity_id`
//! set, we read SoC from the HA REST API instead of polling the inverter
//! directly. Useful when HA already owns the inverter's UDP port (HACS
//! Marstek integration, etc.).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use reqwest::Client;
use serde::Deserialize;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct StateResponse {
    state: serde_json::Value,
}

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let cfg = config.load_full();
    if !cfg.home_assistant.enabled {
        info!("home-assistant integration disabled — task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }
    if cfg.home_assistant.token.trim().is_empty() {
        warn!("home_assistant.enabled = true but no token set — task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let entity_targets: Vec<(String, String, u64)> = cfg
        .batteries
        .iter()
        .filter_map(|b| {
            b.soc_entity_id
                .as_ref()
                .map(|e| (b.id.clone(), e.clone(), b.soc_interval_ms))
        })
        .collect();

    if entity_targets.is_empty() {
        info!("no soc_entity_id configured on any battery — HA task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let timeout = Duration::from_millis(cfg.home_assistant.timeout_ms);
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .context("building HA http client")?;

    let url_base = cfg.home_assistant.url.trim_end_matches('/').to_string();
    let token = cfg.home_assistant.token.clone();

    let mut handles = Vec::new();
    for (battery_id, entity_id, interval_ms) in entity_targets {
        let state = state.clone();
        let client = client.clone();
        let url_base = url_base.clone();
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            poll_loop(state, client, url_base, token, battery_id, entity_id, interval_ms).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    anyhow::bail!("ha SoC tasks ended")
}

async fn poll_loop(
    state: Arc<AppState>,
    client: Client,
    url_base: String,
    token: String,
    battery_id: String,
    entity_id: String,
    interval_ms: u64,
) {
    let url = format!("{url_base}/states/{entity_id}");
    let mut tick = time::interval(Duration::from_millis(interval_ms.max(1000)));
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match fetch_soc(&client, &url, &token).await {
            Ok(soc) => {
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery_id) {
                    b.soc_pct = Some(soc);
                    b.soc_at = Some(std::time::Instant::now());
                    b.soc_source = Some(format!("ha:{entity_id}"));
                    if let Some(e) = &b.last_error {
                        if e.starts_with("ha ") {
                            b.last_error = None;
                        }
                    }
                }
                debug!(battery = %battery_id, entity = %entity_id, soc, "ha SoC");
            }
            Err(e) => {
                debug!(battery = %battery_id, entity = %entity_id, error = %e, "ha SoC failed");
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery_id) {
                    b.last_error = Some(format!("ha SoC: {e}"));
                }
            }
        }
    }
}

async fn fetch_soc(client: &Client, url: &str, token: &str) -> Result<f64> {
    let res = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .context("ha request")?;
    if !res.status().is_success() {
        anyhow::bail!("ha http {}", res.status());
    }
    let body: StateResponse = res.json().await.context("ha json")?;
    body.state
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| body.state.as_f64())
        .ok_or_else(|| anyhow!("non-numeric state"))
}
