//! Marstek Modbus TCP client — SoC poller AND setpoint writer.
//!
//! Two responsibilities, one transport. Both follow the register map
//! from the [ViperRNMC/marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus)
//! integration, which is the authoritative community source for every
//! Marstek Venus E variant:
//!
//! 1. **SoC poller** (`run`) — keeps each `BatteryState.soc_pct` fresh
//!    by reading the SoC holding register every `soc_interval_ms`.
//!    Active whenever HA mode is off (when HA is on, `ha.rs` takes
//!    over and this task idles).
//!
//! 2. **Setpoint writer** (`write_setpoint`, `init_dispatch`,
//!    `failsafe_shutdown`) — the v0.7 modbus-dispatch path. The
//!    dispatcher computes an absolute setpoint per battery and we
//!    write it via registers 42010 (force mode) + 42020/42021 (power).
//!    Marstek firmware has NO watchdog: if we stop writing, the
//!    battery keeps the last setpoint forever. `failsafe_shutdown`
//!    must therefore run on every clean exit, and the dispatcher's
//!    watchdog task force-resets force_mode=0 if the main loop hangs.
//!
//! Concurrency note: a Marstek (or its RS485-to-LAN bridge) typically
//! accepts only one Modbus connection at a time. We don't pool —
//! every op opens its own TCP session, runs, and disconnects. That
//! way the read and write paths can't deadlock each other; the only
//! cost is the connection handshake, which is fast over LAN.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use tokio::sync::{mpsc, watch};
use tokio::time;
use tokio_modbus::client::tcp;
use tokio_modbus::prelude::*;
use tracing::{debug, info, warn};

use crate::config::{BatteryConfig, Config, DispatchMode};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Control registers (identical across A / D / E v1.2 / E v3 per ViperRNMC).
// ---------------------------------------------------------------------------

/// Write `RS485_CTRL_ON` to enable writes in the 42000–42999 range,
/// `RS485_CTRL_OFF` to revert to normal auto operation.
const REG_RS485_CONTROL_MODE: u16 = 42000;
const RS485_CTRL_ON: u16 = 21930;
const RS485_CTRL_OFF: u16 = 21947;

/// Force mode: 0 = standby (auto), 1 = forced charge, 2 = forced discharge.
const REG_FORCE_MODE: u16 = 42010;

/// Setpoint registers. Each clamped to the per-battery max (which itself
/// must be ≤ 2500 W per Marstek firmware limits).
const REG_CHARGE_POWER_SETPOINT: u16 = 42020;
const REG_DISCHARGE_POWER_SETPOINT: u16 = 42021;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// SoC polling task — same as v0.6 but also stashes battery_power readings
// when running in modbus dispatch mode (the dispatcher cross-checks the
// plug against this to detect drift).
// ---------------------------------------------------------------------------

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

    let read_power_too = matches!(cfg.dispatcher.mode, DispatchMode::Modbus);

    let mut handles = Vec::new();
    for battery in batteries {
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
            poll_battery_loop(state, battery, read_power_too).await;
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

async fn poll_battery_loop(state: Arc<AppState>, battery: BatteryConfig, read_power_too: bool) {
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let soc_register = battery.marstek_model.soc_register();
    let power_register = battery.marstek_model.battery_power_register();
    let interval = Duration::from_millis(battery.soc_interval_ms.max(1000));

    info!(
        battery = %battery.id,
        target = %target,
        unit = battery.modbus_unit_id,
        soc_register,
        power_register,
        read_power_too,
        "modbus poller starting"
    );

    loop {
        let soc_result = read_soc(target, unit, soc_register).await;
        let power_result = if read_power_too {
            Some(read_battery_power(target, unit, power_register).await)
        } else {
            None
        };

        let sleep_for = {
            let mut bats = state.batteries.write();
            let mut sleep = interval;
            if let Some(b) = bats.get_mut(&battery.id) {
                match soc_result {
                    Ok(soc_pct) => {
                        b.soc_pct = Some(soc_pct);
                        b.soc_at = Some(std::time::Instant::now());
                        b.soc_source = Some(format!("modbus:{soc_register}"));
                        if let Some(e) = &b.last_error {
                            if e.starts_with("modbus ") {
                                b.last_error = None;
                            }
                        }
                        debug!(battery = %battery.id, soc_pct, "modbus SoC");
                    }
                    Err(e) => {
                        debug!(battery = %battery.id, error = %e, "modbus SoC poll failed");
                        b.last_error = Some(format!("modbus SoC: {e}"));
                        sleep = RECONNECT_BACKOFF;
                    }
                }
                if let Some(Ok(p)) = power_result {
                    b.last_battery_power_w = Some(p);
                }
            }
            sleep
        };
        time::sleep(sleep_for).await;
    }
}

async fn read_soc(target: std::net::SocketAddr, unit: Slave, register: u16) -> Result<f64> {
    let regs = read_holding(target, unit, register, 1).await?;
    let raw = regs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("modbus reg {register}: empty response"))?;
    // Some firmware variants report decipercent (0..=1000); accept either.
    let soc = if raw > 100 {
        f64::from(raw) / 10.0
    } else {
        f64::from(raw)
    };
    if !(0.0..=100.0).contains(&soc) {
        anyhow::bail!("modbus reg {register}: SoC {soc} out of range");
    }
    Ok(soc)
}

async fn read_battery_power(
    target: std::net::SocketAddr,
    unit: Slave,
    register: u16,
) -> Result<f64> {
    let regs = read_holding(target, unit, register, 1).await?;
    let raw = regs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("modbus reg {register}: empty response"))?;
    // int16 — Marstek encodes signed power as two's complement uint16.
    let signed = raw as i16;
    Ok(f64::from(signed))
}

async fn read_holding(
    target: std::net::SocketAddr,
    unit: Slave,
    register: u16,
    count: u16,
) -> Result<Vec<u16>> {
    let mut ctx = time::timeout(CONNECT_TIMEOUT, tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow!("modbus connect timeout to {target}"))?
        .with_context(|| format!("modbus connect to {target}"))?;
    let result = time::timeout(REQUEST_TIMEOUT, ctx.read_holding_registers(register, count))
        .await
        .map_err(|_| anyhow!("modbus read timeout (reg {register})"))?;
    let _ = ctx.disconnect().await;
    result
        .with_context(|| format!("modbus read reg {register}"))?
        .map_err(|e| anyhow!("modbus exception (reg {register}): {e:?}"))
}

// ---------------------------------------------------------------------------
// Write side — used by the v0.7 modbus dispatch path.
// ---------------------------------------------------------------------------

/// Resolved direction + power for a single Modbus write cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setpoint {
    /// Force mode 0 (auto / standby). The battery does whatever its
    /// `user_work_mode` says when no force command is active.
    Standby,
    /// Force mode 1 (charge) at this many watts.
    Charge { watts: u16 },
    /// Force mode 2 (discharge) at this many watts.
    Discharge { watts: u16 },
}

impl Setpoint {
    /// Map a signed setpoint (W; + = discharge, − = charge) onto the
    /// (force_mode, register, power) triple, clamping to the battery's
    /// own max_charge_w / max_discharge_w and Marstek's hard 2500 W
    /// firmware cap.
    pub fn from_signed_watts(w: f64, max_charge: f64, max_discharge: f64) -> Self {
        const HARD_MAX_W: f64 = 2500.0;
        if w.abs() < 0.5 {
            Setpoint::Standby
        } else if w < 0.0 {
            let watts = (-w).clamp(0.0, max_charge.min(HARD_MAX_W)) as u16;
            if watts == 0 {
                Setpoint::Standby
            } else {
                Setpoint::Charge { watts }
            }
        } else {
            let watts = w.clamp(0.0, max_discharge.min(HARD_MAX_W)) as u16;
            if watts == 0 {
                Setpoint::Standby
            } else {
                Setpoint::Discharge { watts }
            }
        }
    }

    /// Signed-watts representation, inverse of `from_signed_watts`.
    pub fn to_signed_watts(self) -> f64 {
        match self {
            Setpoint::Standby => 0.0,
            Setpoint::Charge { watts } => -f64::from(watts),
            Setpoint::Discharge { watts } => f64::from(watts),
        }
    }
}

/// Push a single setpoint to a battery. Opens a fresh Modbus connection,
/// writes the registers in safe order, closes. The caller is expected to
/// have already run `init_dispatch` once during this process so RS485
/// control mode is enabled — without that, the 42010/42020/42021 writes
/// are silently ignored by the Marstek firmware.
pub async fn write_setpoint(battery: &BatteryConfig, sp: Setpoint) -> Result<()> {
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let mut ctx = open(target, unit).await?;

    // Sequence matters: when transitioning charge → discharge (or vice
    // versa) we must set force_mode FIRST. Writing the new power into
    // 42020 while the battery is still in discharge mode (and vice
    // versa) writes the wrong register for the active direction.
    match sp {
        Setpoint::Standby => {
            write_reg(&mut ctx, REG_FORCE_MODE, 0).await?;
        }
        Setpoint::Charge { watts } => {
            write_reg(&mut ctx, REG_FORCE_MODE, 1).await?;
            write_reg(&mut ctx, REG_CHARGE_POWER_SETPOINT, watts).await?;
        }
        Setpoint::Discharge { watts } => {
            write_reg(&mut ctx, REG_FORCE_MODE, 2).await?;
            write_reg(&mut ctx, REG_DISCHARGE_POWER_SETPOINT, watts).await?;
        }
    }

    let _ = ctx.disconnect().await;
    Ok(())
}

/// BMS-configured SoC cutoffs read once during dispatch init.
/// Values are percentages (0..=100); None if the read failed.
#[derive(Debug, Clone, Copy, Default)]
pub struct BmsCutoffs {
    pub charging_cutoff_pct: Option<f64>,
    pub discharging_cutoff_pct: Option<f64>,
}

const REG_CHARGING_CUTOFF: u16 = 44000;
const REG_DISCHARGING_CUTOFF: u16 = 44001;

/// One-time init for each battery in modbus dispatch mode: enable RS485
/// control mode (so the 42xxx registers are writable), park in standby,
/// and read the BMS-configured charging / discharging cutoffs (44000 /
/// 44001) — those drive `effective_soc_full_pct` and `effective_soc_empty_pct`
/// in preference to the TOML defaults. Logs a warning on failure but
/// does NOT abort startup — the dispatcher will keep retrying and the
/// failure surfaces in `last_modbus_write_error` for the UI.
pub async fn init_dispatch(battery: &BatteryConfig) -> Result<BmsCutoffs> {
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let mut ctx = open(target, unit).await?;
    // Enable writes, then explicitly park in standby so we don't leak
    // any stale force_mode left over from a previous run / crash.
    write_reg(&mut ctx, REG_RS485_CONTROL_MODE, RS485_CTRL_ON).await?;
    write_reg(&mut ctx, REG_FORCE_MODE, 0).await?;

    // Read BMS cutoffs. These are 0..=1000 raw (scale 0.1), so the
    // upper-bound check on percentage applies after scaling.
    let cutoffs = read_bms_cutoffs(&mut ctx).await.unwrap_or_else(|e| {
        warn!(
            battery = %battery.id,
            error = %e,
            "could not read BMS cutoffs (44000/44001) — falling back to dispatcher defaults"
        );
        BmsCutoffs::default()
    });

    let _ = ctx.disconnect().await;
    info!(
        battery = %battery.id,
        bms_full_pct = ?cutoffs.charging_cutoff_pct,
        bms_empty_pct = ?cutoffs.discharging_cutoff_pct,
        "modbus dispatch init OK (RS485 control on, force_mode = 0)"
    );
    Ok(cutoffs)
}

async fn read_bms_cutoffs(ctx: &mut tokio_modbus::client::Context) -> Result<BmsCutoffs> {
    let charging = read_single(ctx, REG_CHARGING_CUTOFF).await?;
    let discharging = read_single(ctx, REG_DISCHARGING_CUTOFF).await?;
    // Doc says "0-100% (0.1 scale)" — raw 0..=1000 maps to 0.0..=100.0.
    // Some firmwares might return whole percent; clamp accordingly.
    let scale = |raw: u16| -> Option<f64> {
        let v = if raw > 100 {
            f64::from(raw) / 10.0
        } else {
            f64::from(raw)
        };
        if (0.0..=100.0).contains(&v) {
            Some(v)
        } else {
            None
        }
    };
    Ok(BmsCutoffs {
        charging_cutoff_pct: scale(charging),
        discharging_cutoff_pct: scale(discharging),
    })
}

async fn read_single(ctx: &mut tokio_modbus::client::Context, reg: u16) -> Result<u16> {
    let result = time::timeout(REQUEST_TIMEOUT, ctx.read_holding_registers(reg, 1))
        .await
        .map_err(|_| anyhow!("modbus read timeout (reg {reg})"))?;
    let regs = result
        .with_context(|| format!("modbus read reg {reg}"))?
        .map_err(|e| anyhow!("modbus exception (reg {reg}): {e:?}"))?;
    regs.first()
        .copied()
        .ok_or_else(|| anyhow!("modbus reg {reg}: empty response"))
}

/// Best-effort cleanup on process exit. Writes `force_mode = 0` then
/// `rs485_control = off` so the battery falls back to its configured
/// auto behaviour (anti-feed / trade / manual per `user_work_mode`).
/// Called from main's signal handler AND from a panic_hook — duplicates
/// are harmless because writes are idempotent.
pub async fn failsafe_shutdown(battery: &BatteryConfig) -> Result<()> {
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let mut ctx = open(target, unit).await?;
    write_reg(&mut ctx, REG_FORCE_MODE, 0).await?;
    write_reg(&mut ctx, REG_RS485_CONTROL_MODE, RS485_CTRL_OFF).await?;
    let _ = ctx.disconnect().await;
    info!(battery = %battery.id, "modbus failsafe shutdown OK");
    Ok(())
}

async fn open(
    target: std::net::SocketAddr,
    unit: Slave,
) -> Result<tokio_modbus::client::Context> {
    time::timeout(CONNECT_TIMEOUT, tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow!("modbus connect timeout to {target}"))?
        .with_context(|| format!("modbus connect to {target}"))
}

async fn write_reg(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    value: u16,
) -> Result<()> {
    let result = time::timeout(REQUEST_TIMEOUT, ctx.write_single_register(register, value))
        .await
        .map_err(|_| anyhow!("modbus write timeout (reg {register})"))?;
    result
        .with_context(|| format!("modbus write reg {register} = {value}"))?
        .map_err(|e| anyhow!("modbus exception writing reg {register}: {e:?}"))?;
    debug!(register, value, "modbus write OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-battery writer task — owns the Modbus write side for one battery.
// ---------------------------------------------------------------------------
//
// The dispatcher tick runs every cycle_ms (default 200 ms) but each
// Modbus write costs an RTT through the RS485-to-LAN bridge (~50-200 ms),
// so the writes can't share the dispatcher's loop. Solution: one
// `BatteryWriter` task per battery, fed by a `watch` channel from the
// dispatcher. The writer:
//
//   - receives the latest desired Setpoint on every change,
//   - skips writes inside `setpoint_deadband_w`,
//   - re-writes the current setpoint every `modbus_heartbeat_s` even
//     when unchanged (recovers from any dropped writes + keeps the
//     "I'm still alive" signal flowing — Marstek has no firmware
//     watchdog so heartbeats are our only out-of-band liveness
//     indicator),
//   - on receiving a `Shutdown` command, attempts a failsafe write
//     (force_mode = 0, RS485 control off) and exits.

/// Handle the dispatcher uses to push setpoints. Cheap to clone; the
/// underlying watch sender + mpsc shutdown channel are reference-counted.
#[derive(Clone)]
pub struct ModbusDispatch {
    inner: Arc<ModbusDispatchInner>,
}

struct ModbusDispatchInner {
    batteries: HashMap<String, BatterySink>,
}

struct BatterySink {
    config: BatteryConfig,
    sp_tx: watch::Sender<Setpoint>,
    shutdown_tx: mpsc::Sender<()>,
}

impl ModbusDispatch {
    /// Spawn one writer task per battery with a configured `modbus_host`.
    /// Returns the handle the dispatcher uses to push setpoints.
    pub fn spawn(state: Arc<AppState>, cfg: &Config) -> Self {
        let mut batteries = HashMap::new();
        let heartbeat = Duration::from_secs_f64(cfg.dispatcher.modbus_heartbeat_s.max(1.0));
        let deadband_w = cfg.dispatcher.setpoint_deadband_w;
        for b in &cfg.batteries {
            if b.modbus_host.is_none() {
                warn!(
                    battery = %b.id,
                    "skipping modbus writer: modbus_host not configured"
                );
                continue;
            }
            let (sp_tx, sp_rx) = watch::channel(Setpoint::Standby);
            let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
            let task = BatteryWriter {
                battery: b.clone(),
                rx: sp_rx,
                shutdown: shutdown_rx,
                state: state.clone(),
                heartbeat,
                deadband_w,
                last_written: None,
                last_write_at: None,
            };
            tokio::spawn(task.run());
            batteries.insert(
                b.id.clone(),
                BatterySink {
                    config: b.clone(),
                    sp_tx,
                    shutdown_tx,
                },
            );
        }
        Self {
            inner: Arc::new(ModbusDispatchInner { batteries }),
        }
    }

    /// Push a desired setpoint for one battery. No-op if the battery
    /// has no Modbus writer (= no modbus_host configured).
    pub fn send(&self, battery_id: &str, sp: Setpoint) {
        if let Some(sink) = self.inner.batteries.get(battery_id) {
            // watch::send always succeeds unless all receivers dropped —
            // the writer task only exits on shutdown so this can't fail
            // during normal operation. send_replace overwrites the latest
            // value even if it's identical, ensuring the heartbeat path
            // still sees a "fresh" timestamp on changed-detection.
            let _ = sink.sp_tx.send(sp);
        }
    }

    /// Trigger graceful shutdown across every battery in parallel.
    /// Each writer attempts a failsafe write (force_mode = 0,
    /// RS485 control off) then exits. Bounded by `timeout`.
    pub async fn shutdown(&self, timeout: Duration) {
        for (id, sink) in &self.inner.batteries {
            if sink.shutdown_tx.try_send(()).is_err() {
                debug!(battery = %id, "shutdown signal already sent or task gone");
            }
        }
        // No explicit join — tasks complete on their own. We just give
        // them a bounded window before main() returns.
        time::sleep(timeout).await;
    }

    /// List of battery IDs that have a live Modbus writer.
    pub fn battery_ids(&self) -> Vec<String> {
        self.inner.batteries.keys().cloned().collect()
    }

    /// Best-effort synchronous failsafe — used by panic_hook where we
    /// have no async runtime. Spawns a short-lived tokio runtime and
    /// runs `failsafe_shutdown` per battery serially.
    pub fn panic_failsafe(&self) {
        let configs: Vec<BatteryConfig> = self
            .inner
            .batteries
            .values()
            .map(|s| s.config.clone())
            .collect();
        if configs.is_empty() {
            return;
        }
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("PANIC FAILSAFE: could not build runtime: {e}");
                return;
            }
        };
        rt.block_on(async {
            for cfg in configs {
                match time::timeout(Duration::from_secs(2), failsafe_shutdown(&cfg)).await {
                    Ok(Ok(())) => eprintln!("PANIC FAILSAFE: {} → standby", cfg.id),
                    Ok(Err(e)) => eprintln!("PANIC FAILSAFE {}: {e:#}", cfg.id),
                    Err(_) => eprintln!("PANIC FAILSAFE {} timed out", cfg.id),
                }
            }
        });
    }
}

struct BatteryWriter {
    battery: BatteryConfig,
    rx: watch::Receiver<Setpoint>,
    shutdown: mpsc::Receiver<()>,
    state: Arc<AppState>,
    heartbeat: Duration,
    deadband_w: f64,
    last_written: Option<Setpoint>,
    last_write_at: Option<Instant>,
}

impl BatteryWriter {
    async fn run(mut self) {
        // Initialise the inverter: enable RS485 control + force_mode=0,
        // read BMS cutoffs into AppState so the dispatcher's
        // effective_soc_*_pct gates use the BMS-truth instead of TOML
        // defaults. Retried until it sticks; until then the battery
        // stays inactive.
        loop {
            match init_dispatch(&self.battery).await {
                Ok(cutoffs) => {
                    self.record_success(Setpoint::Standby);
                    self.record_bms_cutoffs(cutoffs);
                    break;
                }
                Err(e) => {
                    self.record_error(format!("init: {e}"));
                    warn!(
                        battery = %self.battery.id,
                        error = %e,
                        "modbus init failed, retrying in 5s"
                    );
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        }

        loop {
            let next_heartbeat = self
                .last_write_at
                .map(|t| (t + self.heartbeat).saturating_duration_since(Instant::now()))
                .unwrap_or(self.heartbeat);
            tokio::select! {
                biased;
                _ = self.shutdown.recv() => {
                    let _ = self.do_write(Setpoint::Standby).await;
                    let _ = failsafe_shutdown(&self.battery).await;
                    info!(battery = %self.battery.id, "modbus writer task exiting");
                    return;
                }
                changed = self.rx.changed() => {
                    if changed.is_err() { return; }
                    let desired = *self.rx.borrow();
                    if self.should_write(desired) {
                        let _ = self.do_write(desired).await;
                    }
                }
                _ = time::sleep(next_heartbeat) => {
                    let desired = *self.rx.borrow();
                    // Heartbeat: even if unchanged, re-issue so a dropped
                    // packet or rebooting bridge can't leave the Marstek
                    // stuck on a stale setpoint.
                    let _ = self.do_write(desired).await;
                }
            }
        }
    }

    /// Decide whether a desired setpoint deserves a Modbus round-trip
    /// right now. Different direction OR ≥ deadband change OR no prior
    /// write yet → yes.
    fn should_write(&self, desired: Setpoint) -> bool {
        let Some(last) = self.last_written else {
            return true;
        };
        if std::mem::discriminant(&desired) != std::mem::discriminant(&last) {
            return true;
        }
        let diff = (desired.to_signed_watts() - last.to_signed_watts()).abs();
        diff >= self.deadband_w
    }

    async fn do_write(&mut self, sp: Setpoint) -> Result<()> {
        match write_setpoint(&self.battery, sp).await {
            Ok(()) => {
                self.record_success(sp);
                debug!(battery = %self.battery.id, ?sp, "setpoint written");
                Ok(())
            }
            Err(e) => {
                self.record_error(format!("write: {e:#}"));
                debug!(battery = %self.battery.id, error = %e, "modbus write failed");
                Err(e)
            }
        }
    }

    fn record_success(&mut self, sp: Setpoint) {
        self.last_written = Some(sp);
        self.last_write_at = Some(Instant::now());
        let mut bats = self.state.batteries.write();
        if let Some(b) = bats.get_mut(&self.battery.id) {
            b.last_modbus_setpoint_w = Some(sp.to_signed_watts());
            b.last_modbus_write_at = self.last_write_at;
            b.last_modbus_write_error = None;
        }
    }

    fn record_error(&mut self, msg: String) {
        let mut bats = self.state.batteries.write();
        if let Some(b) = bats.get_mut(&self.battery.id) {
            b.last_modbus_write_error = Some(msg);
        }
    }

    fn record_bms_cutoffs(&mut self, cutoffs: BmsCutoffs) {
        let mut bats = self.state.batteries.write();
        if let Some(b) = bats.get_mut(&self.battery.id) {
            b.bms_full_pct = cutoffs.charging_cutoff_pct;
            b.bms_empty_pct = cutoffs.discharging_cutoff_pct;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setpoint_standby_for_near_zero() {
        assert_eq!(
            Setpoint::from_signed_watts(0.0, 2500.0, 800.0),
            Setpoint::Standby
        );
        assert_eq!(
            Setpoint::from_signed_watts(0.4, 2500.0, 800.0),
            Setpoint::Standby
        );
        assert_eq!(
            Setpoint::from_signed_watts(-0.4, 2500.0, 800.0),
            Setpoint::Standby
        );
    }

    #[test]
    fn setpoint_charge_negative_signed() {
        assert_eq!(
            Setpoint::from_signed_watts(-1500.0, 2500.0, 800.0),
            Setpoint::Charge { watts: 1500 }
        );
    }

    #[test]
    fn setpoint_discharge_positive_signed() {
        assert_eq!(
            Setpoint::from_signed_watts(700.0, 2500.0, 800.0),
            Setpoint::Discharge { watts: 700 }
        );
    }

    #[test]
    fn setpoint_clamps_to_per_battery_max() {
        // max_charge = 2500 — request 5000 → clamp to 2500.
        assert_eq!(
            Setpoint::from_signed_watts(-5000.0, 2500.0, 800.0),
            Setpoint::Charge { watts: 2500 }
        );
        // max_discharge = 800 — request 2000 → clamp to 800.
        assert_eq!(
            Setpoint::from_signed_watts(2000.0, 2500.0, 800.0),
            Setpoint::Discharge { watts: 800 }
        );
    }

    #[test]
    fn setpoint_respects_firmware_2500w_cap() {
        // max_charge = 9999 (config liar) — clamp to firmware 2500 cap.
        assert_eq!(
            Setpoint::from_signed_watts(-4000.0, 9999.0, 9999.0),
            Setpoint::Charge { watts: 2500 }
        );
    }

    #[test]
    fn setpoint_roundtrip_to_signed_watts() {
        assert_eq!(Setpoint::Standby.to_signed_watts(), 0.0);
        assert_eq!(Setpoint::Charge { watts: 1500 }.to_signed_watts(), -1500.0);
        assert_eq!(Setpoint::Discharge { watts: 700 }.to_signed_watts(), 700.0);
    }
}
