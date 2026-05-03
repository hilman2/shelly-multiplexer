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
    dispatcher, ha, http_admin, http_shelly, marstek, mdns, real_shelly, stuck_detect,
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

    // SUPERVISOR_TOKEN: injected by the HA add-on entry point; lets
    // the multiplexer call the Core API without the user pasting a
    // long-lived token into the config file.
    if let Ok(token) = std::env::var("SUPERVISOR_TOKEN")
        && !token.is_empty()
        && cfg.home_assistant.token.is_empty()
    {
        cfg.home_assistant.token = token;
        info!("home_assistant.token sourced from SUPERVISOR_TOKEN");
    }

    // Persist merged config so the web UI shows the effective values.
    // Drop the SUPERVISOR_TOKEN before writing — secrets don't belong
    // in a file the user might back up or share.
    if cfg_dirty {
        let mut for_disk = cfg.clone();
        if std::env::var("SUPERVISOR_TOKEN").ok().as_deref() == Some(for_disk.home_assistant.token.as_str()) {
            for_disk.home_assistant.token.clear();
        }
        if let Ok(toml) = toml::to_string_pretty(&for_disk) {
            let tmp = cli.config.with_extension("toml.tmp");
            if std::fs::write(&tmp, toml).is_ok() {
                let _ = std::fs::rename(&tmp, &cli.config);
            }
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
            let r = real_shelly::run(s, c).await;
            log_task_exit("real_shelly", r);
        });
    }

    // Dispatcher (loops forever; never returns)
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            dispatcher::run(s, c).await;
            log_task_exit::<()>("dispatcher", Ok(()));
        });
    }

    // Virtual Shelly UDP server
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = virtual_shelly::run(s, c).await;
            log_task_exit("virtual_shelly", r);
        });
    }

    // Marstek telemetry (SoC + actual power) for redispatch
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = marstek::run(s, c).await;
            log_task_exit("marstek", r);
        });
    }

    // Home Assistant SoC poller (idle if home_assistant.enabled=false)
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = ha::run(s, c).await;
            log_task_exit("ha", r);
        });
    }

    // Passive stuck detector (10-min rolling window per battery).
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = stuck_detect::run(s, c).await;
            log_task_exit("stuck_detect", r);
        });
    }

    // mDNS service advertisement (gated on virtual_shelly.enable_mdns).
    // On HA OS the host's Avahi already owns UDP/5353 multicast and our
    // mdns-sd daemon collides with it, so the default in the HA add-on
    // is OFF. Standalone deployments can flip it on via the web UI.
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

    // Virtual Shelly HTTP server
    {
        let s = state.clone();
        let c = cfg_swap.clone();
        tasks.spawn(async move {
            let r = http_shelly::run(s, c).await;
            log_task_exit("http_shelly", r);
        });
    }

    // Management UI
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

/// Helper: every long-running task is supposed to loop forever. If one
/// returns at all (Ok or Err), surface the task name and reason at
/// ERROR so we can tell *which* component triggered the shutdown.
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
