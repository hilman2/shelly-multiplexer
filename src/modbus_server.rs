//! Virtual Modbus TCP server — re-publishes each battery's telemetry
//! on a unit-id-multiplexed Modbus endpoint so HA's existing
//! Marstek-Modbus integrations keep working even though we hold the
//! inverter's only Modbus slot.
//!
//! Design notes:
//!   * Reads are served from `BatteryState.cached_holding_regs`, which
//!     the `BatteryWriter` populates on a `bulk_refresh_ms` cadence
//!     using the connection it already owns. No second TCP socket to
//!     the inverter is ever opened.
//!   * Routing: each request's slave/unit-id is looked up in
//!     `AppState.by_unit_id` to find the battery. Unknown unit IDs get
//!     `GATEWAY_PATH_UNAVAILABLE`.
//!   * Function-code coverage: ReadHoldingRegisters (0x03),
//!     ReadInputRegisters (0x04) — we serve the same cache for both
//!     because the Marstek register map only uses the holding-register
//!     range, but some HA integrations request via 0x04.
//!   * All write functions return `IllegalFunction`. We own setpoint
//!     control; HA must not write back.
//!   * Cached register missing → `IllegalDataAddress` (= the register
//!     is outside our `BULK_READ_RANGES`, so we have nothing to return).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_modbus::prelude::*;
use tokio_modbus::server::tcp::{accept_tcp_connection, Server};
use tracing::{debug, info, warn};

use crate::config::{Config, MarstekModel};
use crate::state::AppState;

/// Resolve a probe register requested by HA to the actual cached
/// register on this variant. Lets us paper over hardcoded probes in
/// upstream HA integrations: the ViperRNMC `marstek_modbus` integration
/// reads register **32104** during the "Test connection" step
/// regardless of which variant the user picked. That register is the
/// V1/V2 SoC slot — V3 firmware doesn't expose it. Without aliasing,
/// HA can't add a V3 device because our virtual server returns
/// IllegalDataAddress for 32104 on V3 units and the integration
/// surfaces that as "Keine Antwort von Unit-ID".
///
/// For each model, list `(probe_address, actual_address)` pairs. The
/// server transparently substitutes `actual_address` when looking up
/// the cache. After the connection test passes, the integration loads
/// the variant-specific YAML and reads the real V3 addresses (34002
/// etc.) directly — no alias needed for normal operation.
fn alias_probe_register(model: MarstekModel, addr: u16) -> u16 {
    match model {
        MarstekModel::VenusEV3 if addr == 32104 => 34002, // V1/V2 SoC slot → V3 SoC
        _ => addr,
    }
}

/// The Service impl that handles one TCP connection's requests. Cheap
/// to clone (it's an Arc to shared state); the tokio-modbus framework
/// hands a fresh one to each accepted connection via `accept_tcp_connection`.
#[derive(Clone)]
struct BatteryProxyService {
    state: Arc<AppState>,
    debug: bool,
}

impl tokio_modbus::server::Service for BatteryProxyService {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, ExceptionCode>> + Send>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let state = self.state.clone();
        let debug = self.debug;
        Box::pin(async move { handle_request(state, req, debug).await })
    }
}

/// Dedicated tracing target for every line emitted by either modbus
/// path (inbound server + outbound bulk-refresh / writes). Lets the
/// user filter with `RUST_LOG=modbus_traffic=debug` or — when
/// `virtual_modbus.debug = true` is set in the addon config — see
/// the same lines at INFO level without any RUST_LOG fiddling.
pub const TRAFFIC_TARGET: &str = "modbus_traffic";

async fn handle_request(
    state: Arc<AppState>,
    req: SlaveRequest<'_>,
    debug_enabled: bool,
) -> Result<Option<Response>, ExceptionCode> {
    use std::sync::atomic::Ordering::Relaxed;
    state
        .modbus_stats
        .server_requests_total
        .fetch_add(1, Relaxed);
    let unit = req.slave;
    let Some(battery_id) = state.by_unit_id.get(&unit).cloned() else {
        state
            .modbus_stats
            .server_requests_gateway_unavailable
            .fetch_add(1, Relaxed);
        log_traffic(
            debug_enabled,
            "in/unknown-unit",
            format_args!("unit={unit} → GatewayPathUnavailable"),
        );
        return Err(ExceptionCode::GatewayPathUnavailable);
    };

    match req.request {
        Request::ReadHoldingRegisters(addr, count)
        | Request::ReadInputRegisters(addr, count) => {
            let bats = state.batteries.read();
            let Some(b) = bats.get(&battery_id) else {
                state
                    .modbus_stats
                    .server_requests_gateway_unavailable
                    .fetch_add(1, Relaxed);
                return Err(ExceptionCode::GatewayPathUnavailable);
            };
            // Distinguish three failure modes:
            //   1. cache COMPLETELY EMPTY → first bulk-refresh hasn't
            //      finished yet. Return ServerDeviceBusy so HA retries.
            //   2. cache has data but not THIS register → not in our
            //      covered set for the variant. IllegalDataAddress
            //      (matches what a real inverter returns).
            //   3. all requested registers in cache → serve them.
            if b.cached_holding_regs.is_empty() {
                state
                    .modbus_stats
                    .server_requests_server_busy
                    .fetch_add(1, Relaxed);
                log_traffic(
                    debug_enabled,
                    "in/busy",
                    format_args!(
                        "unit={unit} battery={battery_id} read holding {addr}..+{count} → ServerDeviceBusy (cache empty, refresh pending)"
                    ),
                );
                return Err(ExceptionCode::ServerDeviceBusy);
            }
            let mut out = Vec::with_capacity(count as usize);
            for offset in 0..count {
                let reg = addr.wrapping_add(offset);
                // Resolve any per-variant probe-register alias before
                // hitting the cache. Lets HA's hardcoded 32104 probe
                // succeed on V3 (V3 firmware doesn't have 32104; we
                // serve 34002's value instead). For variants that
                // expose the requested address natively, the alias
                // function is the identity → no change.
                let cache_key = alias_probe_register(b.marstek_model, reg);
                match b.cached_holding_regs.get(&cache_key) {
                    Some(v) => out.push(*v),
                    None => {
                        state
                            .modbus_stats
                            .server_requests_illegal_address
                            .fetch_add(1, Relaxed);
                        log_traffic(
                            debug_enabled,
                            "in/no-such-reg",
                            format_args!(
                                "unit={unit} battery={battery_id} read holding {addr}..+{count} → IllegalDataAddress on reg {reg} (cache_key={cache_key})"
                            ),
                        );
                        return Err(ExceptionCode::IllegalDataAddress);
                    }
                }
            }
            state
                .modbus_stats
                .server_requests_ok
                .fetch_add(1, Relaxed);
            log_traffic(
                debug_enabled,
                "in/ok",
                format_args!(
                    "unit={unit} battery={battery_id} read holding {addr}..+{count} → {out:?}"
                ),
            );
            Ok(Some(Response::ReadHoldingRegisters(out)))
        }
        // Every write function: refuse. We own the inverter; letting
        // HA scribble force_mode or power setpoints would break the
        // circuit-cap invariant.
        Request::WriteSingleCoil(_, _)
        | Request::WriteMultipleCoils(_, _)
        | Request::WriteSingleRegister(_, _)
        | Request::WriteMultipleRegisters(_, _)
        | Request::MaskWriteRegister(_, _, _)
        | Request::ReadWriteMultipleRegisters(_, _, _, _) => {
            state
                .modbus_stats
                .server_requests_illegal_function
                .fetch_add(1, Relaxed);
            log_traffic(
                debug_enabled,
                "in/write-refused",
                format_args!(
                    "unit={unit} battery={battery_id} write rejected → IllegalFunction (we own control)"
                ),
            );
            Err(ExceptionCode::IllegalFunction)
        }
        // Coils + discrete inputs: Marstek doesn't expose any.
        Request::ReadCoils(_, _) | Request::ReadDiscreteInputs(_, _) => {
            state
                .modbus_stats
                .server_requests_illegal_function
                .fetch_add(1, Relaxed);
            log_traffic(
                debug_enabled,
                "in/coils-refused",
                format_args!(
                    "unit={unit} battery={battery_id} coil/discrete rejected → IllegalFunction (Marstek has none)"
                ),
            );
            Err(ExceptionCode::IllegalFunction)
        }
        other => {
            state
                .modbus_stats
                .server_requests_illegal_function
                .fetch_add(1, Relaxed);
            log_traffic(
                debug_enabled,
                "in/unsupported",
                format_args!(
                    "unit={unit} battery={battery_id} fc={other:?} → IllegalFunction"
                ),
            );
            Err(ExceptionCode::IllegalFunction)
        }
    }
}

/// Emit a single line into the `modbus_traffic` target. When
/// `virtual_modbus.debug = true` the line is at INFO (visible at the
/// addon's default log_level); otherwise at DEBUG. Either way it's
/// uniquely filterable with `RUST_LOG=modbus_traffic=…`.
pub fn log_traffic(debug_enabled: bool, kind: &'static str, args: std::fmt::Arguments<'_>) {
    if debug_enabled {
        info!(target: TRAFFIC_TARGET, kind, "{}", args);
    } else {
        debug!(target: TRAFFIC_TARGET, kind, "{}", args);
    }
}

pub async fn run(state: Arc<AppState>, config: Arc<arc_swap::ArcSwap<Config>>) -> Result<()> {
    let cfg = config.load_full();
    if !cfg.virtual_modbus.enabled {
        info!("virtual_modbus disabled — proxy server idle");
        std::future::pending::<()>().await;
        return Ok(());
    }
    let bind: SocketAddr = cfg
        .virtual_modbus
        .bind_address
        .parse()
        .with_context(|| {
            format!(
                "virtual_modbus.bind_address `{}` is not a valid SocketAddr",
                cfg.virtual_modbus.bind_address
            )
        })?;

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding virtual Modbus server on {bind}"))?;
    info!(
        bind = %bind,
        units = ?state.by_unit_id.keys().collect::<Vec<_>>(),
        "virtual Modbus server listening"
    );

    let service = BatteryProxyService {
        state: state.clone(),
        debug: cfg.virtual_modbus.debug,
    };
    let server = Server::new(listener);
    let state_for_accept = state.clone();
    let debug_for_accept = cfg.virtual_modbus.debug;
    let new_service = move |sa| {
        use std::sync::atomic::Ordering::Relaxed;
        state_for_accept
            .modbus_stats
            .server_connections_accepted
            .fetch_add(1, Relaxed);
        log_traffic(
            debug_for_accept,
            "in/connect",
            format_args!("new TCP connection from {sa}"),
        );
        Ok(Some(service.clone()))
    };
    let on_connected = move |stream, socket_addr| {
        let new_service = new_service.clone();
        async move { accept_tcp_connection(stream, socket_addr, new_service) }
    };
    let on_process_error = |err| {
        warn!(error = %err, "virtual Modbus server connection error");
    };

    server
        .serve(&on_connected, on_process_error)
        .await
        .context("virtual Modbus server failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::collections::HashMap;

    /// Build an AppState with one battery whose virtual_unit_id = 7 and
    /// a tiny cached register block at addresses 32104, 32105.
    fn fixture() -> Arc<AppState> {
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 16
[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
virtual_unit_id = 7
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        cfg.validate().unwrap();
        let state = AppState::from_config(&cfg);
        // Seed the cache.
        let mut bats = state.batteries.write();
        let b = bats.get_mut("a").unwrap();
        b.cached_holding_regs.insert(32104, 42); // SoC = 42 %
        b.cached_holding_regs.insert(32105, 100); // some neighbour
        drop(bats);
        state
    }

    #[tokio::test]
    async fn read_holding_returns_cached_values() {
        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            request: Request::ReadHoldingRegisters(32104, 2),
        };
        let resp = handle_request(state, req, false).await.unwrap();
        match resp {
            Some(Response::ReadHoldingRegisters(vs)) => assert_eq!(vs, vec![42, 100]),
            other => panic!("expected ReadHoldingRegisters, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_input_serves_same_cache_as_holding() {
        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            request: Request::ReadInputRegisters(32104, 1),
        };
        let resp = handle_request(state, req, false).await.unwrap();
        match resp {
            Some(Response::ReadHoldingRegisters(vs)) => assert_eq!(vs, vec![42]),
            other => panic!("expected ReadHoldingRegisters wrap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_unit_id_yields_gateway_path_unavailable() {
        let state = fixture();
        let req = SlaveRequest {
            slave: 99,
            request: Request::ReadHoldingRegisters(32104, 1),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::GatewayPathUnavailable);
    }

    #[tokio::test]
    async fn uncached_register_yields_illegal_data_address() {
        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            request: Request::ReadHoldingRegisters(50000, 1),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::IllegalDataAddress);
    }

    /// Empty cache (= addon just started, first bulk refresh hasn't
    /// completed yet) → ServerDeviceBusy so HA retries rather than
    /// concluding the device is missing.
    #[tokio::test]
    async fn empty_cache_yields_server_device_busy() {
        // Build a state where battery has unit_id 7 but no cached regs
        // (skip the fixture's seed-the-cache step).
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 16
[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
virtual_unit_id = 7
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        cfg.validate().unwrap();
        let state = AppState::from_config(&cfg);
        let req = SlaveRequest {
            slave: 7,
            request: Request::ReadHoldingRegisters(32104, 1),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::ServerDeviceBusy);
    }

    #[tokio::test]
    async fn write_requests_are_rejected() {
        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            request: Request::WriteSingleRegister(42010, 1),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::IllegalFunction);

        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            request: Request::WriteMultipleRegisters(42020, std::borrow::Cow::Owned(vec![100])),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::IllegalFunction);
    }

    #[tokio::test]
    async fn partial_range_with_one_missing_register_returns_exception() {
        // The server doesn't return partial results: any missing
        // register in the requested span fails the whole read with
        // ILLEGAL_DATA_ADDRESS. This matches how real inverters behave
        // and prevents silently returning zero for unknown registers.
        let state = fixture();
        let req = SlaveRequest {
            slave: 7,
            // 32104 is cached, 32106 is not (only 32104 + 32105 seeded).
            request: Request::ReadHoldingRegisters(32104, 3),
        };
        let err = handle_request(state, req, false).await.unwrap_err();
        assert_eq!(err, ExceptionCode::IllegalDataAddress);
    }

    /// HA's ViperRNMC integration hardcodes a read of register 32104
    /// as its connection-test probe regardless of variant. V3 firmware
    /// doesn't expose 32104, so without aliasing the test fails and
    /// the user can't add a V3 device. Verify our server transparently
    /// substitutes 34002 (V3 SoC) for probes of 32104 on V3 units.
    #[tokio::test]
    async fn v3_probe_of_32104_is_aliased_to_34002_so_ha_connect_succeeds() {
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 16
[[batteries]]
id = "v3"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
marstek_model = "venus_e_v3"
virtual_unit_id = 7
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        cfg.validate().unwrap();
        let state = AppState::from_config(&cfg);
        // Seed only 34002 (V3 SoC) — NOT 32104. Probe should still
        // succeed via the alias.
        {
            let mut bats = state.batteries.write();
            let b = bats.get_mut("v3").unwrap();
            b.cached_holding_regs.insert(34002, 970); // SoC = 97.0 %
        }

        let req = SlaveRequest {
            slave: 7,
            request: Request::ReadHoldingRegisters(32104, 1),
        };
        let resp = handle_request(state, req, false).await.unwrap();
        match resp {
            Some(Response::ReadHoldingRegisters(vs)) => assert_eq!(vs, vec![970]),
            other => panic!("expected aliased read to succeed, got {other:?}"),
        }
    }

    /// The same probe on a V1/V2 unit should hit 32104 directly — the
    /// alias function is the identity for V1/V2.
    #[tokio::test]
    async fn v12_probe_of_32104_reads_32104_directly() {
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 16
[[batteries]]
id = "v12"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
marstek_model = "venus_e_v1_v2"
virtual_unit_id = 5
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        cfg.validate().unwrap();
        let state = AppState::from_config(&cfg);
        {
            let mut bats = state.batteries.write();
            let b = bats.get_mut("v12").unwrap();
            b.cached_holding_regs.insert(32104, 87); // SoC = 87 %
            b.cached_holding_regs.insert(34002, 970); // unrelated
        }

        let req = SlaveRequest {
            slave: 5,
            request: Request::ReadHoldingRegisters(32104, 1),
        };
        let resp = handle_request(state, req, false).await.unwrap();
        match resp {
            Some(Response::ReadHoldingRegisters(vs)) => {
                // Must read the literal 32104 cache entry, NOT alias to 34002.
                assert_eq!(vs, vec![87]);
            }
            other => panic!("expected literal read, got {other:?}"),
        }
    }

    #[test]
    fn by_unit_id_default_is_index_plus_one() {
        // Two batteries, no explicit virtual_unit_id → 1 and 2.
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 32
[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
[[batteries]]
id = "b"
address = "192.168.1.52"
circuit = "c1"
plug_url = "http://192.168.1.72"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.92"
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        cfg.validate().unwrap();
        let state = AppState::from_config(&cfg);
        let map: HashMap<u8, String> = state.by_unit_id.clone();
        assert_eq!(map.get(&1).map(String::as_str), Some("a"));
        assert_eq!(map.get(&2).map(String::as_str), Some("b"));
    }

    #[test]
    fn clashing_virtual_unit_ids_fail_validation() {
        // Two batteries explicitly set to the same virtual_unit_id —
        // must fail validation so the user catches the mistake at
        // startup instead of HA reading the wrong battery's telemetry.
        let cfg_str = r#"
[real_shelly]
host = "192.168.1.50"
udp_port = 2020
[virtual_shelly]
[management]
[[circuits]]
id = "c1"
fuse_amps = 32
[[batteries]]
id = "a"
address = "192.168.1.51"
circuit = "c1"
plug_url = "http://192.168.1.71"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.91"
virtual_unit_id = 5
[[batteries]]
id = "b"
address = "192.168.1.52"
circuit = "c1"
plug_url = "http://192.168.1.72"
max_charge_w = 2500
max_discharge_w = 800
modbus_host = "192.168.1.92"
virtual_unit_id = 5
"#;
        let cfg: Config = toml::from_str(cfg_str).unwrap();
        let result = cfg.validate();
        assert!(result.is_err(), "expected validation error for clashing unit IDs");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("virtual_unit_id"), "got: {msg}");
    }
}
