#!/usr/bin/env python3
"""CAGEq DSP sidecar — the real AutoEq-backed engine.

Speaks the same line-delimited JSON-RPC 2.0 as sidecar_stub.py. Methods:
  ping / shutdown
  list_headphones {refresh?}   -> the AutoEq measurement catalogue (cached index)
  list_targets    {refresh?}   -> available AutoEq target curves
  calculate_filters {device, (headphone | measurement | flat), target?, ...}
                  `flat: true` skips AutoEq's own fit entirely (no measurement, no
                  optimizer) — the custom filters alone become the whole correction.
                  No longer called by cageq-core in production (see fetch_raw_curves
                  below) — kept working for the sidecar's own tests/tooling.
  fetch_raw_curves {device, (headphone | measurement), target?} -> raw, unresampled
                  measurement/target curves. What `cageq-core`'s `fit.rs` actually
                  calls now: FR-prep/equalize/the SLSQP fit/loudness all moved to Rust
                  (`cageq-peq-solver`), leaving only the measurement/target database
                  fetch+cache here (it isn't bundled — see below)
  measurement_curves {headphone, target?} -> raw measurement + target curves for the §5.2
                  nerd overlays (see its own docstring)
  filter_response {filters, freqs, fs?} -> combined dB response of `filters` on `freqs`;
                  test-support only, cross-checks the Rust/TS biquad copies (see its own docstring)
  fit_export_eq   {filters, band_count, fs?} -> a low-band-count PEQ fit to a slot's own
                  composed curve, for exporting to a mobile EQ app (see its own docstring)
  fit_fixed_band_eq {filters, preset: "10"|"31", fs?} -> AutoEq's own standard 10-/31-band
                  graphic EQ fit to a slot's own composed curve (see its own docstring)

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
import copy
import math
import tempfile
from concurrent.futures import ThreadPoolExecutor
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
from autoeq.constants import PEQ_CONFIGS  # noqa: E402

# AutoEq's PEQ optimizer measures its convergence "change rate" as d_loss / d_time, timing each
# SLSQP callback with time.time() (peq.py: `from time import time`). On Windows time.time() has
# ~15 ms resolution, so two callbacks in the same tick give d_time == 0 -> a divide-by-zero
# warning and a +/-inf change_rate that can falsely trip the "Change too small" early stop,
# ending the fit prematurely. Swap that name for the monotonic, sub-microsecond perf_counter
# (same seconds unit, so max_time etc. are unaffected) so d_time is always > 0.
from time import perf_counter as _perf_counter  # noqa: E402

autoeq_peq.time = _perf_counter

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

    # Enrich with rigs, fetching each distinct source's name_index.tsv once (cached) — in
    # parallel: this is dozens of independent network round-trips (I/O-bound, so threads give
    # real concurrency despite the GIL), and doing them one at a time on a cold cache is what
    # made a first-ever catalogue build slow enough to look hung to whatever's talking to us.
    sources = sorted({e["source"] for e in index})
    with ThreadPoolExecutor(max_workers=min(12, len(sources)) or 1) as pool:
        rig_maps = dict(zip(sources, pool.map(_rig_map_for_source, sources)))
    for e in index:
        e["rig"] = rig_maps[e["source"]].get((e["form_factor"], e["name"]), "")

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


def _peq_filter(cls, f, fs, fc, gain, q):
    # Wide bounds so the user's exact values aren't clamped to optimiser limits.
    return cls(f, fs, fc=fc, q=q, gain=gain, min_fc=1.0, max_fc=24000.0,
               min_q=0.01, max_q=100.0, min_gain=-60.0, max_gain=60.0)


def _custom_filters(params, f, fs):
    """Parse the user's custom filters (filter.md §3.4) into EqAPO-shaped dicts plus
    their combined response in dB on grid `f`. Each is an AutoEq PEQ filter (same
    biquad model as the fit), so it composes additively with the AutoEq bands; the
    combined curve then drives the §4.1 loudness match and §4.2 clipping ceiling.

    `Tilt` has no AutoEq PEQ class of its own — pivots the spectrum around `freq_hz`,
    realised the same way every other consumer does (`cageq_backend::expand_tilts` /
    biquad.ts's `expandTilts`): a low-shelf cut plus a high-shelf boost of equal and
    opposite magnitude, `gain_db` split across them, at the same corner/Q. Only the
    *curve* is expanded here — the echoed dict stays the single Tilt entry the caller
    sent, so the band round-trips as one control, not two."""
    out, curve = [], np.zeros(len(f))
    for cf in params.get("custom_filters") or []:
        kind = cf.get("kind")
        fc, gain, q = float(cf["freq_hz"]), float(cf["gain_db"]), float(cf["q"])
        # biquad_coefficients() divides by q and by fc with no bounds check of its own
        # (min_q/max_q above only constrain AutoEq's *optimiser*, not a directly-built
        # filter like every one of these) -- q<=0 or fc<=0 silently produces NaN that
        # poisons `curve` and, downstream, the JSON-RPC reply itself (NaN has no valid
        # JSON representation). Reject here instead, same as any other malformed input.
        if not math.isfinite(fc) or fc <= 0.0:
            raise ValueError(f"custom filter {kind!r}: freq_hz must be positive and finite, got {fc!r}")
        if not math.isfinite(q) or q <= 0.0:
            raise ValueError(f"custom filter {kind!r} at {fc} Hz: q must be positive and finite, got {q!r}")
        if not math.isfinite(gain):
            raise ValueError(f"custom filter {kind!r} at {fc} Hz: gain_db must be finite, got {gain!r}")
        if kind == "Tilt":
            curve = curve + _peq_filter(autoeq_peq.LowShelf, f, fs, fc, -gain / 2.0, q).fr
            curve = curve + _peq_filter(autoeq_peq.HighShelf, f, fs, fc, gain / 2.0, q).fr
        else:
            cls = _CUSTOM_FILTER_CLASSES.get(kind)
            if cls is None:
                raise ValueError(f"unknown custom filter kind: {kind!r}")
            curve = curve + _peq_filter(cls, f, fs, fc, gain, q).fr
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
    if params.get("flat"):
        src = "flat"
    else:
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
    enter here.

    `params["flat"]` skips all of this — no measurement fetch, no optimizer — for a
    headphone AutoEq has no measurement for, or one bad enough that fitting to it makes
    things worse than not fitting at all. Returns empty AutoEq bands on AutoEq's own
    standard grid with a zero response, so `calculate_filters`/`filter_response` and the
    §5.2 chart machinery downstream need no changes: the user's custom filters become
    the entire correction."""
    key = _fit_key(params)
    cached = _FIT_CACHE.get(key)
    if cached is not None:
        return cached

    if params.get("flat"):
        flat_fr = FrequencyResponse(name="flat", frequency=[20.0, 20000.0], raw=[0.0, 0.0])
        flat_fr.interpolate()  # same standard grid every other path fits on
        result = ([], flat_fr.frequency, np.zeros(len(flat_fr.frequency)), [])
        if len(_FIT_CACHE) >= _FIT_CACHE_MAX:
            _FIT_CACHE.pop(next(iter(_FIT_CACHE)))
        _FIT_CACHE[key] = result
        return result

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


def fetch_raw_curves(params):
    """Raw, unresampled measurement + target curves — the one piece of
    `calculate_filters`'s pipeline that still needs Python once `cageq-core`'s `fit.rs`
    drives everything downstream (interpolate/center/compensate/smoothen/equalize/
    optimize/loudness) itself via `cageq-peq-solver`: AutoEq's measurement/target
    database (~4.4 GB) isn't bundled, so the catalogue fetch+cache stays here.

    Deliberately returns curves *before* `FrequencyResponse.interpolate()` (unlike
    `_target_raw`, which grids the target immediately) — the Rust side does its own
    interpolation onto the same standard grid, so handing it already-gridded data would
    just make it redo idempotent work for no benefit, and handing it the true raw
    points keeps this method's contract simple: "whatever was fetched", nothing more.
    `None` gaps (a measurement CSV's missing points) are preserved as JSON `null` for
    the Rust side's own None-removal to drop, exactly as `interpolate()` does here."""
    fr = _measurement_fr(params)
    measurement_f = [float(x) for x in fr.frequency]
    measurement_raw = [None if v is None or (isinstance(v, float) and math.isnan(v)) else float(v) for v in fr.raw]

    if params.get("target"):
        tfr = FrequencyResponse.read_csv(_cached_download(params["target"]))
        target_f = [float(x) for x in tfr.frequency]
        target_raw = [None if v is None or (isinstance(v, float) and math.isnan(v)) else float(v) for v in tfr.raw]
    else:
        target_f = [20.0, 20000.0]
        target_raw = [0.0, 0.0]

    return {
        "measurement_f": measurement_f,
        "measurement_raw": measurement_raw,
        "target_f": target_f,
        "target_raw": target_raw,
    }


# --- mobile export (§8) ----------------------------------------------------

# A second, independent fit cache from `_FIT_CACHE` above — keyed on the *composed* filters
# list + band_count + fs, not on a headphone/target selection, so a custom-filter edit (which
# changes what's being fit here) correctly invalidates it while leaving `_FIT_CACHE` alone.
_EXPORT_FIT_CACHE = {}
_EXPORT_FIT_CACHE_MAX = 32


def _export_fit_key(params):
    """A hashable key over exactly the inputs `fit_export_eq` depends on."""
    filters = params.get("filters") or []
    digest = hashlib.sha1(repr([
        (f.get("kind"), round(float(f["freq_hz"]), 2), round(float(f["gain_db"]), 2), round(float(f["q"]), 4))
        for f in filters
    ]).encode()).hexdigest()
    return (digest, max(3, int(params.get("band_count", 8))), int(params.get("fs", 48000)))


def fit_export_eq(params):
    """A second, independent AutoEq PEQ pass: fits `band_count` filters directly to the
    *combined response* of a slot's own full cascade (`filters` — fit stage + content stage +
    tone macros, same wire shape `_custom_filters` already parses, Tilt expansion included) —
    not to a headphone measurement, the way `_autoeq_fit` does. For exporting a low-band-count
    correction to a mobile parametric EQ app: the full cascade routinely runs well past what a
    phone app (or a user typing values in by hand) wants to accept, so this re-targets the same
    optimizer at "match my own curve with fewer filters" instead.

    `band_count` (>= 3) is the *total* filter count: one LowShelf + one HighShelf (same corners
    `_autoeq_fit` uses) + `band_count - 2` Peaking, mirroring `_autoeq_fit`'s own config shape.
    Returns just `{filters, preamp_db}` — no curve payload. The frontend already has
    `composedCurveDb` (biquad.ts, a verified match for this exact biquad model — see the
    biquad-crosscheck tests) and can recompute both the achieved and the full-cascade reference
    curve from bands alone, so there's no reason to duplicate that curve math a third time here
    just to hand back samples the caller can already produce itself.

    `preamp_db` is `-max(achieved curve) - 0.2 dB` headroom, mirroring AutoEq's own
    `write_eqapo_parametric_eq`'s preamp line — a self-contained "don't clip on the destination
    device" value. Deliberately NOT `calculate_filters`' `g_target_db` (the desktop's §4.1
    loudness-matched preamp): that's calibrated for A/B-ing against Dry, which doesn't exist on
    the destination device.

    Cached like `_autoeq_fit` (`_EXPORT_FIT_CACHE`, own key/eviction) — the export dialog's
    band-count slider re-fits on every change, and repeat drags over an already-seen value
    should be instant rather than re-paying the SciPy optimization."""
    key = _export_fit_key(params)
    cached = _EXPORT_FIT_CACHE.get(key)
    if cached is not None:
        return cached

    fs = int(params.get("fs", 48000))
    band_count = max(3, int(params.get("band_count", 8)))
    f = FrequencyResponse.generate_frequencies()
    _, curve = _custom_filters({"custom_filters": params.get("filters") or []}, f, fs)

    fr = FrequencyResponse(name="export", frequency=f, equalization=curve)
    config = {
        "filters": [
            {"type": "LOW_SHELF", "fc": 105.0, "q": 0.7},
            {"type": "HIGH_SHELF", "fc": 10000.0, "q": 0.7},
        ]
        + [{"type": "PEAKING"} for _ in range(band_count - 2)]
    }
    peq = fr.optimize_parametric_eq([config], fs)[0]
    filters = [
        {
            "kind": type(filt).__name__,  # 'LowShelf' | 'HighShelf' | 'Peaking' == Rust FilterType
            "freq_hz": round(float(filt.fc), 2),
            "gain_db": round(float(filt.gain), 2),
            "q": round(float(filt.q), 4),
        }
        for filt in peq.filters
    ]
    result = {"filters": filters, "preamp_db": round(-float(np.max(peq.fr)) - 0.2, 2)}
    if len(_EXPORT_FIT_CACHE) >= _EXPORT_FIT_CACHE_MAX:
        _EXPORT_FIT_CACHE.pop(next(iter(_EXPORT_FIT_CACHE)))  # evict oldest (dicts keep insertion order)
    _EXPORT_FIT_CACHE[key] = result
    return result


# AutoEq's own two standard "graphic EQ" presets (autoeq/constants.py's PEQ_CONFIGS) — the exact
# ones its site's "10-band"/"31-band Graphic EQ" downloads use: fixed ISO-standard center
# frequencies (10-band: 31.25 Hz doubling each band; 31-band: 20 Hz, third-octave steps) and a
# fixed per-preset Q, only gain optimized per band.
_FIXED_BAND_PRESETS = {"10": "10_BAND_GRAPHIC_EQ", "31": "31_BAND_GRAPHIC_EQ"}

# `optimize_fixed_band_eq`'s own `gain_range` knob (frequency_response.py:178): bounds each
# band's optimized gain to within this many dB of the curve's own value sampled *at that band's
# exact Fc*, rather than letting the joint least-squares fit push a band's gain arbitrarily far
# to compensate for its fixed-Q neighbours' overlap. Left unbounded (the default), a dense preset
# fit against a real multi-band curve measurably overshoots/oscillates — reported live as visible
# ripple in the 31-band preset specifically (its bands are narrower and more numerous than
# 10-band's octave spacing, so neighbour interaction bites harder). Verified with a standalone
# diagnostic script against several synthetic multi-band curves before landing this: gain_range
# in the 4-6 dB range consistently roughly halved both the worst-case residual and a
# diff-based ripple metric versus unconstrained, for both presets, with no regression on a
# simpler single-peak curve; 4 dB gave the best numbers without ever being visibly worse.
_FIXED_BAND_GAIN_RANGE_DB = 4.0

_FIXED_BAND_CACHE = {}
_FIXED_BAND_CACHE_MAX = 32


def _fixed_band_key(params):
    """A hashable key over exactly the inputs `fit_fixed_band_eq` depends on."""
    filters = params.get("filters") or []
    digest = hashlib.sha1(repr([
        (f.get("kind"), round(float(f["freq_hz"]), 2), round(float(f["gain_db"]), 2), round(float(f["q"]), 4))
        for f in filters
    ]).encode()).hexdigest()
    preset = _FIXED_BAND_PRESETS.get(str(params.get("preset", "31")), "31_BAND_GRAPHIC_EQ")
    return (digest, preset, int(params.get("fs", 48000)))


def fit_fixed_band_eq(params):
    """AutoEq's own standard 10-/31-band graphic EQ, fit to a slot's own full cascade the same
    way `fit_export_eq` is (same `filters` wire shape, same `_custom_filters` reuse) — but with
    the band Fc/Q *fixed* to one of AutoEq's own presets (`_FIXED_BAND_PRESETS`) instead of free,
    only gain optimized. Unlike `fit_export_eq`'s free-band fit, simply reading the composed
    curve's value at each fixed center frequency would be WRONG here: a real graphic EQ's
    fixed-bandwidth bands overlap and sum, so what to dial into any one band depends on its
    neighbours too — exactly the interaction `optimize_fixed_band_eq`'s SciPy optimizer solves
    for, the same way `calculate_filters`' own free-band fit does for a headphone measurement.

    Returns the same `{filters, preamp_db}` shape as `fit_export_eq` — the frontend already
    formats a Peaking/LowShelf/HighShelf `Band[]` as parametric-syntax text (`parametricEqText`,
    exportFormats.ts), and that's exactly how AutoEq's own site writes these presets out too
    (`write_eqapo_parametric_eq`, not the dense `eqapo_graphic_eq` curve-table format its plain
    "GraphicEQ.txt" download uses) — there's no separate export format to build for this.

    Passes `_FIXED_BAND_GAIN_RANGE_DB` to `optimize_fixed_band_eq` to keep the fit from
    overshooting/rippling (see that constant's own doc) — which itself *mutates* each filter
    dict's `min_gain`/`max_gain` in place when a gain_range is given, so `PEQ_CONFIGS[preset]`
    (a module-level dict, shared and reused by every call and every other consumer of
    autoeq.constants in this process) is deep-copied first; passing it directly would leak one
    request's bounds into every subsequent request's preset, silently, forever."""
    preset = _FIXED_BAND_PRESETS.get(str(params.get("preset", "31")), "31_BAND_GRAPHIC_EQ")
    key = _fixed_band_key(params)
    cached = _FIXED_BAND_CACHE.get(key)
    if cached is not None:
        return cached

    fs = int(params.get("fs", 48000))
    f = FrequencyResponse.generate_frequencies()
    _, curve = _custom_filters({"custom_filters": params.get("filters") or []}, f, fs)

    fr = FrequencyResponse(name="export", frequency=f, equalization=curve)
    config = copy.deepcopy(PEQ_CONFIGS[preset])
    peq = fr.optimize_fixed_band_eq([config], fs, gain_range=_FIXED_BAND_GAIN_RANGE_DB)[0]
    filters = [
        {
            "kind": type(filt).__name__,
            "freq_hz": round(float(filt.fc), 2),
            "gain_db": round(float(filt.gain), 2),
            "q": round(float(filt.q), 4),
        }
        for filt in peq.filters
    ]
    result = {"filters": filters, "preamp_db": round(-float(np.max(peq.fr)) - 0.2, 2)}
    if len(_FIXED_BAND_CACHE) >= _FIXED_BAND_CACHE_MAX:
        _FIXED_BAND_CACHE.pop(next(iter(_FIXED_BAND_CACHE)))
    _FIXED_BAND_CACHE[key] = result
    return result


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
            elif method == "fetch_raw_curves":
                reply(rid, result=fetch_raw_curves(params))
            elif method == "measurement_curves":
                reply(rid, result=measurement_curves(params))
            elif method == "filter_response":
                reply(rid, result=filter_response(params))
            elif method == "fit_export_eq":
                reply(rid, result=fit_export_eq(params))
            elif method == "fit_fixed_band_eq":
                reply(rid, result=fit_fixed_band_eq(params))
            else:
                reply(rid, error={"code": -32601, "message": f"unknown method: {method}"})
        except Exception as e:  # keep the loop alive; report the failure
            reply(rid, error={"code": -32603, "message": f"{type(e).__name__}: {e}"})


if __name__ == "__main__":
    main()
