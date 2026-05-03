//! Marstek Open API client (UDP, JSON-RPC). Polls each configured Marstek
//! battery for SoC + actual power so the dispatcher can:
//!   * weight allocation by SoC,
//!   * skip batteries that have charging or discharging temporarily disabled,
//!   * detect underdelivery and redispatch the gap.
//!
//! Wire format per Marstek Open API Rev 2.0 (LAN, JSON over UDP).
//! Default device port is 30000; per the spec the device replies to the
//! source port of the request so we can run a single shared socket and
//! demultiplex on `recv_from`.
//!
//! Sign convention from `ES.GetMode`:
//!   * `a_power`/`b_power`/`c_power` and `total_power` come from the CT
//!     and report grid-side power. We map them to "battery contribution
//!     toward grid" with the same sign convention as the Shelly
//!     (positive = battery discharging into the grid context, negative =
//!     battery charging from grid surplus).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{BatteryConfig, BatteryVendor, Config};
use crate::state::{AppState, BatteryTelemetry};

const RECV_BUF: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    // Sockets and per-battery tasks are bound at startup based on the
    // currently-loaded config. Adding a battery or changing its
    // marstek_port via the admin UI persists to disk but takes effect
    // only on restart.
    let config = config.load_full();
    // Marstek devices reply on the SAME UDP port the request came from
    // AND expect the request to originate from that very port. Binding
    // ephemeral and letting the kernel pick a source port doesn't work
    // for them. So we open one socket per distinct `marstek_port` value
    // and share it across batteries that use that port.
    let pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Skip batteries whose SoC is sourced from HA — no need to bind a
    // socket on a port that might already belong to HA's own integration.
    let needs_direct_poll = |b: &BatteryConfig| {
        b.vendor == BatteryVendor::Marstek
            && !(config.home_assistant.enabled && b.soc_entity_id.is_some())
    };

    let mut sockets_by_port: HashMap<u16, Arc<UdpSocket>> = HashMap::new();
    let mut blocked_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();
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
                info!(local = %bind_addr, "marstek telemetry socket bound");
                let socket = Arc::new(socket);

                // One receiver task per socket, dispatches replies by request id.
                {
                    let socket = socket.clone();
                    let pending = pending.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; RECV_BUF];
                        loop {
                            let (len, peer) = match socket.recv_from(&mut buf).await {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(error = %e, "marstek recv_from failed");
                                    continue;
                                }
                            };
                            let payload = &buf[..len];
                            let value: Value = match serde_json::from_slice(payload) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(peer = %peer, error = %e, "marstek invalid JSON");
                                    continue;
                                }
                            };
                            let id = value.get("id").and_then(|i| i.as_i64()).unwrap_or(-1);
                            debug!(peer = %peer, id, "marstek reply received");
                            let tx_opt = pending.lock().await.remove(&id);
                            if let Some(tx) = tx_opt {
                                let _ = tx.send(value);
                            } else {
                                debug!(peer = %peer, id, "marstek reply for unknown id");
                            }
                        }
                    });
                }

                sockets_by_port.insert(battery.marstek_port, socket);
            }
            Err(e) => {
                // Most likely cause on HA-OS: an HA Marstek HACS plugin
                // already binds this port. Don't crash the whole add-on
                // — log it, mark the battery so the UI can hint the
                // user toward the HA SoC integration, and continue.
                warn!(
                    port = battery.marstek_port,
                    error = %e,
                    "Marstek UDP port already in use — likely an HA Marstek integration owns it. \
                     Set `home_assistant.enabled = true` and add `soc_entity_id` for affected \
                     batteries to read SoC via HA instead of polling the inverter directly."
                );
                blocked_ports.insert(battery.marstek_port);
            }
        }
    }

    // Pre-mark every battery whose port couldn't be bound, so the UI
    // surfaces a clear "port blocked, use HA integration" hint instead
    // of an indefinite "no SoC yet".
    if !blocked_ports.is_empty() {
        let mut tel = state.telemetry.write();
        for battery in &config.batteries {
            if !needs_direct_poll(battery) {
                continue;
            }
            if !blocked_ports.contains(&battery.marstek_port) {
                continue;
            }
            let entry = tel.entry(battery.id.clone()).or_insert_with(|| {
                BatteryTelemetry {
                    battery_id: battery.id.clone(),
                    ..Default::default()
                }
            });
            entry.last_error = Some(format!(
                "Marstek UDP port {} is already in use (likely the HA Marstek integration). \
                 Enable `home_assistant` and set `soc_entity_id` to source SoC via HA.",
                battery.marstek_port
            ));
        }
    }

    // One polling task per battery (only those still polled directly).
    let mut tasks = tokio::task::JoinSet::new();
    for battery in &config.batteries {
        if !needs_direct_poll(battery) {
            continue;
        }
        if blocked_ports.contains(&battery.marstek_port) {
            continue;
        }
        let socket = sockets_by_port
            .get(&battery.marstek_port)
            .expect("socket for marstek_port created above")
            .clone();
        let battery = battery.clone();
        let state = state.clone();
        let pending = pending.clone();
        tasks.spawn(async move {
            let _ = poll_battery_loop(state, socket, pending, battery).await;
        });
    }

    if tasks.is_empty() {
        info!("no marstek batteries configured — telemetry idle");
        // Keep task alive but idle so the join set in main doesn't bail.
        loop {
            time::sleep(Duration::from_secs(60)).await;
        }
    }

    while let Some(res) = tasks.join_next().await {
        if let Err(e) = res {
            warn!(error = %e, "marstek poll task ended");
        }
    }
    Ok(())
}

async fn poll_battery_loop(
    state: Arc<AppState>,
    socket: Arc<UdpSocket>,
    pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
    battery: BatteryConfig,
) -> Result<()> {
    let target = SocketAddr::new(battery.address, battery.marstek_port);
    info!(battery = %battery.id, target = %target, "marstek poller for battery started");

    let mut interval = time::interval(Duration::from_millis(battery.telemetry_interval_ms));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        match poll_once(&socket, &pending, &battery, target).await {
            Ok(mut t) => {
                let mut tel = state.telemetry.write();
                if let Some(prev) = tel.get(&battery.id) {
                    // Carry forward previous SoC for ΔSoC-based
                    // direction inference in the dispatcher. Only
                    // shift it when the new reading actually differs
                    // (Marstek reports integer percentages).
                    match (prev.soc_percent, t.soc_percent) {
                        (Some(p_soc), Some(n_soc)) if (n_soc - p_soc).abs() > 1e-3 => {
                            t.previous_soc_percent = Some(p_soc);
                            t.previous_soc_at = prev.last_update;
                        }
                        _ => {
                            t.previous_soc_percent = prev.previous_soc_percent;
                            t.previous_soc_at = prev.previous_soc_at;
                        }
                    }
                }
                tel.insert(battery.id.clone(), t);
            }
            Err(e) => {
                let mut tel = state.telemetry.write();
                let entry = tel.entry(battery.id.clone()).or_insert_with(|| {
                    BatteryTelemetry {
                        battery_id: battery.id.clone(),
                        ..Default::default()
                    }
                });
                entry.last_error = Some(format!("{e:#}"));
                debug!(battery = %battery.id, error = %e, "marstek poll failed");
            }
        }
    }
}

async fn poll_once(
    socket: &UdpSocket,
    pending: &Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>,
    battery: &BatteryConfig,
    target: SocketAddr,
) -> Result<BatteryTelemetry> {
    let v = call_method(socket, pending, target, "Bat.GetStatus").await?;
    let parsed: BatStatus = serde_json::from_value(v).context("Bat.GetStatus parse")?;
    Ok(BatteryTelemetry {
        battery_id: battery.id.clone(),
        soc_percent: parsed.soc.map(|x| x as f64),
        last_update: Some(Instant::now()),
        last_error: None,
        // The polling loop fills these from the previous reading.
        previous_soc_percent: None,
        previous_soc_at: None,
    })
}

async fn call_method(
    socket: &UdpSocket,
    pending: &Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>,
    target: SocketAddr,
    method: &str,
) -> Result<Value> {
    let id = next_id();
    let req = json!({
        "id": id,
        "method": method,
        "params": { "id": 0 }
    });
    let payload = serde_json::to_vec(&req)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    pending.lock().await.insert(id, tx);

    if let Err(e) = socket.send_to(&payload, target).await {
        // Clean up the pending slot — otherwise an Err from the kernel
        // (e.g. EHOSTUNREACH because routing flapped) leaves the sender
        // in the map forever, slowly leaking memory and capturing later
        // unrelated replies.
        pending.lock().await.remove(&id);
        return Err(e.into());
    }

    let value = match time::timeout(REQUEST_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            pending.lock().await.remove(&id);
            return Err(anyhow!("response channel dropped"));
        }
        Err(_) => {
            pending.lock().await.remove(&id);
            return Err(anyhow!("marstek request {method} timed out"));
        }
    };

    if let Some(err) = value.get("error") {
        return Err(anyhow!("marstek error: {}", err));
    }
    let result = value
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("marstek response missing 'result'"))?;
    Ok(result)
}

fn next_id() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static COUNTER: AtomicI64 = AtomicI64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Deserialize)]
struct BatStatus {
    soc: Option<f64>,
}
