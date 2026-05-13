#!/usr/bin/with-contenv bashio
# shellcheck shell=bash
set -euo pipefail

# /config/ inside the container = /addon_configs/<slug>/ on the host,
# visible from the Studio Code Server add-on for hand edits.
CONFIG_FILE="/config/config.toml"

# Boot-time options from the HA add-on UI.
REAL_HOST=$(bashio::config 'real_shelly_host')
REAL_PORT=$(bashio::config 'real_shelly_udp_port')
LOG_LEVEL=$(bashio::config 'log_level')

# Seed config.toml on first start with a placeholder pulse-mode template.
# The user MUST edit it via the web UI to add at least one circuit +
# battery (with a Shelly Plug PM Gen3) before the dispatcher does anything
# useful. real_shelly host/port are passed as CLI flags on every start so
# the HA Configuration tab always wins.
if [ ! -f "${CONFIG_FILE}" ]; then
    bashio::log.warning "No config.toml found - writing placeholder. EDIT IT before relying on the add-on!"
    mkdir -p /config
    cat > "${CONFIG_FILE}" <<EOF
# ShellyMultiplexer pulse-mode config.
#
# Edit via Studio Code Server add-on, then restart this add-on to apply.
# real_shelly.host/udp_port are overridden on every start by the HA add-on
# Configuration tab; everything else is sourced from this file only.

[real_shelly]
host = "${REAL_HOST}"
udp_port = ${REAL_PORT}
poll_interval_ms = 250
request_timeout_ms = 1000

[virtual_shelly]
bind_interface = "0.0.0.0"
udp_port = 1010
# http_port = 80 only if your inverter talks to the Shelly via HTTP/REST
# (some Hoymiles models). Marstek Venus E uses UDP/1010 + mDNS only.
http_port = 0
device_mac = ""
device_hostname = ""
firmware = "1.4.4"
# HA OS runs Avahi on UDP/5353 already; running our own mdns-sd daemon
# on top conflicts with it. Leave this off unless you've stopped Avahi.
enable_mdns = false

[management]
bind_address = "0.0.0.0:8080"

[dispatcher]
cycle_ms = 200
deadband_w = 30
hit_tolerance_w = 15
pulse_count = 3
soc_full_pct = 95
soc_empty_pct = 5
plug_stale_s = 2.0
grid_stale_s = 5.0
group_silent_after_stale_s = 60.0
circuit_headroom = 0.95
grid_bias_w = 30
settle_timeout_s = 5.0

[home_assistant]
# SoC source switch.
#   enabled = false → every Marstek battery's SoC is read via Modbus TCP
#                     directly from the inverter (default port 502).
#                     soc_entity_id values are ignored.
#   enabled = true  → SoC is read from HA entities (per-battery
#                     soc_entity_id REQUIRED) and Modbus is not used.
# Token is auto-injected from \$SUPERVISOR_TOKEN so leave it blank here.
# With host_network: true the "supervisor" alias does NOT resolve - we
# hit HA Core directly. Replace homeassistant.local with the host's IP
# if your network has no mDNS.
enabled = false
url = "http://homeassistant.local:8123/api"
token = ""
timeout_ms = 3000

# ----- Add at least one circuit and one battery below via the web UI. -----
# A circuit is a shared protective device (MCB/RCD); the dispatcher
# enforces (sum of plug power on members) <= cap_w * circuit_headroom.
# Each battery REQUIRES a dedicated Shelly Plug PM Gen3 - the plug is
# the safety ground truth.
#
# [[circuits]]
# id = "1"
# fuse_amps = 16
# voltage = 230
# phases = 1
#
# [[batteries]]
# id = "A"
# address = "192.168.1.61"               # static IP of the Marstek
# circuit = "1"
# plug_url = "http://192.168.1.71"       # the dedicated Shelly Plug
# max_charge_w = 2500
# max_discharge_w = 800
# capacity_wh = 2500
# priority_weight = 1.0
# # Modbus SoC (used when [home_assistant].enabled = false).
# # marstek_model selects the SoC register: "venus_e" (reg 34002 — v1/v2/v3)
# # or "venus_e_v12" (reg 32104).
# marstek_model = "venus_e"
# # modbus_host: IP of the RS485-to-LAN bridge wired to the battery's
# # RS485 port. Nearly every Marstek (A / D / E v1 / v2 / v1.2 / E 2.0)
# # needs one — only Venus E V3 with Ethernet speaks Modbus TCP natively
# # (set it to the same IP as \`address\` in that case). Without this the
# # battery stays INACTIVE — dispatcher skips it.
# modbus_host = "192.168.1.91"
# modbus_port = 502
# modbus_unit_id = 1
# # When [home_assistant].enabled = true use this instead:
# # soc_entity_id = "sensor.marstek_a_soc"
EOF
else
    bashio::log.info "Using existing /config/config.toml (real_shelly host/port from HA options)."
fi

export RUST_LOG="shelly_multiplexer=${LOG_LEVEL},${LOG_LEVEL}"
export RUST_BACKTRACE=1

bashio::log.info "Starting shelly-multiplexer (pulse mode, RUST_LOG=${RUST_LOG})..."

# Run instead of exec so we can capture and surface the exit reason in
# the add-on log when the binary crashes.
set +e
/usr/bin/shelly-multiplexer \
    --config "${CONFIG_FILE}" \
    --real-shelly-host "${REAL_HOST}" \
    --real-shelly-udp-port "${REAL_PORT}"
EXIT=$?
set -e
bashio::log.error "shelly-multiplexer exited with status ${EXIT}"
exit "${EXIT}"
