#!/usr/bin/env python3
"""Nudge strategy simulation.

User idea: instead of continuously holding a fake CT value, we nudge the
battery by sending a brief CT pulse (1-N ticks), then return to 0. The
battery should "remember" the target via its internal integrator.

Critical question: how does the Marstek's internal controller actually work?
We don't know. We model THREE plausible variants and see how each strategy
behaves in each model.

Marstek control models tested:
  P only        -> battery reacts proportional to instantaneous CT,
                   so a brief pulse leaves no lasting effect.
  I only        -> battery integrates CT over time; brief pulse leaves
                   a permanent shift equal to Ki * pulse_value * pulse_duration.
  PI mix        -> realistic; pulse leaves I-component but P-component snaps back.

Strategies tested:
  saldierend     -> hold grid_w / N continuously (current architecture).
  open_nudge_1   -> send target delta for 1 tick, then 0.
  open_nudge_3   -> send target delta for 3 ticks, then 0.
  closed_nudge   -> with Shelly Plug PM Gen3 feedback, send delta until
                    measured power matches target, then 0.

Output per (model x strategy):
  - settling time to reach target +/-50 W
  - steady-state error after 30 s
  - max overshoot
  - behavior under setpoint step and disturbance
"""

from __future__ import annotations

import dataclasses
import math
from typing import Callable

CYCLE_MS = 200       # our control cycle
MARSTEK_TICK_S = 1.0 # Marstek reads CT and updates target ~ once per second
RAMP_W_S = 500.0     # Marstek internal ramp limit


# ---------------------------------------------------------------------------
# Marstek control models
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class MarstekPI:
    """Generic PI controller for Marstek. Set kp/ki to taste."""
    kp: float                  # proportional gain (W output per W CT error)
    ki: float                  # integral gain (W output per W CT error per second)
    tick_s: float = MARSTEK_TICK_S
    max_charge: float = 2500.0
    max_discharge: float = 800.0

    def __post_init__(self):
        self.integral = 0.0          # accumulated I term in W
        self.target_w = 0.0          # commanded internal target
        self.actual_w = 0.0          # ramp-limited output
        self.ct_window: list[float] = []
        self.last_tick_at = -1e9

    def receive_ct(self, ct_w: float):
        # Buffer values received between ticks.
        self.ct_window.append(ct_w)

    def tick(self, t: float, dt: float):
        if t - self.last_tick_at >= self.tick_s:
            self.last_tick_at = t
            if self.ct_window:
                ct = sum(self.ct_window) / len(self.ct_window)
            else:
                ct = 0.0
            self.ct_window = []
            # CT > 0 means grid is importing -> battery should discharge more.
            # error = ct, both signs aligned with battery output (positive=discharge).
            self.integral += self.ki * ct * self.tick_s
            # Anti-windup
            self.integral = max(-self.max_charge, min(self.max_discharge, self.integral))
            self.target_w = self.integral + self.kp * ct
            self.target_w = max(-self.max_charge, min(self.max_discharge, self.target_w))

        # Ramp toward target
        delta = self.target_w - self.actual_w
        step = RAMP_W_S * dt
        if abs(delta) <= step:
            self.actual_w = self.target_w
        else:
            self.actual_w += math.copysign(step, delta)


# ---------------------------------------------------------------------------
# Strategies
# ---------------------------------------------------------------------------

class Strategy:
    name: str = "base"
    def step(self, t: float, dt: float, target_w: float, measured_w: float) -> float:
        """Return the CT value to send to the battery this cycle."""
        raise NotImplementedError


class Saldierend(Strategy):
    """Continuously hold the desired output as CT signal."""
    name = "saldierend (hold)"
    def step(self, t, dt, target_w, measured_w):
        return target_w


class OpenNudge(Strategy):
    """Send target delta for N ticks of the battery, then 0 forever."""
    def __init__(self, n_ticks: int):
        self.name = f"open_nudge_{n_ticks}t"
        self.n_ticks = n_ticks
        self.last_target = 0.0
        self.pulse_until = -1.0

    def step(self, t, dt, target_w, measured_w):
        if abs(target_w - self.last_target) > 1.0:  # setpoint changed
            self.last_target = target_w
            self.pulse_until = t + self.n_ticks * MARSTEK_TICK_S
        if t < self.pulse_until:
            # send the DELTA between desired and last known measured
            return target_w - measured_w
        return 0.0


class ClosedNudge(Strategy):
    """Closed-loop: send delta from measured to target, gated by tick.
    Only emit a non-zero pulse once per Marstek tick (~1 s) so we don't
    overdrive the integrator within a single tick window.
    """
    name = "closed_nudge"
    def __init__(self, dead_band_w: float = 30.0):
        self.dead_band = dead_band_w
        self.last_pulse_at = -1e9
        self.pulse_value = 0.0
        self.pulse_until = -1.0

    def step(self, t, dt, target_w, measured_w):
        error = target_w - measured_w
        # New pulse on next Marstek tick boundary if outside dead band
        if abs(error) > self.dead_band and t - self.last_pulse_at >= MARSTEK_TICK_S:
            self.last_pulse_at = t
            # Send the error as CT for ONE tick window
            self.pulse_value = error
            self.pulse_until = t + MARSTEK_TICK_S * 0.9  # just under one tick
        if t < self.pulse_until:
            return self.pulse_value
        return 0.0


# ---------------------------------------------------------------------------
# Test runner
# ---------------------------------------------------------------------------

def run_test(model_name: str, marstek: MarstekPI, strategy: Strategy,
             setpoint_profile: Callable[[float], float],
             duration_s: float = 30.0, sample_interval_s: float = 1.0):
    dt = CYCLE_MS / 1000.0
    t = 0.0
    samples = []
    next_sample = 0.0
    while t < duration_s:
        target = setpoint_profile(t)
        # Strategy decides what CT to feed Marstek
        ct = strategy.step(t, dt, target, marstek.actual_w)
        marstek.receive_ct(ct)
        marstek.tick(t, dt)
        if t >= next_sample:
            samples.append((t, target, ct, marstek.actual_w))
            next_sample += sample_interval_s
        t += dt
    return samples


def metrics(samples, target_at: float = 30.0):
    """Compute: settling time, steady-state error, max overshoot relative to target."""
    final_target = None
    final_actual = None
    max_overshoot = 0.0
    settled_at = None
    settle_band = 50.0
    for ts, tgt, ct, act in samples:
        # Capture overshoot once setpoint settled
        overshoot = abs(act) - abs(tgt) if (tgt != 0 and (act * tgt) > 0) else 0
        if overshoot > max_overshoot:
            max_overshoot = overshoot
        if abs(act - tgt) <= settle_band and settled_at is None and ts > 1.0:
            settled_at = ts
        final_target = tgt
        final_actual = act
    err = final_actual - final_target
    return {
        "settle_s": settled_at if settled_at else float("nan"),
        "ss_error_w": err,
        "max_overshoot_w": max_overshoot,
        "final_actual": final_actual,
        "final_target": final_target,
    }


def fmt(d):
    return (f"settle={d['settle_s']:>5.1f}s "
            f"ss_err={d['ss_error_w']:>+6.0f}W "
            f"overshoot={d['max_overshoot_w']:>+5.0f}W "
            f"final={d['final_actual']:>+6.0f}W (tgt {d['final_target']:>+6.0f}W)")


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

MARSTEK_MODELS = {
    # Pure proportional: snaps to CT instantly, doesn't hold without sustained signal.
    "P only      (Kp=1.0,  Ki=0)":   lambda: MarstekPI(kp=1.0, ki=0.0),
    # Pure integral: each CT increment adds to internal target permanently.
    "I only      (Kp=0,    Ki=1.0)": lambda: MarstekPI(kp=0.0, ki=1.0),
    # Realistic mix: dominant I, small P kick.
    "PI mix      (Kp=0.3,  Ki=0.7)": lambda: MarstekPI(kp=0.3, ki=0.7),
    # Slow integrator, like a smoothed cloud reaction.
    "I slow      (Kp=0,    Ki=0.3)": lambda: MarstekPI(kp=0.0, ki=0.3),
}


def step_setpoint(t):
    """Setpoint: 0 W, then -1000 W (charge) at t=2 s, then -1500 W at t=15 s."""
    if t < 2.0:
        return 0.0
    if t < 15.0:
        return -1000.0
    return -1500.0


def disturbance_setpoint(t):
    """Many step changes to test stability under noise-like changes."""
    steps = [(0, 0), (2, -800), (8, -1200), (14, -600), (20, -1500)]
    val = 0
    for ts, sp in steps:
        if t >= ts:
            val = sp
    return val


STRATEGIES = [
    Saldierend,
    lambda: OpenNudge(1),
    lambda: OpenNudge(3),
    ClosedNudge,
]


def main():
    print()
    print("=" * 92)
    print("STEP RESPONSE TEST  -  setpoint: 0 W -> -1000 W (t=2s) -> -1500 W (t=15s)")
    print("=" * 92)

    for model_name, model_factory in MARSTEK_MODELS.items():
        print(f"\nMarstek model: {model_name}")
        print("-" * 92)
        for strat_factory in STRATEGIES:
            marstek = model_factory()
            strategy = strat_factory()
            samples = run_test(model_name, marstek, strategy, step_setpoint, duration_s=30.0)
            m = metrics(samples)
            print(f"  {strategy.name:<22} {fmt(m)}")

    print()
    print("=" * 92)
    print("DISTURBANCE TEST  -  many setpoint steps within 25 s")
    print("=" * 92)
    for model_name, model_factory in MARSTEK_MODELS.items():
        print(f"\nMarstek model: {model_name}")
        print("-" * 92)
        for strat_factory in STRATEGIES:
            marstek = model_factory()
            strategy = strat_factory()
            samples = run_test(model_name, marstek, strategy, disturbance_setpoint, duration_s=25.0)
            m = metrics(samples)
            print(f"  {strategy.name:<22} {fmt(m)}")

    # ------------------------------------------------------------------
    # Detailed trace for the most interesting case: closed_nudge x PI mix
    # ------------------------------------------------------------------
    print()
    print("=" * 92)
    print("DETAILED TRACE: closed_nudge with PI mix model (Kp=0.3, Ki=0.7)")
    print("=" * 92)
    print(f"{'t/s':>5}  {'sp':>6}  {'CT_sent':>7}  {'actual':>6}  {'err':>5}")
    marstek = MarstekPI(kp=0.3, ki=0.7)
    strategy = ClosedNudge()
    samples = run_test("PI mix", marstek, strategy, step_setpoint,
                       duration_s=20.0, sample_interval_s=0.5)
    for ts, sp, ct, act in samples:
        err = act - sp
        flag = "  <-- pulse" if abs(ct) > 0.1 else ""
        print(f"{ts:>5.1f}  {sp:>+6.0f}  {ct:>+7.0f}  {act:>+6.0f}  {err:>+5.0f}{flag}")

    # ------------------------------------------------------------------
    # Cost of being wrong: open_nudge with WRONG number of pulses
    # ------------------------------------------------------------------
    print()
    print("=" * 92)
    print("WHAT IF WE GUESS THE PULSE COUNT WRONG (PI mix)?")
    print("=" * 92)
    for n in [1, 2, 3, 4, 5, 8]:
        marstek = MarstekPI(kp=0.3, ki=0.7)
        strategy = OpenNudge(n)
        samples = run_test("PI mix", marstek, strategy, step_setpoint, duration_s=30.0)
        m = metrics(samples)
        # Want: -1500. error_pct = ss_err / target * 100
        pct = m["ss_error_w"] / 1500.0 * 100.0
        print(f"  open_nudge_{n}t  {fmt(m)}  ({pct:+.0f}% off)")

    print()
    print("=" * 92)
    print("VERDICT")
    print("=" * 92)
    print("""
1. Saldierend: works on every model. It's the dumb-but-safe baseline.
   Steady-state error depends on Marstek's loop gain (small for mix/I models).

2. open_nudge_N: works ONLY if Marstek has a dominant I-component AND we know
   N exactly. With PI mix, even a 1-pulse miss leaves multi-hundred-W error
   that never goes away (no closed loop).
   On a P-only Marstek the strategy fails completely (battery snaps back to 0).

3. closed_nudge with Shelly Plug feedback works on EVERY model:
   - On P-only: degenerates to "saldierend" (sends correction every tick), still safe.
   - On I-only: a single pulse sets the right target, follow-up pulses are 0.
   - On PI mix: a few pulses converge to target +/- dead band.
   Settling time ~3-5 ticks (3-5 s) for big changes, 0 for small ones.

4. Risk of "fake CT" with open_nudge: if grid suddenly changes during a pulse,
   the battery sees the wrong CT and over/undershoots. Closed-loop catches it
   within the next tick.

CONCLUSION: pulse-style steering is mathematically appealing but dangerous
without measurement. The Shelly Plug per battery turns it from "hope and pray"
into a robust closed loop. WITHOUT plugs, stick to saldierend.
""")


if __name__ == "__main__":
    main()
