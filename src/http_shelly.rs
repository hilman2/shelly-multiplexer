//! HTTP / WebSocket interface of the virtual Shelly. Mounted on the same
//! port the real Shelly would use (default 80). Provides the REST-like
//! `/shelly`, `/settings`, `/status` endpoints, the `/rpc` JSON-RPC
//! endpoint and a `/rpc/<Method>` GET form used by some clients.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde_json::{Value, json};
use tracing::info;

use crate::config::Config;
use crate::rpc::RequestFrame;
use crate::state::AppState;
use crate::virtual_shelly;

#[derive(Clone)]
struct HttpCtx {
    state: Arc<AppState>,
    config: Arc<ArcSwap<Config>>,
    device: virtual_shelly::DeviceContext,
}

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let initial = config.load_full();
    // http_port = 0 disables the virtual Shelly HTTP/REST endpoint
    // entirely. Marstek-only setups don't need it (the inverters use
    // only UDP/1010 + mDNS), and skipping the bind avoids fighting
    // other services for port 80 on the host.
    if initial.virtual_shelly.http_port == 0 {
        info!("virtual shelly HTTP server disabled (http_port=0)");
        std::future::pending::<()>().await;
        return Ok(());
    }
    let bind = format!(
        "{}:{}",
        initial.virtual_shelly.bind_interface, initial.virtual_shelly.http_port
    );
    let device = virtual_shelly::DeviceContext::from_config(&initial);
    let ctx = HttpCtx {
        state: state.clone(),
        config: config.clone(),
        device,
    };
    let router = Router::new()
        .route("/shelly", get(get_shelly))
        .route("/settings", get(get_settings))
        .route("/status", get(get_status))
        .route("/rpc", post(post_rpc).get(get_rpc_root))
        .route("/rpc/{method}", get(get_rpc_method).post(post_rpc_method))
        .with_state(ctx);

    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("parsing bind address {bind}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding shelly http on {addr}"))?;
    info!(addr = %addr, "virtual shelly HTTP server listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("shelly http serve")?;
    Ok(())
}

async fn get_shelly(
    State(ctx): State<HttpCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let req = RequestFrame {
        id: Some(0),
        src: None,
        dst: None,
        method: "Shelly.GetDeviceInfo".into(),
        params: None,
    };
    let cfg = ctx.config.load_full();
    let _ = peer;
    let resp = virtual_shelly::build_response(
        &ctx.state,
        &cfg,
        &ctx.device,
        0.0,
        req,
    );
    Json(resp.result.unwrap_or(json!({})))
}

async fn get_settings(State(ctx): State<HttpCtx>) -> impl IntoResponse {
    let cfg = ctx.config.load_full();
    Json(json!({
        "device": {
            "type": "SPEM-003CEBEU",
            "mac": ctx.device.mac_hex(),
            "hostname": ctx.device.hostname(),
            "num_outputs": 0,
            "num_meters": 3
        },
        "login": {"enabled": false, "unprotected": false, "username": null},
        "fw": format!("v{}", cfg.virtual_shelly.firmware),
        "discoverable": true
    }))
}

async fn get_status(
    State(ctx): State<HttpCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let req = RequestFrame {
        id: Some(0),
        src: None,
        dst: None,
        method: "Shelly.GetStatus".into(),
        params: None,
    };
    let cfg = ctx.config.load_full();
    let _ = peer;
    let resp = virtual_shelly::build_response(
        &ctx.state,
        &cfg,
        &ctx.device,
        0.0,
        req,
    );
    Json(resp.result.unwrap_or(json!({})))
}

async fn post_rpc(
    State(ctx): State<HttpCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let request: RequestFrame = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    };
    let cfg = ctx.config.load_full();
    let _ = peer;
    let resp = virtual_shelly::build_response(&ctx.state, &cfg, &ctx.device, 0.0, request);
    Json(resp).into_response()
}

async fn get_rpc_root() -> impl IntoResponse {
    Json(json!({"info": "use POST /rpc with JSON-RPC body, or GET /rpc/<Method>"}))
}

async fn get_rpc_method(
    State(ctx): State<HttpCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(method): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let mut params_value = serde_json::Map::new();
    for (k, v) in params {
        if let Ok(n) = v.parse::<i64>() {
            params_value.insert(k, json!(n));
        } else {
            params_value.insert(k, json!(v));
        }
    }
    let request = RequestFrame {
        id: Some(0),
        src: None,
        dst: None,
        method,
        params: Some(Value::Object(params_value)),
    };
    let cfg = ctx.config.load_full();
    let _ = peer;
    let resp = virtual_shelly::build_response(&ctx.state, &cfg, &ctx.device, 0.0, request);
    match resp.result {
        Some(v) => Json(v).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": resp.error})),
        )
            .into_response(),
    }
}

async fn post_rpc_method(
    State(ctx): State<HttpCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(method): Path<String>,
    Json(body): Json<Option<Value>>,
) -> impl IntoResponse {
    let request = RequestFrame {
        id: Some(0),
        src: None,
        dst: None,
        method,
        params: body,
    };
    let cfg = ctx.config.load_full();
    let _ = peer;
    let resp = virtual_shelly::build_response(&ctx.state, &cfg, &ctx.device, 0.0, request);
    Json(resp).into_response()
}
