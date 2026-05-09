//! Web UI + management API on a separate port.

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
use axum::routing::{get, put};
use axum::Json;
use rust_embed::Embed;
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::Config;
use crate::state::AppState;

#[derive(Embed)]
#[folder = "src/web_ui/"]
struct WebAssets;

#[derive(Clone)]
struct AdminCtx {
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
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
        .route("/api/config", get(api_get_config).put(api_put_config))
        .route("/api/config/section/{name}", put(api_put_section))
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

// ---------------------------------------------------------------------------
// Static / index
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// /api/status — live runtime state
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    grid_w: Option<f64>,
    grid_age_ms: Option<u128>,
    config_path: String,
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
    soc_full_pct: Option<f64>,
    soc_empty_pct: Option<f64>,
    plug_w: Option<f64>,
    plug_age_ms: Option<u128>,
    pulse_remaining: u32,
    pending_pulse_w: f64,
    plug_w_at_pulse_send: Option<f64>,
    last_pulse_completed_ms_ago: Option<u128>,
    soc_pct: Option<f64>,
    soc_source: Option<String>,
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
            soc_full_pct: b.soc_full_pct,
            soc_empty_pct: b.soc_empty_pct,
            plug_w: b.last_plug_w,
            plug_age_ms: b.last_plug_at.map(|t| now.duration_since(t).as_millis()),
            pulse_remaining: b.pulse_remaining,
            pending_pulse_w: b.pending_pulse_w,
            plug_w_at_pulse_send: b.plug_w_at_pulse_send,
            last_pulse_completed_ms_ago: b
                .last_pulse_completed_at
                .map(|t| now.duration_since(t).as_millis()),
            soc_pct: b.soc_pct,
            soc_source: b.soc_source.clone(),
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
            }
        })
        .collect();

    Json(StatusResponse {
        grid_w,
        grid_age_ms,
        config_path: ctx.config_path.display().to_string(),
        batteries,
        circuits: circuit_infos,
    })
}

async fn api_health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// ---------------------------------------------------------------------------
// /api/config — read & write
// ---------------------------------------------------------------------------

async fn api_get_config(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let cfg = ctx.config.load_full();
    // Mask the SUPERVISOR_TOKEN if it's currently being used (we don't
    // want to leak it to the browser). Detect by comparing to env var.
    let mut value = serde_json::to_value(&*cfg).unwrap_or(json!({}));
    if let Ok(env_tok) = std::env::var("SUPERVISOR_TOKEN") {
        if !env_tok.is_empty() {
            if let Some(ha) = value.get_mut("home_assistant") {
                if let Some(tok) = ha.get_mut("token") {
                    if tok.as_str().map(|s| s == env_tok).unwrap_or(false) {
                        *tok = json!("");
                    }
                }
            }
        }
    }
    let body = json!({
        "config_path": ctx.config_path.display().to_string(),
        "config": value,
    });
    Json(body)
}

/// Replace the entire config. Validates, persists, swaps live config.
async fn api_put_config(
    State(ctx): State<AdminCtx>,
    Json(body): Json<Value>,
) -> Response {
    let new_cfg: Config = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("parse: {e}")),
    };
    apply_new_config(&ctx, new_cfg).await
}

/// Replace one section by name. Body is the new value for that section.
async fn api_put_section(
    State(ctx): State<AdminCtx>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    const SECTIONS: &[&str] = &[
        "real_shelly",
        "virtual_shelly",
        "management",
        "dispatcher",
        "home_assistant",
        "circuits",
        "batteries",
    ];
    if !SECTIONS.contains(&name.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("unknown section '{name}' (allowed: {SECTIONS:?})"),
        );
    }

    let cfg = ctx.config.load_full();
    let mut as_json = match serde_json::to_value(&*cfg) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialise current config: {e}"),
            );
        }
    };
    if let Some(obj) = as_json.as_object_mut() {
        obj.insert(name.clone(), body);
    } else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config root is not an object".into(),
        );
    }
    let new_cfg: Config = match serde_json::from_value(as_json) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("parse: {e}")),
    };
    apply_new_config(&ctx, new_cfg).await
}

async fn apply_new_config(ctx: &AdminCtx, mut new_cfg: Config) -> Response {
    if let Err(e) = new_cfg.validate() {
        return error_response(StatusCode::BAD_REQUEST, format!("validation: {e:#}"));
    }

    // Re-inject the SUPERVISOR_TOKEN if the new config doesn't carry one
    // and the env var has one — so flipping HA enabled in the UI doesn't
    // require pasting a token in.
    if new_cfg.home_assistant.token.trim().is_empty() {
        if let Ok(env_tok) = std::env::var("SUPERVISOR_TOKEN") {
            if !env_tok.is_empty() {
                new_cfg.home_assistant.token = env_tok;
            }
        }
    }

    // Persist to disk. Drop SUPERVISOR_TOKEN before writing (don't put a
    // secret into a file the user might back up).
    let mut for_disk = new_cfg.clone();
    if let Ok(env_tok) = std::env::var("SUPERVISOR_TOKEN") {
        if !env_tok.is_empty() && for_disk.home_assistant.token == env_tok {
            for_disk.home_assistant.token.clear();
        }
    }
    let toml_str = match toml::to_string_pretty(&for_disk) {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialise to TOML: {e}"),
            );
        }
    };
    let tmp = ctx.config_path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, toml_str.as_bytes()) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", tmp.display()),
        );
    }
    if let Err(e) = std::fs::rename(&tmp, &ctx.config_path) {
        let _ = std::fs::remove_file(&tmp);
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "rename {} -> {}: {e}",
                tmp.display(),
                ctx.config_path.display()
            ),
        );
    }

    // Hot-swap the in-memory config so live-tunable fields apply
    // immediately. AppState topology (batteries / circuits maps) was
    // built at startup and is NOT rebuilt — adding a battery, removing
    // one, or changing its IP / circuit / plug_url requires a restart.
    ctx.config.store(Arc::new(new_cfg));
    info!("config updated via admin UI");

    Json(json!({"status": "ok"})).into_response()
}

fn error_response(status: StatusCode, message: String) -> Response {
    warn!(status = %status, "config update rejected: {}", message);
    (status, Json(json!({"error": message}))).into_response()
}
