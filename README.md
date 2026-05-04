# ShellyMultiplexer

Steers multiple battery storage systems by emulating a Shelly Pro 3EM
that each Marstek Venus E (or compatible) polls. From v0.2.0 the
dispatcher is **pulse-based**: each battery has its own virtual
integrator, the dispatcher emits short delta pulses on every change,
and the Marstek's internal integrator commits them — exactly like a
human operator would adjust the CT signal manually, but at 200 ms
cadence and across many batteries at once.

The previous architecture (multiplex with one active battery per
circuit) is gone. The new one needs a **dedicated Shelly Plug PM Gen3
per battery** and exploits two empirically measured Marstek
properties: integrator without decay, and reaction to as few as 2
identical CT samples within ~1.5 s.

## Why pulses

Sending a continuous "fake" CT value never worked precisely — Marstek
reacts to the integral, not the instant value, and the response gain
is firmware-dependent. With a Shelly Plug per battery we can:

1. measure exactly what each battery is currently doing,
2. compute the desired delta to its target,
3. encode that delta as a short burst of N CT samples, and
4. confirm via the next plug reading that the burst landed.

This gives us multi-battery-per-circuit (the multiplex layer is gone),
sub-2-second reaction times, no SoC-direction heuristics, and a hard
safety guarantee: the circuit cap is enforced from live plug
measurements, never from "what we think we asked for".

## Features

- **Virtual Shelly Pro 3EM** on UDP-RPC (port 1010), HTTP-REST and
  HTTP-RPC. Wire-compatible with Marstek Venus E and other batteries
  that integrate via Shelly Pro 3EM emulation.
- **Discoverable via mDNS** as a real Pro 3EM (`_shelly._tcp.local`,
  `_http._tcp.local`, with the right TXT records).
- **Per-battery virtual integrator** + delta pulse queue. Hardware-
  clamped to each battery's `max_charge_w` / `max_discharge_w`.
- **Plug-driven circuit cap** — the sum of plug readings on a circuit
  must stay below `cap_w * circuit_headroom`. Violations scale down
  the affected circuit's targets.
- **Saturation detection** — when the plug stays below commanded for
  more than `saturation_window_s`, the battery is parked at the
  observed ceiling and the unmet watts redispatch to siblings. The
  battery un-saturates automatically when it can keep up again.
- **Group failure mode** — if any plug in a circuit goes silent for
  more than `plug_stale_s`, the whole circuit is muted (CT silent for
  `group_silent_after_stale_s`), forcing every Marstek's watchdog to
  clear its integrator. Resumes once plugs are healthy again.
- **Capacity-weighted distribution** — `priority_weight × directional
  hardware cap` per battery. Falls back gracefully when a battery is
  full / empty / saturated by handing the slack to siblings (grid
  balance > SoC balance).
- **Marstek SoC poller** — used only for the `soc_full_pct` /
  `soc_empty_pct` eligibility gate. Power telemetry is no longer read
  from the Marstek (the plug is faster and authoritative).
- **HA integration** — optional SoC source per battery via the HA Core
  REST API.
- **Standalone calibration tool** — `marstek_calibrate.exe` (Windows
  binary) for empirically characterising a Marstek's pulse response.
  Announces itself via mDNS so the Marstek auto-discovers it. See
  `src/bin/marstek_calibrate.rs`.
- **Cross-platform** — Linux, Windows, macOS.

## Architecture

```
                  ┌──────────────────────────────────────────────────┐
                  │  ShellyMultiplexer (pulse mode)                  │
                  │                                                  │
   Real Shelly    │   Real-Shelly poller (UDP-RPC, ~4 Hz)            │
   ──────────    │            │                                     │
   :2020 (RPC) ◀─┤            ▼                                     │
                  │   Dispatcher loop (200 ms)                       │
                  │   • compute desired_w per battery                │
                  │   • update commanded_w (+= delta)                │
                  │   • queue pulse_count pulses of value=delta      │
                  │            │                                     │
                  │            ▼                                     │
   Marsteks    ──▶│   Virtual Shelly UDP server                      │
   poll :1010    │   • route by source IP -> battery                │
                  │   • drain one pulse per poll                     │
                  │   • drop response if circuit muted               │
                  │                                                  │
   Plugs (one    │   Plug HTTP poller (200 ms per plug)             │
   per battery) ─▶│   • Switch.GetStatus -> last_plug_w             │
   GET /rpc      │   • stale > plug_stale_s -> mute circuit         │
                  │                                                  │
   Browser   ───▶│   Status UI (:8080) — read-only                  │
                  └──────────────────────────────────────────────────┘
```

> ⚠ **Required configuration on the real Shelly:** move its UDP-RPC port
> off the default 1010 (e.g. to 2020). Otherwise the multiplexer can't
> bind 1010 and the batteries will reach the real Shelly directly.

## Building

Requires Rust 1.87+ (edition 2024).

```bash
cargo build --release
# binary: target/release/shelly-multiplexer  (or .exe on Windows)
```

The web UI is embedded into the binary at compile time — no extra files
to deploy.

## Running

1. Copy the example config (or let the HA add-on write a template on
   first start):

   ```bash
   cp config.example.toml config.toml
   ```

2. Edit `config.toml`:
   - real Shelly IP and the *moved* UDP port,
   - one or more `[[circuits]]` entries (`fuse_amps`, `voltage`, `phases`),
   - one `[[battery]]` per battery with `address` (the Marstek's
     static IP), `circuit`, `plug_url` (mandatory), and the hardware
     caps.

3. Start it:

   ```bash
   ./shelly-multiplexer --config config.toml
   ```

4. Open the status UI: <http://localhost:8080>

5. In each battery, point its "Shelly Pro 3EM" target at the
   multiplexer's IP (instead of the real Shelly's IP). For Marstek
   devices, enable the Open API in the Marstek app.

### Privileged ports

UDP 1010 and HTTP 80 are below 1024 and need elevated rights on Linux:

```bash
sudo setcap 'cap_net_bind_service=+ep' target/release/shelly-multiplexer
```

On Windows, run as Administrator or change the ports in `config.toml`.

## Dispatcher tuning knobs

| Field | Default | Purpose |
|---|---|---|
| `cycle_ms` | 200 | how often the dispatcher recomputes desired_w |
| `deadband_w` | 30 | minimum delta before a new pulse queues |
| `hit_tolerance_w` | 15 | `\|commanded - plug\|` ≤ this counts as "pulse landed" |
| `pulse_count` | 3 | pulses per delta change (Marstek needs ≥ 2) |
| `soc_full_pct` | 95 | skip charging at or above this SoC |
| `soc_empty_pct` | 5 | skip discharging at or below this SoC |
| `plug_stale_s` | 2.0 | plug silent this long → mute circuit |
| `group_silent_after_stale_s` | 60.0 | how long the muted circuit stays muted after recovery |
| `circuit_headroom` | 0.95 | use only this fraction of fuse cap |
| `saturation_gap_w` | 100 | `\|commanded - plug\|` > this triggers saturation tracking |
| `saturation_window_s` | 8.0 | gap must persist this long to mark saturated |

## Migrating from v0.1.x

The schema changed completely. After updating, the dispatcher refuses
to start with `battery X: plug_url is required`. Either delete
`config.toml` (the HA add-on writes a v0.2.0 template on next start)
or manually adapt: drop the `[safety]` section, drop per-battery
`min_soc_percent` / `max_soc_percent` / `phase` / `priority`, add
`plug_url` to every battery and renew `[dispatcher]`.

## Calibration tool

`marstek_calibrate.exe` (under `src/bin/marstek_calibrate.rs`) is a
standalone Windows binary for empirically characterising a Marstek's
pulse response. Build with:

```bash
cargo build --release --bin marstek_calibrate
```

Run it on a PC on the same LAN as the Marstek; it announces itself as
a Shelly Pro 3EM via mDNS so the Marstek auto-discovers it. Send
manual pulses (`set -100 3` etc.) and observe the resulting charge
change in the Marstek app. The empirically measured numbers used by
the production dispatcher are documented in `memory/marstek_empirical.md`.

## License

Apache-2.0. The Shelly RPC wire format is reverse-engineered from
publicly available Shelly firmware behaviour and the Apache-2.0-licensed
[uni-meter](https://github.com/sdeigm/uni-meter) project.
