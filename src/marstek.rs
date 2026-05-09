//! Marstek Open API client (UDP, JSON-RPC) — SoC poller.
//!
//! Polls each Marstek for SoC % and writes it into `BatteryState.soc_pct`.
//! The dispatcher uses SoC to skip charging full batteries / discharging
//! empty ones. Output power is now sourced exclusively from the Shelly
//! Plug PM Gen3 (see `plug.rs`).
//!
//! Wire: Marstek replies to the source port of the request, so each
//! distinct `marstek_port` gets one shared UdpSocket bound on that port.
//! On port conflicts (e.g. an HA integration already owns it) we log and
//! skip the affected batteries — the dispatcher continues without SoC for
//! them, falling back to "always eligible".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{BatteryConfig, BatteryVendor, Config};
use crate::state::AppState;

const RECV_BUF: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let config = config.load_full();
    let pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Direct Marstek poll runs ONLY for batteries that have no
    // soc_entity_id configured. If the user pointed us at an HA entity,
    // they are signalling "HA is the SoC source" — even if the HA poll
    // is currently failing we MUST NOT also bang on the Marstek's UDP
    // port: the user's HACS Marstek integration probably owns it
    // already and the device's API doesn't tolerate two clients well.
    let needs_direct_poll = |b: &BatteryConfig| {
        b.vendor == BatteryVendor::Marstek && b.soc_entity_id.is_none()
    };

    let mut sockets_by_port: HashMap<u16, Arc<UdpSocket>> = HashMap::new();
    let mut blocked_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();
    // One global request-id counter per port. Shared across batteries on
    // that port so we never collide on the `pending` map (which keys by id).
    let mut id_counters: HashMap<u16, Arc<AtomicU64>> = HashMap::new();

    for battery in &config.batteries {
        if !needs_direct_poll(battery) {
            continue;
        }
        if sockets_by_port.contains_key(&battery.marstek_port)
            || blocked_ports.contains(&battery.marstek_port)
        {
            continue;
        }
        let bind_addr = format!("0.0.0.0:{}", battery.marstek_port);
        match UdpSocket::bind(&bind_addr).await {
            Ok(socket) => {
                info!(local = %bind_addr, "marstek SoC socket bound");
                let socket = Arc::new(socket);
                let pending_c = pending.clone();
                let socket_c = socket.clone();
                tokio::spawn(async move {
                    receive_loop(socket_c, pending_c).await;
                });
                sockets_by_port.insert(battery.marstek_port, socket);
                id_counters
                    .entry(battery.marstek_port)
                    .or_insert_with(|| Arc::new(AtomicU64::new(1)));
            }
            Err(e) => {
                warn!(local = %bind_addr, error = %e, "marstek SoC port unavailable - skipping batteries on this port");
                blocked_ports.insert(battery.marstek_port);
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery.id) {
                    b.last_error = Some(format!("marstek port {} bind failed: {e}", battery.marstek_port));
                }
            }
        }
    }

    let mut handles = Vec::new();
    for battery in &config.batteries {
        if !needs_direct_poll(battery) {
            continue;
        }
        let Some(socket) = sockets_by_port.get(&battery.marstek_port).cloned() else {
            continue;
        };
        let Some(id_counter) = id_counters.get(&battery.marstek_port).cloned() else {
            continue;
        };
        let state = state.clone();
        let pending = pending.clone();
        let battery = battery.clone();
        handles.push(tokio::spawn(async move {
            poll_battery_loop(state, socket, pending, id_counter, battery).await;
        }));
    }

    if handles.is_empty() {
        // No batteries configured for direct polling — empty template,
        // or all batteries have soc_entity_id and HA is enabled. Idle.
        info!("no batteries to poll for SoC — marstek task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    for h in handles {
        let _ = h.await;
    }
    anyhow::bail!("marstek SoC tasks ended")
}

async fn receive_loop(
    socket: Arc<UdpSocket>,
    pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
) {
    let mut buf = vec![0u8; RECV_BUF];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, _peer)) => {
                let payload = &buf[..len];
                let v: Value = match serde_json::from_slice(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                    let mut p = pending.lock().await;
                    if let Some(tx) = p.remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "marstek socket recv failed");
                time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn poll_battery_loop(
    state: Arc<AppState>,
    socket: Arc<UdpSocket>,
    pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
    id_counter: Arc<AtomicU64>,
    battery: BatteryConfig,
) {
    let interval = Duration::from_millis(battery.soc_interval_ms.max(1000));
    let target = SocketAddr::new(battery.address, battery.marstek_port);
    loop {
        time::sleep(interval).await;
        // Per-port atomic counter — never collides across batteries that
        // share a UDP port and the `pending` map keyed by id.
        let next_id = id_counter.fetch_add(1, Ordering::Relaxed) as i64;
        match request_soc(&socket, &pending, target, next_id).await {
            Ok(soc_pct) => {
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery.id) {
                    b.soc_pct = Some(soc_pct);
                    b.soc_at = Some(std::time::Instant::now());
                    b.soc_source = Some("marstek-direct".into());
                    if let Some(e) = &b.last_error {
                        if e.starts_with("marstek ") {
                            b.last_error = None;
                        }
                    }
                }
                debug!(battery = %battery.id, soc_pct, "marstek SoC");
            }
            Err(e) => {
                debug!(battery = %battery.id, error = %e, "marstek SoC poll failed");
                let mut bats = state.batteries.write();
                if let Some(b) = bats.get_mut(&battery.id) {
                    b.last_error = Some(format!("marstek SoC: {e}"));
                }
            }
        }
    }
}

async fn request_soc(
    socket: &UdpSocket,
    pending: &Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
    target: SocketAddr,
    id: i64,
) -> Result<f64> {
    let req = json!({
        "id": id,
        "method": "ES.GetMode",
        "params": {"id": 0}
    });
    let body = serde_json::to_vec(&req).context("marstek req encode")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending.lock().await.insert(id, tx);

    // Ensure the pending entry is removed on every exit path (timeout,
    // send failure, anywhere we early-return). Without this the map grew
    // unboundedly whenever a Marstek went silent.
    let pending_guard = pending.clone();
    let cleanup = scopeguard::guard((), move |_| {
        let pending_guard = pending_guard.clone();
        tokio::spawn(async move {
            pending_guard.lock().await.remove(&id);
        });
    });

    socket.send_to(&body, target).await.context("marstek send")?;
    let v = time::timeout(REQUEST_TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("timeout"))?
        .map_err(|_| anyhow!("oneshot dropped"))?;
    // Got a response → the receive_loop already removed the entry. Defuse
    // the cleanup so we don't spawn a redundant remove.
    scopeguard::ScopeGuard::into_inner(cleanup);

    let soc = v
        .get("result")
        .and_then(|r| r.get("soc").or_else(|| r.get("battery_soc")))
        .and_then(|s| s.as_f64())
        .ok_or_else(|| anyhow!("response missing soc"))?;
    Ok(soc)
}
