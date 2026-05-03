//! Web UI + management API on a separate port.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::{Config, SAFETY_DEFAULT_CAP_W};
use crate::phase_detect::{self, DetectionStatus, SharedStatus};
use crate::state::{AppState, RuntimeSafety};

#[derive(Embed)]
#[folder = "src/web_ui/"]
struct WebAssets;

#[derive(Clone)]
struct AdminCtx {
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    config_path: PathBuf,
    detection_status: SharedStatus,
}

pub async fn run(
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    config_path: PathBuf,
) -> Result<()> {
    let bind = config.load().management.bind_address.clone();
    let addr: SocketAddr = bind.parse().with_context(|| format!("parsing {bind}"))?;

    let detection_status: SharedStatus =
        Arc::new(parking_lot::RwLock::new(DetectionStatus::default()));

    let ctx = AdminCtx {
        state: state.clone(),
        config,
        config_path,
        detection_status,
    };

    let router = Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(api_status))
        .route("/api/config", get(api_get_config).put(api_put_config))
        .route("/api/config/section/{name}", put(api_put_section))
        .route("/api/safety", get(api_get_safety).post(api_post_safety))
        .route("/api/safety/reset", post(api_reset_safety))
        .route(
            "/api/phase-detect",
            get(api_phase_detect_status).post(api_phase_detect_start),
        )
        .route("/api/health", get(api_health))
        .route("/{*path}", get(serve_static))
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding admin http on {addr}"))?;
    info!(addr = %addr, "management UI listening");

    // If we're bound to all interfaces, surface every routable LAN IP
    // so the user can paste a working URL without guessing or relying
    // on mDNS (HA add-on's webui [HOST] resolves to homeassistant.local
    // which often doesn't work for the user's browser).
    if addr.ip().is_unspecified()
        && let Ok(ifaces) = local_ip_address::list_afinet_netifas()
    {
        for (_name, ip) in ifaces {
            if let std::net::IpAddr::V4(v4) = ip
                && !v4.is_loopback()
                && !v4.is_link_local()
            {
                info!(url = %format!("http://{v4}:{}/", addr.port()), "admin UI URL");
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
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "no").into_response();
    }
    serve_embed(&path)
}

fn serve_embed(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap(),
                )
                .body(Body::from(file.data.into_owned()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Serialize)]
struct StatusResponse {
    real_shelly: RealShellyStatus,
    allocations: Vec<AllocationInfo>,
    safety: SafetyInfo,
    energy: EnergyInfo,
    uptime_seconds: u64,
}

#[derive(Serialize)]
struct SafetyInfo {
    effective_cap_w: f64,
    default_cap_w: f64,
    override_active: bool,
    acknowledged_higher_risk: bool,
    acknowledged_separate_fuses: bool,
    source: &'static str,
    last_changed_ms_ago: Option<u128>,
}

#[derive(Serialize)]
struct RealShellyStatus {
    last_seen_ms_ago: Option<u128>,
    a_act_power: Option<f64>,
    b_act_power: Option<f64>,
    c_act_power: Option<f64>,
    a_voltage: Option<f64>,
    b_voltage: Option<f64>,
    c_voltage: Option<f64>,
    total_act_power: Option<f64>,
}

#[derive(Serialize)]
struct AllocationInfo {
    battery_id: String,
    address: String,
    group: Option<String>,
    factor_a: f64,
    factor_b: f64,
    factor_c: f64,
    /// Per-phase allocation in watts (signed: + = discharge, − = charge).
    phase_w_a: f64,
    phase_w_b: f64,
    phase_w_c: f64,
    /// Net allocation across all phases. On a battery that charges on one
    /// phase and discharges on another, this can be near zero even though
    /// the battery is actively dispatching — see `magnitude_w`.
    allocated_w: f64,
    /// Sum of absolute per-phase allocations. Reflects how hard the
    /// battery is actually working, including cancelling phases.
    magnitude_w: f64,
    last_request_ms_ago: Option<u128>,
    note: Option<String>,
    soc_percent: Option<f64>,
    soc_age_ms: Option<u128>,
    soc_error: Option<String>,
    /// "charging" / "discharging" / null — passive 10-min verdict.
    stuck_direction: Option<crate::state::StuckDirection>,
    /// Number of step events in the rolling window.
    stuck_events_in_window: usize,
}

#[derive(Serialize)]
struct EnergyInfo {
    a_consumed_wh: f64,
    a_returned_wh: f64,
    b_consumed_wh: f64,
    b_returned_wh: f64,
    c_consumed_wh: f64,
    c_returned_wh: f64,
}

async fn api_status(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let snap = ctx.state.snapshot.load_full();
    let now = Instant::now();
    let last_seen_ms_ago = snap.age.map(|t| now.saturating_duration_since(t).as_millis());

    let real = RealShellyStatus {
        last_seen_ms_ago,
        a_act_power: snap.status.a_act_power,
        b_act_power: snap.status.b_act_power,
        c_act_power: snap.status.c_act_power,
        a_voltage: snap.status.a_voltage,
        b_voltage: snap.status.b_voltage,
        c_voltage: snap.status.c_voltage,
        total_act_power: snap.status.total_act_power,
    };

    let allocs = ctx.state.allocations.read();
    let last_polls = ctx.state.last_poll_at.read();
    let tel = ctx.state.telemetry.read();
    let resp = ctx.state.responsiveness.read();
    let allocations: Vec<AllocationInfo> = allocs
        .iter()
        .map(|(ip, a)| {
            let t = tel.get(&a.battery_id);
            let r = resp.get(&a.battery_id);
            AllocationInfo {
                battery_id: a.battery_id.clone(),
                address: ip.to_string(),
                group: a.group.clone(),
                factor_a: a.factors.a,
                factor_b: a.factors.b,
                factor_c: a.factors.c,
                phase_w_a: a.phase_w.a,
                phase_w_b: a.phase_w.b,
                phase_w_c: a.phase_w.c,
                allocated_w: a.allocated_w,
                magnitude_w: a.magnitude_w,
                last_request_ms_ago: last_polls
                    .get(ip)
                    .map(|t| now.saturating_duration_since(*t).as_millis()),
                note: a.note.clone(),
                soc_percent: t.and_then(|t| t.soc_percent),
                soc_age_ms: t.and_then(|t| t.last_update)
                    .map(|x| now.saturating_duration_since(x).as_millis()),
                soc_error: t.and_then(|t| t.last_error.clone()),
                stuck_direction: r.and_then(|r| r.stuck_direction),
                stuck_events_in_window: r.map(|r| r.events.len()).unwrap_or(0),
            }
        })
        .collect();
    drop(tel);
    drop(last_polls);
    drop(resp);

    let safety_state = ctx.state.safety.read();
    let safety = SafetyInfo {
        effective_cap_w: safety_state.effective_cap_w,
        default_cap_w: SAFETY_DEFAULT_CAP_W,
        override_active: safety_state.override_active(),
        acknowledged_higher_risk: safety_state.acknowledged_higher_risk,
        acknowledged_separate_fuses: safety_state.acknowledged_separate_fuses,
        source: safety_state.source,
        last_changed_ms_ago: safety_state
            .last_changed_at
            .map(|t| now.saturating_duration_since(t).as_millis()),
    };

    let e = ctx.state.energy.read();
    let energy = EnergyInfo {
        a_consumed_wh: e.a_consumed_wh,
        a_returned_wh: e.a_returned_wh,
        b_consumed_wh: e.b_consumed_wh,
        b_returned_wh: e.b_returned_wh,
        c_consumed_wh: e.c_consumed_wh,
        c_returned_wh: e.c_returned_wh,
    };

    Json(StatusResponse {
        real_shelly: real,
        allocations,
        safety,
        energy,
        uptime_seconds: ctx.state.started_at.elapsed().as_secs(),
    })
}

#[derive(Deserialize)]
struct SafetyOverrideRequest {
    /// Requested cap in watts. Must be at least the default cap (3000 W);
    /// to lower the cap below 3000 simply edit the TOML and restart, or
    /// POST a value <= 3000 — both ack flags are then ignored.
    max_total_w: f64,
    /// Acknowledgement 1: caller has read the warning about overload
    /// risk if wiring isn't rated for the requested cap.
    acknowledged_higher_risk: bool,
    /// Acknowledgement 2: every battery is on its own protective device.
    acknowledged_separate_fuses: bool,
}

async fn api_get_safety(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let now = Instant::now();
    let s = ctx.state.safety.read();
    Json(SafetyInfo {
        effective_cap_w: s.effective_cap_w,
        default_cap_w: SAFETY_DEFAULT_CAP_W,
        override_active: s.override_active(),
        acknowledged_higher_risk: s.acknowledged_higher_risk,
        acknowledged_separate_fuses: s.acknowledged_separate_fuses,
        source: s.source,
        last_changed_ms_ago: s
            .last_changed_at
            .map(|t| now.saturating_duration_since(t).as_millis()),
    })
}

async fn api_post_safety(
    State(ctx): State<AdminCtx>,
    Json(req): Json<SafetyOverrideRequest>,
) -> Response {
    if !req.max_total_w.is_finite() || req.max_total_w < 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "max_total_w must be a finite non-negative number"})),
        )
            .into_response();
    }
    if req.max_total_w > 20_000.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "max_total_w > 20000 W rejected — this is a residential tool"})),
        )
            .into_response();
    }
    if req.max_total_w > SAFETY_DEFAULT_CAP_W
        && (!req.acknowledged_higher_risk || !req.acknowledged_separate_fuses)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "raising the cap above 3000 W requires both acknowledgements",
                "required": ["acknowledged_higher_risk", "acknowledged_separate_fuses"]
            })),
        )
            .into_response();
    }

    let mut s = ctx.state.safety.write();
    *s = RuntimeSafety {
        effective_cap_w: req.max_total_w,
        acknowledged_higher_risk: req.acknowledged_higher_risk,
        acknowledged_separate_fuses: req.acknowledged_separate_fuses,
        source: "runtime",
        last_changed_at: Some(Instant::now()),
    };
    warn!(
        cap_w = req.max_total_w,
        ack_risk = req.acknowledged_higher_risk,
        ack_fuses = req.acknowledged_separate_fuses,
        "safety cap changed via admin UI"
    );
    Json(json!({
        "ok": true,
        "effective_cap_w": s.effective_cap_w,
        "override_active": s.override_active()
    }))
    .into_response()
}

async fn api_reset_safety(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let cfg = ctx.config.load_full();
    let mut s = ctx.state.safety.write();
    *s = RuntimeSafety::from_config(&cfg.safety);
    info!(cap_w = s.effective_cap_w, "safety cap reset to config defaults");
    Json(json!({
        "ok": true,
        "effective_cap_w": s.effective_cap_w
    }))
}

async fn api_get_config(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    let cfg = ctx.config.load_full();
    Json(json!({
        "real_shelly": &cfg.real_shelly,
        "virtual_shelly": &cfg.virtual_shelly,
        "management": &cfg.management,
        "dispatcher": &cfg.dispatcher,
        "safety": &cfg.safety,
        "groups": &cfg.groups,
        "batteries": &cfg.batteries,
        "config_path": ctx.config_path.display().to_string(),
    }))
}

/// Replace the entire configuration. Validates, persists to the TOML
/// file the multiplexer was started with, and swaps the live config so
/// running tasks pick up changes on their next tick. Settings that bind
/// sockets at startup (UDP/HTTP/management ports, marstek_port) are
/// persisted but require a service restart to take effect.
async fn api_put_config(
    State(ctx): State<AdminCtx>,
    Json(req): Json<Config>,
) -> Response {
    apply_full_config(&ctx, req).await
}

/// Replace a single section of the configuration. The section name is
/// one of: `real_shelly`, `virtual_shelly`, `management`, `dispatcher`,
/// `safety`, `groups`, `batteries`. Body is the new value for that
/// section.
async fn api_put_section(
    State(ctx): State<AdminCtx>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let current = ctx.config.load_full();
    let mut full = match serde_json::to_value(&*current) {
        Ok(v) => v,
        Err(e) => return bad_request(format!("serializing current config: {e}")),
    };
    let allowed = [
        "real_shelly",
        "virtual_shelly",
        "management",
        "dispatcher",
        "safety",
        "groups",
        "batteries",
    ];
    if !allowed.contains(&name.as_str()) {
        return bad_request(format!("unknown section '{name}'"));
    }
    full[name] = body;
    let new_cfg: Config = match serde_json::from_value(full) {
        Ok(c) => c,
        Err(e) => return bad_request(format!("invalid section payload: {e}")),
    };
    apply_full_config(&ctx, new_cfg).await
}

async fn apply_full_config(ctx: &AdminCtx, new_cfg: Config) -> Response {
    if let Err(e) = new_cfg.validate() {
        return bad_request(format!("validation failed: {e:#}"));
    }

    let serialized = match toml::to_string_pretty(&new_cfg) {
        Ok(s) => s,
        Err(e) => {
            return server_error(format!("serializing TOML: {e}"));
        }
    };

    // Write atomically: create a sibling tmp file, then rename. On
    // Windows this is best-effort (rename across an existing file works
    // since Rust 1.52 via MoveFileExW + MOVEFILE_REPLACE_EXISTING).
    let path = &ctx.config_path;
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, &serialized) {
        return server_error(format!("writing {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return server_error(format!("renaming to {}: {e}", path.display()));
    }
    info!(path = %path.display(), "config saved via admin UI");

    // Update the live runtime safety state to match the new config —
    // but only if no runtime override is currently in effect.
    let current_safety_source = ctx.state.safety.read().source;
    if current_safety_source == "config" {
        let mut s = ctx.state.safety.write();
        *s = RuntimeSafety::from_config(&new_cfg.safety);
    }

    // Swap the live config so all hot-reloading tasks see the change.
    ctx.config.store(Arc::new(new_cfg));

    Json(json!({
        "ok": true,
        "restart_hint": "Changes to ports, bind addresses, MAC, hostname, marstek_port, or batteries list need a service restart."
    }))
    .into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg.into()}))).into_response()
}

fn server_error(msg: impl Into<String>) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg.into()})),
    )
        .into_response()
}

async fn api_health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn api_phase_detect_status(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    Json((*ctx.detection_status.read()).clone())
}

async fn api_phase_detect_start(State(ctx): State<AdminCtx>) -> Response {
    if ctx.detection_status.read().running {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "phase detection already running"})),
        )
            .into_response();
    }
    let status = ctx.detection_status.clone();
    let state = ctx.state.clone();
    let cfg_swap = ctx.config.clone();
    let path = ctx.config_path.clone();
    tokio::spawn(async move {
        let res = phase_detect::run_all(&state, &cfg_swap, status.clone()).await;
        match res {
            Ok(results) => {
                if let Err(e) = phase_detect::persist_results(&cfg_swap, &path, &results) {
                    let mut s = status.write();
                    s.last_error = Some(format!("persist failed: {e:#}"));
                }
            }
            Err(e) => {
                let mut s = status.write();
                s.running = false;
                s.last_error = Some(format!("{e:#}"));
            }
        }
    });
    Json(json!({"ok": true, "started": true})).into_response()
}
