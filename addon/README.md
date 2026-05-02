# Shelly Multiplexer — Home Assistant Add-on

Runs the [Shelly Multiplexer](https://github.com/hilman2/shelly-multiplexer)
as an HA add-on. The web UI is integrated into the Home Assistant
sidebar via Ingress, so no separate URL or port forwarding is needed.

## Installation

1. In HA: **Settings → Add-ons → Add-on Store → ⋮ → Repositories**.
2. Add the URL of this repository.
3. Install **Shelly Multiplexer** from the new repository entry.
4. Set the real Shelly's IP and reconfigured UDP port in the add-on's
   **Configuration** tab and start the add-on.
5. Open the multiplexer's web UI from the HA sidebar to add batteries,
   tune the dispatcher and override the safety cap.

## Required setup on the real Shelly

Before starting the add-on the real Shelly Pro 3EM **must** be moved
off the default UDP-RPC port 1010 (the multiplexer needs to bind it).
Use the Shelly UI: **Settings → Advanced → Outbound RPC / UDP RPC**.

## Required network setup

The add-on uses host networking — it listens on the host's IP for:

- UDP/1010 — the virtual Shelly the batteries poll
- TCP/80   — Shelly REST/RPC (**off by default**, only enable it for
  inverters that talk HTTP to the Shelly — some Hoymiles models)
- TCP/8080 — admin web UI (proxied via HA Ingress)

Marstek Venus E and similar use only UDP/1010 + mDNS, so out of the
box the add-on doesn't bind port 80 and won't conflict with HA's own
proxy or anything else on the host. If you need the HTTP endpoint,
flip `virtual_shelly.http_port` in the web UI to 80 (or any free
port) and re-point the inverters at it.

## Persistent state

Configuration lives in `/addon_configs/<slug>/config.toml` on the
host. Backups via the HA backup system include it automatically.

## Updating

When you update the add-on, the `config.toml` is preserved. Schema
changes are forward-compatible — fields you didn't set get sensible
defaults.
