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

    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    info!(path = %cli.config.display(), "config loaded");

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
