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

export type FilterKind = "Peaking" | "LowShelf" | "HighShelf";
export type Band = { kind: FilterKind; freq_hz: number; gain_db: number; q: number };

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
  if (kind === "Peaking") {
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
  for (const band of bands) {
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
  for (const band of bands) {
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
  for (const band of bands) {
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
