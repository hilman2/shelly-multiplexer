//! Marstek calibration tool — minimal interactive virtual Shelly Pro 3EM.
//!
//! Goal: empirically discover how many polls of a given CT value the Marstek
//! must see before its internal integrator commits to the resulting charge
//! / discharge change. We don't need the Shelly Plug yet — observe via the
//! Marstek app.
//!
//! Build (Windows):
//!   cargo build --release --bin marstek_calibrate
//!   -> target\release\marstek_calibrate.exe
//!
//! Usage:
//!   marstek_calibrate.exe          (binds 0.0.0.0:1010)
//!   marstek_calibrate.exe 1010
//!
//! Configure your Marstek to use this PC's LAN IP as the Shelly host.

use anyhow::{Context, Result};
use chrono::Local;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

#[derive(Deserialize)]
struct RequestFrame {
    id: Option<i64>,
    src: Option<String>,
    method: String,
}

#[derive(Serialize)]
struct ResponseFrame {
    id: Option<i64>,
    src: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dst: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

struct State {
    /// Current value (in W) presented to the battery.
    /// Sign convention: positive = grid IMPORT (battery should discharge),
    /// negative = grid EXPORT (battery should charge).
    current_w: AtomicI64,
    /// Polls remaining at this value before auto-revert to 0.
    /// -1 means "indefinite".
    polls_remaining: AtomicI64,
    /// How many polls were originally requested for the active pulse,
    /// purely for display purposes ("3 of 5 left").
    polls_initial: AtomicI64,
    total_polls: AtomicU64,
    /// Polls received since the last user command (reset on each command,
    /// printed in command echo so user sees activity without flooding).
    polls_since_cmd: AtomicU64,
    silent_until: Mutex<Option<Instant>>,
    last_poll_at: Mutex<Option<Instant>>,
    /// IP addresses we've seen at least one poll from. Used to log a one-time
    /// "first contact" line per Marstek so the user knows discovery worked,
    /// without then flooding the console with every subsequent poll.
    seen_peers: Mutex<HashSet<IpAddr>>,
    /// Verbose mode: when true, every poll is logged. Default false.
    verbose: AtomicBool,
}

impl State {
    fn new() -> Self {
        Self {
            current_w: AtomicI64::new(0),
            polls_remaining: AtomicI64::new(-1),
            polls_initial: AtomicI64::new(0),
            total_polls: AtomicU64::new(0),
            polls_since_cmd: AtomicU64::new(0),
            silent_until: Mutex::new(None),
            last_poll_at: Mutex::new(None),
            seen_peers: Mutex::new(HashSet::new()),
            verbose: AtomicBool::new(false),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1010);
    let bind = format!("0.0.0.0:{port}");

    let socket = match UdpSocket::bind(&bind).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ERROR: cannot bind UDP {bind}: {e}");
            eprintln!(
                "Hint: another shelly emulator might be running, or port {port} requires admin."
            );
            std::process::exit(1);
        }
    };
    let socket = Arc::new(socket);

    let mac_hex = derive_mac();
    let hostname_short = format!("shellypro3em-{}", mac_hex.to_lowercase());
    let primary_ip = detect_primary_ip().unwrap_or(IpAddr::from([0, 0, 0, 0]));

    print_banner(&bind, &hostname_short, &mac_hex, &primary_ip);

    // mDNS registration so Marstek can auto-discover us as a Shelly Pro 3EM.
    // The daemon must be held in scope for the entire program lifetime,
    // otherwise it shuts down on drop and the announcement disappears.
    let _mdns_daemon = match register_mdns(&hostname_short, &mac_hex, primary_ip) {
        Ok(d) => {
            println!(
                "[mDNS] registered as {hostname_short}.local on {primary_ip} (services _shelly._tcp + _http._tcp)"
            );
            Some(d)
        }
        Err(e) => {
            eprintln!("[mDNS] WARNING: registration failed: {e}");
            eprintln!("       Marstek auto-discovery won't work; configure the Marstek manually");
            eprintln!("       to point at {primary_ip}:{port}.");
            None
        }
    };
    println!();

    let state = Arc::new(State::new());

    let s = state.clone();
    let sock = socket.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((len, peer)) => handle_request(&sock, &s, peer, &buf[..len]).await,
                Err(e) => eprintln!("[udp error] {e}"),
            }
        }
    });

    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut line = String::new();
    loop {
        print!("> ");
        io::stdout().flush().ok();
        line.clear();
        if handle.read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            print_status(&state);
            continue;
        }
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        // Snapshot polls received since the last command and reset the counter
        // so the next command echo shows fresh activity.
        let polls_since = state.polls_since_cmd.swap(0, Ordering::SeqCst);
        if polls_since > 0 {
            println!("    [{polls_since} polls received since last command]");
        }
        match parts.as_slice() {
            ["quit"] | ["exit"] | ["q"] => break,
            ["help"] | ["h"] | ["?"] => print_help(),
            ["status"] | ["s"] => print_status(&state),
            ["verbose"] | ["v"] => {
                state.verbose.store(true, Ordering::SeqCst);
                println!("    -> VERBOSE: every poll will be logged");
            }
            ["quiet"] => {
                state.verbose.store(false, Ordering::SeqCst);
                println!("    -> QUIET: only pulses, transitions and first contacts will be logged");
            }
            ["zero"] | ["0"] => {
                state.current_w.store(0, Ordering::SeqCst);
                state.polls_remaining.store(-1, Ordering::SeqCst);
                state.polls_initial.store(0, Ordering::SeqCst);
                println!("    -> 0 W indefinitely");
            }
            ["reset"] | ["r"] => {
                let until = Instant::now() + Duration::from_secs(60);
                *state.silent_until.lock().unwrap() = Some(until);
                state.current_w.store(0, Ordering::SeqCst);
                state.polls_remaining.store(-1, Ordering::SeqCst);
                state.polls_initial.store(0, Ordering::SeqCst);
                println!("    -> dropping all responses for 60 s, then 0 W");
            }
            ["set", w] => match w.parse::<i64>() {
                Ok(w) => {
                    state.current_w.store(w, Ordering::SeqCst);
                    state.polls_remaining.store(-1, Ordering::SeqCst);
                    state.polls_initial.store(0, Ordering::SeqCst);
                    println!("    -> {w} W indefinitely");
                }
                Err(_) => println!("    invalid value"),
            },
            ["set", w, n] => match (w.parse::<i64>(), n.parse::<i64>()) {
                (Ok(w), Ok(n)) if n > 0 => {
                    state.current_w.store(w, Ordering::SeqCst);
                    state.polls_remaining.store(n, Ordering::SeqCst);
                    state.polls_initial.store(n, Ordering::SeqCst);
                    println!("    -> pulse: {w} W for next {n} polls, then auto-revert to 0 W");
                }
                _ => println!("    invalid arguments"),
            },
            _ => println!("    unknown command (try 'help')"),
        }
    }

    println!("Bye.");
    Ok(())
}

fn print_banner(bind: &str, hostname: &str, mac: &str, ip: &IpAddr) {
    println!("================================================================");
    println!(" Marstek Calibration Tool  -  virtual Shelly Pro 3EM (manual)");
    println!("================================================================");
    println!();
    println!("Listening on UDP {bind}");
    println!("Advertised hostname: {hostname}.local  (MAC {mac}, IP {ip})");
    println!();
    println!("Sign convention:");
    println!("  positive watts  = grid IMPORT  -> battery should DISCHARGE");
    println!("  negative watts  = grid EXPORT  -> battery should CHARGE");
    println!();
    print_help();
}

fn register_mdns(
    hostname_short: &str,
    mac_hex: &str,
    primary_ip: IpAddr,
) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("starting mdns-sd daemon")?;
    let mac_colon = format_mac_colon(mac_hex);
    let hostname_fqdn = format!("{hostname_short}.local.");
    let port = 80u16;
    let txt = build_txt_records(hostname_short, &mac_colon);
    register_service(
        &daemon,
        "_shelly._tcp.local.",
        hostname_short,
        &hostname_fqdn,
        primary_ip,
        port,
        &txt,
    )?;
    register_service(
        &daemon,
        "_http._tcp.local.",
        hostname_short,
        &hostname_fqdn,
        primary_ip,
        port,
        &txt,
    )?;
    Ok(daemon)
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

fn build_txt_records(hostname: &str, mac_colon: &str) -> HashMap<String, String> {
    let mut t = HashMap::new();
    t.insert("id".into(), hostname.to_string());
    t.insert("mac".into(), mac_colon.to_string());
    t.insert("gen".into(), "2".into());
    t.insert("arch".into(), "esp32".into());
    t.insert("app".into(), "Pro3EM".into());
    t.insert("ver".into(), "1.4.4".into());
    t.insert("fw_id".into(), "20260101-000000/1.4.4-calibrate".into());
    t.insert("model".into(), "SPEM-003CEBEU".into());
    t
}

fn derive_mac() -> String {
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
        .unwrap_or_else(|| "B827EBCAFE42".to_string())
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

fn print_help() {
    println!("Commands:");
    println!("  set <W>           present <W> indefinitely     e.g. 'set -100'");
    println!("  set <W> <N>       present <W> for next <N> polls, then auto 0 W");
    println!("  zero              present 0 W indefinitely (battery should hold level)");
    println!("  reset             drop all replies for 60 s, then 0 W (clears battery state)");
    println!("  status            show current state");
    println!("  help              this help");
    println!("  quit              exit");
    println!();
    println!("Typical calibration sequence (you watch the Marstek app):");
    println!("  1) set -100 1     send -100 W for 1 poll only -> note charge change");
    println!("  2) zero           wait, see whether the charge level holds");
    println!("  3) set -100 2     repeat with 2 polls         -> note charge change");
    println!("  4) ... and so on for 3, 5, 8 polls            -> map polls -> retained W");
    println!("  5) reset          when finished a pulse cycle and want a clean slate");
    println!();
}

fn print_status(state: &State) {
    let cur_w = state.current_w.load(Ordering::SeqCst);
    let rem = state.polls_remaining.load(Ordering::SeqCst);
    let init = state.polls_initial.load(Ordering::SeqCst);
    let total = state.total_polls.load(Ordering::SeqCst);
    let silent_str = match *state.silent_until.lock().unwrap() {
        Some(t) if t > Instant::now() => {
            format!(
                "SILENT for {:.1} s more",
                t.saturating_duration_since(Instant::now()).as_secs_f64()
            )
        }
        _ => "active".to_string(),
    };
    let pulse_str = if rem < 0 {
        "indefinite".to_string()
    } else if init > 0 {
        format!("pulse {}/{} polls remaining", rem, init)
    } else {
        format!("{rem} polls left", rem = rem)
    };
    let last_str = match *state.last_poll_at.lock().unwrap() {
        Some(t) => format!("{:.1} s ago", t.elapsed().as_secs_f64()),
        None => "never".to_string(),
    };
    println!("    [status] sending: {cur_w} W  |  {pulse_str}  |  {silent_str}");
    println!("             total polls received: {total}  |  last poll: {last_str}");
}

async fn handle_request(socket: &UdpSocket, state: &Arc<State>, peer: SocketAddr, payload: &[u8]) {
    let req: RequestFrame = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(_) => return,
    };

    let now = Instant::now();
    let delta_ms: u128 = {
        let mut last = state.last_poll_at.lock().unwrap();
        let d = last.map(|t| now.duration_since(t).as_millis()).unwrap_or(0);
        *last = Some(now);
        d
    };

    let total = state.total_polls.fetch_add(1, Ordering::SeqCst) + 1;
    state.polls_since_cmd.fetch_add(1, Ordering::SeqCst);

    let is_new_peer = state.seen_peers.lock().unwrap().insert(peer.ip());

    let silent = matches!(*state.silent_until.lock().unwrap(), Some(t) if now < t);
    let timestamp = Local::now().format("%H:%M:%S%.3f").to_string();

    let verbose = state.verbose.load(Ordering::SeqCst);

    if silent {
        // Only log silent drops in verbose mode or for new peers - otherwise
        // a 60-second reset would print ~100 lines.
        if verbose || is_new_peer {
            log_line(&format!(
                "[{timestamp}] poll #{total} {peer} {method} -> SILENT (no reply)  Δ{delta_ms} ms",
                method = req.method
            ));
        }
        return;
    }

    let value_to_send = state.current_w.load(Ordering::SeqCst);
    let rem = state.polls_remaining.load(Ordering::SeqCst);
    let init = state.polls_initial.load(Ordering::SeqCst);
    let mut pulse_note = String::new();
    let mut pulse_active = false;
    let mut pulse_just_ended = false;
    if rem > 0 {
        pulse_active = true;
        let pos = init - rem + 1;
        if rem == 1 {
            state.current_w.store(0, Ordering::SeqCst);
            state.polls_remaining.store(-1, Ordering::SeqCst);
            state.polls_initial.store(0, Ordering::SeqCst);
            pulse_note = format!(" [pulse {pos}/{init} - LAST, next reply will be 0 W]");
            pulse_just_ended = true;
        } else {
            state.polls_remaining.store(rem - 1, Ordering::SeqCst);
            pulse_note = format!(" [pulse {pos}/{init}]");
        }
    }

    // Logging policy: stay quiet during steady state so the user can type.
    // Print only the events that matter: new peer first contact, every poll
    // during an active pulse, the auto-revert announcement, or always in
    // verbose mode.
    let should_log = verbose || pulse_active || pulse_just_ended || is_new_peer;
    if should_log {
        let mut line = format!(
            "[{timestamp}] poll #{total} {peer} {method} -> {value_to_send} W{pulse_note}  Δ{delta_ms} ms",
            method = req.method
        );
        if is_new_peer && !pulse_active {
            line.push_str("  [NEW PEER]");
        }
        log_line(&line);
    }

    let result = build_result(&req.method, value_to_send);
    let resp = ResponseFrame {
        id: req.id,
        src: "shellypro3em-calibrate".to_string(),
        dst: req.src,
        result: Some(result),
        error: None,
    };
    if let Ok(bytes) = serde_json::to_vec(&resp) {
        let _ = socket.send_to(&bytes, peer).await;
    }
}

/// Print a log line above the current input prompt without clobbering what
/// the user has typed so far. We rewrite the prompt afterwards.
fn log_line(msg: &str) {
    // \r jumps to column 0, then a long pad of spaces erases the leftover
    // "> " prompt artefact, then \r again, then the message.
    print!("\r{:80}\r{}\n> ", "", msg);
    io::stdout().flush().ok();
}

fn build_result(method: &str, value_w: i64) -> Value {
    let v = value_w as f64;
    let i = v / 230.0;
    let m = method.to_lowercase();
    match m.as_str() {
        "em.getstatus" => json!({
            "id": 0,
            "a_voltage": 230.0, "a_current": i, "a_act_power": v, "a_aprt_power": v.abs(),
            "a_pf": 1.0, "a_freq": 50.0, "a_errors": [],
            "b_voltage": 230.0, "b_current": 0.0, "b_act_power": 0.0, "b_aprt_power": 0.0,
            "b_pf": 1.0, "b_freq": 50.0, "b_errors": [],
            "c_voltage": 230.0, "c_current": 0.0, "c_act_power": 0.0, "c_aprt_power": 0.0,
            "c_pf": 1.0, "c_freq": 50.0, "c_errors": [],
            "n_current": 0.0, "n_errors": [],
            "total_current": i, "total_act_power": v, "total_aprt_power": v.abs(),
            "user_calibrated_phase": [], "errors": []
        }),
        "em.getconfig" => json!({
            "id": 0, "name": null, "blink_mode_selector": "active_energy",
            "phase_selector": "a", "monitor_phase_sequence": true,
            "reverse": {}, "ct_type": "120A"
        }),
        "emdata.getstatus" => json!({
            "id": 0,
            "a_total_act_energy": 0.0, "a_total_act_ret_energy": 0.0,
            "b_total_act_energy": 0.0, "b_total_act_ret_energy": 0.0,
            "c_total_act_energy": 0.0, "c_total_act_ret_energy": 0.0,
            "total_act": 0.0, "total_act_ret": 0.0
        }),
        "emdata.getconfig" => json!({"id": 0}),
        "shelly.getdeviceinfo" => json!({
            "name": null, "id": "shellypro3em-calibrate", "mac": "AABBCCDDEEFF",
            "slot": 1, "model": "SPEM-003CEBEU", "gen": 2, "fw_id": "v1.4.4",
            "ver": "1.4.4", "app": "Pro3EM", "auth_en": false, "auth_domain": null,
            "profile": "triphase"
        }),
        "shelly.getstatus" => json!({
            "ble": {}, "bthome": {}, "cloud": {"connected": false},
            "em:0": {
                "id": 0,
                "a_voltage": 230.0, "a_current": i, "a_act_power": v, "a_aprt_power": v.abs(),
                "a_pf": 1.0, "a_freq": 50.0, "a_errors": [],
                "b_voltage": 230.0, "b_current": 0.0, "b_act_power": 0.0, "b_aprt_power": 0.0,
                "b_pf": 1.0, "b_freq": 50.0, "b_errors": [],
                "c_voltage": 230.0, "c_current": 0.0, "c_act_power": 0.0, "c_aprt_power": 0.0,
                "c_pf": 1.0, "c_freq": 50.0, "c_errors": [],
                "n_current": 0.0, "n_errors": [],
                "total_current": i, "total_act_power": v, "total_aprt_power": v.abs(),
                "user_calibrated_phase": [], "errors": []
            },
            "emdata:0": {
                "id": 0,
                "a_total_act_energy": 0.0, "a_total_act_ret_energy": 0.0,
                "b_total_act_energy": 0.0, "b_total_act_ret_energy": 0.0,
                "c_total_act_energy": 0.0, "c_total_act_ret_energy": 0.0,
                "total_act": 0.0, "total_act_ret": 0.0
            },
            "eth": {"ip": null}, "modbus": {}, "mqtt": {"connected": false},
            "sys": sys_status_json(),
            "temperature:0": {"id": 0, "tC": 35.0, "tF": 95.0},
            "wifi": {"sta_ip": null, "status": "got ip", "ssid": null, "rssi": -55},
            "ws": {"connected": false}
        }),
        "shelly.getconfig" => json!({
            "ble": {"enable": false, "rpc": {"enable": false}},
            "cloud": {"enable": false, "server": null},
            "em:0": {
                "id": 0, "name": null, "blink_mode_selector": "active_energy",
                "phase_selector": "a", "monitor_phase_sequence": true,
                "reverse": {}, "ct_type": "120A"
            },
            "emdata:0": {"id": 0},
            "eth": {"enable": true, "ipv4mode": "dhcp"},
            "modbus": {"enable": false},
            "mqtt": {"enable": false},
            "sys": {
                "device": {"name": null, "mac": "AABBCCDDEEFF", "fw_id": "v1.4.4",
                           "discoverable": true, "eco_mode": false},
                "location": {"tz": "Etc/UTC", "lat": null, "lon": null},
                "debug": {"mqtt": {"enable": false}, "websocket": {"enable": false},
                          "udp": {"addr": null}},
                "ui_data": {}, "rpc_udp": {"dst_addr": null, "listen_port": null},
                "sntp": {"server": "time.google.com"}, "cfg_rev": 1
            },
            "wifi": {
                "ap": {"ssid": "ShellyPro3EM", "is_open": true, "enable": false},
                "sta": {"ssid": null, "is_open": true, "enable": true, "ipv4mode": "dhcp"},
                "sta1": {"ssid": null, "is_open": true, "enable": false},
                "roam": {"rssi_thr": -80, "interval": 60}
            },
            "ws": {"enable": false, "server": null, "ssl_ca": null}
        }),
        "sys.getstatus" => sys_status_json(),
        "sys.getconfig" => json!({
            "device": {"name": null, "mac": "AABBCCDDEEFF", "fw_id": "v1.4.4",
                       "discoverable": true, "eco_mode": false},
            "location": {"tz": "Etc/UTC", "lat": null, "lon": null},
            "sntp": {"server": "time.google.com"}, "cfg_rev": 1
        }),
        "wifi.getstatus" => json!({"sta_ip": null, "status": "got ip", "ssid": null, "rssi": -55}),
        "wifi.getconfig" => json!({
            "ap": {"ssid": "ShellyPro3EM", "is_open": true, "enable": false},
            "sta": {"ssid": null, "is_open": true, "enable": true, "ipv4mode": "dhcp"}
        }),
        "cloud.getstatus" => json!({"connected": false}),
        "cloud.getconfig" => json!({"enable": false, "server": null}),
        "ws.getstatus" => json!({"connected": false}),
        "ws.getconfig" => json!({"enable": false, "server": null, "ssl_ca": null}),
        "mqtt.getstatus" => json!({"connected": false}),
        "mqtt.getconfig" => json!({"enable": false}),
        "eth.getstatus" => json!({"ip": null}),
        "eth.getconfig" => json!({"enable": true, "ipv4mode": "dhcp"}),
        "ble.getstatus" => json!({}),
        "ble.getconfig" => json!({"enable": false, "rpc": {"enable": false}}),
        "modbus.getstatus" => json!({}),
        "modbus.getconfig" => json!({"enable": false}),
        "temperature.getstatus" => json!({"id": 0, "tC": 35.0, "tF": 95.0}),
        "shelly.reboot" => json!({}),
        "script.list" => json!({"scripts": []}),
        "script.getcode" => json!({"data": "", "left": 0}),
        _ => json!({}),
    }
}

fn sys_status_json() -> Value {
    let unixtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    json!({
        "mac": "AABBCCDDEEFF", "restart_required": false,
        "time": format!("{:02}:{:02}", (unixtime / 3600) % 24, (unixtime / 60) % 60),
        "unixtime": unixtime, "uptime": 100,
        "ram_size": 259176, "ram_free": 87268, "ram_min_free": 74044,
        "fs_size": 524288, "fs_free": 196608,
        "cfg_rev": 1, "kvs_rev": 0, "schedule_rev": 0, "webhook_rev": 0,
        "available_updates": {}, "reset_reason": 3
    })
}
