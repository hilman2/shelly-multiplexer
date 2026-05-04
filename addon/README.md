# Shelly Multiplexer — Home Assistant Add-on

Runs the [Shelly Multiplexer](https://github.com/hilman2/shelly-multiplexer)
as an HA add-on. From v0.2.0 the dispatcher is **pulse-based**: each
battery has its own virtual integrator, the dispatcher emits short delta
pulses on every change, and the Marstek's internal integrator commits
them. A dedicated Shelly Plug PM Gen3 per battery is **mandatory** —
plug measurements are the safety ground truth for circuit-cap
enforcement.

## Installation

1. In HA: **Settings → Add-ons → Add-on Store → ⋮ → Repositories**.
2. Add the URL of this repository.
3. Install **Shelly Multiplexer** from the new repository entry.
4. Set the real Shelly's IP and reconfigured UDP port in the add-on's
   **Configuration** tab and start the add-on.
5. From the add-on's **Info** tab, click **OPEN WEB UI** to see the
   live status. The UI is reachable directly at
   `http://<ha-host>:8080`.

> The web UI cannot run via Home Assistant Ingress because the add-on
> needs `host_network: true` (so the inverters can find it on UDP/1010
> and via mDNS), and Ingress is incompatible with host networking.

## Required setup on the real Shelly

Before starting the add-on the real Shelly Pro 3EM **must** be moved
off the default UDP-RPC port 1010 (the multiplexer needs to bind it).
Use the Shelly UI: **Settings → Advanced → Outbound RPC / UDP RPC**.

## Required setup per battery

Each battery you steer needs its own dedicated Shelly Plug PM Gen3 in
line with the Marstek (the plug measures what the inverter actually
draws / delivers). Configure each battery in `/config/config.toml`
under `[[battery]]` with `address` (Marstek static IP), `circuit`,
`plug_url` and the hardware caps. Without `plug_url` the dispatcher
refuses to start.

If the plug for a battery goes silent for more than `plug_stale_s`
(default 2 s), the entire circuit is muted — CT silent for
`group_silent_after_stale_s` (default 60 s) so every Marstek's
watchdog clears its internal target back to zero. Resumes once plug
data is back.

## Required network setup

The add-on uses host networking — it listens on the host's IP for:

- UDP/1010 — the virtual Shelly the batteries poll
- TCP/80   — Shelly REST/RPC (**off by default**, only enable it for
  inverters that talk HTTP to the Shelly — some Hoymiles models)
- TCP/8080 — admin web UI (no auth, trust your network or restrict at
  the firewall)

Marstek Venus E uses only UDP/1010 + mDNS, so out of the box the
add-on doesn't bind port 80.

## Persistent state

Configuration lives in `/addon_configs/<slug>/config.toml` on the
host. Backups via the HA backup system include it automatically. From
v0.2.0 the file is read once at startup — edit via the Studio Code
Server add-on, then restart this add-on to apply.

## Updating from v0.1.x

The v0.2.0 schema is **not** backwards-compatible with v0.1.x. After
updating, the dispatcher will refuse to start with a clear error
message about the missing `plug_url`. Either:

- Delete `/config/config.toml` so the add-on writes the v0.2.0
  template on next start, then add your circuits + batteries; or
- Manually migrate: drop the `[safety]` section, rename
  `[[batteries]]` -> `[[battery]]` if needed, add `plug_url` to
  every battery, drop `min_soc_percent` / `max_soc_percent` /
  `phase` / `priority` / `vendor`-specific fields no longer used.

The v0.2.0 schema is documented in the in-app config dump and in the
project README.
