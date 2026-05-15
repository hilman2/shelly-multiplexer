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

const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Marstek firmware floor (W): commands below this magnitude collapse to
/// Standby because the inverter silently ignores tiny setpoints. Per the
/// hilman2/MarstekVenus reference impl. Exposed `pub` so the dispatcher
/// can run a consolidation pass — splitting a setpoint of e.g. 80 W
/// across two batteries gives 40 W each, which then rounds back to
/// Standby on both. The consolidation pass concentrates such cases onto
/// one battery so the command actually fires.
pub const MARSTEK_MIN_W: f64 = 50.0;

fn connect_timeout() -> Duration {
    Duration::from_millis(crate::config::MODBUS_CONNECT_TIMEOUT_MS)
}
fn request_timeout() -> Duration {
    Duration::from_millis(crate::config::MODBUS_REQUEST_TIMEOUT_MS)
}
fn write_retries() -> u32 {
    crate::config::MODBUS_WRITE_RETRIES
}

// ---------------------------------------------------------------------------
// SoC polling task — same as v0.6 but also stashes battery_power readings
// when running in modbus dispatch mode (the dispatcher cross-checks the
// plug against this to detect drift).
// ---------------------------------------------------------------------------

pub async fn run(state: Arc<AppState>, config: Arc<ArcSwap<Config>>) -> Result<()> {
    let cfg = config.load_full();

    // SoC-source routing:
    //   - modbus dispatch mode → BatteryWriter owns the Modbus session
    //     per battery and piggybacks the SoC read onto its existing
    //     connection. This standalone poll task idles to avoid a second
    //     TCP client competing for the inverter's single Modbus slot.
    //   - pulse dispatch mode + HA enabled → HA bridge owns SoC, we idle.
    //   - pulse dispatch mode + HA disabled → we own SoC via Modbus.
    if matches!(cfg.dispatcher.mode, DispatchMode::Modbus) {
        info!("modbus dispatch mode → SoC piggybacked on BatteryWriter, standalone poll idle");
        std::future::pending::<()>().await;
        return Ok(());
    }
    if matches!(cfg.dispatcher.mode, DispatchMode::Pulse) && cfg.home_assistant.enabled {
        info!("pulse mode + HA enabled → SoC sourced from HA, modbus SoC poll idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let batteries: Vec<BatteryConfig> = cfg.batteries.clone();
    if batteries.is_empty() {
        info!("no batteries configured — modbus task idle");
        std::future::pending::<()>().await;
        return Ok(());
    }

    // In pulse mode there's no BatteryWriter to piggyback on, so we keep
    // the legacy per-battery poll loop. battery_power is only relevant
    // in modbus mode (the dispatcher's plug measurement is authoritative
    // in pulse mode), so we skip that here.
    let read_power_too = false;

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
    let model = battery.marstek_model;
    let soc_register = model.soc_register();
    let soc_scale = model.soc_scale();
    let power_register = model.battery_power_register();
    let power_is_int32 = model.battery_power_is_int32();
    let interval = Duration::from_millis(battery.soc_interval_ms.max(1000));

    info!(
        battery = %battery.id,
        target = %target,
        unit = battery.modbus_unit_id,
        ?model,
        soc_register,
        soc_scale,
        power_register,
        power_is_int32,
        read_power_too,
        "modbus poller starting"
    );

    loop {
        let soc_result = read_soc(target, unit, soc_register, soc_scale).await;
        let power_result = if read_power_too {
            Some(read_battery_power(target, unit, power_register, power_is_int32).await)
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

// ---------------------------------------------------------------------------
// Low-level helpers that operate on an EXISTING `&mut Context`. The
// caller owns the connection lifecycle: BatteryWriter keeps one open
// for its task lifetime and reconnects only on error; the legacy
// pulse-mode poll loop and standalone failsafe helpers open + close
// per operation via the `*_using_*` wrappers further down.
// ---------------------------------------------------------------------------

async fn read_holding_on(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    count: u16,
) -> Result<Vec<u16>> {
    let result = time::timeout(request_timeout(), ctx.read_holding_registers(register, count))
        .await
        .map_err(|_| anyhow!("modbus read timeout (reg {register})"))?;
    result
        .with_context(|| format!("modbus read reg {register}"))?
        .map_err(|e| anyhow!("modbus exception (reg {register}): {e:?}"))
}

async fn read_soc_on(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    scale: f64,
) -> Result<f64> {
    let regs = read_holding_on(ctx, register, 1).await?;
    let raw = regs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("modbus reg {register}: empty response"))?;
    let soc = f64::from(raw) * scale;
    if !(0.0..=100.0).contains(&soc) {
        anyhow::bail!("modbus reg {register}: SoC {soc} out of range");
    }
    Ok(soc)
}

async fn read_battery_power_on(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    is_int32: bool,
) -> Result<f64> {
    if is_int32 {
        // Venus E v1/v2: signed 32-bit power split across TWO registers
        // (big-endian word order — high word first).
        let regs = read_holding_on(ctx, register, 2).await?;
        if regs.len() < 2 {
            anyhow::bail!("modbus reg {register}: expected 2 registers, got {}", regs.len());
        }
        let combined = ((regs[0] as u32) << 16) | (regs[1] as u32);
        Ok(f64::from(combined as i32))
    } else {
        let regs = read_holding_on(ctx, register, 1).await?;
        let raw = regs
            .first()
            .copied()
            .ok_or_else(|| anyhow!("modbus reg {register}: empty response"))?;
        Ok(f64::from(raw as i16))
    }
}

// Open+op+close wrappers — only used by the legacy pulse-mode SoC
// poll loop and one-shot failsafe paths. The hot modbus-dispatch path
// uses the `_on` helpers directly on BatteryWriter::ctx (long-lived).

async fn read_soc(
    target: std::net::SocketAddr,
    unit: Slave,
    register: u16,
    scale: f64,
) -> Result<f64> {
    let mut ctx = open(target, unit).await?;
    let r = read_soc_on(&mut ctx, register, scale).await;
    let _ = ctx.disconnect().await;
    r
}

async fn read_battery_power(
    target: std::net::SocketAddr,
    unit: Slave,
    register: u16,
    is_int32: bool,
) -> Result<f64> {
    let mut ctx = open(target, unit).await?;
    let r = read_battery_power_on(&mut ctx, register, is_int32).await;
    let _ = ctx.disconnect().await;
    r
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
    ///
    /// Power magnitudes below the firmware's `MARSTEK_MIN_W` threshold
    /// collapse to `Standby` because the Marstek silently ignores tiny
    /// commands — safer to explicitly stop than to issue a command that
    /// won't act.
    pub fn from_signed_watts(w: f64, max_charge: f64, max_discharge: f64) -> Self {
        const HARD_MAX_W: f64 = 2500.0;
        if w.abs() < MARSTEK_MIN_W {
            Setpoint::Standby
        } else if w < 0.0 {
            let watts = (-w).clamp(MARSTEK_MIN_W, max_charge.min(HARD_MAX_W)) as u16;
            Setpoint::Charge { watts }
        } else {
            let watts = w.clamp(MARSTEK_MIN_W, max_discharge.min(HARD_MAX_W)) as u16;
            Setpoint::Discharge { watts }
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

/// Push a single setpoint to a battery, retrying up to
/// `MODBUS_WRITE_RETRIES` times on transient failures.
/// Each attempt opens a fresh TCP connection — connect timeouts and
/// half-open sockets from a previous failed attempt don't leak into
/// the retry.
pub async fn write_setpoint(battery: &BatteryConfig, sp: Setpoint) -> Result<()> {
    let max_attempts = write_retries().saturating_add(1);
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        match write_setpoint_once(battery, sp).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < max_attempts {
                    debug!(
                        battery = %battery.id,
                        attempt,
                        max_attempts,
                        error = %e,
                        "modbus write attempt failed — retrying after backoff"
                    );
                    // Short exponential-ish backoff. The Marstek's own
                    // ramp window is in the 1-3 s range so we don't
                    // need to wait much.
                    time::sleep(Duration::from_millis(
                        200u64.saturating_mul(attempt as u64),
                    ))
                    .await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("write_setpoint exhausted retries")))
}

/// One attempt at the write sequence — opens a fresh Modbus connection,
/// runs the proven write sequence from hilman2/MarstekVenus (Python
/// implementation that's been tested on real hardware), closes.
///
/// The Marstek firmware quirks this sequence accounts for:
///
///   1. RS485 control mode (42000 = 21930) can silently drop on
///      firmware reboot / app interaction / power flicker. We re-arm
///      on every call, then wait 100 ms for the firmware to apply it.
///   2. The firmware needs time between writes. Without the sleeps the
///      writes ack but the inverter doesn't act on them. Empirically
///      determined timings: 100 ms after RS485 enable, 200 ms after
///      zeroing the opposite direction, 500 ms after force_mode.
///   3. Switching direction without first zeroing the OTHER direction's
///      power register leaves the inverter in an inconsistent state
///      (it momentarily sees both charge and discharge setpoints
///      non-zero). We zero out the opposite direction first.
///   4. Setpoints below ~50 W are treated as no-op by the firmware.
///      `Setpoint::from_signed_watts` already maps anything that small
///      to `Standby`, so we don't have to clamp here.
///   5. Going to standby: zero BOTH power registers before writing
///      force_mode = 0, so the next direction switch starts from a
///      clean slate.
///
/// Total write time per setpoint cycle is ~0.8-1 s. That's fine
/// because the dispatcher's settle_timeout_s is 10 s and the writer
/// task de-dups intermediate setpoints via its watch channel.
async fn write_setpoint_once(battery: &BatteryConfig, sp: Setpoint) -> Result<()> {
    let target = battery.modbus_target();
    let unit = Slave(battery.modbus_unit_id);
    let mut ctx = open(target, unit).await?;
    // `write_setpoint_once` is the retry path of the OUTER write
    // helper (`write_setpoint`), used by pulse-mode-fallback callers.
    // No traffic-debug flag accessible here, so we pass `false` and let
    // BatteryWriter's `do_write` carry the debug flag for the hot path.
    let r = write_setpoint_on(&mut ctx, sp, &battery.id, false).await;
    let _ = ctx.disconnect().await;
    r
}

/// The actual register-write sequence, operating on a caller-owned
/// connection. BatteryWriter keeps one Modbus session open for its
/// task lifetime and reuses it across writes / SoC reads — opening a
/// fresh TCP socket on every operation puts unnecessary load on the
/// Marstek (or its bridge) when we already have the bus exclusively.
///
/// `battery_id` + `debug_modbus` are threaded through so each
/// individual write step gets a `modbus_traffic` log line with timing.
/// Lets us see which step of the multi-write sequence is the one that
/// times out / fails — invaluable on V3 where ~40 % of dispatch writes
/// were failing in the field with no visibility into the cause.
async fn write_setpoint_on(
    ctx: &mut tokio_modbus::client::Context,
    sp: Setpoint,
    battery_id: &str,
    debug_modbus: bool,
) -> Result<()> {
    // Step 1: re-arm RS485 control mode + give the firmware time.
    logged_write(
        ctx, REG_RS485_CONTROL_MODE, RS485_CTRL_ON, battery_id, debug_modbus, "rs485_on",
    )
    .await?;
    time::sleep(Duration::from_millis(100)).await;

    match sp {
        Setpoint::Standby => {
            // Park cleanly: zero both power setpoints, THEN force_mode=0.
            logged_write(ctx, REG_CHARGE_POWER_SETPOINT, 0, battery_id, debug_modbus, "zero_charge").await?;
            logged_write(ctx, REG_DISCHARGE_POWER_SETPOINT, 0, battery_id, debug_modbus, "zero_discharge").await?;
            logged_write(ctx, REG_FORCE_MODE, 0, battery_id, debug_modbus, "force_standby").await?;
        }
        Setpoint::Charge { watts } => {
            // Zero the opposite (discharge) direction first, then switch
            // mode, then write the new charge power. Sleeps between
            // steps come from the Python reference impl — without them
            // the Marstek silently drops the command.
            logged_write(ctx, REG_DISCHARGE_POWER_SETPOINT, 0, battery_id, debug_modbus, "zero_discharge").await?;
            time::sleep(Duration::from_millis(200)).await;
            logged_write(ctx, REG_FORCE_MODE, 1, battery_id, debug_modbus, "force_charge").await?;
            time::sleep(Duration::from_millis(500)).await;
            logged_write(ctx, REG_CHARGE_POWER_SETPOINT, watts, battery_id, debug_modbus, "set_charge_power").await?;
        }
        Setpoint::Discharge { watts } => {
            logged_write(ctx, REG_CHARGE_POWER_SETPOINT, 0, battery_id, debug_modbus, "zero_charge").await?;
            time::sleep(Duration::from_millis(200)).await;
            logged_write(ctx, REG_FORCE_MODE, 2, battery_id, debug_modbus, "force_discharge").await?;
            time::sleep(Duration::from_millis(500)).await;
            logged_write(ctx, REG_DISCHARGE_POWER_SETPOINT, watts, battery_id, debug_modbus, "set_discharge_power").await?;
        }
    }
    Ok(())
}

/// `write_reg` plus a `modbus_traffic` log line carrying the per-step
/// label, register, value, elapsed milliseconds and outcome. Lets the
/// operator pinpoint which step of `write_setpoint_on`'s multi-write
/// sequence is the slow / failing one without rebuilding with extra
/// `eprintln!`s.
async fn logged_write(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    value: u16,
    battery_id: &str,
    debug_modbus: bool,
    step: &'static str,
) -> Result<()> {
    let start = Instant::now();
    let result = write_reg(ctx, register, value).await;
    let elapsed_ms = start.elapsed().as_millis();
    match &result {
        Ok(()) => {
            crate::modbus_server::log_traffic(
                debug_modbus,
                "out/write-step-ok",
                format_args!(
                    "battery={battery_id} step={step} reg={register} val={value} elapsed={elapsed_ms}ms"
                ),
            );
        }
        Err(e) => {
            crate::modbus_server::log_traffic(
                debug_modbus,
                "out/write-step-fail",
                format_args!(
                    "battery={battery_id} step={step} reg={register} val={value} elapsed={elapsed_ms}ms → {e}"
                ),
            );
        }
    }
    result
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
    let r = init_dispatch_on(&mut ctx, battery).await;
    let _ = ctx.disconnect().await;
    r
}

async fn init_dispatch_on(
    ctx: &mut tokio_modbus::client::Context,
    battery: &BatteryConfig,
) -> Result<BmsCutoffs> {
    // Enable writes, then explicitly park in standby so we don't leak
    // any stale force_mode left over from a previous run / crash.
    write_reg(ctx, REG_RS485_CONTROL_MODE, RS485_CTRL_ON).await?;
    write_reg(ctx, REG_FORCE_MODE, 0).await?;

    // Read BMS cutoffs ONLY when the variant exposes them. The upstream
    // YAMLs list 44000/44001 for Venus E v1/v2 only; A, D, and E v3
    // don't have them, and reading the wrong register on those would
    // either return garbage or a Modbus exception.
    let cutoffs = if battery.marstek_model.supports_bms_cutoffs() {
        read_bms_cutoffs(ctx).await.unwrap_or_else(|e| {
            warn!(
                battery = %battery.id,
                error = %e,
                "could not read BMS cutoffs (44000/44001) — falling back to dispatcher defaults"
            );
            BmsCutoffs::default()
        })
    } else {
        BmsCutoffs::default()
    };

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
    let result = time::timeout(request_timeout(), ctx.read_holding_registers(reg, 1))
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
    time::timeout(connect_timeout(), tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow!("modbus connect timeout to {target}"))?
        .with_context(|| format!("modbus connect to {target}"))
}

async fn write_reg(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    value: u16,
) -> Result<()> {
    let result = time::timeout(request_timeout(), ctx.write_single_register(register, value))
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
// The dispatcher tick runs every `cycle_ms` (default 2 s) but each
// Modbus write costs an RTT through the RS485-to-LAN bridge (~50-200 ms),
// so the writes can't share the dispatcher's loop. Solution: one
// `BatteryWriter` task per battery, fed by a `watch` channel from the
// dispatcher. The writer:
//
//   - receives the latest desired Setpoint on every change,
//   - skips writes inside `dispatcher.deadband_w`,
//   - refreshes the virtual-Modbus cache on two timers — FAST (live
//     telemetry, default 5 s) + SLOW (metadata + schedules + energy
//     counters, default 60 s) — both on the SAME persistent connection
//     so no extra TCP socket is opened against the inverter,
//   - falls back to a legacy SoC-only poll when `virtual_modbus.enabled
//     = false` (still only opens one connection per battery, just reads
//     fewer registers),
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
        let (fast_refresh_interval, slow_refresh_interval) = if cfg.virtual_modbus.enabled {
            (
                Some(Duration::from_millis(
                    cfg.virtual_modbus.bulk_refresh_ms.max(500),
                )),
                Some(Duration::from_millis(
                    cfg.virtual_modbus.slow_refresh_ms.max(1_000),
                )),
            )
        } else {
            (None, None)
        };
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
                debug_modbus: cfg.virtual_modbus.debug,
                last_written: None,
                last_write_at: None,
                soc_interval: Duration::from_millis(b.soc_interval_ms.max(1_000)),
                last_soc_read_at: None,
                fast_refresh_interval,
                last_fast_refresh_at: None,
                slow_refresh_interval,
                last_slow_refresh_at: None,
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

/// Which set of holding registers a refresh tick is targeting.
/// FAST = live telemetry (~25 regs, every `bulk_refresh_ms`).
/// SLOW = metadata, energy counters, schedules, per-cell voltages
/// (~30-80 regs, every `slow_refresh_ms`).
#[derive(Debug, Clone, Copy)]
enum Tier {
    Fast,
    Slow,
}

/// Close a persistent Modbus context cleanly. Used after errors so
/// the next operation forces a fresh `tcp::connect_slave`. Best-effort
/// — if disconnect itself errors, we swallow it (the socket is going
/// away anyway, and the next reconnect will surface real problems).
async fn drop_ctx(ctx: &mut Option<tokio_modbus::client::Context>) {
    if let Some(mut c) = ctx.take() {
        let _ = time::timeout(Duration::from_millis(500), c.disconnect()).await;
    }
}

struct BatteryWriter {
    battery: BatteryConfig,
    rx: watch::Receiver<Setpoint>,
    shutdown: mpsc::Receiver<()>,
    state: Arc<AppState>,
    /// Mirror of `virtual_modbus.debug` so we can log outbound modbus
    /// traffic at the same level as the server logs its inbound side.
    debug_modbus: bool,
    last_written: Option<Setpoint>,
    last_write_at: Option<Instant>,
    /// Upper bound on how often we re-read SoC (and battery_power).
    /// Piggybacked onto the persistent connection we already use for
    /// setpoint writes — no separate TCP socket. A standalone timer in
    /// `run()` fires when no write happened in this window.
    soc_interval: Duration,
    last_soc_read_at: Option<Instant>,
    /// Interval for the FAST register refresh (live telemetry). `None`
    /// disables (= virtual_modbus disabled). Each refresh reads every
    /// address in `marstek_model.fast_registers()` one register at a
    /// time — Marstek bridges reject contiguous-range reads when the
    /// range crosses an undefined register.
    fast_refresh_interval: Option<Duration>,
    last_fast_refresh_at: Option<Instant>,
    /// Interval for the SLOW register refresh (metadata, energy
    /// counters, BMS limits, schedules, per-cell voltages). `None`
    /// disables. Lower-frequency than the fast tier so the per-cycle
    /// Modbus traffic stays bounded.
    slow_refresh_interval: Option<Duration>,
    last_slow_refresh_at: Option<Instant>,
}

impl BatteryWriter {
    async fn run(mut self) {
        // Persistent Modbus session for this battery's lifetime. The
        // Marstek (or its RS485-to-LAN bridge) only allows one Modbus
        // client at a time anyway; we own that slot exclusively in
        // modbus dispatch mode (the standalone SoC poller is idle).
        // Reusing the connection across writes + SoC reads removes the
        // per-op TCP-handshake overhead and the matching teardown load
        // on the bridge. On any error we close + reconnect on the next
        // operation.
        let mut ctx: Option<tokio_modbus::client::Context> = None;

        // Init loop — keeps trying until RS485 control is on and BMS
        // cutoffs (if supported) are stored. Holds the connection
        // open afterwards.
        loop {
            let attempt = self.ensure_conn(&mut ctx).await;
            match attempt {
                Ok(c) => {
                    match init_dispatch_on(c, &self.battery).await {
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
                                "modbus init failed, dropping connection + retrying in 5s"
                            );
                            drop_ctx(&mut ctx).await;
                        }
                    }
                }
                Err(e) => {
                    self.record_error(format!("init connect: {e}"));
                    warn!(
                        battery = %self.battery.id,
                        error = %e,
                        "modbus connect failed during init, retrying in 5s"
                    );
                }
            }
            time::sleep(Duration::from_secs(5)).await;
        }

        // Sentinel for "never fire" — used when a tier is disabled so
        // the corresponding select branch effectively sleeps forever.
        let never = Duration::from_secs(3600 * 24);
        loop {
            // SoC timer: only used when virtual_modbus is OFF (no FAST
            // refresh covering SoC). When FAST is enabled it always
            // reads SoC, so this timer goes dormant.
            let soc_due_in = if self.fast_refresh_interval.is_some() {
                never
            } else {
                self.last_soc_read_at
                    .map(|t| (t + self.soc_interval).saturating_duration_since(Instant::now()))
                    .unwrap_or(self.soc_interval)
            };
            // FAST timer: live telemetry for HA dashboards (~25 regs).
            let fast_due_in = self
                .fast_refresh_interval
                .map(|iv| {
                    self.last_fast_refresh_at
                        .map(|t| (t + iv).saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::ZERO)
                })
                .unwrap_or(never);
            // SLOW timer: metadata + energy counters + schedules
            // + per-cell voltages (~30-80 regs depending on variant).
            let slow_due_in = self
                .slow_refresh_interval
                .map(|iv| {
                    self.last_slow_refresh_at
                        .map(|t| (t + iv).saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::ZERO)
                })
                .unwrap_or(never);
            tokio::select! {
                biased;
                _ = self.shutdown.recv() => {
                    if let Ok(c) = self.ensure_conn(&mut ctx).await {
                        let _ = write_setpoint_on(c, Setpoint::Standby, &self.battery.id, self.debug_modbus).await;
                    }
                    drop_ctx(&mut ctx).await;
                    let _ = failsafe_shutdown(&self.battery).await;
                    info!(battery = %self.battery.id, "modbus writer task exiting");
                    return;
                }
                changed = self.rx.changed() => {
                    if changed.is_err() { return; }
                    let desired = *self.rx.borrow();
                    if self.should_write(desired) {
                        let _ = self.do_write(&mut ctx, desired).await;
                    } else if self.soc_poll_due() && self.fast_refresh_interval.is_none() {
                        // Virtual-modbus off → fall back to legacy SoC poll.
                        let _ = self.poll_soc_only(&mut ctx).await;
                    }
                }
                _ = time::sleep(soc_due_in) => {
                    let _ = self.poll_soc_only(&mut ctx).await;
                }
                _ = time::sleep(fast_due_in) => {
                    if let Err(e) = self.tier_refresh(&mut ctx, Tier::Fast).await {
                        debug!(
                            battery = %self.battery.id,
                            error = %e,
                            "fast refresh failed"
                        );
                    }
                }
                _ = time::sleep(slow_due_in) => {
                    if let Err(e) = self.tier_refresh(&mut ctx, Tier::Slow).await {
                        debug!(
                            battery = %self.battery.id,
                            error = %e,
                            "slow refresh failed"
                        );
                    }
                }
            }
        }
    }

    /// Open the persistent connection if missing, return a mutable
    /// reference for use by the caller. Logs the open under the
    /// `modbus_traffic` target so the operator can see reconnection
    /// events in the field (= side-effect of the dropped-on-error
    /// path in `tier_refresh` / `do_write`).
    async fn ensure_conn<'a>(
        &self,
        ctx: &'a mut Option<tokio_modbus::client::Context>,
    ) -> Result<&'a mut tokio_modbus::client::Context> {
        if ctx.is_none() {
            let target = self.battery.modbus_target();
            let unit = Slave(self.battery.modbus_unit_id);
            match open(target, unit).await {
                Ok(c) => {
                    *ctx = Some(c);
                    crate::modbus_server::log_traffic(
                        self.debug_modbus,
                        "out/connect",
                        format_args!("battery={} → {target}", self.battery.id),
                    );
                }
                Err(e) => {
                    crate::modbus_server::log_traffic(
                        self.debug_modbus,
                        "out/connect-fail",
                        format_args!("battery={} → {target} failed: {e}", self.battery.id),
                    );
                    return Err(e);
                }
            }
        }
        Ok(ctx.as_mut().unwrap())
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
        diff >= crate::config::WRITE_DEADBAND_W
    }

    /// True iff it's time to refresh SoC. Drives the piggyback decision
    /// inside `do_write` and the SoC-only path on watch-channel changes
    /// that don't warrant a write.
    fn soc_poll_due(&self) -> bool {
        match self.last_soc_read_at {
            Some(t) => t.elapsed() >= self.soc_interval,
            None => true,
        }
    }

    /// Run the setpoint write on the persistent connection, then
    /// piggyback a SoC + battery_power read if due. On any error,
    /// close the connection so the next iteration reconnects.
    async fn do_write(
        &mut self,
        ctx_slot: &mut Option<tokio_modbus::client::Context>,
        sp: Setpoint,
    ) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        // Retry budget applies here so a single connect blip doesn't
        // cause a missed setpoint update — without dropping the
        // connection lifecycle benefit.
        let max_attempts = write_retries().saturating_add(1);
        let mut last_err: Option<anyhow::Error> = None;
        self.state
            .modbus_stats
            .outbound_writes_total
            .fetch_add(1, Relaxed);
        for attempt in 1..=max_attempts {
            let ctx = match self.ensure_conn(ctx_slot).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    drop_ctx(ctx_slot).await;
                    time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                    continue;
                }
            };
            match write_setpoint_on(ctx, sp, &self.battery.id, self.debug_modbus).await {
                Ok(()) => {
                    self.state
                        .modbus_stats
                        .outbound_writes_ok
                        .fetch_add(1, Relaxed);
                    self.record_success(sp);
                    crate::modbus_server::log_traffic(
                        self.debug_modbus,
                        "out/write-ok",
                        format_args!(
                            "battery={} setpoint={sp:?} attempt={attempt}",
                            self.battery.id
                        ),
                    );
                    // Piggyback SoC if due. A failure here is logged
                    // but doesn't roll back the successful write.
                    if self.soc_poll_due() {
                        if let Err(e) = self.read_soc_on_open(ctx_slot).await {
                            debug!(
                                battery = %self.battery.id,
                                error = %e,
                                "SoC piggyback read failed (write was OK)"
                            );
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                    // Stale connection? Drop and reconnect on retry.
                    drop_ctx(ctx_slot).await;
                    if attempt < max_attempts {
                        time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                    }
                }
            }
        }
        self.state
            .modbus_stats
            .outbound_writes_failed
            .fetch_add(1, Relaxed);
        let err = last_err.unwrap_or_else(|| anyhow!("write_setpoint exhausted retries"));
        self.record_error(format!("write: {err:#}"));
        crate::modbus_server::log_traffic(
            self.debug_modbus,
            "out/write-fail",
            format_args!(
                "battery={} setpoint failed after {max_attempts} attempts: {err:#}",
                self.battery.id
            ),
        );
        Err(err)
    }

    /// Read SoC + battery_power on the (already-open) persistent
    /// connection. Used as the piggyback after a successful write AND
    /// as the standalone path when the watch channel fires but no
    /// write is warranted.
    async fn read_soc_on_open(
        &mut self,
        ctx_slot: &mut Option<tokio_modbus::client::Context>,
    ) -> Result<()> {
        let model = self.battery.marstek_model;
        let ctx = self.ensure_conn(ctx_slot).await?;
        let soc = read_soc_on(ctx, model.soc_register(), model.soc_scale()).await;
        let bp = read_battery_power_on(
            ctx,
            model.battery_power_register(),
            model.battery_power_is_int32(),
        )
        .await;
        let now = Instant::now();
        self.last_soc_read_at = Some(now);
        let mut bats = self.state.batteries.write();
        if let Some(b) = bats.get_mut(&self.battery.id) {
            if let Ok(soc) = soc {
                b.soc_pct = Some(soc);
                b.soc_at = Some(std::time::Instant::now());
                b.soc_source = Some(format!("modbus:{}", model.soc_register()));
            }
            if let Ok(p) = bp {
                b.last_battery_power_w = Some(p);
            }
        }
        Ok(())
    }

    async fn poll_soc_only(
        &mut self,
        ctx_slot: &mut Option<tokio_modbus::client::Context>,
    ) -> Result<()> {
        if let Err(e) = self.read_soc_on_open(ctx_slot).await {
            drop_ctx(ctx_slot).await;
            return Err(e);
        }
        Ok(())
    }

    /// Read every register in the requested tier (fast or slow) ONE
    /// at a time and merge the values into
    /// `BatteryState.cached_holding_regs`. Also derives SoC +
    /// battery_power from the FAST tier's cache so the dispatcher stays
    /// current without needing the separate SoC poll path.
    ///
    /// Why one-register-at-a-time: the Marstek RS485 bridges return
    /// `IllegalDataAddress` on contiguous-range reads if ANY register
    /// inside the requested range is undefined for this variant, and
    /// the address space is sparse (32100, 32101, 32102, 32103, 32104,
    /// 32105 defined; 32106-32199 not; 32200 defined again, etc.).
    /// Reading individual addresses sidesteps that entirely — each
    /// request asks only for registers we know exist on this variant.
    ///
    /// Per-register errors are tolerated: variant tables are based on
    /// the ViperRNMC YAMLs which are explicitly marked "untested" for
    /// A/D/V3, so a stray IllegalDataAddress on one address just means
    /// we drop it from the cache. The refresh as a whole only errors
    /// when NO register came back — that's the connection-broken case
    /// where the caller drops + reconnects on the next iteration.
    async fn tier_refresh(
        &mut self,
        ctx_slot: &mut Option<tokio_modbus::client::Context>,
        tier: Tier,
    ) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        let model = self.battery.marstek_model;
        let regs = match tier {
            Tier::Fast => model.fast_registers(),
            Tier::Slow => model.slow_registers(),
        };
        let debug_enabled = self.debug_modbus;
        let mut collected: HashMap<u16, u16> = HashMap::new();
        let mut failed: Vec<u16> = Vec::new();
        let mut connection_dead = false;
        // Counter for back-to-back non-firmware (= wire/codec) errors.
        // Resets to 0 on every successful read OR on a real firmware
        // exception (= inverter responded properly with an error code).
        // Drop the ctx only when 3 in a row signal a genuinely dead
        // socket — single transient hiccups don't poison the codec
        // catastrophically, and the field showed v3 occasionally
        // burping on a specific register without the whole session
        // being lost. Forcing reconnect on every single blip caused
        // V3's cache to stall at 5-8 entries.
        let mut consecutive_wire_errors: u32 = 0;
        const WIRE_ERROR_DROP_THRESHOLD: u32 = 3;
        for reg in regs {
            let ctx = match self.ensure_conn(ctx_slot).await {
                Ok(c) => c,
                Err(_) => {
                    connection_dead = true;
                    break;
                }
            };
            self.state
                .modbus_stats
                .outbound_reads_total
                .fetch_add(1, Relaxed);
            match read_holding_on(ctx, *reg, 1).await {
                Ok(values) => {
                    self.state
                        .modbus_stats
                        .outbound_reads_ok
                        .fetch_add(1, Relaxed);
                    if let Some(v) = values.first() {
                        collected.insert(*reg, *v);
                    }
                    consecutive_wire_errors = 0;
                }
                Err(e) => {
                    self.state
                        .modbus_stats
                        .outbound_reads_failed
                        .fetch_add(1, Relaxed);
                    failed.push(*reg);
                    let s = e.to_string();
                    // `read_holding_on` formats firmware-side errors
                    // with the prefix "modbus exception (reg <N>):"
                    // regardless of which exception code (IllegalData
                    // Address, ServerDeviceBusy, Acknowledge, Gateway
                    // PathUnavailable, …). Anything else (timeout,
                    // IO error, decode error) is a wire / codec
                    // problem that may have left the tokio-modbus
                    // codec mid-frame and corrupted the ctx.
                    let is_firmware_exception = s.starts_with("modbus exception");
                    crate::modbus_server::log_traffic(
                        debug_enabled,
                        "out/read-fail",
                        format_args!(
                            "battery={} {:?} reg={} → {} (firmware-exception={is_firmware_exception})",
                            self.battery.id, tier, *reg, e
                        ),
                    );
                    if is_firmware_exception {
                        // Inverter responded properly. Connection is
                        // healthy, the variant just doesn't expose
                        // this register. Reset the wire-error streak
                        // and move on.
                        consecutive_wire_errors = 0;
                    } else {
                        consecutive_wire_errors += 1;
                        if consecutive_wire_errors >= WIRE_ERROR_DROP_THRESHOLD {
                            crate::modbus_server::log_traffic(
                                debug_enabled,
                                "out/conn-suspect",
                                format_args!(
                                    "battery={} {} consecutive non-firmware errors — dropping ctx so next tick reconnects",
                                    self.battery.id, consecutive_wire_errors
                                ),
                            );
                            drop_ctx(ctx_slot).await;
                            connection_dead = true;
                            break;
                        }
                    }
                }
            }
        }
        crate::modbus_server::log_traffic(
            debug_enabled,
            "out/refresh-summary",
            format_args!(
                "battery={} {:?} refresh: {} ok / {} fail / conn_dead={connection_dead}",
                self.battery.id,
                tier,
                collected.len(),
                failed.len(),
            ),
        );
        if collected.is_empty() {
            if connection_dead {
                drop_ctx(ctx_slot).await;
            }
            return Err(anyhow!("{tier:?} refresh returned no data"));
        }

        let now = Instant::now();
        match tier {
            Tier::Fast => {
                self.last_fast_refresh_at = Some(now);
                // FAST tier covers SoC + battery_power, so keep the
                // dedicated SoC timer asleep.
                self.last_soc_read_at = Some(now);
            }
            Tier::Slow => {
                self.last_slow_refresh_at = Some(now);
            }
        }

        // Derive dispatcher-facing values from FAST tier only — SoC and
        // battery_power are guaranteed there for every variant. Doing
        // it on both tiers is harmless but redundant; we keep it FAST-
        // only for clarity.
        if matches!(tier, Tier::Fast) {
            let soc_reg = model.soc_register();
            let soc_scale = model.soc_scale();
            let soc = collected
                .get(&soc_reg)
                .map(|r| f64::from(*r) * soc_scale)
                .filter(|s| (0.0..=100.0).contains(s));
            let bp_reg = model.battery_power_register();
            let bp = if model.battery_power_is_int32() {
                match (collected.get(&bp_reg), collected.get(&(bp_reg + 1))) {
                    (Some(hi), Some(lo)) => {
                        let combined = ((u32::from(*hi)) << 16) | u32::from(*lo);
                        Some(f64::from(combined as i32))
                    }
                    _ => None,
                }
            } else {
                collected.get(&bp_reg).map(|r| f64::from(*r as i16))
            };

            let mut bats = self.state.batteries.write();
            if let Some(b) = bats.get_mut(&self.battery.id) {
                if let Some(s) = soc {
                    b.soc_pct = Some(s);
                    b.soc_at = Some(std::time::Instant::now());
                    b.soc_source = Some(format!("modbus:{soc_reg}"));
                }
                if let Some(p) = bp {
                    b.last_battery_power_w = Some(p);
                }
                // Merge new readings into the cache (don't replace —
                // we only re-read the FAST subset and want to keep the
                // SLOW values until the next slow refresh).
                for (k, v) in &collected {
                    b.cached_holding_regs.insert(*k, *v);
                }
                b.cached_regs_refreshed_at = Some(now);
            }
        } else {
            // SLOW tier: just merge into cache, no derived-value path.
            let mut bats = self.state.batteries.write();
            if let Some(b) = bats.get_mut(&self.battery.id) {
                for (k, v) in &collected {
                    b.cached_holding_regs.insert(*k, *v);
                }
                b.cached_regs_refreshed_at = Some(now);
            }
        }
        Ok(())
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
    fn setpoint_standby_below_marstek_minimum_50w() {
        // Marstek firmware silently ignores commands below ~50 W —
        // map all of those to Standby so we don't issue dead writes.
        assert_eq!(
            Setpoint::from_signed_watts(0.0, 2500.0, 800.0),
            Setpoint::Standby
        );
        assert_eq!(
            Setpoint::from_signed_watts(30.0, 2500.0, 800.0),
            Setpoint::Standby
        );
        assert_eq!(
            Setpoint::from_signed_watts(-49.9, 2500.0, 800.0),
            Setpoint::Standby
        );
        // 50 W and above should pass through.
        assert_eq!(
            Setpoint::from_signed_watts(50.0, 2500.0, 800.0),
            Setpoint::Discharge { watts: 50 }
        );
        assert_eq!(
            Setpoint::from_signed_watts(-50.0, 2500.0, 800.0),
            Setpoint::Charge { watts: 50 }
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
