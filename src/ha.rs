//! Home Assistant Supervisor / Core API client.
//!
//! Used to read entity states (currently just SoC sensors) when the
//! user wants the multiplexer to consume HA-side data instead of
//! polling the inverter directly. Inside an HA add-on the multiplexer
//! reaches the Core API at `http://supervisor/core/api` using
//! `$SUPERVISOR_TOKEN`; standalone deployments can point at a regular
//! HA URL with a long-lived access token.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use reqwest::Client;
use serde::Deserialize;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::state::{AppState, BatteryTelemetry};

#[derive(Debug, Deserialize)]
struct StateResponse {
    state: serde_json::Value,
}

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    // Pull the SoC entity list once at startup. Adding new HA-backed
    // entities or changing the URL/token requires a restart, in line
    // with how the rest of the per-battery setup works.
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
                .map(|e| (b.id.clone(), e.clone(), b.telemetry_interval_ms))
        })
        .collect();

    if entity_targets.is_empty() {
        info!("no battery has soc_entity_id set — HA poller idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let client = Client::builder()
        .timeout(Duration::from_millis(cfg.home_assistant.timeout_ms))
        .build()
        .context("building reqwest client")?;
    let url_base = cfg.home_assistant.url.trim_end_matches('/').to_string();
    let token = cfg.home_assistant.token.clone();

    info!(
        url = %url_base,
        entities = entity_targets.len(),
        "home-assistant SoC poller started"
    );

    let mut tasks = tokio::task::JoinSet::new();
    for (battery_id, entity, interval_ms) in entity_targets {
        let state = state.clone();
        let client = client.clone();
        let url_base = url_base.clone();
        let token = token.clone();
        tasks.spawn(async move {
            poll_entity(state, client, url_base, token, battery_id, entity, interval_ms).await;
        });
    }

    while let Some(res) = tasks.join_next().await {
        if let Err(e) = res {
            warn!(error = %e, "ha poll task ended");
        }
    }
    Err(anyhow!("all HA poll tasks ended"))
}

async fn poll_entity(
    state: Arc<AppState>,
    client: Client,
    url_base: String,
    token: String,
    battery_id: String,
    entity: String,
    interval_ms: u64,
) {
    let url = format!("{}/states/{}", url_base, entity);
    let mut interval = time::interval(Duration::from_millis(interval_ms.max(500)));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        match fetch_state(&client, &url, &token).await {
            Ok(soc) => {
                let mut tel = state.telemetry.write();
                let entry = tel.entry(battery_id.clone()).or_insert_with(|| {
                    BatteryTelemetry {
                        battery_id: battery_id.clone(),
                        ..Default::default()
                    }
                });
                // Roll the previous SoC forward for ΔSoC direction
                // inference in the dispatcher.
                if let Some(p_soc) = entry.soc_percent
                    && (soc - p_soc).abs() > 1e-3
                {
                    entry.previous_soc_percent = Some(p_soc);
                    entry.previous_soc_at = entry.last_update;
                }
                entry.soc_percent = Some(soc);
                entry.last_update = Some(std::time::Instant::now());
                entry.last_error = None;
                debug!(battery = %battery_id, entity = %entity, soc, "HA SoC update");
            }
            Err(e) => {
                let mut tel = state.telemetry.write();
                let entry = tel.entry(battery_id.clone()).or_insert_with(|| {
                    BatteryTelemetry {
                        battery_id: battery_id.clone(),
                        ..Default::default()
                    }
                });
                entry.last_error = Some(format!("{e:#}"));
                warn!(battery = %battery_id, entity = %entity, error = %e, "HA poll failed");
            }
        }
    }
}

async fn fetch_state(client: &Client, url: &str, token: &str) -> Result<f64> {
    let resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("HA request send")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HA returned HTTP {status}: {body}"));
    }
    let parsed: StateResponse = resp.json().await.context("HA response parse")?;
    let state_str = match parsed.state {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        other => return Err(anyhow!("unexpected state value: {other:?}")),
    };
    let soc: f64 = state_str
        .parse()
        .with_context(|| format!("parsing SoC '{state_str}' as float"))?;
    Ok(soc)
}
