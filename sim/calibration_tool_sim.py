#!/usr/bin/env python3
"""Marstek Calibration Tool Simulation.

User's correct insight: Marstek polls us at known intervals, plug measures
exact response. We don't need to GUESS the internal control law - we MEASURE
it with a calibration sequence:

  1. Battery quiet at 0 W, plug confirms.
  2. Present CT = V for N requests, then return to 0.
  3. Wait for settle (~5 s).
  4. Plug reads steady-state retained output W_retained.
  5. Record: (V, N) -> W_retained.

After sweep over a few (V, N), we have a complete characterization.
For any desired output change, we look up the right (V, N) pulse.

Critical model question (empirically discoverable):
  - if Marstek pure proportional: W_retained = 0 always (snaps back to 0)
                                  -> nudge strategy impossible
  - if Marstek has integral: W_retained = Ki * V * N * tick_s
                             -> nudge strategy works, calibration gives us Ki * tick_s
  - if Marstek has decay/leak: W_retained drops over time after pulse
                              -> we need periodic refresh pulses, calibration measures tau

This tool answers all three questions in one calibration run.
"""

from __future__ import annotations

import dataclasses
import math
import time
from typing import Optional

CYCLE_MS = 200
MARSTEK_TICK_S = 1.0
RAMP_W_S = 500.0
SETTLE_OBSERVE_S = 6.0
DECAY_OBSERVE_S = 30.0


# ---------------------------------------------------------------------------
# Marstek model (parametrized)
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class MarstekUnknown:
    """Black-box battery with hidden internal control parameters.

    The calibration tool does not know kp/ki/leak_tau - they're internal to
    the firmware. The tool only sees the input (CT signal) and output (plug
    measurement). After calibration we infer Ki * tick_s from the table.
    """
    name: str
    kp: float = 0.0           # internal: proportional gain
    ki: float = 0.0           # internal: integral gain (per second)
    leak_tau_s: float = 1e9   # internal: integral decay time constant (huge = no leak)
    tick_s: float = MARSTEK_TICK_S
    max_charge: float = 2500.0
    max_discharge: float = 800.0

    def __post_init__(self):
        self.integral = 0.0
        self.target_w = 0.0
        self.actual_w = 0.0
        self.ct_window: list[float] = []
        self.last_tick_at = -1e9

    def receive_ct(self, ct_w: float, t: float):
        self.ct_window.append(ct_w)

    def tick(self, t: float, dt: float):
        # integral leak (slow decay if leak_tau finite)
        if self.leak_tau_s > 0:
            self.integral *= math.exp(-dt / self.leak_tau_s)

        if t - self.last_tick_at >= self.tick_s:
            self.last_tick_at = t
            if self.ct_window:
                ct = sum(self.ct_window) / len(self.ct_window)
                self.ct_window = []
            else:
                ct = 0.0
            self.integral += self.ki * ct * self.tick_s
            self.integral = max(-self.max_charge, min(self.max_discharge, self.integral))
            self.target_w = self.integral + self.kp * ct
            self.target_w = max(-self.max_charge, min(self.max_discharge, self.target_w))

        delta = self.target_w - self.actual_w
        step = RAMP_W_S * dt
        if abs(delta) <= step:
            self.actual_w = self.target_w
        else:
            self.actual_w += math.copysign(step, delta)


# ---------------------------------------------------------------------------
# Calibration tool
# ---------------------------------------------------------------------------

def measure_pulse(battery_factory, value_w: float, n_ticks: int,
                  settle_s: float = SETTLE_OBSERVE_S) -> float:
    """Run one calibration measurement.

    Sequence:
      - pulse: send CT=value for n_ticks * MARSTEK_TICK_S seconds
      - flat:  send CT=0 for settle_s seconds (lets ramp finish + reveals retained level)
    Returns: plug reading after settle.
    """
    bat = battery_factory()
    dt = CYCLE_MS / 1000.0
    t = 0.0
    pulse_end = n_ticks * MARSTEK_TICK_S

    # Phase A: pulse
    while t < pulse_end:
        bat.receive_ct(value_w, t)
        bat.tick(t, dt)
        t += dt

    # Phase B: zero, observe
    flat_end = pulse_end + settle_s
    while t < flat_end:
        bat.receive_ct(0.0, t)
        bat.tick(t, dt)
        t += dt
    return bat.actual_w


def measure_decay(battery_factory, value_w: float, n_ticks: int,
                  observe_s: float = DECAY_OBSERVE_S) -> list[tuple[float, float]]:
    """Apply pulse, then send 0 and sample plug at multiple times to detect decay."""
    bat = battery_factory()
    dt = CYCLE_MS / 1000.0
    t = 0.0
    pulse_end = n_ticks * MARSTEK_TICK_S
    while t < pulse_end:
        bat.receive_ct(value_w, t)
        bat.tick(t, dt)
        t += dt

    samples = []
    sample_at = [pulse_end + s for s in (1.0, 3.0, 5.0, 10.0, 20.0, 30.0)]
    si = 0
    end = pulse_end + observe_s
    while t < end:
        bat.receive_ct(0.0, t)
        bat.tick(t, dt)
        if si < len(sample_at) and t >= sample_at[si]:
            samples.append((t - pulse_end, bat.actual_w))
            si += 1
        t += dt
    return samples


def calibration_sweep(battery_factory, model_label: str):
    print()
    print("=" * 84)
    print(f"CALIBRATION SWEEP: {model_label}")
    print("=" * 84)

    test_values = [-100, -200, -500, -1000]
    test_ticks = [1, 2, 3, 5, 8]

    table: dict[tuple[int, int], float] = {}

    print(f"\n{'CT value':>9}   " + "  ".join(f"{'N='+str(n)+'t':>8s}" for n in test_ticks))
    print(f"{'-'*9}   " + "  ".join(f"{'-'*8}" for _ in test_ticks))
    for v in test_values:
        row = []
        for n in test_ticks:
            retained = measure_pulse(battery_factory, v, n)
            row.append(retained)
            table[(v, n)] = retained
        print(f"{v:>+9.0f} W   " + "  ".join(f"{r:>+7.1f}W" for r in row))

    # Decay test on a representative pulse
    print(f"\nDecay check after pulse (V=-500, N=3): retained over time")
    decay = measure_decay(battery_factory, -500, 3)
    for ts, w in decay:
        print(f"  t+{ts:>5.1f}s  -> {w:>+7.1f} W")

    return table


# ---------------------------------------------------------------------------
# Operational use of calibration table
# ---------------------------------------------------------------------------

def find_pulse_for_target(table: dict, desired_change_w: float) -> tuple[int, int, float]:
    """Look up the (V, N) that best produces the desired_change_w."""
    best = min(table.keys(), key=lambda k: abs(table[k] - desired_change_w))
    return best[0], best[1], table[best]


def operational_demo(battery_factory, table: dict, model_label: str):
    print(f"\n--- Operational use ({model_label}) ---")
    for tgt in (-100, -300, -750, -1500):
        v, n, predicted = find_pulse_for_target(table, tgt)
        actual = measure_pulse(battery_factory, v, n)
        err = actual - tgt
        verdict = "OK" if abs(err) < 50 else "WARN"
        print(f"  want {tgt:>+5.0f} W  ->  send V={v:>+5.0f} N={n}  "
              f"predicted {predicted:>+6.1f}  actual {actual:>+6.1f}  err {err:>+6.1f} [{verdict}]")


# ---------------------------------------------------------------------------
# Models we test against (Marstek's true behavior is one of these)
# ---------------------------------------------------------------------------

MODELS = {
    "P only           (Kp=1.0, Ki=0)":           lambda: MarstekUnknown("P", kp=1.0, ki=0.0),
    "I only           (Kp=0,   Ki=1.0)":         lambda: MarstekUnknown("I", kp=0.0, ki=1.0),
    "PI realistic     (Kp=0.3, Ki=0.7)":         lambda: MarstekUnknown("PI", kp=0.3, ki=0.7),
    "I-dominant slow  (Kp=0.1, Ki=0.4)":         lambda: MarstekUnknown("Idom", kp=0.1, ki=0.4),
    "I + leak (60s)   (Kp=0,   Ki=1.0, tau=60)": lambda: MarstekUnknown("Ileak60", kp=0.0, ki=1.0, leak_tau_s=60.0),
    "I + fast leak    (Kp=0,   Ki=1.0, tau=15)": lambda: MarstekUnknown("Ileak15", kp=0.0, ki=1.0, leak_tau_s=15.0),
}


def main():
    tables = {}
    for label, factory in MODELS.items():
        tables[label] = calibration_sweep(factory, label)

    print()
    print("=" * 84)
    print("OPERATIONAL TEST: drive specific output targets using calibration lookup")
    print("=" * 84)
    for label, factory in MODELS.items():
        operational_demo(factory, tables[label], label)

    print()
    print("=" * 84)
    print("INTERPRETATION GUIDE")
    print("=" * 84)
    print("""
What each calibration table tells us about the real Marstek:

  Row reading "all 0 W"       -> Marstek is pure proportional. Nudges DON'T hold.
                                 Use saldierend (continuous CT).

  Row scaling linearly with N -> Marstek has integral component. Nudges work.
                                 Slope = Ki * tick_s. Use it to compute pulse count
                                 for any desired output change.

  Decay row dropping over time -> integrator leaks. Refresh period = 3 * tau.
                                 Send a refresh pulse every 3*tau seconds to hold.

  Decay row stable             -> integrator persists. Send pulse once, hold via 0.
                                 (User's mental model exactly.)

The calibration takes about 5 minutes per battery (4 values x 5 tick counts x ~7s
per measurement = ~140 s, plus a single decay test at ~30 s). Run once after install,
re-run if firmware updates. Output: a 4x5 table that completely characterizes the
Marstek's response to nudges.
""")


if __name__ == "__main__":
    main()
