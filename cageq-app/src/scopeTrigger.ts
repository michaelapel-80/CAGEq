import { type BiquadCoeffs, type BiquadState, stepBiquad, zeroState } from "./biquad";

/**
 * TimeScope's trigger detector: finds the rising edge the display window anchors on. Separate from
 * TimeScope.tsx so a harness can drive the exact search on synthetic signals with known crossings.
 *
 * The detector signal is the mono mix, low-passed (HF-reject trigger coupling, the knob) and
 * zero-phase, and the edge is where it rises through its own **local mean** rather than through 0
 * (AC trigger coupling). Each piece is explained at its constant below.
 */

export const TRIGGER_FILTER_Q = 0.707; // Butterworth — a clean rolloff, no resonant peaking

// The low-pass is zero-phase: the Butterworth runs forward as samples arrive (into TimeScope's
// `ringTrig`) and backward over that output at search time, so the edge it finds is the crossing
// of the trace's own low-frequency content, at every pitch. A one-way IIR delays each frequency by
// a different amount (2-4 ms at a 60 Hz cutoff, ~10 ms at 20 Hz), so the marker never
// sat on the displayed crossing and the waveform slid under it as the pitch moved. Linear phase
// costs latency and pre-ringing elsewhere; here neither matters, since the search runs over
// buffered history and the filtered signal is never drawn or heard.
//
// Both passes use a raised cutoff so the combined response (|H|², 24 dB/oct) is -3 dB at the knob's
// value: for a 2nd-order Butterworth, per-pass fc = knob / (√2 − 1)^(1/4) ≈ 1.25 × knob.
export const TRIGGER_PASS_FC_SCALE = 1 / Math.pow(Math.SQRT2 - 1, 0.25);

// The backward pass starts at the newest sample, holding its state at that sample's value, and
// needs time to shed that start-up transient: edges are accepted only this many of the pass's
// envelope time constants (1/(Q·2π·fc)) back from the newest sample. Measured on a synthetic-
// ground-truth harness (sines, harmonic-rich tones, glides, noise, 20 Hz-1 kHz cutoffs, 0.2-5
// ms/div): 3 still left up to ±0.6 ms of error at short timebases and low cutoffs, 5 brings every
// case to within a sample or two. At the default 5 ms/div the post-trigger span already covers it,
// so it only adds display lag at short timebases or very low cutoffs.
export const TRIGGER_SETTLE_TAUS = 5;

// AC coupling: the level an edge must rise through is the detector signal's mean over this span,
// centred on the candidate edge, instead of 0. Oscilloscope music shifts its image with
// near-DC content, which can hold the whole waveform above or below zero for seconds, so a
// fixed level of 0 finds no crossing at all and the scope free-runs. Music without such an offset
// averages to ~0 over the span and triggers as before.
//
// A centred average, not a high-pass filter: being symmetric it is zero-phase like the low-pass,
// and it settles in exactly half its span. A zero-phase IIR high-pass with a corner low enough
// to leave bass alone (~10 Hz) needs ~140 ms to settle, all of it display lag; a one-way one
// brings back the pitch-dependent timing error the zero-phase low-pass removed (~0.8 ms at 20 Hz
// for a 2 Hz corner). The search can only accept an edge with a display window's post-trigger
// span of signal after it, so half the span (25 ms) costs no lag at the default 5 ms/div.
// Long enough to span a full cycle down to 20 Hz, so the mean follows the offset, not the music.
export const TRIGGER_BASELINE_MS = 50;

/** RBJ low-pass biquad (cookbook form), in biquad.ts's `BiquadCoeffs` convention. Used only to
 *  condition the *trigger-detector* signal (HF-reject trigger coupling, same idea a real scope's
 *  trigger source filter uses) — the displayed trace is always full-bandwidth. */
export function lowpassCoeffs(f0: number, q: number, fs: number): BiquadCoeffs {
  const w0 = (2 * Math.PI * f0) / fs;
  const cosw0 = Math.cos(w0);
  const alpha = Math.sin(w0) / (2 * q);
  const a0 = 1 + alpha;
  return { b0: (1 - cosw0) / 2 / a0, b1: (1 - cosw0) / a0, b2: (1 - cosw0) / 2 / a0, a1: (-2 * cosw0) / a0, a2: (1 - alpha) / a0 };
}

/** The detector's filter and search state for one rate and knob setting. `state`: the forward
 *  pass, persistent across frames (a real filter, never re-zeroed). `back`: the backward pass,
 *  re-seeded every search. `settle`: the backward pass's start-up span in samples. `half`: half the
 *  baseline span in samples. `base`: scratch for the baseline, reused across frames. */
export type TriggerDetector = {
  coeffs: BiquadCoeffs;
  state: BiquadState;
  back: BiquadState;
  settle: number;
  half: number;
  base: Float64Array;
  rate: number;
  hz: number;
};

export function triggerDetector(rate: number, knobHz: number): TriggerDetector {
  const passHz = knobHz * TRIGGER_PASS_FC_SCALE;
  return {
    coeffs: lowpassCoeffs(passHz, TRIGGER_FILTER_Q, rate),
    state: zeroState(),
    back: zeroState(),
    settle: Math.ceil((TRIGGER_SETTLE_TAUS * rate) / (TRIGGER_FILTER_Q * 2 * Math.PI * passHz)),
    half: Math.round((rate * TRIGGER_BASELINE_MS) / 2000),
    base: new Float64Array(0),
    rate,
    hz: knobHz,
  };
}

/** The forward pass: the detector value to store for one incoming mono sample. */
export function detectorSample(d: TriggerDetector, mono: number): number {
  return stepBiquad(d.coeffs, d.state, mono);
}

/**
 * The most recent rising edge in `[tMin, tMax]` (global sample indices; `ring[phys(i)]` holds the
 * forward-filtered detector value of sample `i`, valid from `oldest` to `newest`), or null if none
 * qualifies — the caller then free-runs.
 *
 * The search *is* the backward pass: it runs the forward-filtered values back through the same
 * biquad from the newest sample, so it only ever covers the settle span plus the distance to the
 * edge it finds. An edge must be settled for both the backward pass and the baseline.
 */
export function findRisingEdge(
  d: TriggerDetector,
  ring: Float64Array,
  phys: (i: number) => number,
  oldest: number,
  newest: number,
  tMin: number,
  tMax: number,
): number | null {
  const h = d.half;
  const lo = Math.max(tMin, oldest + h);
  const cStart = Math.min(tMax, newest - Math.max(d.settle, h + 1)); // newest edge both have settled for
  if (cStart <= lo) return null;

  // The backward pass, into `zp`: the zero-phase detector signal over [first, newest]. Run lazily,
  // `h` samples ahead of the scan, so a search that finds a recent edge stops early as before.
  const first = lo - h;
  const len = newest - first + 1;
  if (d.base.length < len) d.base = new Float64Array(len);
  const zp = d.base;
  const b = d.back;
  const edge = ring[phys(newest)];
  b.x1 = b.x2 = b.y1 = b.y2 = edge; // as if the reversed input had sat at the edge value forever
  let filled = newest + 1; // lowest index computed so far
  const fillTo = (i: number) => {
    while (filled > i) {
      filled--;
      zp[filled - first] = stepBiquad(d.coeffs, b, ring[phys(filled)]);
    }
  };
  fillTo(cStart - h);

  // The baseline is the mean of that same zero-phase signal, a running sum over the centred span
  // moved down one sample at a time. Averaging the forward-only values instead would carry the
  // low-pass's phase delay into the level: the span's leftover of a tone (a 50 ms mean of a 50 Hz
  // sine is 13% of it) then wobbles out of step with the signal and shifts the crossing.
  const w = 2 * h + 1;
  let sum = 0;
  for (let i = cStart - h; i <= cStart + h; i++) sum += zp[i - first];
  let baseAbove = sum / w; // the baseline at c + 1 while walking down
  for (let c = cStart - 1; c >= lo; c--) {
    fillTo(c - h);
    sum += zp[c - h - first] - zp[c + h + 1 - first];
    const base = sum / w;
    // Rising crossing of the baseline between c and c + 1 in forward time; the edge is c + 1.
    if (zp[c - first] <= base && zp[c + 1 - first] > baseAbove) return c + 1;
    baseAbove = base;
  }
  return null;
}
