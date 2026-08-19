/**
 * Shared Fc → colour mapping: warm low → cool high, the bass→treble metaphor, used everywhere a
 * frequency needs a glanceable colour rather than a number — ToneGrid's per-band Fc readout and
 * SpectrumScope's peak readout both key off this same scale, so a given frequency reads as the
 * same hue in both places.
 */

/** Position of `v` on a log scale from `lo`→`hi`, clamped 0..1. */
export const logNorm = (v: number, lo: number, hi: number) => {
  const t = (Math.log(v) - Math.log(lo)) / (Math.log(hi) - Math.log(lo));
  return Math.max(0, Math.min(1, t));
};

// Interpolated in HUE space (not RGB), warm low → cool high, so the mids stay vivid (an
// orange<->blue RGB blend greys out through the middle, where most bands live). The window is
// narrowed to the musical range so typical content spans the whole sweep — sub-bass pins warm,
// the top octave pins cool — instead of bunching up in the blue.
const FC_HUE_LO = 30; // warm orange at the low end (bass)
const FC_HUE_HI = 250; // blue-violet at the top (air)
const FC_HUE_F_LO = 100; // Hz that maps to the warm end
const FC_HUE_F_HI = 15000; // Hz that maps to the cool end

export const fcHue = (hz: number) =>
  `hsl(${Math.round(FC_HUE_LO + (FC_HUE_HI - FC_HUE_LO) * logNorm(hz, FC_HUE_F_LO, FC_HUE_F_HI))}, 58%, 56%)`;
