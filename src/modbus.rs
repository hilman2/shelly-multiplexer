//! Marstek Modbus TCP client — SoC poller.
//!
//! Replaces the old UDP JSON-RPC "Local API" path (removed in v0.5.0
//! because it was slow, racy with other Marstek clients on UDP/30000,
//! and unreliable). Modbus TCP is the stable interface every Venus E
//! firmware exposes; the register map is taken from the
//! [ViperRNMC/marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus)
//! integration. Different variants of the Venus E expose SoC at
//! different holding registers — see `MarstekModel::soc_register`.
//!
//! Mode selection:
//!   - `home_assistant.enabled = false` → this task polls every battery
//!     via Modbus TCP.
//!   - `home_assistant.enabled = true`  → this task idles; SoC is sourced
//!     from HA entities (see `ha.rs`). The two paths are mutually
//!     exclusive by config, never both at once.
//!
//! Each battery polls its own TCP session (a Marstek typically supports
//! a single Modbus connection, so per-battery isolation is the simplest
//! and most robust shape).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use tokio::time;
use tokio_modbus::client::tcp;
use tokio_modbus::prelude::*;
use tracing::{debug, info, warn};

use crate::config::{BatteryConfig, Config};
use crate::state::AppState;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let cfg = config.load_full();

    if cfg.home_assistant.enabled {
        info!("home_assistant.enabled = true → SoC sourced from HA, modbus task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let batteries: Vec<BatteryConfig> = cfg.batteries.clone();
    if batteries.is_empty() {
        info!("no batteries configured — modbus task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let mut handles = Vec::new();
    for battery in batteries {
        // Batteries without a configured Modbus host are INACTIVE — the
        // dispatcher also excludes them. This is the soft-migration path
        // for old v0.4.x configs that still carry the retired Local API
        // fields (`vendor`, `marstek_port`); they load fine, but their
        // batteries sit idle until the user wires up `modbus_host`.
        if battery.modbus_host.is_none() {
            warn!(
                battery = %battery.id,
                "battery has no modbus_host configured — staying INACTIVE \
                 (set modbus_host in [[batteries]] to enable SoC polling)"
            );
            let mut bats = state.batteries.write();
            if let Some(b) = bats.get_mut(&battery.id) {
                b.last_error = Some(
                    "inactive: modbus_host not configured (set the RS485-to-LAN bridge IP, \
                     or the battery IP for Venus E V3 with Ethernet)"
                        .into(),
                );
            }
            continue;
        }
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            poll_battery_loop(state, battery).await;
        }));
    }

    if handles.is_empty() {
        info!("no batteries with a configured modbus_host — modbus task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    for h in handles {
        let _ = h.await;
    }
    anyhow::bail!("modbus SoC tasks ended")
}

async fn poll_battery_loop(state: Arc<AppState>, battery: BatteryConfig) {
    // The Modbus endpoint is normally the battery's own IP, but Venus E 2.0
    // exposes Modbus only on an external RS485-to-LAN bridge — so we
    // resolve via `modbus_target()` which honours `modbus_host` if set.
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let register = battery.marstek_model.soc_register();
    let interval = Duration::from_millis(battery.soc_interval_ms.max(1000));

    info!(
        battery = %battery.id,
        target = %target,
        unit = battery.modbus_unit_id,
        register,
        "modbus SoC poller starting"
    );

    loop {
        let result = poll_once(target, unit, register).await;
        // parking_lot guards are !Send — close the scope before the
        // upcoming sleep so the future stays Send across .await.
        let sleep_for = {
            let mut bats = state.batteries.write();
            match result {
                Ok(soc_pct) => {
                    if let Some(b) = bats.get_mut(&battery.id) {
                        b.soc_pct = Some(soc_pct);
                        b.soc_at = Some(std::time::Instant::now());
                        b.soc_source = Some(format!("modbus:{register}"));
                        if let Some(e) = &b.last_error {
                            if e.starts_with("modbus ") {
                                b.last_error = None;
                            }
                        }
                    }
                    debug!(battery = %battery.id, soc_pct, "modbus SoC");
                    interval
                }
                Err(e) => {
                    debug!(battery = %battery.id, error = %e, "modbus SoC poll failed");
                    if let Some(b) = bats.get_mut(&battery.id) {
                        b.last_error = Some(format!("modbus SoC: {e}"));
                    }
                    // Don't hammer a dead inverter — a Marstek that's off
                    // or mid-reboot takes tens of seconds to come back.
                    // Back off then resume the normal interval.
                    RECONNECT_BACKOFF
                }
            }
        };
        time::sleep(sleep_for).await;
    }
}

async fn poll_once(target: std::net::SocketAddr, unit: Slave, register: u16) -> Result<f64> {
    let mut ctx = time::timeout(CONNECT_TIMEOUT, tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow!("modbus connect timeout to {target}"))?
        .with_context(|| format!("modbus connect to {target}"))?;

    let result = time::timeout(REQUEST_TIMEOUT, ctx.read_holding_registers(register, 1))
        .await
        .map_err(|_| anyhow!("modbus read timeout (reg {register})"))?;

    // Always close the session — the Marstek only accepts one client at a
    // time; leaving the socket open between polls would lock everyone else
    // out of the inverter.
    let _ = ctx.disconnect().await;

    let regs = result
        .with_context(|| format!("modbus read reg {register}"))?
        .map_err(|e| anyhow!("modbus exception (reg {register}): {e:?}"))?;
    let raw = regs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("modbus reg {register}: empty response"))?;

    // Marstek SoC registers report whole percent in a uint16 (0..=100).
    // Some firmware variants report decipercent (0..=1000); we accept
    // either by clamping anything >100 with a /10 scale.
    let soc = if raw > 100 { f64::from(raw) / 10.0 } else { f64::from(raw) };
    if !(0.0..=100.0).contains(&soc) {
        anyhow::bail!("modbus reg {register}: SoC {soc} out of range");
    }
    Ok(soc)
}
