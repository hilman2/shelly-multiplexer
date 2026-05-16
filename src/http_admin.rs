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
use axum::routing::{get, post, put};
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
        .route("/api/cutoff/{battery_id}/reset", post(api_cutoff_reset))
        .route("/api/health", get(api_health))
        .route("/api/modbus/debug", get(api_modbus_debug))
        .route("/api/modbus/decoded/{battery_id}", get(api_modbus_decoded))
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

async fn serve_index(headers: axum::http::HeaderMap) -> Response {
    serve_embed("index.html", &headers)
}

async fn serve_static(
    Path(path): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    serve_embed(&path, &headers)
}

/// Embedded-asset response with ETag-based revalidation.
///
/// v0.11.9: until now `serve_embed` set no cache-related headers at
/// all, which means browsers fell back to heuristic caching and would
/// silently re-use a months-old `app.js` even after we pushed a new
/// dispatcher release. The user hit exactly that: v0.11.7's UI fix
/// (drop the hardcoded × 0.95 on the dashboard headroom column) was
/// shipped in the binary but his browser kept showing the stale
/// version.
///
/// Fix: emit an ETag derived from rust-embed's per-file SHA-256 hash
/// (computed at compile time, so essentially free at request time)
/// and `Cache-Control: no-cache` so the browser revalidates EVERY
/// load. Cheap path on identical content (32-byte If-None-Match
/// check → 304 Not Modified, empty body); guaranteed pickup on any
/// asset change (different hash → 200 with new content).
fn serve_embed(path: &str, req_headers: &axum::http::HeaderMap) -> Response {
    let Some(content) = WebAssets::get(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Strong ETag: hex-encoded SHA-256 of the embedded file. Quoted
    // per RFC 7232 §2.3. The hash is part of the embedded metadata
    // populated by the proc macro, so each release ships a stable
    // tag per file.
    let etag_value = format!("\"{}\"", hex_encode(&content.metadata.sha256_hash()));
    if let Some(in_match) = req_headers.get(header::IF_NONE_MATCH) {
        if in_match.as_bytes() == etag_value.as_bytes() {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag_value)
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::empty())
                .unwrap();
        }
    }
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(mime.as_ref()).unwrap(),
        )
        .header(header::ETAG, etag_value)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(content.data.into_owned()))
        .unwrap()
}

/// Hex-encode a byte slice without pulling in another dep.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// /api/status — live runtime state
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    grid_w: Option<f64>,
    grid_age_ms: Option<u128>,
    config_path: String,
    /// "modbus" or "pulse" — surfaced so the UI can show "modbus dispatch
    /// active" in the header and conditionally hide pulse-only columns.
    dispatch_mode: &'static str,
    batteries: Vec<BatteryInfo>,
    circuits: Vec<CircuitInfo>,
}

#[derive(Serialize)]
struct BatteryInfo {
    id: String,
    circuit: String,
    address: String,
    /// `false` when no SoC source is configured for the active mode —
    /// dispatcher skips this battery entirely. Frontend renders an
    /// "inactive" pill so the user knows to fill in `modbus_host` (or
    /// `soc_entity_id` in HA mode).
    active: bool,
    max_charge_w: f64,
    max_discharge_w: f64,
    /// SoC-aware effective caps. Equal to the hardware caps unless a
    /// taper is configured AND currently engaged.
    effective_max_charge_w: f64,
    effective_max_discharge_w: f64,
    capacity_wh: f64,
    priority_weight: f64,
    soc_full_pct: Option<f64>,
    soc_empty_pct: Option<f64>,
    charge_taper_soc_pct: Option<f64>,
    charge_taper_w: Option<f64>,
    discharge_taper_soc_pct: Option<f64>,
    discharge_taper_w: Option<f64>,
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
    /// Operational state flags — derived in `api_status` from the
    /// raw fields above. The frontend renders one pill per true flag.
    /// Multiple can be true at once (e.g. tapered AND at_limit when
    /// the battery is doing all the (reduced) power it can).
    charge_tapered: bool,
    discharge_tapered: bool,
    at_charge_limit: bool,
    at_discharge_limit: bool,
    soc_full_gated: bool,
    soc_empty_gated: bool,
    /// Empirical direction lockouts (set by `detect_pulse_outcomes`
    /// when the battery refuses a directional pulse). Remaining
    /// lockout time in ms, None if not locked. Lets the UI distinguish
    /// "locked (likely full / empty)" from the SoC-based gates.
    charge_locked_for_ms: Option<u128>,
    discharge_locked_for_ms: Option<u128>,
    /// Most recent Modbus setpoint (signed W; + = discharge, − = charge,
    /// 0 = standby). None until the first successful write.
    last_modbus_setpoint_w: Option<f64>,
    last_modbus_write_ago_ms: Option<u128>,
    last_modbus_write_error: Option<String>,
    /// Battery's own power reading via Modbus (W; sign convention same
    /// as the plug). Useful sanity check against the plug PM Gen3.
    last_battery_power_w: Option<f64>,
    /// BMS-configured charging cutoff (% SoC) — read from Modbus reg
    /// 44000 at init. None until the read succeeds.
    bms_full_pct: Option<f64>,
    bms_empty_pct: Option<f64>,
    /// Current plug relay state (true = closed). None until first poll.
    plug_relay_state: Option<bool>,
    /// Remaining time (ms) before the emergency-cutoff recovery window
    /// expires. None if the plug is currently NOT in cutoff.
    plug_cut_for_ms: Option<u128>,
    /// Reason the plug was cut. Surfaced in the UI tooltip.
    plug_cut_reason: Option<String>,
    /// Circuit-level mute (plug or grid stale) — surfaced here too
    /// so the per-battery row can show "silent" without the user
    /// having to cross-reference the circuit table.
    circuit_silent: bool,
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
    let cfg = ctx.config.load_full();
    let dcfg = &cfg.dispatcher;
    let snap = ctx.state.snapshot.load_full();
    let grid_w = snap.status.total_act_power;
    let grid_age_ms = snap.age.map(|t| now.duration_since(t).as_millis());

    let bats = ctx.state.batteries.read();
    let circuits = ctx.state.circuits.read();

    let batteries: Vec<BatteryInfo> = bats
        .values()
        .map(|b| {
            let circuit_silent = circuits
                .get(&b.circuit)
                .and_then(|c| c.silent_until)
                .map(|t| t > now)
                .unwrap_or(false);
            BatteryInfo {
                id: b.id.clone(),
                circuit: b.circuit.clone(),
                address: b.address.to_string(),
                active: b.active,
                max_charge_w: b.max_charge_w,
                max_discharge_w: b.max_discharge_w,
                effective_max_charge_w: b.effective_max_charge_w(),
                effective_max_discharge_w: b.effective_max_discharge_w(),
                capacity_wh: b.capacity_wh,
                priority_weight: b.priority_weight,
                soc_full_pct: b.soc_full_pct,
                soc_empty_pct: b.soc_empty_pct,
                charge_taper_soc_pct: b.charge_taper_soc_pct,
                charge_taper_w: b.charge_taper_w,
                discharge_taper_soc_pct: b.discharge_taper_soc_pct,
                discharge_taper_w: b.discharge_taper_w,
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
                charge_tapered: b.is_charge_tapered(),
                discharge_tapered: b.is_discharge_tapered(),
                at_charge_limit: b.is_at_charge_limit(),
                at_discharge_limit: b.is_at_discharge_limit(),
                soc_full_gated: b.is_soc_full_gated(dcfg.soc_full_pct),
                soc_empty_gated: b.is_soc_empty_gated(dcfg.soc_empty_pct),
                charge_locked_for_ms: b
                    .charge_locked_until
                    .and_then(|t| t.checked_duration_since(now))
                    .map(|d| d.as_millis()),
                discharge_locked_for_ms: b
                    .discharge_locked_until
                    .and_then(|t| t.checked_duration_since(now))
                    .map(|d| d.as_millis()),
                last_modbus_setpoint_w: b.last_modbus_setpoint_w,
                last_modbus_write_ago_ms: b
                    .last_modbus_write_at
                    .map(|t| now.duration_since(t).as_millis()),
                last_modbus_write_error: b.last_modbus_write_error.clone(),
                last_battery_power_w: b.last_battery_power_w,
                bms_full_pct: b.bms_full_pct,
                bms_empty_pct: b.bms_empty_pct,
                plug_relay_state: b.plug_relay_state,
                plug_cut_for_ms: b
                    .plug_cut_until
                    .and_then(|t| t.checked_duration_since(now))
                    .map(|d| d.as_millis()),
                plug_cut_reason: b.plug_cut_reason.clone(),
                circuit_silent,
            }
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

    let dispatch_mode = match cfg.dispatcher.mode {
        crate::config::DispatchMode::Modbus => "modbus",
        crate::config::DispatchMode::Pulse => "pulse",
    };

    Json(StatusResponse {
        grid_w,
        grid_age_ms,
        config_path: ctx.config_path.display().to_string(),
        dispatch_mode,
        batteries,
        circuits: circuit_infos,
    })
}

async fn api_health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

/// Modbus traffic + cache snapshot. Returns enough state for the
/// operator to diagnose why HA's Modbus integration can or can't read
/// a specific battery: counters per response type + per-battery cache
/// freshness + the FULL register cache (as `address: u16` pairs) so a
/// curl/browser can introspect exactly what we'd serve to HA.
async fn api_modbus_debug(State(ctx): State<AdminCtx>) -> impl IntoResponse {
    use std::sync::atomic::Ordering::Relaxed;
    let stats = &ctx.state.modbus_stats;
    let bats = ctx.state.batteries.read();
    let now = std::time::Instant::now();
    let batteries: Vec<Value> = bats
        .values()
        .map(|b| {
            // Sort cache by address so the JSON output is stable.
            let mut sorted: Vec<(u16, u16)> = b.cached_holding_regs.iter()
                .map(|(k, v)| (*k, *v))
                .collect();
            sorted.sort_unstable_by_key(|(k, _)| *k);
            let cache: serde_json::Map<String, Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.to_string(), Value::from(v)))
                .collect();
            json!({
                "id": b.id,
                "virtual_unit_id": b.virtual_unit_id,
                "cache_size": b.cached_holding_regs.len(),
                "cache_refreshed_age_s": b.cached_regs_refreshed_at
                    .map(|t| now.saturating_duration_since(t).as_secs_f64()),
                "last_modbus_setpoint_w": b.last_modbus_setpoint_w,
                "last_modbus_write_age_s": b.last_modbus_write_at
                    .map(|t| now.saturating_duration_since(t).as_secs_f64()),
                "last_modbus_write_error": b.last_modbus_write_error,
                "cache": cache,
            })
        })
        .collect();
    Json(json!({
        "server": {
            "connections_accepted": stats.server_connections_accepted.load(Relaxed),
            "requests_total": stats.server_requests_total.load(Relaxed),
            "requests_ok": stats.server_requests_ok.load(Relaxed),
            "requests_illegal_address": stats.server_requests_illegal_address.load(Relaxed),
            "requests_server_busy": stats.server_requests_server_busy.load(Relaxed),
            "requests_illegal_function": stats.server_requests_illegal_function.load(Relaxed),
            "requests_gateway_unavailable": stats.server_requests_gateway_unavailable.load(Relaxed),
        },
        "outbound": {
            "reads_total": stats.outbound_reads_total.load(Relaxed),
            "reads_ok": stats.outbound_reads_ok.load(Relaxed),
            "reads_failed": stats.outbound_reads_failed.load(Relaxed),
            "writes_total": stats.outbound_writes_total.load(Relaxed),
            "writes_ok": stats.outbound_writes_ok.load(Relaxed),
            "writes_failed": stats.outbound_writes_failed.load(Relaxed),
        },
        "batteries": batteries,
    }))
}

/// Decoded register view: cached `u16`s turned into typed, scaled,
/// unit-bearing values per the variant's known register map. Drives
/// the "Battery details" UI tab — same view HA's ViperRNMC integration
/// would show, but rendered from our cache.
async fn api_modbus_decoded(
    State(ctx): State<AdminCtx>,
    Path(battery_id): Path<String>,
) -> Response {
    let bats = ctx.state.batteries.read();
    let Some(b) = bats.get(&battery_id) else {
        return (StatusCode::NOT_FOUND, format!("unknown battery: {battery_id}")).into_response();
    };
    let cfg = ctx.config.load_full();
    // Find the BatteryConfig for the marstek_model — BatteryState
    // doesn't carry it, the dispatcher reads it from the live config.
    let model = cfg
        .batteries
        .iter()
        .find(|c| c.id == battery_id)
        .map(|c| c.marstek_model);
    let Some(model) = model else {
        return (StatusCode::NOT_FOUND, format!("battery {battery_id} not in current config"))
            .into_response();
    };
    let decoded = crate::modbus_decode::decode(model, &b.cached_holding_regs);
    let now = std::time::Instant::now();
    Json(json!({
        "battery_id": battery_id,
        "virtual_unit_id": b.virtual_unit_id,
        "marstek_model": model,
        "cache_size": b.cached_holding_regs.len(),
        "cache_refreshed_age_s": b.cached_regs_refreshed_at
            .map(|t| now.saturating_duration_since(t).as_secs_f64()),
        "registers": decoded,
    }))
    .into_response()
}

/// Manual override for the emergency plug cutoff. Re-enables the plug
/// relay immediately, regardless of the recovery window. Used from
/// the UI's "reset" button once the operator has verified the
/// underlying issue is resolved.
async fn api_cutoff_reset(
    State(ctx): State<AdminCtx>,
    Path(battery_id): Path<String>,
) -> Response {
    match crate::dispatcher::manual_reset_cutoff(ctx.state.clone(), &battery_id).await {
        Ok(()) => Json(json!({"status": "ok", "battery": battery_id})).into_response(),
        Err(e) => error_response(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
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
        "virtual_modbus",
        "management",
        "dispatcher",
        "home_assistant",
        "location",
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
    // Per-battery activation IS refreshed though, so flipping
    // `home_assistant.enabled` or adding a `modbus_host` / `soc_entity_id`
    // takes effect on the next dispatcher cycle.
    ctx.state.refresh_activity(&new_cfg);
    ctx.config.store(Arc::new(new_cfg));
    info!("config updated via admin UI");

    Json(json!({"status": "ok"})).into_response()
}

fn error_response(status: StatusCode, message: String) -> Response {
    warn!(status = %status, "config update rejected: {}", message);
    (status, Json(json!({"error": message}))).into_response()
}
