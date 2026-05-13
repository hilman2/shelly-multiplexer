#!/usr/bin/env bash
#
# Shelly Multiplexer interactive installer for Debian / Ubuntu /
# Raspberry Pi OS.
#
# What it does:
#   - detects your CPU architecture
#   - fetches the latest GitHub release
#   - verifies the SHA-256 checksum
#   - creates a system user + directories
#   - asks for the basics (real Shelly IP, ports, admin UI port)
#   - generates /etc/shelly-multiplexer/config.toml
#   - installs the binary + systemd unit, enables and starts it
#
# Re-running upgrades the binary in-place and keeps your config.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/hilman2/shelly-multiplexer/main/scripts/install.sh | sudo bash
# or, downloaded first (recommended so you can read it):
#   wget https://raw.githubusercontent.com/hilman2/shelly-multiplexer/main/scripts/install.sh
#   sudo bash install.sh

set -euo pipefail

REPO="hilman2/shelly-multiplexer"
BIN_PATH="/usr/local/bin/shelly-multiplexer"
SERVICE_PATH="/etc/systemd/system/shelly-multiplexer.service"
CONF_DIR="/etc/shelly-multiplexer"
CONF_PATH="$CONF_DIR/config.toml"
STATE_DIR="/var/lib/shelly-multiplexer"
USER_NAME="shelly-multiplexer"

# ---------- helpers ----------

if [ -t 1 ]; then
    c_green=$'\033[1;32m'; c_yellow=$'\033[1;33m'; c_red=$'\033[1;31m'
    c_blue=$'\033[1;34m'; c_dim=$'\033[2m'; c_reset=$'\033[0m'
else
    c_green=; c_yellow=; c_red=; c_blue=; c_dim=; c_reset=
fi

info()  { printf "%s==>%s %s\n" "$c_blue"  "$c_reset" "$*"; }
ok()    { printf "%s ✓%s %s\n" "$c_green" "$c_reset" "$*"; }
warn()  { printf "%s !%s %s\n" "$c_yellow" "$c_reset" "$*"; }
fatal() { printf "%s ✗%s %s\n" "$c_red"  "$c_reset" "$*" >&2; exit 1; }

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        fatal "This installer must run as root. Try: sudo bash $0"
    fi
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 \
        || fatal "Required command not found: $1. Install it and try again."
}

require_tty() {
    if [ ! -e /dev/tty ] || ! { : > /dev/tty; } 2>/dev/null; then
        fatal "No interactive terminal. Download the script first and run it as 'sudo bash install.sh'."
    fi
}

# ask <varname> <prompt> [default]
ask() {
    local __var="$1" __prompt="$2" __default="${3:-}" __ans
    if [ -n "$__default" ]; then
        printf "%s%s%s [%s]: " "$c_dim" "$__prompt" "$c_reset" "$__default" > /dev/tty
    else
        printf "%s%s%s: " "$c_dim" "$__prompt" "$c_reset" > /dev/tty
    fi
    IFS= read -r __ans < /dev/tty
    if [ -z "$__ans" ] && [ -n "$__default" ]; then
        __ans="$__default"
    fi
    printf -v "$__var" "%s" "$__ans"
}

# ask_yes <prompt> [default y/n] → returns 0 for yes, 1 for no
ask_yes() {
    local __prompt="$1" __default="${2:-n}" __ans
    local __hint
    case "$__default" in
        y|Y) __hint="Y/n" ;;
        *)   __hint="y/N" ;;
    esac
    printf "%s%s%s [%s]: " "$c_dim" "$__prompt" "$c_reset" "$__hint" > /dev/tty
    IFS= read -r __ans < /dev/tty
    [ -z "$__ans" ] && __ans="$__default"
    case "$__ans" in
        y|Y|yes|YES|Yes) return 0 ;;
        *)               return 1 ;;
    esac
}

# ---------- detection ----------

detect_target() {
    case "$(uname -m)" in
        x86_64)   echo "x86_64-unknown-linux-musl" ;;
        aarch64)  echo "aarch64-unknown-linux-musl" ;;
        armv7l)   echo "armv7-unknown-linux-musleabihf" ;;
        armv6l)   echo "arm-unknown-linux-musleabihf" ;;
        *)        fatal "Unsupported architecture: $(uname -m). Build from source." ;;
    esac
}

fetch_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -m1 '"tag_name"' \
        | sed -E 's/.*"v?([^"]+)".*/\1/'
}

primary_ip() {
    # First non-loopback IPv4 the system advertises.
    hostname -I 2>/dev/null | awk '{for (i=1; i<=NF; i++) if ($i !~ /^127\./ && $i !~ /:/) { print $i; exit }}'
}

# ---------- main ----------

require_root
require_cmd curl
require_cmd tar
require_cmd sha256sum
require_cmd systemctl

info "Shelly Multiplexer installer"
TARGET=$(detect_target)
ok "Architecture: $(uname -m) → $TARGET"

info "Querying GitHub for the latest release ..."
VERSION=$(fetch_latest_version) || true
[ -n "${VERSION:-}" ] || fatal "Could not determine the latest release."
ok "Latest version: v$VERSION"

# Detect existing install
EXISTING_INSTALL=no
EXISTING_VERSION=""
if [ -x "$BIN_PATH" ]; then
    EXISTING_INSTALL=yes
    EXISTING_VERSION=$("$BIN_PATH" --version 2>/dev/null | awk '{print $NF}' || true)
    info "Existing install detected: v${EXISTING_VERSION:-unknown}"
fi

# Will we generate a fresh config?
GENERATE_CONFIG=no
if [ ! -f "$CONF_PATH" ]; then
    GENERATE_CONFIG=yes
fi

if [ "$GENERATE_CONFIG" = "yes" ]; then
    require_tty
    cat > /dev/tty <<EOF

${c_yellow}Before you continue:${c_reset}
The multiplexer needs the real Shelly Pro 3EM moved off the default
UDP-RPC port 1010 so it can bind 1010 itself. In the Shelly's web UI:
  Settings → Advanced → Outbound RPC / UDP-RPC → set to e.g. 2020.
If you haven't done that yet, do it now in a second window, then come
back here.

EOF

    ask SHELLY_IP   "Real Shelly Pro 3EM IP address"
    [ -n "$SHELLY_IP" ] || fatal "Real Shelly IP is required."
    ask SHELLY_PORT "Real Shelly UDP-RPC port (the one you moved it to)" "2020"
    ask ADMIN_PORT  "Admin UI port (where the config UI listens)" "8080"
    ask HTTP_PORT   "Virtual Shelly HTTP port (80 = privileged; 8081 = unprivileged)" "80"

    cat > /dev/tty <<EOF

${c_dim}You can add your fuses and batteries later in the admin UI;
the installer only writes a minimal bootstrap config.${c_reset}

EOF
fi

# ---------- download ----------

TARBALL="shelly-multiplexer-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/v${VERSION}/${TARBALL}"
SHA_URL="${URL}.sha256"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${TARBALL} ..."
curl -fsSL "$URL" -o "$TMPDIR/$TARBALL"

if curl -fsSL "$SHA_URL" -o "$TMPDIR/$TARBALL.sha256" 2>/dev/null; then
    info "Verifying SHA-256 ..."
    (cd "$TMPDIR" && sha256sum -c "$TARBALL.sha256" >/dev/null) \
        || fatal "Checksum mismatch — refusing to install."
    ok "Checksum OK"
else
    warn "No published checksum — skipping verification."
fi

info "Extracting ..."
tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
EXTRACT_DIR="$TMPDIR/shelly-multiplexer-${VERSION}-${TARGET}"

# ---------- system user + dirs ----------

if ! id -u "$USER_NAME" >/dev/null 2>&1; then
    info "Creating system user '$USER_NAME' ..."
    useradd --system --home "$STATE_DIR" --shell /usr/sbin/nologin "$USER_NAME"
fi
install -d -o "$USER_NAME" -g "$USER_NAME" "$CONF_DIR"
install -d -o "$USER_NAME" -g "$USER_NAME" "$STATE_DIR"

# ---------- binary ----------

if systemctl is-active --quiet shelly-multiplexer; then
    info "Stopping running service for upgrade ..."
    systemctl stop shelly-multiplexer
fi
info "Installing binary → $BIN_PATH"
install -m 755 "$EXTRACT_DIR/shelly-multiplexer" "$BIN_PATH"

# ---------- service ----------

info "Installing systemd unit → $SERVICE_PATH"
install -m 644 "$EXTRACT_DIR/shelly-multiplexer.service" "$SERVICE_PATH"

# ---------- config ----------

if [ "$GENERATE_CONFIG" = "yes" ]; then
    info "Writing bootstrap config → $CONF_PATH"
    cat > "$CONF_PATH" <<EOF
# Shelly Multiplexer — bootstrap config, generated by install.sh.
# Everything below can also be edited live in the admin UI at
# http://<this-host>:${ADMIN_PORT}/  — without a restart.

[real_shelly]
host = "${SHELLY_IP}"
udp_port = ${SHELLY_PORT}
poll_interval_ms = 250
request_timeout_ms = 1000

[virtual_shelly]
bind_interface = "0.0.0.0"
udp_port = 1010
http_port = ${HTTP_PORT}
firmware = "1.4.4"
enable_mdns = true

[management]
bind_address = "0.0.0.0:${ADMIN_PORT}"

[dispatcher]
# Defaults are tuned for Marstek Venus E. Tweak in the admin UI.

[home_assistant]
enabled = false

# Add your fuses here — or do it in the admin UI:
# [[circuits]]
# id = "basement"
# fuse_amps = 16
# phases = 1
# voltage = 230

# One [[batteries]] per battery. Each battery REQUIRES its own
# Shelly Plug PM Gen3 for plug_url — that's the safety ground truth.
# [[batteries]]
# id = "marstek-1"
# address = "192.168.1.51"
# circuit = "basement"
# plug_url = "http://192.168.1.61"
# max_charge_w = 2500
# max_discharge_w = 2500
EOF
    chown "$USER_NAME:$USER_NAME" "$CONF_PATH"
    chmod 640 "$CONF_PATH"
    ok "Config written"
else
    ok "Existing config kept: $CONF_PATH"
fi

# ---------- enable + start ----------

systemctl daemon-reload
systemctl enable shelly-multiplexer >/dev/null
info "Starting service ..."
systemctl restart shelly-multiplexer

# Give it a moment so we can report status accurately.
sleep 2

# ---------- summary ----------

if systemctl is-active --quiet shelly-multiplexer; then
    ok "Service is running"
else
    warn "Service did not come up. Check: journalctl -u shelly-multiplexer -n 50"
fi

# Read admin port from config if we didn't ask (upgrade path).
EFFECTIVE_ADMIN_PORT="${ADMIN_PORT:-}"
if [ -z "$EFFECTIVE_ADMIN_PORT" ] && [ -f "$CONF_PATH" ]; then
    EFFECTIVE_ADMIN_PORT=$(grep -E '^[[:space:]]*bind_address[[:space:]]*=' "$CONF_PATH" 2>/dev/null \
        | head -n 1 \
        | sed -E 's/.*:([0-9]+).*/\1/' || true)
fi
[ -n "$EFFECTIVE_ADMIN_PORT" ] || EFFECTIVE_ADMIN_PORT=8080

PRIMARY_IP=$(primary_ip 2>/dev/null || true)
[ -n "$PRIMARY_IP" ] || PRIMARY_IP="<this-host>"

cat <<EOF

${c_green}Installation complete.${c_reset}

  Binary:    $BIN_PATH
  Config:    $CONF_PATH
  Service:   shelly-multiplexer (systemd)
  Logs:      journalctl -u shelly-multiplexer -f
  Admin UI:  http://${PRIMARY_IP}:${EFFECTIVE_ADMIN_PORT}/

Next steps:
  1. Open the admin UI and add your circuits + batteries (each battery
     needs a dedicated Shelly Plug PM Gen3 reachable on plug_url).
  2. In each battery's app, point its "Shelly Pro 3EM" target at this
     host's IP. For Marstek devices, also enable the Modbus TCP server
     (port 502) so the multiplexer can read SoC — or use HA mode by
     enabling [home_assistant] in config.toml.

To re-run this installer later (upgrades the binary, keeps your config):
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh | sudo bash

To remove everything:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/uninstall.sh | sudo bash

EOF
