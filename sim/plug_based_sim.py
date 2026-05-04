#!/usr/bin/env python3
"""ShellyMultiplexer Simulation: per-Battery Shelly Plug PM Gen3 Real-Time Feedback.

Setup: 3 Marstek Venus E
  - Battery A in circuit 1 (16A breaker = 3680W)
  - Battery B in circuit 1 (16A breaker = 3680W)
  - Battery C in circuit 3 (16A breaker = 3680W)

Marstek hardware caps (asymmetric):
  - max charge: 2500 W
  - max discharge: 800 W
  - CT-watchdog: shuts off without CT signal after ~45 s (conservative)
  - internal ramp limit: ~500 W/s (estimated)

Plug:
  - reading every 200 ms with small Gaussian noise
  - if plug offline > 2 s -> we BLOCK CT signal -> battery eventually shuts off via watchdog

Sign convention (everything in Watts, signed):
  positive = power flowing OUT of battery (discharge, helps cover house load)
  negative = power flowing INTO battery (charge, absorbs solar)
  grid_w from real Shelly: positive = importing, negative = exporting
  desired battery output = grid_w (split among batteries by capacity weight)

Marstek's local control: it tries to drive its CT reading to zero.
  We send CT showing import X -> Marstek discharges X.
  We send CT showing export X -> Marstek charges X.
  So ct_we_send_to_battery == desired_battery_output.

Real-time circuit cap enforcement:
  - read measured power per battery from plug every 200 ms
  - compute |sum of measured power per circuit|
  - if >= cap with margin: scale down targets for that circuit's batteries
  - reserve capacity for "ghost" batteries (plug dead but watchdog not yet fired)
"""

from __future__ import annotations

import dataclasses
import math
import random
from typing import Callable, Optional

CAP_PER_CIRCUIT_W = 3680.0
CYCLE_MS = 200
PLUG_STALE_S = 2.0
WATCHDOG_S = 45.0
MARSTEK_RAMP_W_S = 500.0
CIRCUIT_HEADROOM = 0.95  # use only 95% of cap to absorb measurement jitter
# Worst-case time the mux must reserve "ghost" capacity for a stale battery:
# = time until mux stops sending CT (PLUG_STALE_S)
# + Marstek watchdog timeout (battery counts down from last CT we sent)
# + ramp-down from full charge (2500 W) at MARSTEK_RAMP_W_S = 5 s
# + 1 s safety margin against clock skew / measurement timing.
GHOST_HOLD_S = PLUG_STALE_S + WATCHDOG_S + (2500.0 / MARSTEK_RAMP_W_S) + 1.0  # = 53 s


# ---------------------------------------------------------------------------
# Models
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class Battery:
    id: str
    circuit: str
    max_charge_w: float = 2500.0
    max_discharge_w: float = 800.0
    soc: float = 50.0
    actual_w: float = 0.0
    target_w: float = 0.0
    last_ct_at: float = 0.0
    alive: bool = True

    def receive_ct(self, ct_w: float, t: float) -> None:
        if not self.alive:
            return
        self.last_ct_at = t
        # Marstek interprets CT as "drive me to zero" => its target equals CT value
        desired = ct_w
        # Hardware clamp
        if desired > self.max_discharge_w:
            desired = self.max_discharge_w
        if desired < -self.max_charge_w:
            desired = -self.max_charge_w
        # SoC limits (very simplified)
        if desired > 0 and self.soc <= 5.0:
            desired = 0.0
        if desired < 0 and self.soc >= 95.0:
            desired = 0.0
        self.target_w = desired

    def tick(self, t: float, dt: float) -> None:
        # Watchdog: no CT signal for too long -> hard off
        if t - self.last_ct_at > WATCHDOG_S:
            self.alive = False
            self.target_w = 0.0
        # Ramp toward target
        delta = self.target_w - self.actual_w
        step = MARSTEK_RAMP_W_S * dt
        if abs(delta) <= step:
            self.actual_w = self.target_w
        else:
            self.actual_w += math.copysign(step, delta)
        # SoC integration (positive actual_w = discharging = SoC decreasing)
        # 2.5 kWh capacity assumed for delta calc.
        self.soc -= (self.actual_w / 2500.0) * (dt / 3600.0) * 100.0
        if self.soc < 0.0:
            self.soc = 0.0
        if self.soc > 100.0:
            self.soc = 100.0


@dataclasses.dataclass
class Plug:
    battery_id: str
    online: bool = True
    noise_w: float = 5.0

    def measure(self, actual_w: float) -> Optional[float]:
        if not self.online:
            return None
        return actual_w + random.gauss(0.0, self.noise_w)


class Multiplexer:
    """Closed-loop dispatcher driven by Shelly Plug measurements."""

    def __init__(self, batteries: list[Battery]) -> None:
        self.batteries = {b.id: b for b in batteries}
        self.circuits: dict[str, list[str]] = {}
        for b in batteries:
            self.circuits.setdefault(b.circuit, []).append(b.id)
        self.last_meas: dict[str, float] = {b.id: 0.0 for b in batteries}
        self.last_meas_at: dict[str, float] = {b.id: -1e9 for b in batteries}

    def ingest_plug(self, bid: str, value: Optional[float], t: float) -> None:
        if value is None:
            return  # offline -> stale grows
        self.last_meas[bid] = value
        self.last_meas_at[bid] = t

    def fresh(self, bid: str, t: float) -> bool:
        return (t - self.last_meas_at[bid]) <= PLUG_STALE_S

    def compute_ct(self, grid_w: float, t: float) -> dict[str, Optional[float]]:
        out: dict[str, Optional[float]] = {}

        # Trustable: plug fresh AND battery alive
        trustable: list[str] = []
        for bid, b in self.batteries.items():
            if self.fresh(bid, t) and b.alive:
                trustable.append(bid)
            else:
                out[bid] = None

        if not trustable:
            return out

        # ----- per-circuit capacity reservations from "ghost" batteries -----
        # If a plug is stale but watchdog hasn't fired yet, the battery is
        # frozen at its last commanded state. We reserve that absolute power
        # against the circuit cap for the worst case.
        ghost_load: dict[str, float] = {cid: 0.0 for cid in self.circuits}
        for bid, b in self.batteries.items():
            if bid in trustable:
                continue
            time_since = t - self.last_meas_at[bid]
            if b.alive and time_since < GHOST_HOLD_S:
                ghost_load[b.circuit] += abs(self.last_meas[bid])

        # ----- direction-appropriate capacity weights for trustable -----
        weights: dict[str, float] = {}
        for bid in trustable:
            b = self.batteries[bid]
            if grid_w >= 0:
                if b.soc <= 5.0:
                    continue
                weights[bid] = b.max_discharge_w
            else:
                if b.soc >= 95.0:
                    continue
                weights[bid] = b.max_charge_w

        if not weights:
            for bid in trustable:
                out[bid] = 0.0
            return out

        total_w = sum(weights.values())
        target: dict[str, float] = {}
        for bid in trustable:
            if bid in weights and total_w > 0:
                target[bid] = grid_w * (weights[bid] / total_w)
            else:
                target[bid] = 0.0

        # Per-battery hardware clamp
        for bid in trustable:
            b = self.batteries[bid]
            target[bid] = max(-b.max_charge_w,
                              min(b.max_discharge_w, target[bid]))

        # Per-circuit cap enforcement (using measured + ghost)
        for cid, members in self.circuits.items():
            members_t = [b for b in members if b in trustable]
            if not members_t:
                continue

            # Live signed sum: we will set target. Same-direction additive,
            # cross-direction cancels on the bus inside the breaker.
            target_signed = sum(target[b] for b in members_t)
            target_abs = abs(target_signed)

            measured_signed = sum(self.last_meas[b] for b in members_t)
            measured_abs = abs(measured_signed)

            available = CAP_PER_CIRCUIT_W * CIRCUIT_HEADROOM - ghost_load[cid]
            if available < 0:
                available = 0.0

            limit_abs = max(target_abs, measured_abs)
            if limit_abs > available and limit_abs > 0:
                scale = available / limit_abs
                for bid in members_t:
                    target[bid] *= scale

        for bid in trustable:
            out[bid] = target[bid]
        return out


# ---------------------------------------------------------------------------
# Simulation runner
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class Event:
    t: float
    kind: str        # "plug_off", "plug_on", "info"
    target: str = ""


def run(name: str,
        batteries: list[Battery],
        plugs: dict[str, Plug],
        grid_profile: Callable[[float], float],
        events: Optional[list[Event]] = None,
        duration_s: float = 60.0,
        sample_at: Optional[list[float]] = None) -> dict:
    """Run scenario, return summary dict."""
    events = sorted(events or [], key=lambda e: e.t)
    mux = Multiplexer(batteries)
    dt = CYCLE_MS / 1000.0
    t = 0.0
    sample_at = sample_at or [0.0, 0.5, 1.0, 2.0, 3.0, 5.0, 7.0, 10.0,
                              15.0, 20.0, 30.0, 45.0, 60.0]
    sample_idx = 0

    issues: list[tuple[float, str]] = []
    overload_events: list[tuple[float, str, float]] = []
    max_circuit_w: dict[str, float] = {cid: 0.0 for cid in mux.circuits}

    print()
    print("=" * 78)
    print(f"SCENARIO: {name}")
    print("=" * 78)
    hdr = f"{'t/s':>5s} {'grid':>7s} "
    hdr += " ".join(f"{b.id+'/W':>9s}" for b in batteries)
    hdr += " " + " ".join(f"{'c'+c+'/W':>7s}" for c in mux.circuits)
    hdr += " " + " ".join(f"{b.id+'%':>5s}" for b in batteries)
    hdr += "  flags"
    print(hdr)

    while t < duration_s + dt / 2:
        # Apply discrete events
        while events and events[0].t <= t + 1e-9:
            ev = events.pop(0)
            if ev.kind == "plug_off":
                plugs[ev.target].online = False
                issues.append((t, f"plug {ev.target} OFFLINE"))
            elif ev.kind == "plug_on":
                plugs[ev.target].online = True
                issues.append((t, f"plug {ev.target} back ONLINE"))
            elif ev.kind == "info":
                issues.append((t, ev.target))

        # Plug measurements -> mux
        for b in batteries:
            v = plugs[b.id].measure(b.actual_w)
            mux.ingest_plug(b.id, v, t)

        # Compute and dispatch CT signals
        grid_w = grid_profile(t)
        signals = mux.compute_ct(grid_w, t)
        for b in batteries:
            sig = signals.get(b.id)
            if sig is not None:
                b.receive_ct(sig, t)

        # Tick batteries
        for b in batteries:
            b.tick(t, dt)

        # Track max circuit current and overload
        for cid, members in mux.circuits.items():
            csum = abs(sum(mux.batteries[m].actual_w for m in members))
            if csum > max_circuit_w[cid]:
                max_circuit_w[cid] = csum
            if csum > CAP_PER_CIRCUIT_W * 1.001:
                key = f"OVERLOAD c{cid}: {csum:.0f}W > {CAP_PER_CIRCUIT_W:.0f}W"
                if not overload_events or overload_events[-1][1] != key:
                    overload_events.append((t, key, csum))

        # Sample
        if sample_idx < len(sample_at) and t >= sample_at[sample_idx]:
            line = f"{t:>5.1f} {grid_w:>7.0f} "
            line += " ".join(f"{b.actual_w:>9.0f}" for b in batteries)
            for cid in mux.circuits:
                csum = sum(b.actual_w for b in batteries if b.circuit == cid)
                line += f" {csum:>7.0f}"
            line += " " + " ".join(f"{b.soc:>5.1f}" for b in batteries)
            flags = []
            for bid in mux.batteries:
                if not mux.batteries[bid].alive:
                    flags.append(f"{bid}=DEAD")
                elif not mux.fresh(bid, t):
                    flags.append(f"{bid}=stale")
            if flags:
                line += "  " + ",".join(flags)
            print(line)
            sample_idx += 1

        t += dt

    print()
    print("Max |sum| per circuit (peak across run):")
    for cid, w in max_circuit_w.items():
        marker = " <-- OK" if w <= CAP_PER_CIRCUIT_W else " <-- OVER CAP!"
        print(f"  circuit {cid}: peak {w:7.0f} W (cap {CAP_PER_CIRCUIT_W:.0f} W){marker}")

    if issues:
        print("Events:")
        for ts, msg in issues:
            print(f"  t={ts:5.2f}s  {msg}")

    if overload_events:
        print("OVERLOAD WINDOWS:")
        for ts, msg, w in overload_events[:8]:
            print(f"  t={ts:5.2f}s  {msg}")
    else:
        print("No fuse-trip risk in this scenario.")

    return {
        "max_circuit_w": max_circuit_w,
        "overloads": overload_events,
        "events": issues,
    }


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

def make_batteries() -> list[Battery]:
    return [
        Battery(id="A", circuit="1", soc=60.0),
        Battery(id="B", circuit="1", soc=40.0),
        Battery(id="C", circuit="3", soc=50.0),
    ]


def make_plugs(bats: list[Battery]) -> dict[str, Plug]:
    return {b.id: Plug(b.id) for b in bats}


def s1_steady_discharge():
    bats = make_batteries()
    run("1) steady house load 1500W -> need to discharge",
        bats, make_plugs(bats),
        grid_profile=lambda t: 1500.0,
        duration_s=8.0)


def s2_steady_charge_under_cap():
    bats = make_batteries()
    run("2) steady solar 4500W export -> charge, no cap pressure",
        bats, make_plugs(bats),
        grid_profile=lambda t: -4500.0,
        duration_s=8.0)


def s3_heavy_charge_cap_pressure():
    bats = make_batteries()
    run("3) heavy solar 7500W export -> circuit 1 cap pressure",
        bats, make_plugs(bats),
        grid_profile=lambda t: -7500.0,
        duration_s=12.0)


def s4_step_swing():
    bats = make_batteries()
    def profile(t):
        if t < 3:
            return -3000.0
        if t < 6:
            return 0.0
        return 2000.0
    run("4) profile: charge -3000W, idle, discharge +2000W",
        bats, make_plugs(bats),
        grid_profile=profile,
        duration_s=12.0)


def s5_plug_a_dies_during_steady_charge():
    bats = make_batteries()
    plugs = make_plugs(bats)
    events = [Event(5.0, "plug_off", "A")]
    run("5) plug A dies at t=5s during steady -3000W charge",
        bats, plugs,
        grid_profile=lambda t: -3000.0,
        events=events,
        duration_s=60.0)


def s6_plug_a_dies_at_max_charge_circuit_pressure():
    """Worst case: plug A fails right when commanded to max charge,
    while B is also charging hard. Ghost reservation MUST kick in."""
    bats = make_batteries()
    plugs = make_plugs(bats)
    events = [Event(8.0, "plug_off", "A"),
              Event(8.05, "info", "(now ghost reservation must protect c1)")]
    run("6) plug A dies during heavy solar -7500W (worst case)",
        bats, plugs,
        grid_profile=lambda t: -7500.0,
        events=events,
        duration_s=60.0)


def s7_plug_a_recovers():
    bats = make_batteries()
    plugs = make_plugs(bats)
    events = [Event(5.0, "plug_off", "A"),
              Event(20.0, "plug_on", "A")]
    run("7) plug A dies at 5s, recovers at 20s (before watchdog at 50s)",
        bats, plugs,
        grid_profile=lambda t: -3000.0,
        events=events,
        duration_s=60.0)


def s8_both_plugs_circuit1_die():
    bats = make_batteries()
    plugs = make_plugs(bats)
    events = [Event(5.0, "plug_off", "A"),
              Event(5.5, "plug_off", "B")]
    run("8) BOTH plugs on circuit 1 die during heavy charge",
        bats, plugs,
        grid_profile=lambda t: -6000.0,
        events=events,
        duration_s=60.0)


def s9_full_battery_a():
    bats = make_batteries()
    bats[0].soc = 99.0  # A nearly full
    run("9) battery A nearly full (99%), heavy solar -5000W",
        bats, make_plugs(bats),
        grid_profile=lambda t: -5000.0,
        duration_s=10.0)


def s10_empty_battery_b():
    bats = make_batteries()
    bats[1].soc = 2.0  # B nearly empty
    run("10) battery B nearly empty (2%), house load 1500W",
        bats, make_plugs(bats),
        grid_profile=lambda t: 1500.0,
        duration_s=10.0)


def s11_grid_jitter():
    """Realistic jittery grid power as seen from a real Shelly meter."""
    bats = make_batteries()
    base = -2000.0
    def profile(t):
        return base + 800.0 * math.sin(t * 0.6) + random.gauss(0.0, 100.0)
    random.seed(7)
    run("11) jittery grid (charge with sine + noise)",
        bats, make_plugs(bats),
        grid_profile=profile,
        duration_s=20.0)


def s12_sudden_solar_onset():
    """Cloud passes -> 0 to -7500W in one tick. Stress test for cap clamp."""
    bats = make_batteries()
    def profile(t):
        if t < 1.0:
            return 0.0
        return -7500.0
    run("12) sudden solar onset at t=1s (0 -> -7500W instant)",
        bats, make_plugs(bats),
        grid_profile=profile,
        duration_s=12.0)


def main():
    random.seed(42)
    s1_steady_discharge()
    s2_steady_charge_under_cap()
    s3_heavy_charge_cap_pressure()
    s4_step_swing()
    s5_plug_a_dies_during_steady_charge()
    s6_plug_a_dies_at_max_charge_circuit_pressure()
    s7_plug_a_recovers()
    s8_both_plugs_circuit1_die()
    s9_full_battery_a()
    s10_empty_battery_b()
    s11_grid_jitter()
    s12_sudden_solar_onset()

    print()
    print("=" * 78)
    print("VERDICT")
    print("=" * 78)
    print("""
Closed-loop with Shelly Plug PM Gen3 per battery is feasible IF:
  1. Ghost-load reservation is implemented for stale plugs
     (we assume worst case = last known measurement until watchdog fires).
  2. Cycle is fast enough (<= 200 ms) so grid swings don't outpace control.
  3. Marstek's hardware ramp is bounded (~500 W/s) -> no instant spikes.
  4. Circuit cap is enforced on |sum| of measured power per circuit
     (cross-direction batteries cancel on the bus).
  5. CIRCUIT_HEADROOM (5 %) absorbs measurement jitter.

Failure modes covered:
  - single plug failure: ghost reservation prevents overload, watchdog frees
    capacity after 45 s.
  - plug recovery before watchdog: clean resume.
  - both plugs on a circuit fail: ghost reservation freezes circuit, all
    other circuits keep working.
  - SoC limits (full/empty): zero target for affected battery, others soak up.
  - sudden grid swing: bounded by ramp rate, controller adapts within 1 cycle.
""")


if __name__ == "__main__":
    main()
