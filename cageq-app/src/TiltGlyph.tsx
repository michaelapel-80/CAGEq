import type { TiltMode } from "./kWeighting";

/** Tiny slope glyphs for the Off/RTA/K-weighted Tilt toggle (SpectrumScope.tsx, EqChart.tsx),
 *  replacing text labels — "K-W" was word-wrapping in the toggle's cramped `.vs-tune-seg` width
 *  (see App.css's own doc on that class, sized for `.vs-tuning`'s narrow panel). Each glyph is
 *  literally what pink noise reads as in that mode: Off is the real -3 dB/octave spectral-density
 *  decline; RTA is flat (the whole point of that mode, a +3 dB/octave tilt applied to cancel the
 *  natural decline — see `scope.tiltRta`'s own doc for why the tooltip names that applied tilt,
 *  not this resulting flat line); K-weighted's path is a hand-tuned curve (user-supplied), not a
 *  literal sample of `kWeightingDb` — a first attempt plotting the real function's exact points
 *  came out too close to flat-then-flat in the middle to read as a curve at all at this size; this
 *  keeps the real shape's character (steep low-end rise, gentler middle, rise to a treble plateau)
 *  while staying legible that small. */
const KW_PATH =
  "m 1.1596579,11.076718 c 0,0 1.3206481,-4.4763323 6.3424516,-4.458056 4.9701595,0.018088 5.3801815,-0.028534 7.6582425,-1.8149339 2.156681,-1.6912174 4.092726,-1.8679578 5.129341,-1.8764163 1.371624,-0.011192 0.992536,-0.029673 2.618166,-0.022913";

/** `stroke="currentColor"` picks up `.pl-toggle button`'s own active/hover color for free — no
 *  separate glyph styling needed. The button itself still needs its own `title`/`aria-label`
 *  (the mode name that used to be the visible text) since the glyph alone carries no text for a
 *  tooltip or a screen reader. */
export function TiltGlyph({ mode }: { mode: TiltMode }) {
  const d = mode === "off" ? "M4 4 L20 10" : mode === "rta" ? "M4 7 L20 7" : KW_PATH;
  return (
    <svg viewBox="0 0 24 14" width="18" height="11" aria-hidden="true">
      <path d={d} fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}
