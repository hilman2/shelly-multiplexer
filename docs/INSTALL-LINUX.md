# Native Linux installation (Debian / Ubuntu / Raspberry Pi OS)

This guide installs Shelly Multiplexer as a systemd service from the
prebuilt static binaries published on each GitHub Release. No Docker,
no Rust toolchain on the target machine required.

## 1. Choose the right archive

| Hardware | Target tarball |
|---|---|
| x86_64 server / NAS / generic PC | `shelly-multiplexer-<ver>-x86_64-unknown-linux-musl.tar.gz` |
| Raspberry Pi 3 / 4 / 5 (64-bit OS), Pi Zero 2 W | `shelly-multiplexer-<ver>-aarch64-unknown-linux-musl.tar.gz` |
| Raspberry Pi 2 / 3 / 4 on 32-bit Raspberry Pi OS | `shelly-multiplexer-<ver>-armv7-unknown-linux-musleabihf.tar.gz` |
| Raspberry Pi 1 / Pi Zero / Pi Zero W (ARMv6) | `shelly-multiplexer-<ver>-arm-unknown-linux-musleabihf.tar.gz` |

Find the latest version at
<https://github.com/hilman2/shelly-multiplexer/releases>.

If unsure which one your Pi needs:

```bash
uname -m
# aarch64       → aarch64-unknown-linux-musl
# armv7l        → armv7-unknown-linux-musleabihf
# armv6l        → arm-unknown-linux-musleabihf
# x86_64        → x86_64-unknown-linux-musl
```

The binaries are statically linked against musl — they run on any glibc
version and any reasonably recent kernel.

## 2. Prerequisites

> **IMPORTANT** — before starting the multiplexer, move the real Shelly
> Pro 3EM off the default UDP-RPC port 1010 (e.g. to 2020) in the Shelly
> web UI: *Settings → Advanced → Outbound RPC / UDP-RPC*. Otherwise
> the multiplexer can't bind 1010 on the same LAN.

## 3. Install

```bash
# Set these for your release + hardware:
VERSION=0.4.5
TARGET=aarch64-unknown-linux-musl   # change to your platform from the table above

# Download + extract
cd /tmp
curl -fLO "https://github.com/hilman2/shelly-multiplexer/releases/download/v${VERSION}/shelly-multiplexer-${VERSION}-${TARGET}.tar.gz"
tar -xzf "shelly-multiplexer-${VERSION}-${TARGET}.tar.gz"
cd "shelly-multiplexer-${VERSION}-${TARGET}"

# Create a dedicated unprivileged user
sudo useradd --system --home /var/lib/shelly-multiplexer \
             --shell /usr/sbin/nologin shelly-multiplexer || true

# Directories
sudo install -d -o shelly-multiplexer -g shelly-multiplexer /etc/shelly-multiplexer
sudo install -d -o shelly-multiplexer -g shelly-multiplexer /var/lib/shelly-multiplexer

# Binary
sudo install -m 755 shelly-multiplexer /usr/local/bin/shelly-multiplexer

# Config (template — edit before starting)
sudo install -m 640 -o shelly-multiplexer -g shelly-multiplexer \
             config.example.toml /etc/shelly-multiplexer/config.toml
sudoedit /etc/shelly-multiplexer/config.toml

# systemd service
sudo install -m 644 shelly-multiplexer.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now shelly-multiplexer
```

## 4. Verify

```bash
sudo systemctl status shelly-multiplexer
journalctl -u shelly-multiplexer -f
```

The admin UI is reachable at `http://<host-ip>:8080`.

The virtual Shelly listens on UDP 1010 (Marstek polling) and HTTP 80
(`/shelly`, `/settings`, `/rpc`). mDNS announces it on the LAN as
`shellypro3em-<mac>.local`.

## 5. Upgrade

```bash
# Repeat steps 1 + 3 (download new tarball, re-install binary), then:
sudo systemctl restart shelly-multiplexer
```

The config in `/etc/shelly-multiplexer/config.toml` is preserved across
upgrades; only the binary in `/usr/local/bin/` is overwritten.

## 6. Notes

- **Privileged ports** — UDP 1010 and HTTP 80 are below 1024. The
  service grants `CAP_NET_BIND_SERVICE` to the non-root user so binding
  works without root. If you set `[virtual_shelly] http_port` above
  1023, you can drop the capability from the unit.
- **Firewall** — open UDP 1010 (Marstek → multiplexer), TCP 80
  (Marstek HTTP probes), TCP 8080 (admin UI), UDP 5353 (mDNS).
- **Home Assistant SoC** — set `[home_assistant] enabled = true` and
  paste a long-lived token; per-battery `soc_entity_id` reads from HA.
- **Logs** — `journalctl -u shelly-multiplexer`. Raise verbosity with
  `Environment=RUST_LOG=shelly_multiplexer=debug` in a drop-in
  (`sudo systemctl edit shelly-multiplexer`).
