#!/usr/bin/env python3
"""CAGEq DSP sidecar — the real AutoEq-backed engine.

Speaks the same line-delimited JSON-RPC 2.0 as sidecar_stub.py. Methods:
  ping / shutdown
  list_headphones {refresh?}   -> the AutoEq measurement catalogue (cached index)
  list_targets    {refresh?}   -> available AutoEq target curves
  calculate_filters {device, (headphone | measurement), target?, ...}

Measurement/target data is NOT bundled (the AutoEq repo is ~4.4 GB); we build a
searchable index from GitHub's tree API once, then fetch each chosen headphone's
CSV on demand from raw.githubusercontent and cache it locally. AutoEq is MIT.

Requires the Python 3.10 venv with autoeq (see requirements.txt). AutoEq pulls
matplotlib, so we force the headless Agg backend before importing it.
"""
import sys
import os
import types
import json
import hashlib
import tempfile
import urllib.request
import urllib.parse

os.environ.setdefault("MPLBACKEND", "Agg")  # headless subprocess: no GUI backend

# Stub matplotlib before AutoEq imports it. autoeq.peq does a module-level
# `from matplotlib import pyplot as plt, ticker` purely to support its plotting
# methods, which this sidecar never calls — but importing the real matplotlib costs
# ~0.17 s of the ~0.4 s cold start (measured), paid on the very first request and thus
# on app launch. A permissive no-op stub makes the import free and turns any stray
# plotting call into a silent no-op (behaviour-neutral: we compute curves, never render).
class _NoopModule(types.ModuleType):
    __path__ = []                       # look like a package for `from matplotlib import ...`

    def __getattr__(self, _name):       # every attribute is a no-op callable...
        return _noop


def _noop(*_a, **_k):                   # ...that also returns a no-op when called/chained
    return _noop


for _m in ("matplotlib", "matplotlib.pyplot", "matplotlib.ticker"):
    sys.modules[_m] = _NoopModule(_m)
sys.modules["matplotlib"].pyplot = sys.modules["matplotlib.pyplot"]
sys.modules["matplotlib"].ticker = sys.modules["matplotlib.ticker"]

import numpy as np  # noqa: E402
import autoeq.peq as autoeq_peq  # noqa: E402
from autoeq.frequency_response import FrequencyResponse  # noqa: E402

# User custom-filter kinds -> AutoEq PEQ filter classes (same biquad model as the fit,
# so a custom filter's response composes exactly with the AutoEq bands).
_CUSTOM_FILTER_CLASSES = {
    "Peaking": autoeq_peq.Peaking,
    "LowShelf": autoeq_peq.LowShelf,
    "HighShelf": autoeq_peq.HighShelf,
}

GH_API = "https://api.github.com/repos/jaakkopasanen/AutoEq"
GH_RAW = "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master"


# --- cache + http ---------------------------------------------------------

def _cache_dir():
    d = os.environ.get("CAGEQ_CACHE_DIR") or os.path.join(tempfile.gettempdir(), "cageq-cache")
    os.makedirs(d, exist_ok=True)
    return d


def _http_get(url, binary=False):
    req = urllib.request.Request(
        url, headers={"User-Agent": "CAGEq", "Accept": "application/vnd.github+json"}
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        data = r.read()
    return data if binary else data.decode("utf-8")


def _cached_download(rel_path):
    """Fetch a repo file (by repo-relative path) from raw.githubusercontent, cached
    by path. Returns the local file path."""
    safe = rel_path.replace("/", "__").replace("\\", "__")
    cpath = os.path.join(_cache_dir(), "files", safe)
    os.makedirs(os.path.dirname(cpath), exist_ok=True)
    if not os.path.exists(cpath):
        data = _http_get(GH_RAW + "/" + urllib.parse.quote(rel_path), binary=True)
        with open(cpath, "wb") as fh:
            fh.write(data)
    return cpath


# --- AutoEq catalogue -----------------------------------------------------

def _rig_map_for_source(source):
    """{(form_factor, name): rig} for one source, parsed from its name_index.tsv
    (columns: url, source_name, name, form, rig). Best-effort: {} if the source has no
    name_index.tsv (404) or on any error. The .tsv is cached like any repo file."""
    try:
        path = _cached_download("measurements/" + source + "/name_index.tsv")
    except Exception:
        return {}
    rigs = {}
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            next(fh, None)  # header row
            for line in fh:
                cols = line.rstrip("\n").split("\t")
                if len(cols) < 5:
                    continue
                name, form, rig = cols[2].strip(), cols[3].strip(), cols[4].strip()
                # Skip ignored/unpublished rows; first published entry wins (they're
                # consistent per model — multiple raw rows map to the same rig).
                if name and form != "ignore" and rig:
                    rigs.setdefault((form, name), rig)
    except Exception:
        return {}
    return rigs


def build_index(refresh=False):
    """The headphone catalogue: [{source, form_factor, name, path, rig}]. Built once
    from the measurements git tree (one recursive call, ~6800 entries), then enriched
    with the measurement rig from each source's name_index.tsv, and cached to disk.
    `rig` is "" for sources without a name_index.tsv."""
    # v2: added the `rig` field — a fresh name skips stale v1 caches automatically.
    idx_path = os.path.join(_cache_dir(), "headphone_index_v2.json")
    if not refresh and os.path.exists(idx_path):
        with open(idx_path, encoding="utf-8") as fh:
            return json.load(fh)
    root = json.loads(_http_get(GH_API + "/git/trees/master"))
    meas_sha = next(e["sha"] for e in root["tree"] if e["path"] == "measurements")
    tree = json.loads(_http_get(GH_API + f"/git/trees/{meas_sha}?recursive=1"))
    index = []
    for e in tree["tree"]:
        if e["type"] != "blob" or not e["path"].endswith(".csv"):
            continue
        parts = e["path"].split("/")  # <source>/data/<form-factor>/<model>.csv
        if len(parts) >= 4 and parts[1] == "data":
            index.append({
                "source": parts[0],
                "form_factor": parts[2],
                "name": parts[-1][:-4],
                "path": "measurements/" + e["path"],
                "rig": "",
            })

    # Enrich with rigs, fetching each source's name_index.tsv once (cached).
    rig_cache = {}
    for e in index:
        src = e["source"]
        if src not in rig_cache:
            rig_cache[src] = _rig_map_for_source(src)
        e["rig"] = rig_cache[src].get((e["form_factor"], e["name"]), "")

    index.sort(key=lambda h: (h["name"].lower(), h["source"]))
    with open(idx_path, "w", encoding="utf-8") as fh:
        json.dump(index, fh)
    return index


def list_targets(refresh=False):
    """Available target curves: [{name, path}]. From the targets/ dir listing."""
    cpath = os.path.join(_cache_dir(), "targets_index.json")
    if not refresh and os.path.exists(cpath):
        with open(cpath, encoding="utf-8") as fh:
            return json.load(fh)
    entries = json.loads(_http_get(GH_API + "/contents/targets"))
    targets = [
        {"name": e["name"][:-4], "path": "targets/" + e["name"]}
        for e in entries
        if e["type"] == "file" and e["name"].endswith(".csv")
    ]
    targets.sort(key=lambda t: t["name"].lower())
    with open(cpath, "w", encoding="utf-8") as fh:
        json.dump(targets, fh)
    return targets


# --- loudness (filter.md §4.1) --------------------------------------------

# ITU-R BS.1770-4 K-weighting, as a cascade of two biquads specified at 48 kHz:
#   stage 1 — "head" high-shelf (+~4 dB above ~1.5 kHz),
#   stage 2 — "RLB" high-pass (rolls off the low bass).
# Coefficients are the standard's reference values (fs = 48 kHz).
_KW_S1_B = (1.53512485958697, -2.69169618940638, 1.19839281085285)
_KW_S1_A = (1.0, -1.69065929318241, 0.73248077421585)
_KW_S2_B = (1.0, -2.0, 1.0)
_KW_S2_A = (1.0, -1.99004745483398, 0.99007225036621)


def _k_weight_power(f, fs=48000.0):
    """The K-weighting *power* response |H_k(f)|^2 on frequency grid `f`, evaluated
    analytically from the digital biquad cascade (z = e^{-jω}, ω = 2π f / fs)."""
    z = np.exp(-1j * 2.0 * np.pi * f / fs)

    def _mag2(b, a):
        num = b[0] + b[1] * z + b[2] * z * z
        den = a[0] + a[1] * z + a[2] * z * z
        return np.abs(num / den) ** 2

    return _mag2(_KW_S1_B, _KW_S1_A) * _mag2(_KW_S2_B, _KW_S2_A)


def loudness_target_db(f, g_eq_db):
    """§4.1 relative loudness compensation G_target for an EQ curve `g_eq_db` on the
    log grid `f`. A K-weighted pink-noise energy model: how much the curve raises the
    perceived loudness of pink noise, negated so applying it is level-neutral vs. dry.

    NOT a measurement of real audio and NOT an absolute LUFS level — a per-curve
    broadband offset so A/B/Dry comparisons judge timbre, not level."""
    w_k = _k_weight_power(f)
    # Pink noise's power *density* is 1/f, but the energy in a bin is density x bin
    # width — so the Jacobian matters. np.gradient gives the local spacing, making
    # this correct on any grid: on a log grid dF is proportional to f, the 1/f
    # cancels, and every bin carries equal energy (i.e. constant energy per octave);
    # on a linear grid it reduces to plain 1/f. Using 1/f directly as a per-bin
    # weight on a log grid double-counts the pink slope (it models ~1/f^2) and
    # massively over-weights the bottom octaves — the bug this replaces.
    bin_energy = np.gradient(f) / f
    p_dry = np.sum(bin_energy * w_k)
    p_wet = np.sum(bin_energy * w_k * 10.0 ** (g_eq_db / 10.0))
    delta_l = 10.0 * np.log10(p_wet / p_dry)
    return -delta_l


# --- the fit --------------------------------------------------------------

def _measurement_fr(params):
    """Build the source FrequencyResponse from a selected headphone (fetched CSV) or
    a raw measurement array."""
    if params.get("headphone"):
        return FrequencyResponse.read_csv(_cached_download(params["headphone"]))
    measurement = params.get("measurement") or []
    if len(measurement) < 2:
        raise ValueError("need 'headphone' (a catalogue path) or a 'measurement' array")
    freq = np.array([p["frequency"] for p in measurement], dtype=float)
    raw = np.array([p["raw_db"] for p in measurement], dtype=float)
    return FrequencyResponse(name=params.get("device", "measurement"), frequency=freq, raw=raw)


def _target_raw(params, grid):
    """Target dB values on `grid`: a named AutoEq target (fetched), else flat."""
    if params.get("target"):
        tfr = FrequencyResponse.read_csv(_cached_download(params["target"]))
        tfr.interpolate(f=grid)
        return tfr.raw
    return np.zeros(len(grid))


def _custom_filters(params, f, fs):
    """Parse the user's custom filters (filter.md §3.4) into EqAPO-shaped dicts plus
    their combined response in dB on grid `f`. Each is an AutoEq PEQ filter (same
    biquad model as the fit), so it composes additively with the AutoEq bands; the
    combined curve then drives the §4.1 loudness match and §4.2 clipping ceiling."""
    out, curve = [], np.zeros(len(f))
    for cf in params.get("custom_filters") or []:
        kind = cf.get("kind")
        cls = _CUSTOM_FILTER_CLASSES.get(kind)
        if cls is None:
            raise ValueError(f"unknown custom filter kind: {kind!r}")
        fc, gain, q = float(cf["freq_hz"]), float(cf["gain_db"]), float(cf["q"])
        # Wide bounds so the user's exact values aren't clamped to optimiser limits.
        filt = cls(f, fs, fc=fc, q=q, gain=gain, min_fc=1.0, max_fc=24000.0,
                   min_q=0.01, max_q=100.0, min_gain=-60.0, max_gain=60.0)
        curve = curve + filt.fr
        out.append({"kind": kind, "freq_hz": round(fc, 2), "gain_db": round(gain, 2), "q": round(q, 4)})
    return out, curve


# The AutoEq fit is the expensive step (SciPy optimisation, ~1-2 s). It depends only
# on the measurement + target + fit params — NOT on custom filters — so we cache it by
# those inputs. Changing only custom filters (add/edit/remove) then reuses the cached
# fit and just recombines (filter.md §5.2 performance rule). In-process dict; the
# sidecar handles requests one at a time, so no locking is needed.
_FIT_CACHE = {}
_FIT_CACHE_MAX = 32


def _fit_key(params):
    """A hashable key over exactly the inputs the AutoEq fit depends on."""
    src = params.get("headphone")
    if not src:
        meas = params.get("measurement") or []
        digest = hashlib.sha1(repr([(round(float(p["frequency"]), 4), round(float(p["raw_db"]), 4)) for p in meas]).encode()).hexdigest()
        src = "meas:" + digest
    return (src, params.get("target") or "",
            float(params.get("max_gain", 6.0)), int(params.get("peaking_filters", 8)), int(params.get("fs", 48000)))


def _subsample_curve(f, db, n=140):
    """A compact [{f, db}, ...] sampling of a dense curve for the UI chart (§5.2). The
    grid is already log-spaced, so picking evenly-spaced indices keeps it log-even; ~140
    points is smooth at chart width while keeping the JSON-RPC message small."""
    m = len(f)
    if m <= n:
        idx = list(range(m))
    else:
        step = m / n
        idx = [int(i * step) for i in range(n)]
        if idx[-1] != m - 1:
            idx.append(m - 1)
    return [{"f": round(float(f[i]), 2), "db": round(float(db[i]), 3)} for i in idx]


def _autoeq_fit(params):
    """Run (or reuse from cache) the AutoEq parametric fit. Returns
    (filter_dicts, f_grid, response_db, reference_curve) — response_db is the AutoEq
    bands' combined response on f_grid, reference_curve is the *ideal* correction
    (AutoEq's gain-limited target-minus-measured, `fr.equalization`) that the parametric
    fit chases, subsampled for the chart. Cached by [`_fit_key`]; custom filters never
    enter here."""
    key = _fit_key(params)
    cached = _FIT_CACHE.get(key)
    if cached is not None:
        return cached

    fr = _measurement_fr(params)
    fr.interpolate()  # AutoEq's standard log grid (20..20k, f_step 1.01)
    fr.center()
    target = FrequencyResponse(name="target", frequency=fr.frequency, raw=_target_raw(params, fr.frequency))
    fr.compensate(target)
    fr.smoothen()
    fr.equalize(max_gain=float(params.get("max_gain", 6.0)))

    # The ideal correction curve the parametric fit targets (§5.2 chart reference): the
    # gain-limited inverse of the smoothed deviation from target. The fitted bands should
    # hug it; the visible gap is the residual the 10-band parametric couldn't capture.
    reference_curve = _subsample_curve(fr.frequency, fr.equalization)

    peaking = int(params.get("peaking_filters", 8))
    fs = int(params.get("fs", 48000))
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
    result = (filters, peq.f, peq.fr, reference_curve)
    if len(_FIT_CACHE) >= _FIT_CACHE_MAX:
        _FIT_CACHE.pop(next(iter(_FIT_CACHE)))  # evict oldest (dicts keep insertion order)
    _FIT_CACHE[key] = result
    return result


def filter_response(params):
    """The combined dB response of `filters` on the given `freqs`, via the exact same
    AutoEq PEQ model the fit uses (`_custom_filters`). Test-support only: it exists so a
    cross-language check can pin the Rust (`morph.rs`) and TypeScript (`biquad.ts`)
    copies of the biquad math against this reference implementation — three copies exist
    (a slot switch must draw curves without the sidecar), and silent divergence between
    them would make the chart, the fit and the tonal-morph disagree (§5.2/§5.3a)."""
    fs = int(params.get("fs", 48000))
    f = np.array(params["freqs"], dtype=float)
    _, curve = _custom_filters({"custom_filters": params.get("filters") or []}, f, fs)
    return {"db": [round(float(v), 6) for v in curve]}


def measurement_curves(params):
    """Raw headphone measurement + target curve for the §5.2 nerd overlays, both on AutoEq's
    log grid and **centered the same way** so they share one relative-dB (dBr) reference —
    like the AutoEq site's graph (raw vs target). Raw is *before* target compensation (the
    headphone's own FR shape); target is the named AutoEq target (empty ⇒ flat 0). UI-only,
    never written to EqAPO."""
    fr = _measurement_fr(params)
    fr.interpolate()
    fr.center()
    raw_curve = _subsample_curve(fr.frequency, fr.raw)
    if params.get("target"):
        tfr = FrequencyResponse(name="target", frequency=fr.frequency, raw=_target_raw(params, fr.frequency))
        tfr.center()  # same centering as the raw → shared reference
        target_curve = _subsample_curve(fr.frequency, tfr.raw)
    else:
        target_curve = []
    return {"raw_curve": raw_curve, "target_curve": target_curve}


def calculate_filters(params):
    device = params.get("device", "Unknown")
    # cached; no re-fit on custom-filter changes
    autoeq_filters, f, autoeq_curve, reference_curve = _autoeq_fit(params)

    # Append the user's custom filters (§3.4) and combine their response with the
    # AutoEq curve; the *combined* curve drives the level policy.
    custom, custom_curve = _custom_filters(params, f, int(params.get("fs", 48000)))
    filters = autoeq_filters + custom  # new list — never mutate the cached fit
    combined = autoeq_curve + custom_curve

    # We report the two curve-derived quantities the Rust core needs to compose the
    # final preamp (filter.md §4.0/§4.2); the DSP does not own the user's base pre-gain
    # or clipping policy:
    #   g_target_db   — §4.1 K-weighted loudness compensation for this curve,
    #   g_max_peak_db — the composed EQ curve's positive peak, for the §4.2 ceiling.
    g_target = loudness_target_db(f, combined)
    g_max_peak = float(np.max(combined))
    return {
        "device": device,
        "filters": filters,
        "g_target_db": round(g_target, 2),
        "g_max_peak_db": round(g_max_peak, 2),
        # §5.2 chart: the ideal correction the fit chases (independent of custom filters,
        # so it rides along with the cached fit). Never written to EqAPO.
        "reference_curve": reference_curve,
    }


# --- JSON-RPC loop --------------------------------------------------------

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
        params = req.get("params") or {}
        try:
            if method == "ping":
                reply(rid, result={"pong": True})
            elif method == "list_headphones":
                reply(rid, result={"headphones": build_index(refresh=bool(params.get("refresh")))})
            elif method == "list_targets":
                reply(rid, result={"targets": list_targets(refresh=bool(params.get("refresh")))})
            elif method == "calculate_filters":
                reply(rid, result=calculate_filters(params))
            elif method == "measurement_curves":
                reply(rid, result=measurement_curves(params))
            elif method == "filter_response":
                reply(rid, result=filter_response(params))
            else:
                reply(rid, error={"code": -32601, "message": f"unknown method: {method}"})
        except Exception as e:  # keep the loop alive; report the failure
            reply(rid, error={"code": -32603, "message": f"{type(e).__name__}: {e}"})


if __name__ == "__main__":
    main()
