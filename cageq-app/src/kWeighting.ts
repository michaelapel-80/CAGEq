/**
 * ITU-R BS.1770-4 K-weighting — a direct TS port of `sidecar_dsp.py`'s `_KW_S1_B/A`, `_KW_S2_B/A`,
 * `_k_weight_power` (the same coefficients §4.1's loudness match already uses there, specified at
 * a fixed 48 kHz reference — a simplification that loudness match already accepts, and harmless
 * here too: the curve's only real features sit at ~38 Hz and ~1.5 kHz, both well away from where
 * bilinear-transform frequency warping at another sample rate would actually matter).
 *
 * Used as a perceptual-loudness "Tilt" option for SpectrumScope/EqChart's spectrum backdrop — a
 * real, standards-derived shape instead of an arbitrary dB/octave slope. Also exports the shared
 * `TiltMode` type and `tiltMode` persistence-migration coercion both views' tilt controls use.
 *
 * A generic direct complex evaluation (real/imaginary trig terms, mirroring Python's
 * `z = exp(-1j·w)`, `|num/den|²`), not `biquad.ts`'s `filterResponseDb` — that one's "phi"
 * shortcut exploits the RBJ cookbook's specific gain-split coefficient form, which these two
 * K-weighting biquads (plain designed b/a pairs, not RBJ-derived) aren't in.
 */

const KW_FS = 48000;
const KW_S1_B: readonly [number, number, number] = [1.53512485958697, -2.69169618940638, 1.19839281085285];
const KW_S1_A: readonly [number, number, number] = [1.0, -1.69065929318241, 0.73248077421585];
const KW_S2_B: readonly [number, number, number] = [1.0, -2.0, 1.0];
const KW_S2_A: readonly [number, number, number] = [1.0, -1.99004745483398, 0.99007225036621];

/** |H(f)|² for one biquad (plain b/a form, a0 assumed 1 — true of both K-weighting stages), at a
 *  fixed 48 kHz reference (`KW_FS`). */
function biquadMag2(b: readonly [number, number, number], a: readonly [number, number, number], f: number): number {
  const w = (2 * Math.PI * f) / KW_FS;
  const c1 = Math.cos(w);
  const s1 = -Math.sin(w); // z^-1 = e^{-jw} = cos(w) - j sin(w)
  const c2 = Math.cos(2 * w);
  const s2 = -Math.sin(2 * w);
  const numRe = b[0] + b[1] * c1 + b[2] * c2;
  const numIm = b[1] * s1 + b[2] * s2;
  const denRe = a[0] + a[1] * c1 + a[2] * c2;
  const denIm = a[1] * s1 + a[2] * s2;
  const numMag2 = numRe * numRe + numIm * numIm;
  const denMag2 = denRe * denRe + denIm * denIm;
  return numMag2 / denMag2;
}

/** The two-stage K-weighting cascade's power response in dB at `f`, raw (not yet normalized). */
function kWeightRawDb(f: number): number {
  const mag2 = biquadMag2(KW_S1_B, KW_S1_A, f) * biquadMag2(KW_S2_B, KW_S2_A, f);
  return 10 * Math.log10(mag2);
}

// Anchors the curve to 0 dB at 1 kHz, computed once — the same reference point the RTA tilt
// already preserves a tone's true level at, so a 1 kHz tone reads identically in every tilt mode.
const KW_REF_1K_DB = kWeightRawDb(1000);

/** The K-weighting curve in dB at `f`, normalized to 0 dB at 1 kHz. */
export function kWeightingDb(f: number): number {
  return kWeightRawDb(f) - KW_REF_1K_DB;
}

/** The three spectrum-backdrop tilt modes SpectrumScope and EqChart each pick independently
 *  (own `Params.tilt`/`SpecParams.tilt`, own storage key — see each view's own doc for why they
 *  stay independent rather than sharing one setting). "off": the density-correct reading as-is.
 *  "rta": the conventional analyzer reading (`db_raw`/`db_lin_raw`) instead. "kweighted": the RTA
 *  baseline plus {@link kWeightingDb} layered on top — a real, standards-derived perceptual-
 *  loudness tilt instead of an arbitrary dB/octave slope (à la FabFilter Pro-Q's default 4.5
 *  dB/oct Tilt), reusing the exact curve the project's own §4.1 loudness match already trusts. */
export type TiltMode = "off" | "rta" | "kweighted";

/** A saved tilt setting predates the three-mode version (was a plain boolean) — both views'
 *  `useTunableParams` merge `{...defaults, ...stored}`, so an old `true`/`false` would otherwise
 *  leak through as an invalid value. Coerced explicitly wherever a mode is read, not just
 *  defaulted, since a naive `!== "off"` fallback would silently turn a saved `false` (tilt was
 *  off) into the RTA path instead. */
export function tiltMode(v: TiltMode | boolean): TiltMode {
  return v === true ? "rta" : v === false ? "off" : v;
}
