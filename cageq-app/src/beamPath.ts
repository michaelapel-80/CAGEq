// The GPU beam's path between samples (TimeScope, Vectorscope): the band-limited signal the samples
// stand for, not straight chords between them. Chords put the beam's speed in the wrong places —
// constant along each one, where the real signal slows into its turning points — and at every join
// the light piled up into a dot. Measured against a finely sampled sine at 16 samples per cycle:
// chords 0.36 rms off (worst +0.65 of the peak), reconstructed 0.05 (+0.03, with phosphor.ts's
// round spot). Each sample interval is drawn in a piece per PIECE_PX of on-screen travel, at most
// MAX_PIECES (sparse samples need pieces of ~6 px or less to reach that), each interpolated with
// Lanczos (LANCZOS_A lobes) from a table of LANCZOS_PHASES fractional positions.

export const PIECE_PX = 4;
export const MAX_PIECES = 16;
export const LANCZOS_A = 4;
export const LANCZOS_PHASES = 64;
/** Taps per interpolated value. */
export const LANCZOS_TAPS = 2 * LANCZOS_A;

// Row `ph` holds the LANCZOS_TAPS weights for a position ph/LANCZOS_PHASES past a sample, taps from
// LANCZOS_A − 1 samples before it to LANCZOS_A after; each row normalized to sum to 1, so a constant
// stays exactly constant (the raw kernel's sum ripples by a fraction of a percent).
const LANCZOS_TABLE = (() => {
  const t = new Float32Array((LANCZOS_PHASES + 1) * LANCZOS_TAPS);
  for (let ph = 0; ph <= LANCZOS_PHASES; ph++) {
    const f = ph / LANCZOS_PHASES;
    let sum = 0;
    for (let k = 0; k < LANCZOS_TAPS; k++) {
      const x = f - (k - LANCZOS_A + 1);
      const w = Math.abs(x) < 1e-9 ? 1 : (LANCZOS_A * Math.sin(Math.PI * x) * Math.sin((Math.PI * x) / LANCZOS_A)) / (Math.PI * Math.PI * x * x);
      t[ph * LANCZOS_TAPS + k] = w;
      sum += w;
    }
    for (let k = 0; k < LANCZOS_TAPS; k++) t[ph * LANCZOS_TAPS + k] /= sum;
  }
  return t;
})();

/** The table row for a position `frac` (0..1) of the way through a sample interval. */
export function lanczosRow(frac: number): number {
  return Math.round(frac * LANCZOS_PHASES) * LANCZOS_TAPS;
}

/** The band-limited value at `row`'s position in interval `i` of `vals`, a series padded by
 *  LANCZOS_A on each side (sample `s` at `vals[s + LANCZOS_A]`, the ends held beyond it). */
export function lanczosAt(vals: Float32Array, i: number, row: number): number {
  let v = 0;
  // Taps i − LANCZOS_A + 1 … i + LANCZOS_A, which sit at vals[i + 1 …].
  for (let k = 0; k < LANCZOS_TAPS; k++) v += vals[i + 1 + k] * LANCZOS_TABLE[row + k];
  return v;
}
