#!/usr/bin/env python3
"""Generates ground-truth fixtures for `cageq-peq-solver` from the real `autoeq`
package — the reference this crate's Rust port is validated against
(`tests/solver_fixtures.rs`), per the CAGEq stack decision to build a synthetic
ground-truth harness before trusting a numerical port (rather than iterating live).

Run with the sidecar's own venv, which already pins the exact `autoeq` version CAGEq
ships against:

    ..\cageq-sidecar\.venv\Scripts\python fixtures\generate_fixtures.py

Deliberately synthetic, not live headphone measurements: every target curve here is a
constructed numpy array, so fixtures are reproducible without a network fetch and their
shape is legible from this file alone (a Gaussian bump is obviously "one peak", not
"whatever HD 600.csv happens to contain this week"). If AutoEq's own measurement/target
quirks ever need covering too, add fixtures built from real CSVs as a separate case
rather than folding them in here.

Fixture format (`tests/fixtures/*.json`): {
  "name": str,
  "input": {"f": [...], "fs": int, "target": [...], "bands": [
      {"type": "PEAKING"|"LOW_SHELF"|"HIGH_SHELF",
       "fc": float|null, "q": float|null, "gain": float|null}, ...]},
  "output": {"bands": [{"type", "fc", "q", "gain"}, ...], "loss": float}
}
`fc`/`q`/`gain` null in the input means "optimize this parameter"; CAGEq's own config
(`sidecar_dsp.py`) fixes fc/q on both shelves and leaves everything free on the 8
peaking bands, which is what `cageq_config()` below reproduces.
"""
import json
import os
import sys
import types
from pathlib import Path

import numpy as np

# Same headless-import trick as sidecar_dsp.py: autoeq.peq imports matplotlib.pyplot at
# module level purely for its unused plot() method.
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
from autoeq.peq import PEQ  # noqa: E402

FIXTURES_DIR = Path(__file__).parent.parent / "tests" / "fixtures"
FS = 48000


def standard_grid():
    """AutoEq's own standard log grid (20 Hz - 20 kHz, ~1.01 step) — reproduced via the
    real `FrequencyResponse.interpolate()` rather than hand-derived, so the fixture grid
    is guaranteed identical to what the sidecar actually fits on."""
    fr = FrequencyResponse(name="grid", frequency=[20.0, 20000.0], raw=[0.0, 0.0])
    fr.interpolate()
    return fr.frequency


def cageq_config(peaking=8):
    """The exact band shape `sidecar_dsp.py`'s `calculate_filters` builds
    (sidecar_dsp.py:406-412): both shelves pinned at fc/q, gain free; N fully-free
    peaking bands."""
    return {
        "filters": [
            {"type": "LOW_SHELF", "fc": 105.0, "q": 0.7},
            {"type": "HIGH_SHELF", "fc": 10000.0, "q": 0.7},
        ]
        + [{"type": "PEAKING"} for _ in range(peaking)]
    }


def band_to_json(filt):
    name_map = {"LowShelf": "LOW_SHELF", "Peaking": "PEAKING", "HighShelf": "HIGH_SHELF"}
    return {
        "type": name_map[type(filt).__name__],
        "fc": float(filt.fc),
        "q": float(filt.q),
        "gain": float(filt.gain),
    }


def input_band_to_json(filt_dict):
    return {
        "type": filt_dict["type"],
        "fc": filt_dict.get("fc"),
        "q": filt_dict.get("q"),
        "gain": filt_dict.get("gain"),
    }


def make_target(f, kind):
    """Synthetic target curves spanning the shapes the optimizer's heuristics branch
    on: nothing to grab onto (flat), a single peak/dip for `Peaking::init`'s biggest-peak
    search, a broadband tilt for the shelves' transition-point search, and a busier
    multi-feature curve closer to a real compensated headphone deviation."""
    f = np.array(f)
    log_f = np.log2(f)
    if kind == "flat":
        return np.zeros_like(f)
    if kind == "single_peak":
        return 8.0 * np.exp(-((log_f - np.log2(2000.0)) ** 2) / (2 * 0.3**2))
    if kind == "single_dip":
        return -8.0 * np.exp(-((log_f - np.log2(150.0)) ** 2) / (2 * 0.3**2))
    if kind == "broadband_tilt":
        # Smooth low-to-high slope: exercises the shelves' "where does the average
        # level change" search rather than a single localized feature.
        span = (log_f - log_f.min()) / (log_f.max() - log_f.min())
        return 6.0 * (span - 0.5)
    if kind == "busy_multi_feature":
        return (
            5.0 * np.exp(-((log_f - np.log2(80.0)) ** 2) / (2 * 0.2**2))
            - 4.0 * np.exp(-((log_f - np.log2(500.0)) ** 2) / (2 * 0.15**2))
            + 3.0 * np.exp(-((log_f - np.log2(3000.0)) ** 2) / (2 * 0.25**2))
            - 6.0 * np.exp(-((log_f - np.log2(8000.0)) ** 2) / (2 * 0.2**2))
        )
    raise ValueError(kind)


def generate(name, target_kind, peaking=8):
    f = standard_grid()
    target = make_target(f, target_kind)
    config = cageq_config(peaking)

    peq = PEQ.from_dict(config, f, FS, target=target)
    input_bands = [input_band_to_json(filt) for filt in config["filters"]]

    peq.optimize()
    loss = float(peq._optimizer_loss(None, parse=False))  # filters already hold the final params

    fixture = {
        "name": name,
        "input": {"f": [float(x) for x in f], "fs": FS, "target": [float(x) for x in target], "bands": input_bands},
        "output": {"bands": [band_to_json(filt) for filt in peq.filters], "loss": loss},
    }
    FIXTURES_DIR.mkdir(parents=True, exist_ok=True)
    out_path = FIXTURES_DIR / f"{name}.json"
    out_path.write_text(json.dumps(fixture, indent=2))
    print(f"wrote {out_path} (loss={loss:.6f})")


def main():
    for kind in ["flat", "single_peak", "single_dip", "broadband_tilt", "busy_multi_feature"]:
        generate(kind, kind)


if __name__ == "__main__":
    main()
