//! Web UI + management API on a separate port.
//!
//! Minimal version for the pulse-based dispatcher: serves the embedded
//! web UI and exposes a `/api/status` endpoint that surfaces per-battery
//! commanded vs measured power, pulse queue depth, plug freshness, and
//! per-circuit silence state.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use rust_embed::Embed;
use serde::Serialize;
use serde_json::Value;
use tracing::info;

use crate::config::Config;
use crate::state::AppState;

#[derive(Embed)]
#[folder = "src/web_ui/"]
struct WebAssets;

#[derive(Clone)]
struct AdminCtx {
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    #[allow(dead_code)]
    config_path: PathBuf,
}

pub async fn run(
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    config_path: PathBuf,
) -> Result<()> {
    let bind = config.load().management.bind_address.clone();
    let addr: SocketAddr = bind.parse().with_context(|| format!("parsing {bind}"))?;

    let ctx = AdminCtx {
        state: state.clone(),
        config,
        config_path,
    };

    let router = Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(api_status))
        .route("/api/health", get(api_health))
        .route("/{*path}", get(serve_static))
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding admin http on {addr}"))?;
    info!(addr = %addr, "management UI listening");

    if addr.ip().is_unspecified() {
        if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
            for (_name, ip) in ifaces {
                if let std::net::IpAddr::V4(v4) = ip {
                    if !v4.is_loopback() && !v4.is_link_local() {
                        info!(url = %format!("http://{v4}:{}/", addr.port()), "admin UI URL");
                    }
                }
            }
        }
    }
    axum::serve(listener, router)
        .await
        .context("admin http serve")?;
    Ok(())
}

async fn serve_index() -> Response {
    serve_embed("index.html")
}

async fn serve_static(Path(path): Path<String>) -> Response {
    serve_embed(&path)
}

fn serve_embed(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap(),
                )
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Serialize)]
struct StatusResponse {
    grid_w: Option<f64>,
    grid_age_ms: Option<u128>,
    batteries: Vec<BatteryInfo>,
    circuits: Vec<CircuitInfo>,
}

#[derive(Serialize)]
struct BatteryInfo {
    id: String,
    circuit: String,
    address: String,
    max_charge_w: f64,
    max_discharge_w: f64,
    capacity_wh: f64,
    priority_weight: f64,
    commanded_w: f64,
    plug_w: Option<f64>,
    plug_age_ms: Option<u128>,
    pulse_queue_len: usize,
    saturated: bool,
    saturation_ceiling_w: Option<f64>,
    soc_pct: Option<f64>,
    last_marstek_poll_ms_ago: Option<u128>,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct CircuitInfo {
    id: String,
    fuse_amps: f64,
    voltage: f64,
    phases: u8,
    cap_w: f64,
    silent_for_ms: Option<u128>,
    member_ids: Vec<String>,
    measured_sum_w: f64,
    commanded_sum_w: f64,
}

async fn api_status(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let now = std::time::Instant::now();
    let snap = ctx.state.snapshot.load_full();
    let grid_w = snap.status.total_act_power;
    let grid_age_ms = snap.age.map(|t| now.duration_since(t).as_millis());

    let bats = ctx.state.batteries.read();
    let circuits = ctx.state.circuits.read();

    let batteries: Vec<BatteryInfo> = bats
        .values()
        .map(|b| BatteryInfo {
            id: b.id.clone(),
            circuit: b.circuit.clone(),
            address: b.address.to_string(),
            max_charge_w: b.max_charge_w,
            max_discharge_w: b.max_discharge_w,
            capacity_wh: b.capacity_wh,
            priority_weight: b.priority_weight,
            commanded_w: b.commanded_w,
            plug_w: b.last_plug_w,
            plug_age_ms: b.last_plug_at.map(|t| now.duration_since(t).as_millis()),
            pulse_queue_len: b.pulse_queue.len(),
            saturated: b.saturated,
            saturation_ceiling_w: b.saturation_ceiling_w,
            soc_pct: b.soc_pct,
            last_marstek_poll_ms_ago: b
                .last_marstek_poll_at
                .map(|t| now.duration_since(t).as_millis()),
            last_error: b.last_error.clone(),
        })
        .collect();

    let circuit_infos: Vec<CircuitInfo> = circuits
        .values()
        .map(|c| {
            let measured_sum_w: f64 = c
                .member_ids
                .iter()
                .filter_map(|id| bats.get(id).and_then(|b| b.last_plug_w))
                .sum();
            let commanded_sum_w: f64 = c
                .member_ids
                .iter()
                .filter_map(|id| bats.get(id).map(|b| b.commanded_w))
                .sum();
            CircuitInfo {
                id: c.config.id.clone(),
                fuse_amps: c.config.fuse_amps,
                voltage: c.config.voltage,
                phases: c.config.phases,
                cap_w: c.cap_w(),
                silent_for_ms: c.silent_until.and_then(|t| {
                    if t > now {
                        Some(t.duration_since(now).as_millis())
                    } else {
                        None
                    }
                }),
                member_ids: c.member_ids.clone(),
                measured_sum_w,
                commanded_sum_w,
            }
        })
        .collect();

    Json(StatusResponse {
        grid_w,
        grid_age_ms,
        batteries,
        circuits: circuit_infos,
    })
}

async fn api_health(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let _ = ctx.config.load();
    Json(serde_json::json!({"status": "ok"}))
}

#[allow(dead_code)]
fn _unused() -> Value {
    serde_json::json!({})
}
