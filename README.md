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
- **Direct Modbus dispatch (v0.7 default)** — instead of emulating a
  Shelly Pro 3EM and steering each Marstek via per-poll CT deltas,
  the dispatcher writes absolute power setpoints directly via Modbus
  (`force_mode` register 42010 + `set_charge_power` 42020 /
  `set_discharge_power` 42021). No more delta math, no settle
  guesswork — we tell each battery exactly what to do, and the plug
  feedback confirms it landed. Pulse mode is still available as
  `dispatcher.mode = "pulse"` for installs without per-battery Modbus
  access (or for non-Marstek inverters).
- **Sequential per-circuit dispatch** — within a circuit we issue at
  most ONE new setpoint per cycle, and only to a battery whose
  previous write has settled (plug confirms the new power). This
  gives the structural property that **a circuit can't go over cap**
  even when commands are batched or BMS taper kicks in: the plug
  measurement is always up-to-date before the next decision fires.
- **Marstek SoC via Modbus TCP** — the multiplexer reads SoC from the
  Modbus register matched by `marstek_model`. Only **Venus E V3 with
  Ethernet** speaks Modbus TCP natively; every other Marstek variant
  (Venus A, D, E V1, V2, V1.2, E 2.0) needs an external RS485-to-LAN
  bridge (Waveshare RS485-to-RJ45, Elfin EW11, PUSR DR134, M5Stack
  Atom S3 + RS485, …) wired to the battery's RS485 port. `modbus_host`
  on each battery points at that bridge. Power telemetry still comes
  from the plug, not the battery itself.
- **BMS cutoffs as SoC gate** — at startup we read each Marstek's
  configured `charging_cutoff_capacity` (register 44000) and
  `discharging_cutoff_capacity` (44001) and use those as the
  effective full / empty thresholds. The user's BMS setting is the
  authoritative truth, far better than the dispatcher's TOML default.
- **Emergency plug cutoff (safety)** — if a circuit's measured plug
  sum exceeds `cap × headroom + emergency_cutoff_margin_w` for
  `emergency_cutoff_grace_s` seconds, the worst-offending battery's
  Shelly Plug PM Gen3 relay is opened (`Switch.Set`, `on = false`).
  Auto-recovery after `emergency_cutoff_recovery_s`; manual reset
  via the admin UI's "reset" button on the offending row.
- **Night cutoff (efficiency)** — between sunset and sunrise, empty
  batteries can have their plugs opened to skip the Marstek's
  inverter standby loss (~5-15 W per unit, ~60-180 Wh per winter
  night). Requires `[location].latitude` + `longitude` and
  `dispatcher.night_cutoff_enabled = true`. Restored automatically
  at sunrise.
- **Failsafe shutdown** — Marstek firmware has no Modbus watchdog,
  so the multiplexer carries the responsibility. SIGTERM / Ctrl-C
  handlers AND a Rust `panic_hook` write `force_mode = 0` plus
  `rs485_control = off` to every battery before exit, so the
  Marsteks fall back to their auto behaviour.
- **Home Assistant SoC mode** — alternative SoC source. When
  `[home_assistant].enabled = true`, every battery's SoC is read from
  its `soc_entity_id` via the HA Core REST API and the Modbus poller
  stays idle. HA mode and Modbus mode are mutually exclusive.
- **Admin UI on `:8080`** — live status (per-battery plug power, SoC,
  pulse queue, taper / at-limit / silent state) **and** full live
  configuration (real Shelly host / port, circuits, batteries,
  dispatcher tuning, HA mode). The TOML on disk is just the bootstrap
  seed; every setting can be edited at runtime without a restart.
- **Cross-platform** — Linux (x86_64, aarch64, armv7, armv6), Windows
  (x86_64), macOS (build from source).

## Installation

Three supported paths:

- **Home Assistant add-on** — install via the HA Add-on Store from
  the repository URL. The supervisor pulls prebuilt OCI images per
  architecture; see [addon/README.md](addon/README.md).
- **Native Linux (Debian / Ubuntu / Raspberry Pi OS)** — one-line
  interactive installer:
  ```bash
  curl -fsSL https://raw.githubusercontent.com/hilman2/shelly-multiplexer/main/scripts/install.sh | sudo bash
  ```
  Detects your architecture (x86_64, aarch64, armv7, armv6), pulls the
  latest release, asks for the basics, writes a bootstrap config and
  starts the systemd service. Full walkthrough (and manual steps):
  [docs/INSTALL-LINUX.md](docs/INSTALL-LINUX.md).
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
multiplexer's IP (instead of the real Shelly's IP). For SoC reads you
need either:

- **HA mode** — set `[home_assistant].enabled = true` and a
  `soc_entity_id` per battery; the multiplexer reads SoC from HA.
- **Modbus mode** — point `modbus_host` at the Modbus TCP endpoint of
  each battery. For Venus E V3 with Ethernet that's the battery's own
  IP; for every other variant it's the IP of an external RS485-to-LAN
  bridge (Waveshare / Elfin EW11 / PUSR DR134 / M5Stack Atom S3 +
  RS485) wired to the battery's RS485 port.

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
[virtual_shelly]           ← face we present to the batteries (pulse mode only)
[management]               ← admin UI bind address
[dispatcher]               ← global control-loop tuning + safety thresholds
[home_assistant]           ← SoC source switch (HA mode)
[location]                 ← lat/lon (only used by night cutoff)
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

#### Mode

| Field | Default | Purpose |
|---|---|---|
| `mode` | `modbus` | Dispatch backend. `modbus` writes setpoints directly via Modbus (v0.7+ default — requires `modbus_host` per battery). `pulse` keeps the legacy CT-emulation path (Shelly Pro 3EM virtual server). |

#### Cycle & deadband

| Field | Default | Purpose |
|---|---|---|
| `cycle_ms` | 200 | Dispatcher tick rate. Also the plug poll rate (clamped to ≥ 100 ms). |
| `deadband_w` | 30 | Minimum delta magnitude before a pulse is queued (pulse mode) / before a setpoint is changed (modbus mode). Both a noise filter and Marstek-quantisation buffer. |
| `pulse_count` | 3 | Number of identical CT samples per delta. Marstek commits a value after 2 polls; 3 is a safety margin. (Pulse mode only.) |
| `grid_bias_w` | 30 | Asymmetric grid setpoint. The dispatcher leaves this margin on the import side when discharging and on the export side when charging — never tries to hit grid_w = 0 exactly. Set to 0 for symmetric dispatching. |

#### Modbus dispatch tuning (mode = "modbus")

| Field | Default | Purpose |
|---|---|---|
| `setpoint_deadband_w` | 20 | Skip the Modbus write when the new setpoint is within this many watts of the last successfully written value. Reduces Modbus traffic over slow RS485-to-LAN bridges. |
| `modbus_heartbeat_s` | 5 | Re-write the current setpoint at least this often even when unchanged — serves as both a "recovery from dropped writes" mechanism AND a process-level liveness signal (Marstek firmware has no watchdog). |
| `modbus_watchdog_grace_s` | 30 | If the dispatcher loop hasn't ticked for this long, the watchdog task force-writes `force_mode = 0` to every battery and exits. Catches hangs the SIGTERM handler can't see. 0 disables the watchdog. |

#### Emergency plug cutoff

Last line of defence: physically opens the Shelly Plug PM Gen3 relay
when soft control fails and a circuit drifts over its fuse cap.

| Field | Default | Purpose |
|---|---|---|
| `emergency_cutoff_margin_w` | 200 | Trigger threshold: cap × headroom + this margin. 0 disables the feature. |
| `emergency_cutoff_grace_s` | 5 | The over-cap condition has to persist this long before the relay opens (lets startup transients pass). |
| `emergency_cutoff_recovery_s` | 600 | Auto-reset after this many seconds. Manual reset via admin UI is also available. |

#### Night cutoff

Disconnects empty batteries between sunset and sunrise to skip the
Marstek's ~5-15 W inverter standby loss. Requires `[location]`.

| Field | Default | Purpose |
|---|---|---|
| `night_cutoff_enabled` | `false` | Master switch. Validation requires `[location].latitude` + `longitude` when set. |
| `night_cutoff_soc_margin_pct` | 2.0 | Hysteresis margin: a battery has to be within this % of the effective empty cutoff to be cut, and rise more than this % above to be restored. |

### `[location]` — geographic location for sun-based features

| Field | Default | Purpose |
|---|---|---|
| `latitude` | — | Decimal degrees (-90 to 90). Required when `night_cutoff_enabled = true`. |
| `longitude` | — | Decimal degrees (-180 to 180). Required when `night_cutoff_enabled = true`. |

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

#### Empirical full/empty detection (no-SoC mode)

For installs without a SoC source (no Modbus bridge, no HA sensor) the
dispatcher infers "full" / "empty" from refused pulses: if a
significant directional pulse goes out and the plug doesn't move
within `settle_timeout_s`, that direction is locked for the lockout
duration. The opposite direction stays free — a battery that won't
take more charge (full) is still used for discharge, and vice versa.
After the lockout expires the direction is retried; a successful
pulse anywhere clears the matching lockout early.

This acts as a backstop even when SoC IS available: if the SoC reading
is wrong or stale, the empirical layer still catches refusals.

| Field | Default | Purpose |
|---|---|---|
| `soc_unknown_lockout_s` | 600 | How long a direction is locked after a refusal (seconds). |

### `[home_assistant]` — SoC source switch

The dispatcher reads battery SoC from exactly one source. This block is
the global switch:

- `enabled = false` (default) — SoC is polled via Modbus TCP. Each
  battery typically needs a `modbus_host` (plus `marstek_model`,
  `modbus_port`, `modbus_unit_id`).
- `enabled = true` — SoC is polled from HA. Each battery typically
  needs a `soc_entity_id`.

The two modes are mutually exclusive; the previous "Local API" path
(direct UDP JSON-RPC on port 30000) was removed in v0.5.0.

**No SoC source? Still works.** Since v0.6, a battery without ANY
SoC source still participates. The dispatcher derives "full" /
"empty" empirically by watching whether each Marstek honours its
directional pulses — see [Empirical full/empty detection](#empirical-fullempty-detection-no-soc-mode)
under `[dispatcher]`. The admin UI flags such batteries with a "no
SoC" pill so you can tell at a glance which ones run in
empirical-only mode.

**Upgrade behaviour from v0.4.x:** old configs that still carry the
retired `vendor` and `marstek_port` fields load unchanged — Serde
ignores the unknown fields. Affected batteries participate in
dispatch via empirical detection until you wire up a SoC source.

| Field | Default | Purpose |
|---|---|---|
| `enabled` | `false` | SoC mode switch (see above). |
| `url` | `http://homeassistant.local:8123/api` | HA Core REST base. |
| `token` | — | Long-lived access token. Required when `enabled = true`. |
| `timeout_ms` | 3000 | Per-request timeout. |

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
| `marstek_model` | `venus_e` | Picks the Modbus SoC register: `venus_e` (reg 34002, fits Venus E v1/v2/v3) or `venus_e_v12` (reg 32104, Venus E v1.2). Register map sourced from the [ViperRNMC marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus) project. |
| `modbus_host` | — | Modbus TCP host. Needed in Modbus mode (`[home_assistant].enabled = false`); ignored in HA mode. Usually the IP of an RS485-to-LAN bridge — only Venus E V3 with Ethernet speaks Modbus TCP natively, in which case set this to the same value as `address`. Every other variant (Venus A, D, E V1, V2, V1.2, E 2.0) needs an external bridge such as Waveshare RS485-to-RJ45, Elfin EW11, PUSR DR134 or M5Stack Atom S3 + RS485. **Until this is set, the battery stays inactive** — the dispatcher skips it entirely. Not auto-derived from `address` on purpose: silently polling the wrong IP for SoC is a worse failure mode than asking for an explicit setting. |
| `modbus_port` | 502 | Modbus TCP port (most bridges default to 502; some use 8899 or 4196 — check the bridge's web UI). |
| `modbus_unit_id` | 1 | Modbus unit / slave ID. |
| `soc_interval_ms` | 30000 | How often we poll the battery's SoC. |
| `soc_entity_id` | — | HA entity ID. **Required** when `[home_assistant].enabled = true`; ignored otherwise. |
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

## License

Apache-2.0. The Shelly RPC wire format is reverse-engineered from
publicly available Shelly firmware behaviour and the Apache-2.0-licensed
[uni-meter](https://github.com/sdeigm/uni-meter) project.
