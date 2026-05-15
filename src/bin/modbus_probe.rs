//! Field-debug helper. Connects via Modbus TCP, probes every register
//! address in the chosen variant's known-register table, and prints
//! per-address whether the inverter returned a value, an
//! `IllegalDataAddress` exception, some other Modbus exception, an
//! I/O error, or a timeout.
//!
//! Use this when an HA integration can't read a battery to see what
//! the inverter actually exposes. Output is single-line per register
//! so you can diff variants or scrub for a specific name with grep.
//!
//! Example:
//!   cargo run --bin modbus_probe -- 192.168.33.210 --model venus_e_v1_v2
//!   cargo run --bin modbus_probe -- 192.168.32.65 --model venus_e_v3

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::time;
use tokio_modbus::client::tcp;
use tokio_modbus::prelude::*;

use shelly_multiplexer::config::MarstekModel;
use shelly_multiplexer::modbus_decode::register_table;

#[derive(Parser)]
#[command(about = "Probe what Modbus registers a Marstek inverter actually exposes")]
struct Args {
    /// IP address of the RS485-to-LAN bridge (or V3 with native Ethernet)
    host: String,
    /// TCP port (default Modbus = 502)
    #[arg(short, long, default_value_t = 502)]
    port: u16,
    /// Modbus unit / slave ID
    #[arg(short, long, default_value_t = 1)]
    unit: u8,
    /// Variant: venus_e_v1_v2 / venus_e_v3 / venus_d / venus_a
    #[arg(short, long, default_value = "venus_e_v3")]
    model: String,
    /// Per-register timeout in milliseconds
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
    /// Delay between successive reads (ms) — some bridges throttle.
    #[arg(long, default_value_t = 50)]
    delay_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let target: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let unit = Slave(args.unit);
    let model = match args.model.as_str() {
        "venus_e_v1_v2" | "venus_e_v12" | "venus_e_v1v2" => MarstekModel::VenusEV1V2,
        "venus_e_v3" | "venus_e" => MarstekModel::VenusEV3,
        "venus_d" => MarstekModel::VenusD,
        "venus_a" => MarstekModel::VenusA,
        other => anyhow::bail!("unknown model: {other}"),
    };

    println!(
        "Connecting to {target} unit {} as {:?} (timeout {} ms / read)...",
        args.unit, model, args.timeout_ms
    );
    let mut ctx = time::timeout(Duration::from_secs(10), tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    println!("Connected.\n");

    let table = register_table(model);
    println!(
        "Probing {} registers from {:?}'s known-register table:\n",
        table.len(),
        model
    );
    println!(
        "  {:>6}  {:<32}  {:<14}  result",
        "addr", "name", "section"
    );
    println!("  {:>6}  {:<32}  {:<14}  ------", "----", "----", "-------");

    let mut ok = 0u32;
    let mut illegal = 0u32;
    let mut timeout = 0u32;
    let mut other = 0u32;
    let read_timeout = Duration::from_millis(args.timeout_ms);

    for def in table {
        let result = time::timeout(read_timeout, ctx.read_holding_registers(def.address, 1)).await;
        let line = match result {
            Ok(Ok(Ok(values))) => {
                let raw = values.first().copied().unwrap_or(0);
                ok += 1;
                format!("OK  raw={raw} (0x{raw:04X})")
            }
            Ok(Ok(Err(e))) => {
                let s = format!("{e:?}");
                if s.contains("IllegalDataAddress") {
                    illegal += 1;
                    "IllegalDataAddress".to_string()
                } else {
                    other += 1;
                    format!("EXCEPTION  {s}")
                }
            }
            Ok(Err(e)) => {
                other += 1;
                format!("IO error: {e}")
            }
            Err(_) => {
                timeout += 1;
                format!("TIMEOUT (>{} ms)", args.timeout_ms)
            }
        };
        println!(
            "  {:>6}  {:<32}  {:<14}  {line}",
            def.address, def.name, def.section
        );
        if args.delay_ms > 0 {
            time::sleep(Duration::from_millis(args.delay_ms)).await;
        }
    }

    println!(
        "\nSummary: {ok} OK / {illegal} IllegalDataAddress / {timeout} TIMEOUT / {other} other  ({} total)",
        table.len()
    );

    let _ = ctx.disconnect().await;
    Ok(())
}
