use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use shelly_multiplexer::config::Config;
use shelly_multiplexer::state::AppState;
use shelly_multiplexer::{
    dispatcher, http_admin, http_shelly, marstek, mdns, real_shelly, virtual_shelly,
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
    // First line out of stderr before anything else happens. If the
    // binary panics during init we want at least *this* in the log.
    eprintln!("shelly-multiplexer starting (pid {})", std::process::id());

    let cli = Cli::parse();

    let env_filter = if let Some(spec) = cli.log {
        EnvFilter::try_new(spec).context("invalid --log filter")?
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };
    // Write to stderr — Docker line-buffers stdout in 64 KB chunks, so
    // a fast crash loses any tracing output. stderr is unbuffered.
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    let mut cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    info!(path = %cli.config.display(), "config loaded");

    // Apply CLI overrides for real_shelly connection details. The HA
    // add-on passes these from the user's add-on options so changes
    // there propagate without forcing a config-file edit.
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

    // Persist merged config so the web UI shows the effective values.
    if cfg_dirty
        && let Ok(toml) = toml::to_string_pretty(&cfg)
    {
        let tmp = cli.config.with_extension("toml.tmp");
        if std::fs::write(&tmp, toml).is_ok() {
            let _ = std::fs::rename(&tmp, &cli.config);
        }
    }

    let state = AppState::new(&cfg.safety);
    let cfg_swap = Arc::new(ArcSwap::from_pointee(cfg));

    let mut tasks = tokio::task::JoinSet::new();

    // Real-Shelly poller
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            if let Err(e) = real_shelly::run(s, c).await {
                error!("real shelly poller stopped: {e:#}");
            }
        });
    }

    // Dispatcher
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            dispatcher::run(s, c).await;
        });
    }

    // Virtual Shelly UDP server
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            if let Err(e) = virtual_shelly::run(s, c).await {
                error!("virtual shelly UDP server stopped: {e:#}");
            }
        });
    }

    // Marstek telemetry (SoC + actual power) for redispatch
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            if let Err(e) = marstek::run(s, c).await {
                error!("marstek telemetry stopped: {e:#}");
            }
        });
    }

    // mDNS service advertisement so we appear as a Shelly Pro 3EM
    {
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            if let Err(e) = mdns::run(c).await {
                error!("mdns advertisement stopped: {e:#}");
            }
        });
    }

    // Virtual Shelly HTTP server
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            if let Err(e) = http_shelly::run(s, c).await {
                error!("virtual shelly HTTP server stopped: {e:#}");
            }
        });
    }

    // Management UI
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        let p = cli.config.clone();
        tasks.spawn(async move {
            if let Err(e) = http_admin::run(s, c, p).await {
                error!("management UI stopped: {e:#}");
            }
        });
    }

    info!("shelly-multiplexer ready");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
            Ok(())
        }
        _ = wait_first(&mut tasks) => {
            // A spawned task ended unexpectedly — that's a fatal error.
            // Returning Err makes the process exit non-zero so the
            // supervisor's restart loop is justified instead of looking
            // like a clean shutdown.
            error!("a critical task exited; shutting down");
            eprintln!("FATAL: a critical task exited prematurely");
            anyhow::bail!("critical task exited prematurely")
        }
    }
}

async fn wait_first(tasks: &mut tokio::task::JoinSet<()>) {
    let _ = tasks.join_next().await;
}
