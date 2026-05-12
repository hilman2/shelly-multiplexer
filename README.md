# ShellyMultiplexer

Steers multiple battery storage systems (Marstek Venus E and other
batteries that integrate via Shelly Pro 3EM emulation) by impersonating
a Shelly Pro 3EM on the LAN. The dispatcher computes one *target*
power setpoint per battery from the live grid reading and each
battery's own plug measurement, then encodes the difference as a short
burst of CT samples that the battery's internal integrator commits.

A **dedicated Shelly Plug PM Gen3 per battery** is mandatory: its
reading is the authoritative ground truth for the dispatcher and for
the per-circuit fuse cap.

## How it works

```
                  ┌────────────────────────────────────────────────────┐
                  │  ShellyMultiplexer                                  │
                  │                                                    │
   Real Shelly    │   Grid poller (UDP-RPC, ~4 Hz)                     │
   ──────────    │            │                                       │
   :2020 (RPC) ◀─┤            ▼                                       │
                  │   Dispatcher tick (200 ms)                         │
                  │   • desired_total = Σ plug + grid_correction       │
                  │   • split target across batteries weighted by      │
                  │     priority × capacity × soc_room                 │
                  │   • clamp each target to [low_bound, high_bound]   │
                  │   • delta = target - plug; queue pulse_count       │
                  │     copies of delta for the next polls             │
                  │            │                                       │
                  │            ▼                                       │
   Marsteks    ──▶│   Virtual Shelly UDP server (:1010)                │
   poll :1010    │   • route by source IP → battery                   │
                  │   • drain one pulse per poll                       │
                  │   • drop the response while circuit is muted       │
                  │                                                    │
   Plugs (one    │   Plug HTTP poller (per-battery, 200 ms)           │
   per battery) ─▶│   • Switch.GetStatus → last_plug_w                │
   GET /rpc      │   • track last_plug_movement_at (±10 W)             │
                  │   • stale > plug_stale_s → mute the circuit        │
                  │                                                    │
   Browser   ───▶│   Admin UI (:8080) — live status + full live       │
                  │                       config editor                │
                  └────────────────────────────────────────────────────┘
```

The dispatcher gates the next pulse on **two** plug observations:
the plug must have moved by more than `plug_stable_w` (proving the
battery reacted) AND then stayed within that band for at least
`plug_stable_duration_s` (proving the reaction has finished). This
prevents stacking a new delta on top of a still-in-flight one. A
`settle_timeout_s` escape hatch releases the gate if a battery
refuses to react.

> ⚠ **Required configuration on the real Shelly:** move its UDP-RPC port
> off the default 1010 (e.g. to 2020). Otherwise the multiplexer can't
> bind 1010 and the batteries reach the real Shelly directly.

## Features

- **Virtual Shelly Pro 3EM** on UDP-RPC (port 1010), HTTP-REST and
  HTTP-RPC. Wire-compatible with Marstek Venus E and other batteries
  that integrate via Shelly Pro 3EM emulation.
- **Discoverable via mDNS** as a real Pro 3EM (`_shelly._tcp.local`,
  `_http._tcp.local`, correct TXT records).
- **Multiple batteries per circuit.** Targets are split so all eligible
  batteries always run in the same direction; one battery cannot end up
  charging while another discharges on the same fuse.
- **SoC-balanced distribution** — emptier batteries get more charge
  power, fuller ones get more discharge power. All cells age together
  and reach the healthy mid-SoC band roughly in sync.
- **Plug-driven circuit cap** — the signed sum of plug readings on a
  circuit, plus any pending deltas, must stay below
  `fuse_amps × voltage × phases × circuit_headroom`. Violations shrink
  the deltas toward zero; the dispatcher never flips direction to
  "fix" an over-cap state.
- **Stale-measurement safety** — a stale plug mutes its circuit; a
  stale grid measurement mutes every circuit. CT signal goes silent
  for `group_silent_after_stale_s` (≥ 60 s) so every battery's
  watchdog clears its integrator. Resumes once measurements recover.
- **SoC-aware power tapering** — optional per-battery taper knobs
  reduce the effective charge / discharge cap near the SoC edges, so
  the dispatcher doesn't try to push more than the BMS will accept.
- **Marstek SoC poller** — used for the `soc_full_pct` / `soc_empty_pct`
  eligibility gate. Power telemetry comes from the plug, not the
  battery itself.
- **Home Assistant SoC bridge** — optional per-battery SoC source via
  the HA Core REST API (a long-lived token plus the entity ID).
- **Admin UI on `:8080`** — live status (per-battery plug power, SoC,
  pulse queue, taper / at-limit / silent state) **and** full live
  configuration (real Shelly host / port, circuits, batteries,
  dispatcher tuning, HA bridge). The TOML on disk is just the bootstrap
  seed; every setting can be edited at runtime without a restart.
- **Standalone calibration tool** — see [Calibration tool](#calibration-tool).
- **Cross-platform** — Linux (x86_64, aarch64, armv7, armv6), Windows
  (x86_64), macOS (build from source).

## Installation

Three supported paths:

- **Home Assistant add-on** — install via the HA Add-on Store from
  the repository URL. The supervisor pulls prebuilt OCI images per
  architecture; see [addon/README.md](addon/README.md).
- **Native Linux (Debian / Ubuntu / Raspberry Pi OS)** — download the
  prebuilt static binary for your arch from
  [Releases](https://github.com/hilman2/shelly-multiplexer/releases)
  and install as a systemd service. Walkthrough:
  [docs/INSTALL-LINUX.md](docs/INSTALL-LINUX.md). Targets: x86_64,
  aarch64 (Pi 3/4/5, Pi Zero 2), armv7 (Pi 2/3 32-bit), armv6 (Pi Zero / Pi 1).
- **Native Windows** — download the `.exe`, run interactively or as a
  service via NSSM. Walkthrough:
  [docs/INSTALL-WINDOWS.md](docs/INSTALL-WINDOWS.md).

## Building from source

Requires Rust 1.87+ (edition 2024).

```bash
cargo build --release
# binary: target/release/shelly-multiplexer  (or .exe on Windows)
```

The web UI is embedded into the binary at compile time — no extra
files to deploy.

## Running

```bash
# Bootstrap config (only needed for the very first start; afterwards
# everything is editable in the admin UI):
cp config.example.toml config.toml

# Start it:
./shelly-multiplexer --config config.toml

# Admin UI:
#   http://<host>:8080
```

In each battery's app, point its "Shelly Pro 3EM" target at the
multiplexer's IP (instead of the real Shelly's IP). For Marstek
devices, also enable the Open API.

### Privileged ports

UDP 1010 and HTTP 80 are below 1024 and need elevated rights on Linux:

```bash
sudo setcap 'cap_net_bind_service=+ep' target/release/shelly-multiplexer
```

(The systemd unit shipped with the Linux release tarballs already
grants this to the non-root service user.)

On Windows, run as Administrator or change the ports in `config.toml`.

## Configuration

`config.toml` is the bootstrap seed — the multiplexer reads it once at
startup. The admin UI is the source of truth at runtime: every field
below can be edited live without a restart.

All sections in one diagram:

```
[real_shelly]              ← grid measurement source
[virtual_shelly]           ← face we present to the batteries
[management]               ← admin UI bind address
[dispatcher]               ← global control-loop tuning
[home_assistant]           ← optional SoC bridge
[[circuits]] ...           ← shared fuses
[[batteries]] ...          ← one entry per Marstek + its plug
```

### `[real_shelly]` — grid measurement source

The real Shelly Pro 3EM that measures the house's grid power.

| Field | Default | Purpose |
|---|---|---|
| `host` | — | IP of the real Shelly Pro 3EM. **Required.** |
| `udp_port` | — | The Shelly's UDP-RPC port AFTER you move it off 1010 in the Shelly UI. **Required.** |
| `poll_interval_ms` | 250 | How often we poll the real Shelly for grid_w. |
| `request_timeout_ms` | 1000 | Per-poll timeout. |

### `[virtual_shelly]` — face presented to the batteries

| Field | Default | Purpose |
|---|---|---|
| `bind_interface` | `0.0.0.0` | Interface(s) the virtual Shelly listens on. |
| `udp_port` | 1010 | UDP-RPC port. Batteries expect 1010 — do not change unless you know what you're doing. |
| `http_port` | 80 | HTTP-REST / HTTP-RPC port. Privileged; set to ≥ 1024 to drop the `CAP_NET_BIND_SERVICE` requirement. |
| `device_mac` | (auto) | MAC reported in `Shelly.GetDeviceInfo`. Empty = derived from primary NIC. |
| `device_hostname` | (auto) | Hostname reported. Empty = `shellypro3em-<mac>`. |
| `firmware` | `1.4.4` | Firmware string reported. Match what your batteries expect. |
| `enable_mdns` | `true` | Announce ourselves on the LAN as a Pro 3EM. |

### `[management]` — admin UI

| Field | Default | Purpose |
|---|---|---|
| `bind_address` | `0.0.0.0:8080` | Where to listen for the admin UI / management REST API. |

### `[dispatcher]` — control-loop tuning

The defaults reflect empirically measured Marstek Venus E behaviour;
most installs don't need to touch them.

#### Cycle & deadband

| Field | Default | Purpose |
|---|---|---|
| `cycle_ms` | 200 | Dispatcher tick rate. Also the plug poll rate (clamped to ≥ 100 ms). |
| `deadband_w` | 30 | Minimum delta magnitude before a pulse is queued. Both a noise filter and Marstek-quantisation buffer. |
| `pulse_count` | 3 | Number of identical CT samples per delta. Marstek commits a value after 2 polls; 3 is a safety margin. |
| `grid_bias_w` | 30 | Asymmetric grid setpoint. The dispatcher leaves this margin on the import side when discharging and on the export side when charging — never tries to hit grid_w = 0 exactly. Set to 0 for symmetric dispatching. |

#### Pulse-settle gate

After a pulse the dispatcher waits for the plug to actually move and
then stop moving before queueing the next one. This prevents stacking
deltas on top of a still-in-flight reaction.

| Field | Default | Purpose |
|---|---|---|
| `plug_stable_w` | 10 | Plug-reading delta (W) below which two consecutive samples count as "no movement". |
| `plug_stable_duration_s` | 1.5 | The plug must stay within `plug_stable_w` for this long before the previous pulse is considered done. |
| `settle_timeout_s` | 5.0 | Hard escape hatch: accept the cycle as done after this long, even if the battery refused to react. |
| `hit_tolerance_w` | 15 | **Deprecated.** Still loaded from old configs; ignored. |

#### SoC gates

| Field | Default | Purpose |
|---|---|---|
| `soc_full_pct` | 95 | Skip CHARGING the battery at or above this SoC. |
| `soc_empty_pct` | 5 | Skip DISCHARGING the battery at or below this SoC. |

Per-battery overrides are also available (see `[[batteries]]`).

#### Freshness & circuit cap

| Field | Default | Purpose |
|---|---|---|
| `plug_stale_s` | 2.0 | Plug silent this long → mute its circuit. |
| `grid_stale_s` | 5.0 | Real Shelly silent this long → mute every circuit. |
| `group_silent_after_stale_s` | 60.0 | After recovery, drop UDP responses for this long so every Marstek's watchdog clears its integrator. Must be ≥ Marstek watchdog timeout (~60 s). |
| `circuit_headroom` | 0.95 | Use only this fraction of the calculated fuse cap (jitter buffer). |

### `[home_assistant]` — optional SoC bridge

Lets you read each battery's SoC from a Home Assistant entity instead
of (or in addition to) polling the Marstek directly.

| Field | Default | Purpose |
|---|---|---|
| `enabled` | `false` | Master switch. |
| `url` | `http://homeassistant.local:8123/api` | HA Core REST base. |
| `token` | — | Long-lived access token. Required when `enabled = true`. |
| `timeout_ms` | 3000 | Per-request timeout. |

Per-battery `soc_entity_id` (see below) selects which HA entity feeds
that battery's SoC.

### `[[circuits]]` — shared fuses

One entry per shared protective device (typically one MCB). The cap
is `fuse_amps × voltage × phases × circuit_headroom`.

| Field | Default | Purpose |
|---|---|---|
| `id` | — | Symbolic name (referenced from `[[batteries]].circuit`). Required. |
| `fuse_amps` | — | Rated current of the shared MCB. Required. |
| `phases` | 1 | 1 = single-phase, 3 = three-phase. |
| `voltage` | 230 | Nominal phase voltage. |

Example for a B16 single-phase circuit: `fuse_amps = 16, phases = 1,
voltage = 230` → 3680 W cap, or 3496 W with the default 0.95 headroom.

### `[[batteries]]` — one per Marstek + its dedicated plug

| Field | Default | Purpose |
|---|---|---|
| `id` | — | Symbolic name. Required. |
| `address` | — | Static IP the Marstek uses when polling the virtual Shelly. Used to route per-Marstek pulse queues. Required. |
| `circuit` | — | Which `[[circuits]]` entry this battery sits on. Required. |
| `plug_url` | — | HTTP base URL of the dedicated Shelly Plug PM Gen3 measuring this battery. **Mandatory** — the plug is authoritative for the cap math. Required. |
| `max_charge_w` | — | Hardware charge cap. Required. |
| `max_discharge_w` | — | Hardware discharge cap. Required. |
| `capacity_wh` | (auto) | Usable capacity, used as a distribution weight. Defaults to `max_charge_w + max_discharge_w` if unset. |
| `priority_weight` | 1.0 | Manual multiplier on top of capacity (bigger = more share of work). |
| `vendor` | `marstek` | `marstek` / `hoymiles` / `generic`. Controls only the SoC polling method; the pulse path is identical. |
| `marstek_port` | 30000 | UDP port of the Marstek Open API (SoC read). |
| `soc_interval_ms` | 30000 | How often we poll the battery's SoC. |
| `soc_entity_id` | — | If set + `[home_assistant].enabled`, read SoC from this HA entity instead of the Marstek. |
| `soc_full_pct` | (inherit) | Per-battery override of `[dispatcher].soc_full_pct`. |
| `soc_empty_pct` | (inherit) | Per-battery override of `[dispatcher].soc_empty_pct`. |
| `charge_taper_soc_pct` | — | Above this SoC, cap effective charge at `charge_taper_w`. Models BMS charge tapering near full. |
| `charge_taper_w` | — | Effective max charge power once `charge_taper_soc_pct` is exceeded. |
| `discharge_taper_soc_pct` | — | Below this SoC, cap effective discharge at `discharge_taper_w`. Models battery sag near empty. |
| `discharge_taper_w` | — | Effective max discharge power below `discharge_taper_soc_pct`. |

Example taper: a 5 kWh Marstek that the BMS limits to 1000 W above
90 % SoC → `charge_taper_soc_pct = 90`, `charge_taper_w = 1000`. The
dispatcher honours that cap when computing each cycle's target so the
integrator never asks for more than the battery accepts.

## Distribution algorithm

Per cycle, with N eligible batteries on one or more circuits:

1. `grid_correction = grid_w - grid_bias_w` (asymmetric, with a deadband).
2. `desired_total = Σ plug_w + grid_correction`. If `desired_total` is
   negative the system net charges; positive means net discharge.
3. Each battery gets a share of `desired_total` weighted by
   `priority_weight × capacity_wh × soc_room`, where `soc_room` is
   `(soc_full − soc)` when charging and `(soc − soc_empty)` when
   discharging. Emptier batteries get more charge, fuller batteries
   get more discharge.
4. Each share is clamped to the battery's SoC-bounded
   `[low_bound, high_bound]` window (hardware caps plus optional
   tapers). Any clipped excess is redistributed to the remaining
   batteries up to six times.
5. For each battery, `delta = target - plug_w`. Deltas below
   `deadband_w` are dropped.
6. Per-circuit cap check: if the post-pulse plug sum would exceed
   `fuse cap × circuit_headroom`, all of that circuit's deltas are
   scaled toward zero (never sign-flipped) so the sum lands on the cap.
7. Each surviving delta is queued as `pulse_count` identical CT samples
   for the next Marstek polls. The dispatcher waits for plug stability
   before queueing the next delta to the same battery (see
   `plug_stable_*`).

This means: (a) the primary goal — meeting the grid setpoint — is
always pursued within physical limits, (b) batteries on the same
circuit can never end up in opposite directions, and (c) SoCs
naturally converge over time.

## Calibration tool

`marstek_calibrate` (under `src/bin/marstek_calibrate.rs`) is a
standalone binary for empirically characterising a Marstek's pulse
response. Build with:

```bash
cargo build --release --bin marstek_calibrate
```

Run it on a PC on the same LAN as the Marstek; it announces itself as
a Shelly Pro 3EM via mDNS so the Marstek auto-discovers it. Send
manual pulses (e.g. `set -100 3`) and observe the resulting power
change in the Marstek app. The empirically measured numbers backing
the production defaults are documented in
`memory/marstek_empirical.md`.

## License

Apache-2.0. The Shelly RPC wire format is reverse-engineered from
publicly available Shelly firmware behaviour and the Apache-2.0-licensed
[uni-meter](https://github.com/sdeigm/uni-meter) project.
