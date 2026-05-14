//! mDNS / Bonjour service registration so the multiplexer is
//! discoverable as a Shelly Pro 3EM on the LAN.
//!
//! A real Shelly Pro 3EM advertises two services on its primary IP:
//!   * `_shelly._tcp.local` on the HTTP port
//!   * `_http._tcp.local`   on the HTTP port
//!
//! Both carry TXT records that integrations key off of:
//!   * `id=shellypro3em-<mac>`
//!   * `mac=<MAC with colons>`
//!   * `gen=2`
//!   * `arch=esp32`     (Pro 3EM is ESP32-based)
//!   * `fw_id=...`
//!   * `app=Pro3EM`
//!   * `ver=...`
//!
//! Rust crate: `mdns-sd` (pure Rust, works on Windows, Linux, macOS).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio::time;
use tracing::{info, warn};

use crate::config::Config;

const SERVICE_HTTP: &str = "_http._tcp.local.";
const SERVICE_SHELLY: &str = "_shelly._tcp.local.";

pub async fn run(config: Arc<ArcSwap<Config>>) -> Result<()> {
    // mDNS records are registered once. Hostname/MAC/firmware edits
    // require a restart to re-advertise.
    let config = config.load_full();
    // In modbus dispatch mode the virtual Shelly is fully disabled, so
    // there's no Pro 3EM behaviour to advertise. Advertising it anyway
    // would mislead other LAN devices (HA discovery, other tools).
    if matches!(config.dispatcher.mode, crate::config::DispatchMode::Modbus) {
        info!("dispatcher.mode = modbus → mDNS advertisement disabled");
        std::future::pending::<()>().await;
        return Ok(());
    }
    let daemon = ServiceDaemon::new().context("starting mdns-sd daemon")?;

    let device_mac = derive_mac(&config);
    let mac_lower = device_mac.to_lowercase();
    let mac_colon = format_mac_colon(&device_mac);
    let hostname_short = if !config.virtual_shelly.device_hostname.is_empty() {
        config.virtual_shelly.device_hostname.to_lowercase()
    } else {
        format!("shellypro3em-{}", mac_lower)
    };
    let hostname_fqdn = format!("{hostname_short}.local.");

    // mDNS records carry a TCP port. When the virtual Shelly HTTP
    // server is disabled (http_port = 0) the saldierende inverters
    // (Marstek etc.) only need the discovery record + UDP/1010, not
    // the advertised TCP port — but the record needs a non-zero port
    // to be valid, so we advertise the standard Shelly HTTP port 80.
    let port = if config.virtual_shelly.http_port == 0 {
        80
    } else {
        config.virtual_shelly.http_port
    };

    let primary_ip = detect_primary_ip().unwrap_or_else(|| {
        warn!("could not detect primary IPv4 address — advertising on all interfaces");
        IpAddr::from([0, 0, 0, 0])
    });

    let txt = build_txt_records(&hostname_short, &mac_colon, &config.virtual_shelly.firmware);

    register_service(
        &daemon,
        SERVICE_SHELLY,
        &hostname_short,
        &hostname_fqdn,
        primary_ip,
        port,
        &txt,
    )?;
    register_service(
        &daemon,
        SERVICE_HTTP,
        &hostname_short,
        &hostname_fqdn,
        primary_ip,
        port,
        &txt,
    )?;

    info!(
        hostname = %hostname_short,
        ip = %primary_ip,
        port,
        "mDNS services registered as Shelly Pro 3EM"
    );

    // Keep the task alive — the daemon owns its own threads internally.
    loop {
        time::sleep(Duration::from_secs(3600)).await;
    }
}

fn register_service(
    daemon: &ServiceDaemon,
    service_type: &str,
    instance_name: &str,
    hostname_fqdn: &str,
    ip: IpAddr,
    port: u16,
    txt: &HashMap<String, String>,
) -> Result<()> {
    let info = ServiceInfo::new(
        service_type,
        instance_name,
        hostname_fqdn,
        ip,
        port,
        Some(txt.clone()),
    )
    .context("building ServiceInfo")?
    .enable_addr_auto();
    daemon
        .register(info)
        .with_context(|| format!("registering {service_type}"))?;
    Ok(())
}

fn build_txt_records(hostname: &str, mac_colon: &str, fw_version: &str) -> HashMap<String, String> {
    let mut t = HashMap::new();
    t.insert("id".into(), hostname.to_string());
    t.insert("mac".into(), mac_colon.to_string());
    t.insert("gen".into(), "2".into());
    t.insert("arch".into(), "esp32".into());
    t.insert("app".into(), "Pro3EM".into());
    t.insert("ver".into(), fw_version.to_string());
    t.insert(
        "fw_id".into(),
        format!("20260101-000000/{fw_version}-multiplexer"),
    );
    t.insert("model".into(), "SPEM-003CEBEU".into());
    t
}

fn derive_mac(config: &Config) -> String {
    if !config.virtual_shelly.device_mac.is_empty() {
        return config.virtual_shelly.device_mac.to_uppercase();
    }
    mac_address::get_mac_address()
        .ok()
        .flatten()
        .map(|m| {
            let b = m.bytes();
            format!(
                "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                b[0], b[1], b[2], b[3], b[4], b[5]
            )
        })
        .unwrap_or_else(|| "B827EB364242".to_string())
}

fn format_mac_colon(mac_hex: &str) -> String {
    let upper = mac_hex.to_uppercase();
    if upper.len() != 12 {
        return upper;
    }
    format!(
        "{}:{}:{}:{}:{}:{}",
        &upper[0..2],
        &upper[2..4],
        &upper[4..6],
        &upper[6..8],
        &upper[8..10],
        &upper[10..12]
    )
}

fn detect_primary_ip() -> Option<IpAddr> {
    // Walk all interfaces, take the first non-loopback IPv4. If only IPv6
    // is present, fall back to that. We deliberately don't call out to a
    // public host to "find" the routing IP — that fails offline.
    let interfaces = local_ip_address::list_afinet_netifas().ok()?;
    let mut ipv6_fallback = None;
    for (_, ip) in interfaces {
        match ip {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
                return Some(IpAddr::V4(v4));
            }
            IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unspecified() => {
                ipv6_fallback.get_or_insert(IpAddr::V6(v6));
            }
            _ => {}
        }
    }
    ipv6_fallback
}
