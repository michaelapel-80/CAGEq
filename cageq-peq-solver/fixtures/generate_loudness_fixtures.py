#!/usr/bin/env python3
"""Ground-truth fixtures for `cageq-peq-solver::loudness` against the live sidecar's
own `loudness_target_db`/`_k_weight_power` (filter.md §4.1) — imported directly as a
module (no JSON-RPC round trip needed; these aren't exposed as their own RPC method).

Run with the sidecar's own venv:

    ..\cageq-sidecar\.venv\Scripts\python fixtures\generate_loudness_fixtures.py
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

sys.path.insert(0, str(Path(__file__).parent.parent.parent / "cageq-sidecar" / "python"))
import sidecar_dsp  # noqa: E402
from autoeq.frequency_response import FrequencyResponse  # noqa: E402

FIXTURES_DIR = Path(__file__).parent.parent / "tests" / "fixtures_loudness"


def standard_grid():
    fr = FrequencyResponse(name="grid", frequency=[20.0, 20000.0], raw=[0.0, 0.0])
    fr.interpolate()
    return fr.frequency


def generate(name, curve_fn):
    f = standard_grid()
    curve = curve_fn(f)
    g_target = sidecar_dsp.loudness_target_db(f, curve)
    g_peak = float(np.max(curve))

    fixture = {
        "name": name,
        "input": {"f": [float(x) for x in f], "curve": [float(x) for x in curve]},
        "output": {"g_target_db": g_target, "g_max_peak_db": g_peak},
    }
    FIXTURES_DIR.mkdir(parents=True, exist_ok=True)
    out_path = FIXTURES_DIR / f"{name}.json"
    out_path.write_text(json.dumps(fixture))
    print(f"wrote {out_path} (g_target={g_target:.4f} g_peak={g_peak:.4f})")


def main():
    generate("flat", lambda f: np.zeros_like(f))
    generate("uniform_boost", lambda f: np.full_like(f, 4.5))
    generate("bass_boost_shelf", lambda f: 6.0 / (1.0 + (f / 150.0) ** 2))
    generate("treble_cut_and_bass_boost", lambda f: 3.0 / (1.0 + (f / 100.0) ** 2) - 2.0 / (1.0 + (3000.0 / f) ** 2))
    generate("noisy_realistic", lambda f: 4.0 * np.exp(-((np.log2(f) - np.log2(80.0)) ** 2) / (2 * 0.3**2)) - 3.0 * np.exp(-((np.log2(f) - np.log2(6000.0)) ** 2) / (2 * 0.4**2)))


if __name__ == "__main__":
    main()
