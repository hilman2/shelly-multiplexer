//! Reproduce our BatteryWriter init pattern: open connection, write
//! rs485_control + force_mode, then probe how many reads succeed
//! before the connection breaks. If V3's bridge dislikes the write
//! sequence this surfaces it: a baseline probe (no writes) and this
//! test will differ.

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
struct Args {
    host: String,
    #[arg(short, long, default_value_t = 502)]
    port: u16,
    #[arg(short, long, default_value_t = 1)]
    unit: u8,
    #[arg(short, long, default_value = "venus_e_v3")]
    model: String,
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let target: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let unit = Slave(args.unit);
    let model = match args.model.as_str() {
        "venus_e_v3" | "venus_e" => MarstekModel::VenusEV3,
        "venus_e_v1_v2" => MarstekModel::VenusEV1V2,
        other => anyhow::bail!("unknown model: {other}"),
    };
    let to = Duration::from_millis(args.timeout_ms);
    println!("Connecting to {target} unit {} (timeout {}ms)...", args.unit, args.timeout_ms);
    let mut ctx = time::timeout(Duration::from_secs(10), tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    println!("Connected.\n");

    println!("Step 1: write rs485_control=21930 (ON) to register 42000");
    match time::timeout(to, ctx.write_single_register(42000, 21930)).await {
        Ok(Ok(Ok(()))) => println!("  → OK"),
        Ok(Ok(Err(e))) => println!("  → EXCEPTION: {e:?}"),
        Ok(Err(e)) => println!("  → IO error: {e}"),
        Err(_) => println!("  → TIMEOUT"),
    }
    time::sleep(Duration::from_millis(100)).await;

    println!("Step 2: write force_mode=0 to register 42010");
    match time::timeout(to, ctx.write_single_register(42010, 0)).await {
        Ok(Ok(Ok(()))) => println!("  → OK"),
        Ok(Ok(Err(e))) => println!("  → EXCEPTION: {e:?}"),
        Ok(Err(e)) => println!("  → IO error: {e}"),
        Err(_) => println!("  → TIMEOUT"),
    }

    println!("\nStep 3: read every register on the SAME connection (mimics tier_refresh)\n");
    let table = register_table(model);
    let mut ok = 0;
    let mut illegal = 0;
    let mut other = 0;
    let mut consecutive_failures = 0u32;
    let mut first_failure_at_index: Option<usize> = None;
    for (idx, def) in table.iter().enumerate() {
        match time::timeout(to, ctx.read_holding_registers(def.address, 1)).await {
            Ok(Ok(Ok(values))) => {
                let raw = values.first().copied().unwrap_or(0);
                println!("  [{idx:>3}]  {:>6}  {:<32}  OK  raw={raw}", def.address, def.name);
                ok += 1;
                consecutive_failures = 0;
            }
            Ok(Ok(Err(e))) => {
                let s = format!("{e:?}");
                if s.contains("IllegalDataAddress") {
                    illegal += 1;
                    println!("  [{idx:>3}]  {:>6}  {:<32}  IllegalDataAddress", def.address, def.name);
                } else {
                    other += 1;
                    if first_failure_at_index.is_none() {
                        first_failure_at_index = Some(idx);
                    }
                    consecutive_failures += 1;
                    println!("  [{idx:>3}]  {:>6}  {:<32}  EXCEPTION  {s}", def.address, def.name);
                }
            }
            Ok(Err(e)) => {
                other += 1;
                if first_failure_at_index.is_none() {
                    first_failure_at_index = Some(idx);
                }
                consecutive_failures += 1;
                println!("  [{idx:>3}]  {:>6}  {:<32}  IO error: {e}", def.address, def.name);
                if consecutive_failures >= 5 {
                    println!("  >>> 5 consecutive failures → connection dead, aborting");
                    break;
                }
            }
            Err(_) => {
                other += 1;
                if first_failure_at_index.is_none() {
                    first_failure_at_index = Some(idx);
                }
                consecutive_failures += 1;
                println!("  [{idx:>3}]  {:>6}  {:<32}  TIMEOUT", def.address, def.name);
                if consecutive_failures >= 5 {
                    println!("  >>> 5 consecutive failures → connection dead, aborting");
                    break;
                }
            }
        }
    }
    println!(
        "\nSummary: {ok} OK / {illegal} IllegalDataAddress / {other} other  ({} total)",
        table.len()
    );
    if let Some(i) = first_failure_at_index {
        println!("First non-Illegal failure occurred at index {i} (reg {}).", table[i].address);
    }
    let _ = ctx.disconnect().await;
    Ok(())
}
