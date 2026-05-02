# ShellyMultiplexer

Splits the per-phase grid measurement of one Shelly Pro 3EM across
multiple battery storage systems so they no longer fight each other.

Without the multiplexer, every battery reads the same Shelly value and
reacts to it independently — the result is N-fold over-correction and
oscillation. The multiplexer sits in front of the real Shelly, polls
it at higher frequency than the batteries do, and presents each battery
with its own, scaled-down view of grid power so the sum of their
reactions matches reality.

## Features

- **Virtual Shelly Pro 3EM** on UDP-RPC (port 1010), HTTP-REST (`/shelly`,
  `/settings`, `/status`) and HTTP-RPC (`/rpc`). Wire-compatible with
  Marstek, Hoymiles, Anker Solix and other batteries that integrate via
  Shelly Pro 3EM emulation.
- **Discoverable on every channel a real Pro 3EM uses:**
  - **mDNS / Bonjour:** advertises `_shelly._tcp.local` and
    `_http._tcp.local` with TXT records `id`, `mac`, `gen=2`,
    `arch=esp32`, `app=Pro3EM`, `ver`, `fw_id`, `model=SPEM-003CEBEU`.
  - **HTTP identity endpoints:** `/shelly` and `/rpc/Shelly.GetDeviceInfo`.
  - **UDP-RPC discovery:** any `Shelly.GetDeviceInfo` request to UDP
    port 1010 — broadcast or unicast — gets a complete reply.
- **Group-aware allocation** — assign batteries to a group with a shared
  fuse and the dispatcher will never let them collectively exceed that
  fuse's rating, even at peak.
- **Per-battery limits** — each battery has its own `max_charge_w` and
  `max_discharge_w`, applied per phase.
- **Phase awareness** — single-phase batteries (e.g. Marstek B2500/Venus
  on L1) only see the phase they're connected to.
- **Marstek Open API integration** — pulls live SoC, actual delivered
  power and charge/discharge permission flags via UDP. Used to drive:
  - SoC-weighted allocation (`strategy = "by_soc"`)
  - skip-when-disabled (a battery that can't currently discharge is
    excluded from the discharge plan)
  - **redispatch on underdelivery** — if a battery sustainedly delivers
    less than its share, the unmet portion is shifted to other batteries
    with headroom.
- **Anti-oscillation:** rate-limited per-battery setpoint changes plus a
  configurable deadband.
- **Global safety cap** — hard 3000 W ceiling on the absolute sum of all
  allocations. Lifting it requires two explicit acknowledgements (TOML
  flags or web-UI confirm flow).
- **Web UI** for live status, allocation visibility, telemetry and the
  safety override flow.
- **Cross-platform** — Linux, Windows, macOS.

## Architecture

Detailed design notes live in [`docs/architecture.md`](docs/architecture.md).
The short version:

```
                  ┌──────────────────────────────────────────────┐
                  │                                              │
   Real Shelly    │  Multiplexer                                 │
   ──────────    │  ──────────                                  │
   :2020 (RPC) ◀─┤  Real-Shelly poller (UDP-RPC, ~4 Hz)         │
                  │     │                                       │
                  │     ▼                                       │
                  │  Dispatcher (groups, caps, telemetry, redispatch)
                  │     │                                       │
                  │     ▼                                       │
   Batteries  ───▶│  Virtual Shelly Pro 3EM                     │
   poll :1010    │   (UDP :1010, HTTP :80 — Shelly-compatible)  │
                  │                                              │
   Marstek API   │  Marstek telemetry (UDP :30000 → battery)    │
                  │                                              │
   Browser   ───▶│  Web UI / management API (:8080)             │
                  └──────────────────────────────────────────────┘
```

> ⚠ **Required configuration on the real Shelly:** move its UDP-RPC port
> off the default 1010 (e.g. to 2020). Otherwise the multiplexer can't
> bind 1010 and the batteries will reach the real Shelly directly.

## Building

Requires Rust 1.85+ (edition 2024).

```bash
cargo build --release
# binary: target/release/shelly-multiplexer  (or .exe on Windows)
```

The web UI is embedded into the binary at compile time — no extra files
to deploy.

## Running

1. Copy the example config:

   ```bash
   cp config.example.toml config.toml
   ```

2. Edit `config.toml`:
   - Real Shelly IP and the *moved* UDP port,
   - Battery IPs, max powers, group assignment, phase,
   - Group fuse ratings.

3. Start it:

   ```bash
   ./shelly-multiplexer --config config.toml
   ```

4. Open the web UI: <http://localhost:8080>

5. In each battery, point its "Shelly Pro 3EM" target at the
   multiplexer's IP (instead of the real Shelly's IP). For Marstek
   devices, enable the Open API in the Marstek app and confirm the UDP
   port matches `marstek_port` in `config.toml`.

### Privileged ports

UDP 1010 and HTTP 80 are below 1024 and need elevated rights on Linux:

```bash
sudo setcap 'cap_net_bind_service=+ep' target/release/shelly-multiplexer
```

On Windows, run as Administrator or change the ports in `config.toml`.

### mDNS across subnets / VLANs

mDNS uses link-local multicast and **does not cross subnet or VLAN
boundaries** by default. If the multiplexer and the batteries live on
different VLANs, either:

- enable an mDNS repeater on your gateway (Avahi reflector, OPNsense
  "Avahi" plugin, UniFi mDNS reflection, etc.), **or**
- manually configure each battery with the multiplexer's IP — this
  works regardless of mDNS and is what Marstek devices need anyway.

On the same subnet, no extra setup is needed; batteries scanning for a
Shelly Pro 3EM will find the multiplexer.

## Allocation strategies

| `strategy` | Behaviour |
|---|---|
| `equal` | Every eligible battery in the group gets the same share. Default. |
| `by_capacity` | Proportional to per-battery max charge/discharge limits. |
| `by_soc` | Discharge: prefer batteries with high SoC. Charge: prefer batteries with low SoC. Falls back to `equal` for batteries without telemetry. |
| `priority` | Lower `priority` value contributes first; higher tiers only kick in when the lower tier saturates. |

## Safety cap

By default, the multiplexer will never dispatch more than **3000 W**
total in either direction (charging or discharging summed across all
batteries). This is a residential-friendly default that protects against
configuration mistakes — no single mis-configured fuse can be
overloaded.

To raise the cap above 3000 W, **both** of the following are required:

1. Edit `config.toml`:
   ```toml
   [safety]
   max_total_w = 5000
   acknowledged_higher_risk = true
   acknowledged_separate_fuses = true
   ```
2. Or use the web UI's two-step confirm flow (Override active is shown
   in the header). Runtime overrides are deliberately not persisted —
   restarting always returns the system to the value in `config.toml`.

The acknowledgements explicitly state:

- `acknowledged_higher_risk`: I understand that going above 3000 W can
  cause overload, equipment damage, or fire if the wiring isn't rated.
- `acknowledged_separate_fuses`: every battery is on its **own**,
  adequately rated protective device. No two batteries share a
  protective device whose rated current is below their summed output.

## Testing without batteries

For development you can run the binary against a local-only smoke
config:

```bash
cargo run -- --config config.test.toml
curl http://127.0.0.1:18080/api/health
```

The poller will log timeouts (no real Shelly at the configured IP) but
all servers come up and the web UI works.

## License

Apache-2.0. The Shelly RPC wire format is reverse-engineered from
publicly available Shelly firmware behaviour and the Apache-2.0-licensed
[uni-meter](https://github.com/sdeigm/uni-meter) project.
