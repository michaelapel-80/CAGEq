#!/usr/bin/env python3
"""CAGEq DSP sidecar — the real AutoEq-backed engine.

Speaks the exact same line-delimited JSON-RPC 2.0 as sidecar_stub.py (ping /
shutdown / calculate_filters), but calculate_filters runs the AutoEq pipeline:
interpolate the measurement + target onto the log grid, compute the error, and fit
parametric filters. Drop-in replacement for the stub behind the same contract.

Requires the Python 3.10 venv with autoeq installed (see requirements.txt). AutoEq
pulls matplotlib, so we force the headless Agg backend before importing it.
"""
import sys
import os
import json

os.environ.setdefault("MPLBACKEND", "Agg")  # headless subprocess: no GUI backend

import numpy as np  # noqa: E402
from autoeq.frequency_response import FrequencyResponse  # noqa: E402


def calculate_filters(params):
    """measurement (+ optional target) -> parametric filters in DeviceConfig shape.

    params:
      device: str
      measurement: [{frequency, raw_db}, ...]   (>= 2 points)
      target:      [{frequency, target_db}, ...] (optional; flat if omitted)
      peaking_filters: int (default 8)   — plus a low + high shelf => 8+2 bands
      max_gain: float (default 6.0)      — AutoEq equalization gain ceiling
      fs: int (default 48000)
    """
    device = params.get("device", "Unknown")
    measurement = params.get("measurement") or []
    if len(measurement) < 2:
        raise ValueError("measurement needs at least 2 points")

    m_freq = np.array([p["frequency"] for p in measurement], dtype=float)
    m_raw = np.array([p["raw_db"] for p in measurement], dtype=float)

    fr = FrequencyResponse(name=device, frequency=m_freq, raw=m_raw)
    fr.interpolate()  # onto AutoEq's standard log grid (20..20k, f_step 1.01)
    fr.center()

    # Target on fr's grid: provided points (interpolated) or flat.
    target_points = params.get("target")
    if target_points:
        t_freq = np.array([p["frequency"] for p in target_points], dtype=float)
        t_val = np.array([p["target_db"] for p in target_points], dtype=float)
        tfr = FrequencyResponse(name="target", frequency=t_freq, raw=t_val)
        tfr.interpolate(f=fr.frequency)
        target = FrequencyResponse(name="target", frequency=fr.frequency, raw=tfr.raw)
    else:
        target = FrequencyResponse(name="target", frequency=fr.frequency, raw=np.zeros(len(fr.frequency)))

    fr.compensate(target)  # error = raw - target
    fr.smoothen()
    fr.equalize(max_gain=float(params.get("max_gain", 6.0)))

    peaking = int(params.get("peaking_filters", 8))
    fs = int(params.get("fs", 48000))
    # Low + high shelf with fixed fc/q (gain optimized) plus N fully-optimized peaks.
    config = {
        "filters": [
            {"type": "LOW_SHELF", "fc": 105.0, "q": 0.7},
            {"type": "HIGH_SHELF", "fc": 10000.0, "q": 0.7},
        ]
        + [{"type": "PEAKING"} for _ in range(peaking)]
    }
    peq = fr.optimize_parametric_eq([config], fs)[0]

    filters = [
        {
            "kind": type(f).__name__,  # 'LowShelf' | 'HighShelf' | 'Peaking' == Rust FilterType
            "freq_hz": round(float(f.fc), 2),
            "gain_db": round(float(f.gain), 2),
            "q": round(float(f.q), 4),
        }
        for f in peq.filters
    ]
    # AutoEq's preamp: enough negative gain to keep the summed curve from clipping.
    # (CAGE's Auto-LUFS loudness-match, filter.md §4.1, is a separate later stage.)
    return {"device": device, "preamp_db": round(float(-peq.max_gain), 2), "filters": filters}


def reply(rid, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": rid}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        rid = req.get("id")
        method = req.get("method", "")
        if method == "shutdown":
            reply(rid, result={"bye": True})
            return
        try:
            if method == "ping":
                reply(rid, result={"pong": True})
            elif method == "calculate_filters":
                reply(rid, result=calculate_filters(req.get("params") or {}))
            else:
                reply(rid, error={"code": -32601, "message": f"unknown method: {method}"})
        except Exception as e:  # keep the loop alive; report the failure
            reply(rid, error={"code": -32603, "message": f"{type(e).__name__}: {e}"})


if __name__ == "__main__":
    main()
