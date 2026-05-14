# ShellyMultiplexer

Coordinates several battery storage systems on the same grid so they
work together instead of fighting. The multiplexer reads grid power
from a Shelly Pro 3EM and the actual output of each battery from its
own Shelly Plug PM Gen3, then writes per-battery power setpoints over
Modbus TCP. One Marstek charges, one discharges, both inside the same
fuse cap — handled.

```
                  ┌────────────────────────────────────────────────┐
                  │  ShellyMultiplexer                              │
                  │                                                │
   Real Shelly    │   Grid poller (UDP-RPC, ~4 Hz)                 │
   ──────────    │            │                                   │
   :2020 (RPC) ◀─┤            ▼                                   │
                  │   Dispatcher tick (200 ms)                     │
                  │   • desired_total = Σ plug + grid_correction   │
                  │   • split target across batteries weighted by  │
                  │     priority × capacity × soc_room             │
                  │   • clamp each target to BMS-derived bounds    │
                  │   • per-circuit cap on the sum of setpoints    │
                  │            │                                   │
                  │            ▼                                   │
                  │   Per-battery Modbus writer                    │
                  │   • 42000=21930 (RS485 control)                │
                  │   • zero opposite direction, sleep             │
                  │   • 42010 = 1/2 (charge/discharge), sleep      │
                  │   • 42020 / 42021 = power_w                    │
   Marstek    ◀───┤            ▲                                   │
   Modbus :502   │            │                                   │
                  │   Sequential per circuit — one write per cycle │
                  │   per circuit, after the previous battery's    │
                  │   plug has settled.                            │
                  │                                                │
   Plug PM    ───▶│   Plug HTTP poller (per-battery, 200 ms)      │
   Gen3          │   • Switch.GetStatus → last_plug_w             │
                  │   • emergency cutoff via Switch.Set on overload│
                  │                                                │
   Browser   ───▶│   Admin UI (:8080) — live status + config      │
                  └────────────────────────────────────────────────┘
```

## Hardware required per install

- **One real Shelly Pro 3EM** measuring grid power.
- **One Shelly Plug PM Gen3 per battery**, between the battery's AC
  output and the wall. The plug is authoritative for circuit-cap math
  and serves as the emergency relay.
- **Modbus reach to each Marstek.** Venus E v3 speaks Modbus TCP
  natively over its Ethernet port. Every other variant (Venus A, D,
  E v1, E v2) requires an external RS485-to-LAN bridge wired to the
  battery's RS485 port — e.g. Waveshare RS485-to-RJ45, Elfin EW11,
  PUSR DR134, M5Stack Atom S3 + RS485.

If you can't put each battery on Modbus, the multiplexer also supports
a pulse-mode fallback (Shelly Pro 3EM emulation with CT delta pulses)
— see [`dispatcher.mode`](#mode).

## Features

- **Direct power setpoints.** The dispatcher computes one target wattage
  per battery and writes it via Modbus (`force_mode` register 42010,
  `set_charge_power` 42020, `set_discharge_power` 42021). Re-arms
  `rs485_control_mode` (42000 = 21930) before every write, sequences
  writes with the same sleeps the upstream community proved necessary
  (100 ms after RS485 enable, 200 ms after zeroing the opposite
  direction, 500 ms after `force_mode`).
- **Sequential per-circuit dispatch.** Within a circuit only ONE
  battery's setpoint changes per cycle, and only after the previous
  write has settled (the plug confirms the new power). Structural
  consequence: a circuit cannot go over its fuse cap, because every
  decision uses fresh plug readings rather than predictions about
  commands that haven't materialised yet.
- **SoC-balanced distribution.** Charge gets weighted toward the
  emptier batteries, discharge toward the fuller ones; SoCs converge
  over time and cells age evenly.
- **BMS-derived SoC gates.** At startup the multiplexer reads each
  Marstek's user-configured charging / discharging cutoff (registers
  44000 / 44001 — Venus E v1/v2 exposes them) and uses those as the
  effective full / empty thresholds. The user's BMS setting beats the
  multiplexer's TOML default.
- **Hardware emergency cutoff.** If a circuit's measured plug sum
  exceeds `cap × headroom + emergency_cutoff_margin_w` for
  `emergency_cutoff_grace_s` seconds, the worst-offending battery's
  Shelly Plug relay is physically opened. Auto-recovery; manual reset
  via the admin UI's "reset" button on the offending row.
- **Night cutoff.** Optional efficiency feature: between sunset and
  sunrise, batteries at the empty cutoff have their plugs opened to
  skip the Marstek inverter's ~5-15 W standby draw. Restored
  automatically at sunrise. Requires `[location].latitude` +
  `longitude`.
- **Failsafe shutdown.** Marstek firmware has no Modbus watchdog —
  if the controller dies the battery stays on the last setpoint
  forever. SIGTERM / SIGINT / Ctrl-C handlers AND a Rust `panic_hook`
  write `force_mode = 0` plus `rs485_control = off` to every battery
  before exit. An in-process watchdog catches main-loop hangs and
  force-resets if the dispatcher hasn't ticked for
  `modbus_watchdog_grace_s` seconds.
- **Pulse-mode fallback.** Inverters reachable only via the Shelly
  Pro 3EM CT-emulation protocol can still be steered via per-poll
  delta pulses — see `dispatcher.mode = "pulse"`. Includes an
  empirical full/empty detection layer (locked direction on refusal).
- **Home Assistant SoC bridge** (pulse mode only). Read each battery's
  SoC from an HA entity via the HA Core REST API. In Modbus mode SoC
  comes directly from the inverter's Modbus register.
- **Admin UI on `:8080`.** Live per-battery status, every config knob
  editable at runtime. The TOML on disk is just the bootstrap seed.
- **Cross-platform.** Linux (x86_64, aarch64, armv7, armv6), Windows
  (x86_64), macOS (build from source). Distributed as a Home Assistant
  add-on, prebuilt binaries, or `cargo build`.

## Installation

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

In the Marstek app for each battery, enable Modbus control. The
multiplexer talks to each Marstek individually (no shared Modbus
gateway needed across batteries — each can have its own RS485 bridge).

## Configuration

`config.toml` is the bootstrap seed — the multiplexer reads it once at
startup. The admin UI is the source of truth at runtime: every field
below can be edited live without an add-on restart.

All sections:

```
[real_shelly]              ← grid measurement source
[virtual_shelly]           ← only used in pulse mode
[management]               ← admin UI bind address
[dispatcher]               ← control loop + all safety thresholds
[home_assistant]           ← pulse-mode SoC bridge (optional)
[location]                 ← lat/lon for night cutoff
[[circuits]] ...           ← shared fuses
[[batteries]] ...          ← one entry per Marstek + its plug
```

### `[real_shelly]` — grid measurement source

| Field | Default | Purpose |
|---|---|---|
| `host` | — | IP of the real Shelly Pro 3EM. **Required.** |
| `udp_port` | — | The Shelly's UDP-RPC port. Move it off the default 1010 in the Shelly UI so the multiplexer can bind 1010 in pulse mode. **Required.** |
| `poll_interval_ms` | 250 | How often we poll the real Shelly for grid_w. |
| `request_timeout_ms` | 1000 | Per-poll timeout. |

### `[virtual_shelly]` — face presented to the batteries in pulse mode

Ignored when `dispatcher.mode = "modbus"` (the virtual Shelly UDP server
and mDNS advertisement are disabled in modbus mode).

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

### `[dispatcher]` — control loop

#### Mode

| Field | Default | Purpose |
|---|---|---|
| `mode` | `modbus` | `modbus` writes power setpoints directly via Modbus (requires `modbus_host` per battery). `pulse` emulates a Shelly Pro 3EM and steers via CT delta pulses — for inverters without Modbus access. |

#### Cycle & deadband

| Field | Default | Purpose |
|---|---|---|
| `cycle_ms` | 200 | Dispatcher tick rate. Also the plug poll rate (clamped to ≥ 100 ms). |
| `deadband_w` | 30 | Minimum delta magnitude that triggers action. Noise filter and Marstek-quantisation buffer. |
| `grid_bias_w` | 30 | Asymmetric grid target. Leave this margin on the import side when discharging and on the export side when charging — never tries to hit grid_w = 0 exactly. Set to 0 for symmetric dispatching. |

#### Modbus dispatch tuning

| Field | Default | Purpose |
|---|---|---|
| `setpoint_deadband_w` | 20 | Skip the Modbus write when the new setpoint is within this many watts of the last successfully written value. Reduces traffic over slow RS485-to-LAN bridges. |
| `modbus_heartbeat_s` | 5 | Re-write the current setpoint at least this often even when unchanged. Recovers from dropped writes AND acts as a process-liveness signal — Marstek firmware has no watchdog. |
| `modbus_watchdog_grace_s` | 30 | If the dispatcher loop hasn't ticked for this long, the in-process watchdog force-writes `force_mode = 0` to every battery and exits. Catches hangs the SIGTERM handler can't see. 0 disables the watchdog. |

#### Emergency plug cutoff

Last line of defence: physically opens the Shelly Plug PM Gen3 relay
when soft control fails and a circuit drifts over its fuse cap.

| Field | Default | Purpose |
|---|---|---|
| `emergency_cutoff_margin_w` | 200 | Trigger threshold: `cap × headroom + this`. 0 disables the feature. |
| `emergency_cutoff_grace_s` | 5 | The over-cap condition has to persist this long before the relay opens (lets startup transients pass). |
| `emergency_cutoff_recovery_s` | 600 | Auto-reset after this many seconds. Manual reset via admin UI is also available. |

#### Night cutoff

Disconnects empty batteries between sunset and sunrise to skip the
Marstek's ~5-15 W inverter standby loss. Requires `[location]`.

| Field | Default | Purpose |
|---|---|---|
| `night_cutoff_enabled` | `false` | Master switch. Validation requires `[location].latitude` + `longitude` when set. |
| `night_cutoff_soc_margin_pct` | 2.0 | Hysteresis margin: a battery has to be within this % of the effective empty cutoff to be cut, and rise more than this % above to be restored. |

#### SoC gates

| Field | Default | Purpose |
|---|---|---|
| `soc_full_pct` | 95 | Skip CHARGING the battery at or above this SoC. BMS-derived value (Modbus reg 44000) takes precedence when available. |
| `soc_empty_pct` | 5 | Skip DISCHARGING the battery at or below this SoC. BMS-derived value (reg 44001) takes precedence. |

Per-battery overrides also available — see `[[batteries]]`.

#### Freshness & circuit cap

| Field | Default | Purpose |
|---|---|---|
| `plug_stale_s` | 2.0 | Plug silent this long → mute its circuit. |
| `grid_stale_s` | 5.0 | Real Shelly silent this long → mute every circuit. |
| `group_silent_after_stale_s` | 60.0 | After recovery, drop UDP responses for this long so every Marstek's watchdog clears its integrator (pulse mode). Must be ≥ Marstek watchdog timeout (~60 s). |
| `circuit_headroom` | 0.95 | Use only this fraction of the calculated fuse cap (jitter buffer). |
| `settle_timeout_s` | 5.0 | Hard escape hatch: accept the dispatch cycle as done after this long. In modbus mode this gates the next per-circuit write; in pulse mode the next pulse queue. |

#### Pulse-mode tuning

Used when `dispatcher.mode = "pulse"`.

| Field | Default | Purpose |
|---|---|---|
| `pulse_count` | 3 | Number of identical CT samples per delta. Marstek commits a value after 2 polls; 3 is a safety margin. |
| `plug_stable_w` | 10 | Plug-reading delta (W) below which two consecutive samples count as "no movement". |
| `plug_stable_duration_s` | 1.5 | The plug must stay within `plug_stable_w` for this long before the previous pulse is considered done. |
| `soc_unknown_lockout_s` | 600 | For installs without a SoC source: when a battery refuses a directional pulse, that direction is locked for this many seconds. The opposite direction stays free. |

### `[location]` — geographic location for night cutoff

| Field | Default | Purpose |
|---|---|---|
| `latitude` | — | Decimal degrees (-90 to 90). Required when `night_cutoff_enabled = true`. |
| `longitude` | — | Decimal degrees (-180 to 180). Required when `night_cutoff_enabled = true`. |

### `[home_assistant]` — optional SoC source (pulse mode only)

In modbus mode SoC is read directly from each Marstek's Modbus register
and this block is ignored. In pulse mode you can opt into reading SoC
from a Home Assistant entity instead — useful when the Marstek's
Modbus is unreachable but HA already polls it.

| Field | Default | Purpose |
|---|---|---|
| `enabled` | `false` | When true (and dispatcher mode is `pulse`), SoC for each battery is read from its `soc_entity_id`. |
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
| `address` | — | Static IP the Marstek uses on its WiFi interface (used for pulse-mode CT routing). Required. |
| `circuit` | — | Which `[[circuits]]` entry this battery sits on. Required. |
| `plug_url` | — | HTTP base URL of the dedicated Shelly Plug PM Gen3 measuring this battery. **Mandatory** — the plug is authoritative for the cap math and the emergency cutoff relay. Required. |
| `max_charge_w` | — | Hardware charge cap. Required. |
| `max_discharge_w` | — | Hardware discharge cap. Required. |
| `capacity_wh` | (auto) | Usable capacity, used as a distribution weight. Defaults to `max_charge_w + max_discharge_w` if unset. |
| `priority_weight` | 1.0 | Manual multiplier on top of capacity (bigger = more share of work). |
| `marstek_model` | `venus_e_v1_v2` | Picks the Modbus register map per the [ViperRNMC marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus) project. Allowed: `venus_a`, `venus_d`, `venus_e_v1_v2`, `venus_e_v3`. Each variant exposes SoC + battery_power at different addresses. |
| `modbus_host` | — | Modbus TCP host. For Venus E V3 with Ethernet, set this to the same value as `address`. For every other variant (A, D, E V1, V2) point it at the RS485-to-LAN bridge's IP. Required in modbus mode. |
| `modbus_port` | 502 | Modbus TCP port. Most bridges default to 502; some use 8899 or 4196 — check the bridge's web UI. |
| `modbus_unit_id` | 1 | Modbus unit / slave ID. |
| `soc_interval_ms` | 30000 | How often we poll the battery's SoC (and `battery_power` in modbus mode). |
| `soc_entity_id` | — | HA entity ID. Used only in pulse mode + HA enabled. |
| `soc_full_pct` | (inherit) | Per-battery override of `[dispatcher].soc_full_pct`. BMS-derived value still takes top precedence. |
| `soc_empty_pct` | (inherit) | Per-battery override of `[dispatcher].soc_empty_pct`. |
| `charge_taper_soc_pct` | — | Above this SoC, cap effective charge at `charge_taper_w`. Models BMS charge tapering near full. |
| `charge_taper_w` | — | Effective max charge power once `charge_taper_soc_pct` is exceeded. |
| `discharge_taper_soc_pct` | — | Below this SoC, cap effective discharge at `discharge_taper_w`. Models battery sag near empty. |
| `discharge_taper_w` | — | Effective max discharge power below `discharge_taper_soc_pct`. |

Example taper: a 5 kWh Marstek that the BMS limits to 1000 W above
90 % SoC → `charge_taper_soc_pct = 90`, `charge_taper_w = 1000`.

## Marstek register map per variant

Sourced from the upstream [ViperRNMC/marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus)
integration. The Marstek HA add-on's connection dialog uses the same
four-way split.

| Variant       | SoC reg | SoC scale | battery_power reg | bp dtype | BMS cutoffs (44000/44001) |
|---------------|---------|-----------|--------------------|----------|----------------------------|
| Venus A       | 32104   | 1         | 30001              | int16    | not exposed                |
| Venus D       | 32104   | 1         | 30001              | int16    | not exposed                |
| Venus E v1/v2 | 32104   | 1         | 32102              | int32    | exposed                    |
| Venus E v3    | 34002   | 0.1       | 30001              | int16    | not exposed                |

Control registers are identical across all four:

| Register | Function | Values |
|----------|----------|--------|
| 42000    | RS485 control mode | 21930 = on, 21947 = off |
| 42010    | Force mode | 0 = standby, 1 = charge, 2 = discharge |
| 42020    | Charge power setpoint | 0–2500 W |
| 42021    | Discharge power setpoint | 0–2500 W |

## Distribution algorithm

Per dispatcher cycle:

1. `grid_correction = grid_w - grid_bias_w` (asymmetric, with a deadband).
2. `desired_total = Σ plug_w + grid_correction`. Negative → net charge,
   positive → net discharge.
3. Each battery gets a share of `desired_total` weighted by
   `priority_weight × capacity_wh × soc_room`, where `soc_room` is
   `(soc_full − soc)` when charging and `(soc − soc_empty)` when
   discharging. Emptier batteries get more charge, fuller batteries
   get more discharge. Direction-locked batteries get weight 0.
4. Each share is clamped to the battery's SoC-bounded
   `[low_bound, high_bound]` window (hardware caps plus optional
   tapers). Any clipped excess is redistributed to the remaining
   batteries up to six times.
5. Per-circuit cap: the sum of targets is scaled so that
   `Σ |target| ≤ fuse_cap × circuit_headroom`.
6. In **modbus mode**: per circuit, the battery with the largest
   target-vs-current-setpoint delta whose previous write has settled
   gets one Modbus write this cycle. Other batteries on the same
   circuit wait their turn. Result: the post-write circuit sum is
   guaranteed bounded by the cap.
7. In **pulse mode**: each surviving delta is queued as `pulse_count`
   identical CT samples for the next Marstek polls. The dispatcher
   waits for plug stability before queueing the next delta to the
   same battery.

Properties this gives:
- The primary goal — meeting the grid setpoint — is pursued within
  physical limits.
- Batteries on the same circuit never end up in opposite directions.
- SoCs converge over time.
- Circuit caps cannot be exceeded by control errors.

## License

Apache-2.0. The Shelly RPC wire format is reverse-engineered from
publicly available Shelly firmware behaviour and the Apache-2.0
[uni-meter](https://github.com/sdeigm/uni-meter) project. Marstek
register map adopted from the Apache-2.0
[ViperRNMC/marstek_venus_modbus](https://github.com/ViperRNMC/marstek_venus_modbus)
integration.
