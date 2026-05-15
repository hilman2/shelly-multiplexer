//! Reproduces the BatteryWriter's persistent-connection loop:
//!   1. Open ONE Modbus TCP session
//!   2. Do the init writes (rs485_control = ON, force_mode = 0)
//!   3. Loop: every `interval_ms`, read every register in the variant
//!      table on that SAME connection
//!   4. After each refresh, print a summary line
//!
//! If V3's bridge has an op-count limit, idle timeout, or any other
//! lifecycle quirk our addon's pattern triggers but a one-shot probe
//! doesn't, this test surfaces it: you see exactly how many refreshes
//! succeed before things start failing, and the kind of failure (IO,
//! timeout, exception).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

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
    interval_ms: u64,
    #[arg(long, default_value_t = 10)]
    cycles: u32,
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
    let interval = Duration::from_millis(args.interval_ms);

    println!(
        "Connecting to {target} unit {} ({} cycles, {} ms interval, {} ms timeout)...",
        args.unit, args.cycles, args.interval_ms, args.timeout_ms
    );
    let mut ctx = time::timeout(Duration::from_secs(10), tcp::connect_slave(target, unit))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    println!("Connected.\n");

    // ----- Init writes (matches BatteryWriter::init_dispatch_on) -----
    println!("Init: write rs485_control=21930 to reg 42000");
    match time::timeout(to, ctx.write_single_register(42000, 21930)).await {
        Ok(Ok(Ok(()))) => println!("  → OK"),
        e => {
            println!("  → FAIL: {e:?}");
            return Ok(());
        }
    }
    time::sleep(Duration::from_millis(100)).await;
    println!("Init: write force_mode=0 to reg 42010");
    match time::timeout(to, ctx.write_single_register(42010, 0)).await {
        Ok(Ok(Ok(()))) => println!("  → OK\n"),
        e => {
            println!("  → FAIL: {e:?}");
            return Ok(());
        }
    }

    // ----- Refresh loop -----
    let table = register_table(model);
    println!("Looping: read all {} registers per cycle:\n", table.len());
    println!(
        "{:>4}  {:>4}  {:>8}  {:>5}  {:>4}  {:>4}  {:>4}",
        "cyc", "ok", "elapsed", "ilg", "tmo", "io", "exc"
    );
    println!("{:>4}  {:>4}  {:>8}  {:>5}  {:>4}  {:>4}  {:>4}", "---", "--", "----", "---", "---", "--", "---");

    let mut total_op_count: u64 = 2; // counting init writes
    for cycle in 1..=args.cycles {
        let start = Instant::now();
        let mut ok = 0;
        let mut illegal = 0;
        let mut timeout_count = 0;
        let mut io_err = 0;
        let mut other_exc = 0;
        let mut first_failure_op: Option<u64> = None;
        for def in table {
            total_op_count += 1;
            match time::timeout(to, ctx.read_holding_registers(def.address, 1)).await {
                Ok(Ok(Ok(_))) => ok += 1,
                Ok(Ok(Err(e))) => {
                    let s = format!("{e:?}");
                    if s.contains("IllegalDataAddress") {
                        illegal += 1;
                    } else {
                        other_exc += 1;
                        if first_failure_op.is_none() {
                            first_failure_op = Some(total_op_count);
                        }
                    }
                }
                Ok(Err(_)) => {
                    io_err += 1;
                    if first_failure_op.is_none() {
                        first_failure_op = Some(total_op_count);
                    }
                }
                Err(_) => {
                    timeout_count += 1;
                    if first_failure_op.is_none() {
                        first_failure_op = Some(total_op_count);
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        println!(
            "{cycle:>4}  {ok:>4}  {:>6}ms  {illegal:>5}  {timeout_count:>4}  {io_err:>4}  {other_exc:>4}{}",
            elapsed.as_millis(),
            if let Some(op) = first_failure_op {
                format!("  first-fail-at-op={op}")
            } else {
                String::new()
            }
        );

        if cycle < args.cycles {
            // Subtract the time we spent reading from the inter-cycle
            // wait so the cycle period stays roughly constant — same
            // semantics as the BatteryWriter's `tokio::time::sleep`.
            let remaining = interval.saturating_sub(elapsed);
            time::sleep(remaining).await;
        }
    }

    println!("\nTotal ops over {} cycles: ~{}", args.cycles, total_op_count);
    let _ = ctx.disconnect().await;
    Ok(())
}
