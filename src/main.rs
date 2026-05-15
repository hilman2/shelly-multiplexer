use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use shelly_multiplexer::config::{Config, DispatchMode};
use shelly_multiplexer::modbus::ModbusDispatch;
use shelly_multiplexer::state::AppState;
use shelly_multiplexer::{
    dispatcher, ha, http_admin, http_shelly, mdns, modbus, modbus_server, plug, real_shelly,
    virtual_shelly,
};

#[derive(Parser, Debug)]
#[command(name = "shelly-multiplexer", version, about)]
struct Cli {
    /// Path to TOML configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Override RUST_LOG (e.g. "shelly_multiplexer=debug,info")
    #[arg(long)]
    log: Option<String>,

    /// Override `real_shelly.host` from the TOML config (HA add-on
    /// passes this from the user's add-on options on every start).
    #[arg(long)]
    real_shelly_host: Option<IpAddr>,

    /// Override `real_shelly.udp_port` from the TOML config.
    #[arg(long)]
    real_shelly_udp_port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("shelly-multiplexer starting (pid {})", std::process::id());

    let cli = Cli::parse();

    let env_filter = if let Some(spec) = cli.log {
        EnvFilter::try_new(spec).context("invalid --log filter")?
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    let mut cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    info!(path = %cli.config.display(), "config loaded");

    let mut cfg_dirty = false;
    if let Some(host) = cli.real_shelly_host
        && cfg.real_shelly.host != host
    {
        info!(old = %cfg.real_shelly.host, new = %host, "real_shelly.host overridden by CLI");
        cfg.real_shelly.host = host;
        cfg_dirty = true;
    }
    if let Some(port) = cli.real_shelly_udp_port
        && cfg.real_shelly.udp_port != port
    {
        info!(
            old = cfg.real_shelly.udp_port,
            new = port,
            "real_shelly.udp_port overridden by CLI"
        );
        cfg.real_shelly.udp_port = port;
        cfg_dirty = true;
    }

    if let Ok(token) = std::env::var("SUPERVISOR_TOKEN")
        && !token.is_empty()
        && cfg.home_assistant.token.is_empty()
    {
        cfg.home_assistant.token = token;
        info!("home_assistant.token sourced from SUPERVISOR_TOKEN");
    }

    if cfg_dirty {
        let mut for_disk = cfg.clone();
        if std::env::var("SUPERVISOR_TOKEN").ok().as_deref()
            == Some(for_disk.home_assistant.token.as_str())
        {
            for_disk.home_assistant.token.clear();
        }
        match toml::to_string_pretty(&for_disk) {
            Ok(toml) => {
                let tmp = cli.config.with_extension("toml.tmp");
                if let Err(e) = std::fs::write(&tmp, &toml) {
                    error!(path = %tmp.display(), error = %e, "CLI-override write failed");
                } else if let Err(e) = std::fs::rename(&tmp, &cli.config) {
                    error!(
                        from = %tmp.display(),
                        to = %cli.config.display(),
                        error = %e,
                        "CLI-override rename failed"
                    );
                    let _ = std::fs::remove_file(&tmp);
                } else {
                    info!(path = %cli.config.display(), "CLI overrides persisted to config");
                }
            }
            Err(e) => error!(error = %e, "CLI-override serialise failed"),
        }
    }

    let state = AppState::from_config(&cfg);
    let dispatch_mode = cfg.dispatcher.mode;

    // ModbusDispatch (= per-battery writer task pool) only spins up in
    // modbus mode. We hand a clone into the dispatcher AND keep one
    // here so the panic_hook / signal handlers can trigger failsafe.
    let modbus_dispatch = match dispatch_mode {
        DispatchMode::Modbus => Some(ModbusDispatch::spawn(state.clone(), &cfg)),
        DispatchMode::Pulse => None,
    };

    // panic_hook: if anything in this process panics, do our best to put
    // the batteries into standby before unwinding. Doesn't help against
    // SIGKILL / power-loss, but covers Rust-level crashes — the dominant
    // class of "controller dies, battery keeps charging" failures.
    if let Some(md) = modbus_dispatch.clone() {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("PANIC FAILSAFE triggered: {info}");
            md.panic_failsafe();
            prev_hook(info);
        }));
    }

    let cfg_swap = Arc::new(ArcSwap::from_pointee(cfg));

    let mut tasks = tokio::task::JoinSet::new();

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = real_shelly::run(s, c).await;
            log_task_exit("real_shelly", r);
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        let md = modbus_dispatch.clone();
        tasks.spawn(async move {
            dispatcher::run(s, c, md).await;
            log_task_exit::<()>("dispatcher", Ok(()));
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = virtual_shelly::run(s, c).await;
            log_task_exit("virtual_shelly", r);
        });
    }

    // Per-battery Shelly Plug PM Gen3 pollers (mandatory for safety).
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = plug::run(s, c).await;
            log_task_exit("plug", r);
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = modbus::run(s, c).await;
            log_task_exit("modbus", r);
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = modbus_server::run(s, c).await;
            log_task_exit("modbus_server", r);
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = ha::run(s, c).await;
            log_task_exit("ha", r);
        });
    }

    {
        let cfg_now = cfg_swap.load_full();
        if cfg_now.virtual_shelly.enable_mdns {
            let c = cfg_swap.clone();
            tasks.spawn(async move {
                let r = mdns::run(c).await;
                log_task_exit("mdns", r);
            });
        } else {
            info!("mDNS advertisement disabled (virtual_shelly.enable_mdns = false)");
        }
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = http_shelly::run(s, c).await;
            log_task_exit("http_shelly", r);
        });
    }

    {
        let s = state.clone();
        let c = cfg_swap.clone();
        let p = cli.config.clone();
        tasks.spawn(async move {
            let r = http_admin::run(s, c, p).await;
            log_task_exit("http_admin", r);
        });
    }

    info!("shelly-multiplexer ready");

    // Graceful shutdown: drain the ModbusDispatch handle so each writer
    // task gets a chance to fire its failsafe (force_mode=0,
    // RS485 control off) before we drop the runtime. 5 s is enough for
    // each writer to push one final write (≈ 1 s of Modbus traffic).
    let shutdown_grace = Duration::from_secs(5);
    let signal_md = modbus_dispatch.clone();
    let signal_outcome: Result<()> = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
            if let Some(md) = &signal_md {
                info!("triggering modbus failsafe (force_mode=0 on every battery)");
                md.shutdown(shutdown_grace).await;
            }
            Ok(())
        }
        _ = wait_first(&mut tasks) => {
            error!("a critical task exited; shutting down");
            eprintln!("FATAL: a critical task exited prematurely");
            if let Some(md) = &signal_md {
                md.shutdown(shutdown_grace).await;
            }
            Err(anyhow::anyhow!("critical task exited prematurely"))
        }
    };
    signal_outcome
}

fn log_task_exit<T: std::fmt::Debug>(name: &str, result: Result<T>) {
    match result {
        Ok(_) => {
            error!(task = name, "task exited cleanly (should loop forever)");
            eprintln!("FATAL task '{name}' exited cleanly");
        }
        Err(e) => {
            error!(task = name, "task failed: {e:#}");
            eprintln!("FATAL task '{name}' failed: {e:#}");
        }
    }
}

async fn wait_first(tasks: &mut tokio::task::JoinSet<()>) {
    let _ = tasks.join_next().await;
}
