#!/usr/bin/env bash
#
# Shelly Multiplexer uninstaller. Removes the binary, the systemd unit
# and the system user. Asks before removing the config and state dirs
# so you don't lose a tuned setup by accident.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/hilman2/shelly-multiplexer/main/scripts/uninstall.sh | sudo bash
# or:
#   sudo bash uninstall.sh

set -euo pipefail

BIN_PATH="/usr/local/bin/shelly-multiplexer"
SERVICE_PATH="/etc/systemd/system/shelly-multiplexer.service"
CONF_DIR="/etc/shelly-multiplexer"
STATE_DIR="/var/lib/shelly-multiplexer"
USER_NAME="shelly-multiplexer"

if [ -t 1 ]; then
    c_green=$'\033[1;32m'; c_yellow=$'\033[1;33m'; c_red=$'\033[1;31m'
    c_blue=$'\033[1;34m'; c_dim=$'\033[2m'; c_reset=$'\033[0m'
else
    c_green=; c_yellow=; c_red=; c_blue=; c_dim=; c_reset=
fi

info() { printf "%s==>%s %s\n" "$c_blue"  "$c_reset" "$*"; }
ok()   { printf "%s ✓%s %s\n" "$c_green" "$c_reset" "$*"; }
warn() { printf "%s !%s %s\n" "$c_yellow" "$c_reset" "$*"; }

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root: sudo bash $0" >&2
    exit 1
fi

ask_yes() {
    local __prompt="$1" __default="${2:-n}" __ans
    local __hint
    case "$__default" in y|Y) __hint="Y/n" ;; *) __hint="y/N" ;; esac
    if [ ! -e /dev/tty ]; then
        # Non-interactive run: take the safe default.
        [ "$__default" = "y" ] && return 0 || return 1
    fi
    printf "%s%s%s [%s]: " "$c_dim" "$__prompt" "$c_reset" "$__hint" > /dev/tty
    IFS= read -r __ans < /dev/tty
    [ -z "$__ans" ] && __ans="$__default"
    case "$__ans" in y|Y|yes|YES|Yes) return 0 ;; *) return 1 ;; esac
}

info "Stopping + disabling service ..."
systemctl stop shelly-multiplexer 2>/dev/null || true
systemctl disable shelly-multiplexer 2>/dev/null || true

if [ -f "$SERVICE_PATH" ]; then
    rm -f "$SERVICE_PATH"
    systemctl daemon-reload
    ok "Removed $SERVICE_PATH"
fi

if [ -f "$BIN_PATH" ]; then
    rm -f "$BIN_PATH"
    ok "Removed $BIN_PATH"
fi

if [ -d "$CONF_DIR" ]; then
    if ask_yes "Remove config directory $CONF_DIR (your tuned config.toml lives here)?" "n"; then
        rm -rf "$CONF_DIR"
        ok "Removed $CONF_DIR"
    else
        warn "Kept $CONF_DIR — you can delete it manually later."
    fi
fi

if [ -d "$STATE_DIR" ]; then
    if ask_yes "Remove state directory $STATE_DIR?" "n"; then
        rm -rf "$STATE_DIR"
        ok "Removed $STATE_DIR"
    else
        warn "Kept $STATE_DIR."
    fi
fi

if id -u "$USER_NAME" >/dev/null 2>&1; then
    if ask_yes "Remove system user '$USER_NAME'?" "y"; then
        userdel "$USER_NAME" 2>/dev/null || true
        ok "Removed user $USER_NAME"
    fi
fi

ok "Uninstall complete."
