/**
 * Biquad response maths for the §5.2 chart.
 *
 * This is a deliberate, exact mirror of AutoEq's `autoeq/peq.py` — both
 * `biquad_coefficients()` and `PEQFilter.fr` (its numerically-stable `phi` form).
 * Keeping the same model matters: the DSP fits with it and the config-writer exports
 * the same Fc/Gain/Q to EqAPO, so a curve drawn with different conventions (shelf Q in
 * particular) would quietly lie about what you're hearing.
 *
 * Computing this client-side is what makes dragging instant — no IPC round-trip per
 * frame (filter.md §5.2 performance rule: a drag recomputes only the additive curve).
 */

export type FilterKind = "Peaking" | "LowShelf" | "HighShelf" | "Bandpass" | "Tilt";
export type Band = { kind: FilterKind; freq_hz: number; gain_db: number; q: number };

/**
 * Expand every `Tilt` band into the complementary shelf pair that actually realises it
 * — a low-shelf cut and a high-shelf boost of equal-and-opposite magnitude, pivoting at
 * the same `freq_hz` — passing every other band through unchanged. Mirrors
 * `cageq_backend::expand_tilts` (Rust); {@link coefficients} has no tilt formula of its
 * own, by the same reasoning as that function's doc: neither EqualizerAPO nor the RBJ
 * cookbook has a native single-stage tilt, so every entry point below that walks a
 * `Band[]` calls this first instead.
 */
export function expandTilts(bands: Band[]): Band[] {
  const out: Band[] = [];
  for (const b of bands) {
    if (b.kind === "Tilt") {
      out.push({ kind: "LowShelf", freq_hz: b.freq_hz, gain_db: -b.gain_db / 2, q: b.q });
      out.push({ kind: "HighShelf", freq_hz: b.freq_hz, gain_db: b.gain_db / 2, q: b.q });
    } else {
      out.push(b);
    }
  }
  return out;
}

/** Sample rate the filters are defined against (matches the sidecar's `fs` default). */
export const FS = 48000;

/**
 * AutoEq's `biquad_coefficients()`: `[a0, a1, a2, b0, b1, b2]`, with `a1`/`a2` already
 * negated (its `fr` re-negates them — mirrored in {@link filterResponseDb}).
 */
function coefficients(kind: FilterKind, fc: number, gainDb: number, q: number, fs: number) {
  const a = Math.pow(10, gainDb / 40);
  const w0 = (2 * Math.PI * fc) / fs;
  const alpha = Math.sin(w0) / (2 * q);
  const cosw = Math.cos(w0);
  const sqrtA = Math.sqrt(a);

  let a0: number, a1: number, a2: number, b0: number, b1: number, b2: number;
  if (kind === "Tilt") {
    // No native tilt formula, by construction — see expandTilts's doc. Every real call
    // site expands first; landing here means one didn't.
    throw new Error("coefficients() called with Tilt — expandTilts must run first");
  } else if (kind === "Bandpass") {
    // RBJ band-pass, 0 dB peak (gain ignored — the §5.2 isolate audition uses unity peak).
    a0 = 1 + alpha;
    a1 = -(-2 * cosw) / a0;
    a2 = -(1 - alpha) / a0;
    b0 = alpha / a0;
    b1 = 0;
    b2 = -alpha / a0;
  } else if (kind === "Peaking") {
    a0 = 1 + alpha / a;
    a1 = -(-2 * cosw) / a0;
    a2 = -(1 - alpha / a) / a0;
    b0 = (1 + alpha * a) / a0;
    b1 = (-2 * cosw) / a0;
    b2 = (1 - alpha * a) / a0;
  } else if (kind === "LowShelf") {
    a0 = a + 1 + (a - 1) * cosw + 2 * sqrtA * alpha;
    a1 = -(-2 * (a - 1 + (a + 1) * cosw)) / a0;
    a2 = -(a + 1 + (a - 1) * cosw - 2 * sqrtA * alpha) / a0;
    b0 = (a * (a + 1 - (a - 1) * cosw + 2 * sqrtA * alpha)) / a0;
    b1 = (2 * a * (a - 1 - (a + 1) * cosw)) / a0;
    b2 = (a * (a + 1 - (a - 1) * cosw - 2 * sqrtA * alpha)) / a0;
  } else {
    a0 = a + 1 - (a - 1) * cosw + 2 * sqrtA * alpha;
    a1 = -(2 * (a - 1 - (a + 1) * cosw)) / a0;
    a2 = -(a + 1 - (a - 1) * cosw - 2 * sqrtA * alpha) / a0;
    b0 = (a * (a + 1 + (a - 1) * cosw + 2 * sqrtA * alpha)) / a0;
    b1 = (-2 * a * (a - 1 + (a + 1) * cosw)) / a0;
    b2 = (a * (a + 1 + (a - 1) * cosw - 2 * sqrtA * alpha)) / a0;
  }
  return [1.0, a1, a2, b0, b1, b2] as const;
}

/** Difference-equation coefficients (a0 = 1) for one band's biquad in the direct-form convention
 *  `y = b0·x + b1·x₁ + b2·x₂ − a1·y₁ − a2·y₂` — the a's un-negated from {@link coefficients}. */
export type BiquadCoeffs = { b0: number; b1: number; b2: number; a1: number; a2: number };
export function biquadCoeffs(band: Band, fs = FS): BiquadCoeffs {
  const [, a1, a2, b0, b1, b2] = coefficients(band.kind, band.freq_hz, band.gain_db, band.q, fs);
  return { b0, b1, b2, a1: -a1, a2: -a2 };
}

/** The **inverse** biquad: fed a sample that band already filtered, it returns the original —
 *  1/H(z), i.e. numerator and denominator swapped (then renormalised to a0 = 1). Cascade the
 *  inverses of every band (reverse order) to undo an EQ chain in the time domain. Exact for the
 *  minimum-phase EQ this app builds; deep cuts become peaks in the inverse (noise-amplifying). */
export function inverseBiquadCoeffs(band: Band, fs = FS): BiquadCoeffs {
  const { b0, b1, b2, a1, a2 } = biquadCoeffs(band, fs);
  return { b0: 1 / b0, b1: a1 / b0, b2: a2 / b0, a1: b1 / b0, a2: b2 / b0 };
}

/** Per-biquad running state for sample-by-sample filtering (Direct Form I), one set per channel —
 *  used to run a cascade (e.g. the inverse cascade above, for undistort) live over a sample stream,
 *  as opposed to the frequency-domain `filterResponseDb`/`composedCurveDb` used for chart curves. */
export type BiquadState = { x1: number; x2: number; y1: number; y2: number };
export const zeroState = (): BiquadState => ({ x1: 0, x2: 0, y1: 0, y2: 0 });
export function stepBiquad(c: BiquadCoeffs, s: BiquadState, x: number): number {
  const y = c.b0 * x + c.b1 * s.x1 + c.b2 * s.x2 - c.a1 * s.y1 - c.a2 * s.y2;
  s.x2 = s.x1;
  s.x1 = x;
  s.y2 = s.y1;
  s.y1 = y;
  return y;
}

/** A built inverse cascade + its own running per-channel state — an "undistort" filter for one
 *  particular EQ, ready to run sample-by-sample. */
export type InverseCascade = { coeffs: BiquadCoeffs[]; stateL: BiquadState[]; stateR: BiquadState[]; gain: number };

export function buildInverseCascade(filters: Band[], preampDb: number, fs: number): InverseCascade {
  const coeffs = expandTilts(filters).map((b) => inverseBiquadCoeffs(b, fs)).reverse(); // undo in reverse order
  return { coeffs, stateL: coeffs.map(zeroState), stateR: coeffs.map(zeroState), gain: Math.pow(10, preampDb / 20) };
}

function stepInverseCascade(c: InverseCascade, l: number, r: number): [number, number] {
  l /= c.gain; // undo the preamp, then run the inverse cascade sample-by-sample
  r /= c.gain;
  for (let k = 0; k < c.coeffs.length; k++) {
    l = stepBiquad(c.coeffs[k], c.stateL[k], l);
    r = stepBiquad(c.coeffs[k], c.stateR[k], r);
  }
  return [l, r];
}

/** How long the undistort views crossfade an outgoing correction (sample-domain cascade or
 *  frequency-domain curve) into a new one, in ms. Matches the ~10 ms raised-cosine both real
 *  backends use for a slot/dry switch (`DRY_FADE_MS` in `cageq-apo/src/dsp.rs`) — not a claim of
 *  sample-accurate sync with whatever the real engine (a separate process, with no shared clock)
 *  is actually doing at that instant, just close enough that a display stops manufacturing a
 *  discontinuity of its own. */
export const UNDISTORT_FADE_MS = 10;

/**
 * Crossfading pair of {@link InverseCascade}s: retargeting used to swap the running cascade the
 * instant new filters arrived, which is exactly the bug this fixes. A scope/spectrum that snaps
 * to a new inverse filter mid-stream applies the *new* filter's math to samples that are still
 * the *old* filter's real output — a genuine, computable discontinuity that looks exactly like a
 * DSP glitch, even though the real (crossfaded) audio never had one. See `apo-switch-artifacts`
 * memory, "NOT a bug", for how that got mistaken for one.
 *
 * Deliberately simpler than the real engine's mid-switch handling (`Cascade::apply_coeffs`'s
 * `switch_outgoing`, which pins the *original* pre-switch chain even through a retarget): this
 * always fades from whatever the previous target was, so a rapid run of retargets (a live tone
 * drag) just keeps chaining short fades rather than tracking one true origin. That is a visual
 * approximation, not the audio path, so smooth-and-simple wins over exact.
 */
export type FadingInverse = { from: InverseCascade | null; to: InverseCascade; filtersRef: Band[] | null; rate: number; fadeLeft: number; fadeFrames: number };

export function retargetFadingInverse(prev: FadingInverse | null, filters: Band[], preampDb: number, rate: number): FadingInverse {
  const to = buildInverseCascade(filters, preampDb, rate);
  // No prior cascade, or the sample rate itself changed (a device change, not a filter switch —
  // the old state doesn't even apply at the new rate): nothing to fade from.
  if (!prev || prev.rate !== rate) {
    return { from: null, to, filtersRef: filters, rate, fadeLeft: 0, fadeFrames: 1 };
  }
  const fadeFrames = Math.max(1, Math.round(rate * (UNDISTORT_FADE_MS / 1000)));
  return { from: prev.to, to, filtersRef: filters, rate, fadeLeft: fadeFrames, fadeFrames };
}

/** Advance one sample through a `FadingInverse`, returning the (possibly blended) undistorted
 *  L/R pair. Mutates `f` (decrements the fade, drops `from` once it settles) — same "runs down,
 *  then goes away" shape as the real engine's outgoing chain. */
export function stepFadingInverse(f: FadingInverse, l: number, r: number): [number, number] {
  const [tl, tr] = stepInverseCascade(f.to, l, r);
  if (f.from === null || f.fadeLeft <= 0) return [tl, tr];
  const [fl, fr] = stepInverseCascade(f.from, l, r);
  const t = 1 - f.fadeLeft / f.fadeFrames;
  const w = 0.5 - 0.5 * Math.cos(Math.PI * t); // raised cosine, matching the real engines' own crossfade
  f.fadeLeft--;
  if (f.fadeLeft <= 0) f.from = null;
  return [fl + (tl - fl) * w, fr + (tr - fr) * w];
}

/**
 * Frequency-domain analogue of {@link FadingInverse}, for views that only ever have a per-bin dB
 * correction curve — not raw samples to run an actual crossfading filter over (SpectrumScope's
 * trace, EqChart's spectrum backdrop). Same fix, same reasoning: snapping the curve the instant
 * the EQ changes draws a one-frame jump that isn't in the real (crossfaded) audio at all.
 */
export type FadingCurve = { from: Float64Array | null; to: Float64Array; fadeMs: number };

/** `to.length` differing from the previous curve means a bin-count change (a resize), not a
 *  filter switch — nothing meaningful to fade from. */
export function retargetFadingCurve(prev: FadingCurve | null, to: Float64Array): FadingCurve {
  if (!prev || prev.to.length !== to.length) return { from: null, to, fadeMs: UNDISTORT_FADE_MS };
  return { from: prev.to, to, fadeMs: 0 };
}

/** Advance a `FadingCurve` by `dtMs` of wall-clock time (this is frame-rate-driven, not
 *  sample-driven — there's no sample stream here) and return the blended curve. Mutates `f`. */
export function stepFadingCurve(f: FadingCurve, dtMs: number): Float64Array {
  if (f.from === null) return f.to;
  f.fadeMs = Math.min(UNDISTORT_FADE_MS, f.fadeMs + dtMs);
  const t = f.fadeMs / UNDISTORT_FADE_MS;
  const w = 0.5 - 0.5 * Math.cos(Math.PI * t); // raised cosine, matching the real engines' own crossfade
  const out = new Float64Array(f.to.length);
  for (let i = 0; i < out.length; i++) out[i] = f.from[i] + (f.to[i] - f.from[i]) * w;
  if (f.fadeMs >= UNDISTORT_FADE_MS) f.from = null;
  return out;
}

/** One band's magnitude response in dB over `freqs` (mirrors `PEQFilter.fr`). */
export function filterResponseDb(band: Band, freqs: Float64Array, fs = FS): Float64Array {
  let [a0, a1, a2, b0, b1, b2] = coefficients(band.kind, band.freq_hz, band.gain_db, band.q, fs);
  a1 = -a1; // AutoEq flips these back before evaluating
  a2 = -a2;

  const bSum = (b0 + b1 + b2) ** 2;
  const aSum = (a0 + a1 + a2) ** 2;
  const out = new Float64Array(freqs.length);
  for (let i = 0; i < freqs.length; i++) {
    const w = (2 * Math.PI * freqs[i]) / fs;
    const phi = 4 * Math.sin(w / 2) ** 2;
    const num = bSum + (b0 * b2 * phi - (b1 * (b0 + b2) + 4 * b0 * b2)) * phi;
    const den = aSum + (a0 * a2 * phi - (a1 * (a0 + a2) + 4 * a0 * a2)) * phi;
    out[i] = 10 * Math.log10(num) - 10 * Math.log10(den);
  }
  return out;
}

/** The composed EQ curve: every band summed (a biquad cascade adds in dB). */
export function composedCurveDb(bands: Band[], freqs: Float64Array, fs = FS): Float64Array {
  const total = new Float64Array(freqs.length);
  for (const band of expandTilts(bands)) {
    const r = filterResponseDb(band, freqs, fs);
    for (let i = 0; i < total.length; i++) total[i] += r[i];
  }
  return total;
}

/** Composed **phase** response in degrees over `freqs` — the phase shift the filter chain
 *  introduces (a nerd overlay; the magnitude is what you hear). A cascade multiplies, so
 *  phases add: sum each biquad's arg(H(e^jω)) = arg(numerator) − arg(denominator). Not
 *  wrapped — a minimum-phase EQ stays bounded and wrapping would add fake ±180° jumps.
 *  Uses the same coefficients as {@link filterResponseDb}; the denominator's true a1/a2 are
 *  the negation of what `coefficients()` returns (that helper pre-negates them, a0 = 1). */
export function phaseDeg(bands: Band[], freqs: Float64Array, fs = FS): Float64Array {
  const out = new Float64Array(freqs.length);
  for (const band of expandTilts(bands)) {
    const [, a1, a2, b0, b1, b2] = coefficients(band.kind, band.freq_hz, band.gain_db, band.q, fs);
    const a1t = -a1;
    const a2t = -a2;
    for (let i = 0; i < freqs.length; i++) {
      const w = (2 * Math.PI * freqs[i]) / fs;
      const c1 = Math.cos(w);
      const s1 = Math.sin(w);
      const c2 = Math.cos(2 * w);
      const s2 = Math.sin(2 * w);
      // e^{-jω} = c1 − j·s1, e^{-2jω} = c2 − j·s2
      const nRe = b0 + b1 * c1 + b2 * c2;
      const nIm = -(b1 * s1 + b2 * s2);
      const dRe = 1 + a1t * c1 + a2t * c2;
      const dIm = -(a1t * s1 + a2t * s2);
      out[i] += Math.atan2(nIm, nRe) - Math.atan2(dIm, dRe);
    }
  }
  for (let i = 0; i < out.length; i++) out[i] *= 180 / Math.PI;
  return out;
}

/** The filter chain's **impulse response** h[n] — a unit impulse cascaded through each
 *  biquad's difference equation (Direct Form I): the time-domain "ring" of the EQ. Exact
 *  (no FFT). `n` samples at `fs`. Same coefficient convention as above. */
export function impulseResponse(bands: Band[], n = 480, fs = FS): Float64Array {
  let sig = new Float64Array(n);
  sig[0] = 1;
  for (const band of expandTilts(bands)) {
    const [, a1, a2, b0, b1, b2] = coefficients(band.kind, band.freq_hz, band.gain_db, band.q, fs);
    const a1t = -a1;
    const a2t = -a2;
    const out = new Float64Array(n);
    let x1 = 0;
    let x2 = 0;
    let y1 = 0;
    let y2 = 0;
    for (let i = 0; i < n; i++) {
      const x = sig[i];
      const y = b0 * x + b1 * x1 + b2 * x2 - a1t * y1 - a2t * y2;
      out[i] = y;
      x2 = x1;
      x1 = x;
      y2 = y1;
      y1 = y;
    }
    sig = out;
  }
  return sig;
}

/** Log-spaced frequency grid (constant ratio per octave), the audio-standard axis. */
export function logGrid(points = 480, fMin = 20, fMax = 20000): Float64Array {
  const out = new Float64Array(points);
  const ratio = Math.log(fMax / fMin);
  for (let i = 0; i < points; i++) out[i] = fMin * Math.exp((ratio * i) / (points - 1));
  return out;
}
