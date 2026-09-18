#!/usr/bin/env python3
"""Ground-truth fixtures for `cageq-peq-solver::prep` (interpolate/center/compensate/
smoothen) — the FR-prep chain that runs before the PEQ fit, mirroring
`generate_fixtures.py`'s role for the optimizer itself. Same rationale: a synthetic,
reproducible ground-truth harness built before trusting the Rust port, not live
iteration.

Run with the sidecar's own venv:

    ..\cageq-sidecar\.venv\Scripts\python fixtures\generate_prep_fixtures.py

Deliberately exercises `sidecar_dsp.py`'s exact call sequence — `fr.interpolate();
fr.center(); fr.compensate(target); fr.smoothen()` — with every parameter CAGEq never
overrides left at AutoEq's default, on synthetic (not fetched) measurement/target
curves so the fixtures need no network access and their shape is legible from this file.
One fixture includes a `None` gap in the raw measurement to exercise `interpolate()`'s
"remove None values" pass, which `generate_fixtures.py`'s curves never touch.
"""
import json
import os
import sys
import types
from pathlib import Path

import numpy as np

os.environ.setdefault("MPLBACKEND", "Agg")


class _NoopModule(types.ModuleType):
    __path__ = []

    def __getattr__(self, _name):
        return _noop


def _noop(*_a, **_k):
    return _noop


for _m in ("matplotlib", "matplotlib.pyplot", "matplotlib.ticker"):
    sys.modules[_m] = _NoopModule(_m)
sys.modules["matplotlib"].pyplot = sys.modules["matplotlib.pyplot"]
sys.modules["matplotlib"].ticker = sys.modules["matplotlib.ticker"]

from autoeq.frequency_response import FrequencyResponse  # noqa: E402

FIXTURES_DIR = Path(__file__).parent.parent / "tests" / "fixtures_prep"

# A coarse, irregular frequency axis (not the standard grid) — measurements arrive at
# whatever points the underlying hardware/software captured, and `interpolate()` has to
# resample that onto AutoEq's standard grid, so the fixture should exercise that rather
# than start pre-gridded.
MEASUREMENT_F = [20.0, 31.5, 50.0, 80.0, 125.0, 200.0, 315.0, 500.0, 800.0, 1250.0, 2000.0, 3150.0, 5000.0, 8000.0, 12500.0, 20000.0]
TARGET_F = [20.0, 100.0, 1000.0, 10000.0, 20000.0]


def make_measurement(kind):
    f = np.array(MEASUREMENT_F)
    log_f = np.log2(f)
    if kind == "flat":
        return f, np.zeros_like(f)
    if kind == "tilted_with_a_peak":
        span = (log_f - log_f.min()) / (log_f.max() - log_f.min())
        tilt = 4.0 * (span - 0.5)
        peak = 6.0 * np.exp(-((log_f - np.log2(3000.0)) ** 2) / (2 * 0.3**2))
        return f, tilt + peak
    if kind == "with_a_gap":
        raw = 3.0 * np.exp(-((log_f - np.log2(150.0)) ** 2) / (2 * 0.4**2))
        raw = list(raw)
        raw[5] = None  # 200 Hz measured as a gap — exercises interpolate()'s None-removal
        return f, np.array(raw, dtype=object)
    if kind == "sharp_narrow_peak":
        # Steep and narrow enough to force `equalize()`'s slope limiter to actually
        # clip (unlike the gentler curves above, which mostly stay under 18 dB/octave).
        return f, 12.0 * np.exp(-((log_f - np.log2(2500.0)) ** 2) / (2 * 0.08**2))
    raise ValueError(kind)


def make_target():
    # A generic "flat-ish with a little bass tilt" target, standing in for a real
    # AutoEq target CSV (e.g. Harman) without needing to fetch one.
    f = np.array(TARGET_F)
    return f, np.array([2.0, 1.0, 0.0, 0.0, -1.0])


def to_list(arr):
    return [None if v is None or (isinstance(v, float) and np.isnan(v)) else float(v) for v in arr]


def generate(name, measurement_kind):
    m_f, m_raw = make_measurement(measurement_kind)
    t_f, t_raw = make_target()

    fr = FrequencyResponse(name="measurement", frequency=m_f, raw=m_raw)
    fr.interpolate()
    fr.center()
    target = FrequencyResponse(name="target", frequency=t_f, raw=t_raw)
    fr.compensate(target)
    fr.smoothen()
    fr.equalize(max_gain=6.0)  # max_slope/concha_interference/etc. left at AutoEq's defaults, matching sidecar_dsp.py

    fixture = {
        "name": name,
        "input": {"measurement_f": to_list(m_f), "measurement_raw": to_list(m_raw), "target_f": to_list(t_f), "target_raw": to_list(t_raw)},
        "output": {
            "f": to_list(fr.frequency),
            "raw": to_list(fr.raw),
            "target": to_list(fr.target),
            "error": to_list(fr.error),
            "smoothed": to_list(fr.smoothed),
            "error_smoothed": to_list(fr.error_smoothed),
            "equalization": to_list(fr.equalization),
        },
    }
    FIXTURES_DIR.mkdir(parents=True, exist_ok=True)
    out_path = FIXTURES_DIR / f"{name}.json"
    out_path.write_text(json.dumps(fixture))
    print(f"wrote {out_path} ({len(fr.frequency)} grid points)")


def main():
    for kind in ["flat", "tilted_with_a_peak", "with_a_gap", "sharp_narrow_peak"]:
        generate(kind, kind)


if __name__ == "__main__":
    main()
