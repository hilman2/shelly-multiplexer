//! Polls the real Shelly Pro 3EM via UDP-RPC and feeds snapshots into
//! `AppState`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use serde_json::json;
use tokio::net::UdpSocket;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::rpc::{EmStatusIncoming, ResponseFrame};
use crate::state::{AppState, EmSnapshot};

const RECV_BUF: usize = 16 * 1024;

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    // Local socket binding does not depend on a config field that the
    // user can change at runtime — bind once.
    let bind = "0.0.0.0:0".parse::<SocketAddr>()?;
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("binding poller socket on {bind}"))?;
    // Intentionally NOT calling socket.connect(): a Shelly with both
    // Wi-Fi and Ethernet may reply from a different source IP than the
    // one we sent to (the OS picks the egress interface based on
    // routing, and the source-IP follows that). With connect() the
    // kernel would drop those replies; using send_to / recv_from we
    // accept replies from any address, then verify by matching the
    // response's RPC `id` to our request.

    {
        let cfg = config.load();
        info!(
            target = %SocketAddr::new(cfg.real_shelly.host, cfg.real_shelly.udp_port),
            "real-shelly poller started"
        );
    }

    let mut request_id: i64 = 1;
    let mut last_log = Instant::now();
    let mut consecutive_errors: u32 = 0;
    let mut tick_count: u64 = 0;
    let mut buf = vec![0u8; RECV_BUF];
    // Track previous successful tick for energy integration. None on the
    // first poll or after a recovery (we don't extrapolate energy across
    // outage gaps).
    let mut last_success: Option<Instant> = None;

    loop {
        // Re-read config each tick so changes from the admin UI are
        // picked up without restart (host, port, intervals, timeout).
        let cfg = config.load_full();
        let target = SocketAddr::new(cfg.real_shelly.host, cfg.real_shelly.udp_port);
        let timeout = Duration::from_millis(cfg.real_shelly.request_timeout_ms);
        let interval_ms = cfg.real_shelly.poll_interval_ms.max(1);
        time::sleep(Duration::from_millis(interval_ms)).await;
        request_id = request_id.wrapping_add(1);
        tick_count = tick_count.wrapping_add(1);

        match poll_once(&socket, &mut buf, request_id, timeout, target).await {
            Ok(status) => {
                if tick_count.is_multiple_of(10) {
                    info!(
                        tick = tick_count,
                        a_w = ?status.a_act_power,
                        b_w = ?status.b_act_power,
                        c_w = ?status.c_act_power,
                        total_w = ?status.total_act_power,
                        "real shelly poll tick"
                    );
                }
                let now = Instant::now();
                if let Some(prev) = last_success {
                    let dt = now.duration_since(prev).as_secs_f64();
                    // Only integrate over short, contiguous gaps. A poll
                    // failure or a long pause means we don't actually know
                    // what happened in between, so we skip rather than
                    // assume the last reading held.
                    if dt > 0.0 && dt < 5.0 {
                        state.energy.write().integrate(&status, dt);
                    }
                }
                // EMA smoothing on the grid power reading. Time constant
                // comes from dispatcher.grid_smoothing_s (live-reloaded).
                // PV inverter PWM ripple often swings the raw reading by
                // ±2 kW at 4 Hz; without smoothing the dispatcher would
                // chase that noise instead of the real load.
                let smoothed_grid_w = {
                    let prev_snap = state.snapshot.load_full();
                    let raw = status.total_act_power;
                    let tau = config.load_full().dispatcher.grid_smoothing_s.max(0.0);
                    match (raw, prev_snap.smoothed_grid_w, last_success) {
                        // First sample or tau=0 → no smoothing, pass through.
                        (Some(r), None, _) => Some(r),
                        (Some(r), _, _) if tau == 0.0 => Some(r),
                        // Stale-prev: more than tau elapsed → reset to current.
                        (Some(r), Some(_), Some(prev_t))
                            if now.duration_since(prev_t).as_secs_f64() > tau * 4.0 =>
                        {
                            Some(r)
                        }
                        (Some(r), Some(prev), Some(prev_t)) => {
                            let dt = now.duration_since(prev_t).as_secs_f64();
                            let alpha = 1.0 - (-dt / tau).exp();
                            Some(prev + alpha * (r - prev))
                        }
                        (Some(r), Some(_), None) => Some(r),
                        (None, prev, _) => prev,
                    }
                };
                last_success = Some(now);
                state.snapshot.store(Arc::new(EmSnapshot {
                    status,
                    age: Some(now),
                    smoothed_grid_w,
                }));
                if consecutive_errors > 0 {
                    info!("real shelly recovered after {consecutive_errors} failed polls");
                    consecutive_errors = 0;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                // Reset integration anchor — we'll re-anchor on the next
                // successful poll instead of integrating across the outage.
                last_success = None;
                if last_log.elapsed() > Duration::from_secs(5) {
                    warn!(error = %e, errors = consecutive_errors, "real shelly poll failed");
                    last_log = Instant::now();
                }
            }
        }
    }
}

async fn poll_once(
    socket: &UdpSocket,
    buf: &mut [u8],
    request_id: i64,
    timeout: Duration,
    target: SocketAddr,
) -> Result<EmStatusIncoming> {
    let req = json!({
        "id": request_id,
        "src": "shelly-multiplexer",
        "method": "EM.GetStatus",
        "params": { "id": 0 }
    });
    let payload = serde_json::to_vec(&req)?;
    socket.send_to(&payload, target).await?;

    // Read replies until we either get one matching our request id or we
    // hit the timeout. We intentionally accept replies from any source
    // address — Shelly devices with multiple interfaces (Wi-Fi + LAN)
    // may reply from a different IP than the one we sent to, depending
    // on which interface the kernel picks for the return path.
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let recv = time::timeout_at(deadline, socket.recv_from(buf)).await;
        let (len, peer) = match recv {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(anyhow!("real shelly poll timed out")),
        };
        let frame: ResponseFrame = match serde_json::from_slice(&buf[..len]) {
            Ok(f) => f,
            Err(e) => {
                debug!(peer = %peer, error = %e, "ignoring non-JSON UDP packet");
                continue;
            }
        };
        if frame.id != Some(request_id) {
            debug!(
                peer = %peer,
                got_id = ?frame.id,
                want_id = request_id,
                "ignoring out-of-band UDP reply"
            );
            continue;
        }
        if let Some(err) = frame.error {
            return Err(anyhow!(
                "real shelly returned error {}: {}",
                err.code,
                err.message
            ));
        }
        let result = frame
            .result
            .ok_or_else(|| anyhow!("real shelly returned empty result"))?;
        let status: EmStatusIncoming = serde_json::from_value(result)
            .context("parsing EmStatus from real shelly result")?;
        debug!(
            peer = %peer,
            a = status.a_act_power,
            b = status.b_act_power,
            c = status.c_act_power,
            total = status.total_act_power,
            "real shelly snapshot"
        );
        return Ok(status);
    }
}

