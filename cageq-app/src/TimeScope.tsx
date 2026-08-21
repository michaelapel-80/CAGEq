import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { type Band, type BiquadCoeffs, type BiquadState, inverseBiquadCoeffs, zeroState, stepBiquad } from "./biquad";
import { scopeStream } from "./streams";
import { createPhosphor } from "./phosphor";
import type { ScopeData, ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters (see Vectorscope's identical rationale — a live panel beats a
 *  recompile). `undistort` shares Vectorscope's meaning and machinery: it undoes the §4.1/§4.2
 *  preamp *and* inverse-filters the applied EQ, recovering the pre-EQ level and shape rather than
 *  just amplifying the post-EQ trace — a manual gain knob would need re-tuning every time the
 *  preamp changes (a different headphone/target/slot), this tracks it automatically. `msPerDivIdx`
 *  is an index into `MS_PER_DIV_STEPS` (a classic scope-dial time/div, not a free-form span) —
 *  decoupled from the backend's ~16 ms emission size once a ring buffer is needed for triggering
 *  anyway, so it's exposed in both modes, not just when triggered; there's deliberately no
 *  longer-timescale/envelope mode, this stays a short-timescale instrument.
 *  `trigger`/`triggerFilterHz` are surfaced separately (`trigger` as a quick toolbar toggle, not a
 *  panel slider — it's a mode switch flipped often, same reasoning as the L/R⇄Mix toggle). */
type Params = { trailTau: number; tail: number; glow: number; beam: number; undistort: boolean; msPerDivIdx: number; trigger: boolean; triggerFilterHz: number };
// Classic 1-2-5 time/div sequence, same convention a real scope's dial steps through. 10 divisions
// (DIVISIONS) is the standard horizontal graticule count, so total span = ms/div × 10.
const MS_PER_DIV_STEPS = [0.2, 0.5, 1, 2, 5, 10];
const DIVISIONS = 10;
const DEFAULTS: Params = { trailTau: 0.06, tail: 12, glow: 0.8, beam: 3.0, undistort: true, msPerDivIdx: 4, trigger: true, triggerFilterHz: 80 }; // 2 ms/div × 10 = 20 ms
const REF_SIZE = 512; // beam width authored against this reference height, then scaled
const GRID_ALPHA = 0.22;
const DIV_LINE_ALPHA = GRID_ALPHA * 0.6; // division ticks read as finer/subtler than the lane centrelines
// Peak-hold ballistics for the faint clip-reference lines — instant attack, brief hold, then
// linear-dB release (a PPM-style follower, same shape as the level meter's — values mirror
// cageq-monitor's PEAK_RELEASE_DB_PER_SEC/PEAK_HOLD/DB_FLOOR for a consistent feel app-wide).
// Tracked locally per lane from the *displayed* samples (not the meter's own value) so it stays
// correct in both modes: with undistort on, the meter's raw-output peak wouldn't match a trace
// that's now been amplified/reshaped — the line would sit *inside* the beam it's supposed to
// reference. Tracking the same outL/outR the trace itself draws is correct by construction.
const PEAK_RELEASE_DB_PER_SEC = 20;
const PEAK_HOLD_MS = 1200;
const DB_FLOOR = -120;
// Peak *detection* takes the K-th largest rectified sample in the block, not the plain max and
// not a windowed/smoothed value — a sustained tone has many samples near its own peak, one every
// cycle for as long as it lasts, scattered across the whole block rather than adjacent; an
// isolated glitch — lossy-codec pre/post-echo artifacts, or ringing from the undistort inverse
// filter (a deep EQ cut inverts into a resonant boost, see inverseBiquadCoeffs in biquad.ts, which
// decays cycle to cycle rather than sustaining) — only ever has a handful, clustered right at its
// start. Requiring K corroborating samples *anywhere* in the block (not contiguous) tells the two
// apart correctly regardless of frequency: two earlier attempts at a contiguous window — a
// one-pole envelope, then a sliding-window erosion — both discounted *every* brief event in
// proportion to its width, so widening either to catch glitches also read genuine high-frequency
// content low (its rectified envelope dips near zero every half-cycle, same as a glitch decaying,
// at any window wide enough to matter). Expressed as a duration (PEAK_CONFIRM_MS → K = samples in
// that many ms) so it's identical in effect at 44.1/48/96/192 kHz. Only affects this reference
// line — the drawn trace itself is untouched.
const PEAK_CONFIRM_MS = 1.0;
// Peak-line opacity. These used to be drawn *into* the accumulating trail every frame with
// source-over at 0.35, which doesn't stay at 0.35: re-blending over the faded previous frame
// converges on L = 0.35·C + 0.65·(L·keep), i.e. ~0.65·C at the default trail (and brighter still at
// longer ones — the reference line's brightness silently tracked a cosmetic slider). Drawn once per
// frame onto a cleared layer it would be exactly 0.35·C, about half as bright, which is what made
// them look washed out after the move. This is that convergence point, now fixed and predictable
// instead of trail-dependent.
const PEAK_LINE_ALPHA = 0.65;

// --- Triggering ---------------------------------------------------------------------------
// A stable oscilloscope-style trigger needs pre/post-trigger history the raw ~16 ms `scope`
// windows alone don't provide, so incoming samples are appended to a ring buffer (raw display
// samples + a persistently-filtered mono trigger-detector signal, both written incrementally as
// data streams in — never re-filtered from scratch each frame, which would both waste work on the
// overlapping portion and reset the filter's state every frame, corrupting its settling).
const RING_CAP = 1 << 17; // ~2.7 s at 48 kHz / ~680 ms at 192 kHz — generous, trivial memory
const TRIGGER_PRE_FRAC = 0.25; // where the trigger point sits across the display window
const TRIGGER_SEARCH_MS = 250; // how far back to look for a qualifying edge before giving up
const TRIGGER_FILTER_Q = 0.707; // Butterworth — a clean rolloff, no resonant peaking

/** RBJ low-pass biquad (cookbook form), in this file's `BiquadCoeffs` convention. Used only to
 *  condition the *trigger-detector* signal (HF-reject trigger coupling, same idea a real scope's
 *  trigger source filter uses) — the displayed trace is always full-bandwidth. Kept local: it's a
 *  TimeScope-internal rendering detail, not part of the EQ pipeline other code needs to share. */
function lowpassCoeffs(f0: number, q: number, fs: number): BiquadCoeffs {
  const w0 = (2 * Math.PI * f0) / fs;
  const cosw0 = Math.cos(w0);
  const alpha = Math.sin(w0) / (2 * q);
  const a0 = 1 + alpha;
  return { b0: (1 - cosw0) / 2 / a0, b1: (1 - cosw0) / a0, b2: (1 - cosw0) / 2 / a0, a1: (-2 * cosw0) / a0, a2: (1 - alpha) / a0 };
}

/** Lane geometry (vertical centre + amplitude scale + label) for the current channel mode — shared
 *  between the graticule and the trace so they always agree. `amp` leaves a small margin (0.5 of
 *  the lane's half-height) so a full-scale sample doesn't touch the lane's edge/divider. */
type Lane = { cy: number; amp: number; label: string | null };
function laneLayout(mode: "lr" | "mix", H: number): Lane[] {
  if (mode === "mix") return [{ cy: H / 2, amp: H * 0.5, label: null }];
  const laneH = H / 2;
  return [
    { cy: laneH / 2, amp: laneH * 0.5, label: "L" },
    { cy: laneH + laneH / 2, amp: laneH * 0.5, label: "R" },
  ];
}

/** The K-th largest of `mag[0..n)` (rectified samples) — the peak-detection core described at
 *  `PEAK_CONFIRM_MS` above. `k = 1` is the plain max; `k` clamps to `n` for a block shorter than
 *  the confirmation window, which just falls back to the max rather than under-reading. Sorts
 *  `mag`'s first `n` entries in place (ascending — `%TypedArray%.sort()`'s numeric default, unlike
 *  `Array`'s lexicographic one, so no comparator needed) — the caller's scratch buffer, so it's
 *  fine to mutate; cheap at these block sizes (a handful of thousand samples at most) and simpler
 *  than a partial-selection algorithm for no measurable cost at this rate. */
function kthLargest(mag: Float64Array, n: number, k: number): number {
  if (n === 0) return 0;
  const view = mag.subarray(0, n);
  view.sort();
  return view[Math.max(0, n - Math.min(k, n))];
}

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback. */
function parseHex(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/**
 * §5.3d time-domain scope (v2, with triggering — see filter.md discussion): the loopback's L/R
 * channels plotted against time, stacked in two lanes (L on top, R on bottom), each with its own
 * zero-centreline, phosphor-style persistence. `trigger` off (default) is free-running — a fixed
 * span (ms/div × DIVISIONS) of the most recent samples, redrawn each `scope` event; on non-periodic material
 * (most real mixes) consecutive windows won't retrace the same shape, so persistence reads as a
 * soft blur rather than a locked waveform. `trigger` on searches a lowpass-filtered mono version of
 * the signal (HF-reject trigger coupling — the display itself stays full-bandwidth) for the most
 * recent rising zero-crossing within `TRIGGER_SEARCH_MS`, anchoring the display window there
 * instead; no qualifying edge in range falls back to the same free-running position (the auto-
 * trigger/timeout behaviour, for free — no separate timer needed since the search is itself bounded
 * to a fixed lookback each frame). Best on bass/kick-heavy or genuinely periodic (oscilloscope-
 * music) material; broadband, transient-heavy passages may still drift between frames.
 *
 * Deliberately does *not* auto-flag clipping (a v1 threshold highlight was tried and dropped): the
 * §4.2 headroom pre-gain already keeps normal operation away from true digital clipping, streaming
 * loudness normalization mostly erases what "hot" means by the time a listener hears it, and a
 * fixed |sample| threshold can't tell real clipping from a squarish kick transient sitting near
 * full-scale — a false-positive machine, not a useful indicator. Eyeballing flat-topping yourself
 * is still exactly what this view is for; it just shouldn't pretend to automate it.
 *
 * Sits to the left of the vectorscope in the scope chart view — same instrument row, same
 * "loopback monitor" family, sharing its `scope` event stream, its `scope-eq`-driven undistort
 * (inverse-filter cascade), and viewer-count gating with Vectorscope.
 */
export function TimeScope() {
  const { t } = useTranslation();
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  // The peak-hold lines get their own layer above the trail: they're redrawn at a constant alpha
  // every frame, and the trail accumulates *additively*, so drawing them into it would stack them
  // toward white instead of holding steady (see phosphor.ts's closing note).
  const peakRef = useRef<HTMLCanvasElement>(null);
  const scopeRef = useRef<ScopeData | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;
  // L/R (two half-height lanes) vs mixdown (one full-height lane, mono sum) — mixdown trades
  // channel separation for 2x the vertical scale, handy when you just want to see how hot/quiet
  // the signal is rather than compare channels. A quick toggle, not a tuning-panel knob — flipped
  // often enough to want single-click access.
  const [mode, setMode] = useState<"lr" | "mix">("mix");
  const modeRef = useRef(mode);
  modeRef.current = mode;

  // Width tracks the flex row's leftover space (not square, unlike the vectorscope); height tracks
  // `.chart-wrap`'s own `aspect-ratio:720/215` (App.css) via the same wrapper's rendered box, so
  // this instrument's height agrees with the Eq/Impulse views' viewBox-scaled SVGs instead of
  // drifting from a hardcoded value and shifting the layout on every chart-view switch.
  const [width, setWidth] = useState(320);
  const [height, setHeight] = useState(215);
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      setWidth(Math.max(80, Math.floor(r.width)));
      setHeight(Math.max(60, Math.floor(r.height)));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    measure();
    return () => ro.disconnect();
  }, []);
  const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
  const resW = Math.round(width * dpr);
  const resH = Math.round(height * dpr);

  // The active EQ cascade (for undistort) + the built inverse cascade and its running state —
  // identical structure to Vectorscope's, independent instance (own running filter state, since
  // this draws its own trace from the same raw stream).
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0 });
  const invRef = useRef<{
    active: boolean;
    filtersRef: Band[] | null;
    rate: number;
    coeffs: BiquadCoeffs[];
    stateL: BiquadState[];
    stateR: BiquadState[];
    gain: number;
  }>({ active: false, filtersRef: null, rate: 0, coeffs: [], stateL: [], stateR: [], gain: 1 });

  // Own the scope-stream + `scope-eq` subscriptions — independent of the vectorscope's, so either
  // can be open alone. The sample stream is a Channel-backed bus (streams.ts — not `listen`
  // events, whose per-event webview eval churned WebView2's memory at 60 fps); `scope-eq` stays a
  // real event, it only fires on EQ changes. `set_scope_viewer` is a shared count (see
  // Vectorscope/App) gating whether the backend produces scope data at all.
  useEffect(() => {
    let active = true;
    const unsubScope = scopeStream.subscribe((s) => {
      if (active) scopeRef.current = s;
    });
    let unlistenEq: (() => void) | undefined;
    void (async () => {
      unlistenEq = await listen<ScopeEq>("scope-eq", (e) => {
        if (active) eqRef.current = e.payload;
      });
      if (active) void emit("scope-eq-request");
    })();
    void invoke("set_scope_viewer", { active: true });
    return () => {
      active = false;
      unsubScope();
      unlistenEq?.();
      void invoke("set_scope_viewer", { active: false });
    };
  }, []);

  // Static graticule: a centreline per lane (+ the divider between them, in L/R mode). Redrawn on
  // resize or a mode switch (the lane layout itself changes).
  useEffect(() => {
    const cv = gridRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));
    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${GRID_ALPHA})`;
    ctx.lineWidth = Math.max(1, H / REF_SIZE);
    ctx.beginPath();
    if (mode === "lr") {
      ctx.moveTo(0, H / 2); // lane divider
      ctx.lineTo(W, H / 2);
    }
    for (const lane of laneLayout(mode, H)) {
      ctx.moveTo(0, lane.cy);
      ctx.lineTo(W, lane.cy);
    }
    ctx.stroke();
    // Time divisions — the classic scope graticule (see ms/div in the tuning panel). Fixed count
    // (DIVISIONS), independent of the actual ms/div value: only the *labelled* span changes with
    // that slider, the grid itself doesn't need to know it. Subtler than the lane centrelines.
    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${DIV_LINE_ALPHA})`;
    ctx.beginPath();
    for (let i = 1; i < DIVISIONS; i++) {
      const x = (i / DIVISIONS) * W;
      ctx.moveTo(x, 0);
      ctx.lineTo(x, H);
    }
    ctx.stroke();
    ctx.fillStyle = `rgba(${ar},${ag},${ab},0.5)`;
    ctx.font = `${Math.round(H * 0.05)}px system-ui, sans-serif`;
    ctx.textBaseline = "middle";
    for (const lane of laneLayout(mode, H)) if (lane.label) ctx.fillText(lane.label, 6, lane.cy - lane.amp * 0.55);
  }, [resW, resH, mode]);

  // The trace: this frame's beam goes onto a scratch canvas handed to the phosphor accumulator,
  // which owns the decay and the additive composite — same machinery as the vectorscope, see
  // phosphor.ts for why the decay isn't an in-place canvas fade any more.
  useEffect(() => {
    const cv = trailRef.current;
    const peakCv = peakRef.current;
    const peakCtx = peakCv?.getContext("2d");
    if (!cv || !peakCv || !peakCtx) return;
    const phos = createPhosphor(cv);
    if (!phos) return;
    // See SpectrumScope.tsx's identical log — diagnosing a machine-specific "one view reads darker
    // than the others" report by checking whether any view silently fell back off the GPU
    // half-float accumulator (phosphor.ts's `precise`).
    if (!phos.precise) console.warn("[TimeScope] phosphor fell back to the 8-bit canvas accumulator (no half-float GPU support)");
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    let drawn: ScopeData | null = null;
    // Per-lane peak-hold (dBFS) and the timestamp its release may resume after — indexed to match
    // `lanes` each frame; resized (and reset) on a mode switch, since "lane 0" means something
    // different in L/R vs mixdown. A brief reset on an intentional mode change is unsurprising.
    let peakDb: number[] = [];
    let peakHoldUntil: number[] = [];
    let magScratch = new Float64Array(0); // reused rectified-magnitude buffer for kthLargest

    // Triggering: a circular buffer of the (possibly undistorted) display samples, plus a parallel
    // buffer of a lowpass-filtered mono trigger-detector value at the same indices — both written
    // incrementally below as data arrives. `totalWritten` is a monotonic sample counter (not a
    // wrapping pointer); physical ring index = `phys(globalIndex)`. See the module-level comment.
    const ringL = new Float64Array(RING_CAP);
    const ringR = new Float64Array(RING_CAP);
    const ringTrig = new Float64Array(RING_CAP);
    let totalWritten = 0;
    const phys = (globalIdx: number) => (((globalIdx % RING_CAP) + RING_CAP) % RING_CAP);
    // The trigger filter's running state persists across windows (a real filter, not re-zeroed each
    // frame) — rebuilt only when the rate or the tuned cutoff changes, same pattern as `invRef`.
    const trig = { coeffs: null as BiquadCoeffs | null, state: zeroState(), rate: 0, hz: 0 };

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const p = paramsRef.current;
      const mode = modeRef.current;
      const W = cv.width;
      const H = cv.height;
      const lanes = laneLayout(mode, H);

      if (peakDb.length !== lanes.length) {
        peakDb = lanes.map(() => DB_FLOOR);
        peakHoldUntil = lanes.map(() => 0);
      }
      // Release: continuous decay once past the hold window, every frame regardless of whether new
      // data arrived this frame (so it doesn't step, it eases down smoothly between scope emits).
      for (let lane = 0; lane < lanes.length; lane++) {
        if (now >= peakHoldUntil[lane]) peakDb[lane] = Math.max(peakDb[lane] - PEAK_RELEASE_DB_PER_SEC * dt, DB_FLOOR);
      }

      const s = scopeRef.current;
      const n = s ? Math.floor(s.xy.length / 2) : 0;
      const isNew = !!s && s.signal && n >= 2 && s !== drawn;
      // Computed up front (before drawing) when new data arrives, so the peak-hold attack below and
      // the trace path further down both read the identical (possibly undistorted) samples.
      let outL: Float64Array | null = null;
      let outR: Float64Array | null = null;
      // The window actually drawn — free-run: the newest ms/div × DIVISIONS span; triggered:
      // anchored at the most recent qualifying edge found in the ring buffer. Set below, after
      // outL/outR exist.
      let dispL: Float64Array | null = null;
      let dispR: Float64Array | null = null;
      let dispN = 0;
      if (isNew) {
        drawn = s;
        const xy = s!.xy;

        // Undistort: (re)build the inverse cascade when the filters/rate change or the mode turns
        // on, then run each sample back through it — same machinery/rationale as Vectorscope's.
        const eq = eqRef.current;
        const rate = s!.rate && s!.rate > 0 ? s!.rate : 48000;
        const iv = invRef.current;
        if (p.undistort) {
          if (!iv.active || iv.filtersRef !== eq.filters || iv.rate !== rate) {
            iv.active = true;
            iv.filtersRef = eq.filters;
            iv.rate = rate;
            iv.coeffs = eq.filters.map((b) => inverseBiquadCoeffs(b, rate)).reverse(); // undo in reverse order
            iv.stateL = iv.coeffs.map(zeroState);
            iv.stateR = iv.coeffs.map(zeroState);
            iv.gain = Math.pow(10, eq.preampDb / 20);
          }
        } else {
          iv.active = false;
        }
        // Runs with an empty cascade too when there's just a preamp to undo (e.g. Dry, which
        // carries the §4.1 loudness-match gain but no EQ) — pure gain recovery, no filtering.
        const undistort = p.undistort && (iv.coeffs.length > 0 || iv.gain !== 1);
        outL = new Float64Array(n);
        outR = new Float64Array(n);
        for (let i = 0; i < n; i++) {
          let l = xy[2 * i];
          let r = xy[2 * i + 1];
          if (undistort) {
            l /= iv.gain; // undo the preamp, then run the inverse cascade sample-by-sample
            r /= iv.gain;
            for (let k = 0; k < iv.coeffs.length; k++) {
              l = stepBiquad(iv.coeffs[k], iv.stateL[k], l);
              r = stepBiquad(iv.coeffs[k], iv.stateR[k], r);
            }
          }
          outL[i] = l;
          outR[i] = r;
        }

        // Attack: this window's peak per lane, from the exact values about to be drawn — correct
        // in both modes by construction (it's the same array the trace path reads below). The
        // rectified signal's K-th largest sample over PEAK_CONFIRM_MS worth of samples — see there.
        const confirmSamples = Math.max(1, Math.round(rate * (PEAK_CONFIRM_MS / 1000)));
        if (magScratch.length < n) magScratch = new Float64Array(n);
        for (let lane = 0; lane < lanes.length; lane++) {
          for (let i = 0; i < n; i++) {
            magScratch[i] = Math.abs(mode === "mix" ? (outL[i] + outR[i]) / 2 : lane === 0 ? outL[i] : outR[i]);
          }
          const blockPeak = kthLargest(magScratch, n, confirmSamples);
          const blockDb = 20 * Math.log10(Math.max(blockPeak, 1e-6));
          if (blockDb >= peakDb[lane]) {
            peakDb[lane] = blockDb;
            peakHoldUntil[lane] = now + PEAK_HOLD_MS;
          }
        }

        // Feed the ring buffer: raw display samples plus a lowpass-filtered mono trigger-detector
        // value, one sample at a time so the filter's state stays continuous across windows (no
        // re-filtering, no transient reset). Rebuild the filter only when rate/cutoff changed.
        if (!trig.coeffs || trig.rate !== rate || trig.hz !== p.triggerFilterHz) {
          trig.coeffs = lowpassCoeffs(p.triggerFilterHz, TRIGGER_FILTER_Q, rate);
          trig.state = zeroState();
          trig.rate = rate;
          trig.hz = p.triggerFilterHz;
        }
        for (let i = 0; i < n; i++) {
          const idx = phys(totalWritten);
          ringL[idx] = outL[i];
          ringR[idx] = outR[i];
          ringTrig[idx] = stepBiquad(trig.coeffs, trig.state, (outL[i] + outR[i]) / 2);
          totalWritten++;
        }

        // Pick the display window: free-run always anchors at the newest valid position; triggered
        // searches backward (bounded to TRIGGER_SEARCH_MS) for the most recent rising zero-crossing
        // in the filtered trigger signal, falling back to the free-run anchor if none qualifies —
        // that fallback *is* the auto-trigger/timeout behaviour, no separate timer needed since the
        // search window itself is bounded fresh every frame.
        const msPerDiv = MS_PER_DIV_STEPS[p.msPerDivIdx] ?? MS_PER_DIV_STEPS[0];
        const windowSamples = Math.max(2, Math.round(rate * ((msPerDiv * DIVISIONS) / 1000)));
        const preSamples = Math.round(windowSamples * TRIGGER_PRE_FRAC);
        const postSamples = windowSamples - preSamples;
        const ringCount = Math.min(totalWritten, RING_CAP);
        if (ringCount >= windowSamples) {
          const tMax = totalWritten - postSamples; // newest position with enough post-trigger data
          let t = tMax;
          if (p.trigger) {
            const searchSamples = Math.round(rate * (TRIGGER_SEARCH_MS / 1000));
            const tMin = Math.max(totalWritten - ringCount + preSamples, tMax - searchSamples);
            for (let c = tMax; c > tMin; c--) {
              if (ringTrig[phys(c - 1)] <= 0 && ringTrig[phys(c)] > 0) {
                t = c;
                break;
              }
            }
          }
          dispN = windowSamples;
          dispL = new Float64Array(windowSamples);
          dispR = new Float64Array(windowSamples);
          for (let j = 0; j < windowSamples; j++) {
            const idx = phys(t - preSamples + j);
            dispL[j] = ringL[idx];
            dispR[j] = ringR[idx];
          }
        }
      }

      // 1) Start this frame's trace on a cleared scratch canvas; decaying everything before it is
      // the accumulator's job, applied in commit() below.
      const ctx = phos.begin();

      // 2) Faint peak-hold line(s), mirrored ± around each lane's centreline (a waveform is
      // bipolar; the peak tracked above is a magnitude). On their own cleared layer rather than in
      // the trail: they're redrawn at a constant alpha every frame and the trail is additive, so
      // accumulating them would stack them toward white. The layer sits *below* the trail (DOM
      // order — see the canvases below), preserving the original intent that the beam reads over
      // the reference line wherever they cross, rather than the line painting across the beam.
      peakCtx.clearRect(0, 0, W, H);
      peakCtx.strokeStyle = `rgba(230,162,60,${PEAK_LINE_ALPHA})`; // #e6a23c — same amber as .vbar-peak
      peakCtx.lineWidth = Math.max(1, H / REF_SIZE);
      peakCtx.beginPath();
      for (let lane = 0; lane < lanes.length; lane++) {
        const { cy, amp } = lanes[lane];
        const dy = Math.pow(10, peakDb[lane] / 20) * amp;
        peakCtx.moveTo(0, cy - dy);
        peakCtx.lineTo(W, cy - dy);
        peakCtx.moveTo(0, cy + dy);
        peakCtx.lineTo(W, cy + dy);
      }
      peakCtx.stroke();

      // 3) The trace itself — the extracted (free-run or triggered) display window, not the raw
      // per-event samples: the window is a fixed ms/div × DIVISIONS span, decoupled from the
      // backend's emission size, and (when triggered) anchored at the found edge rather than
      // "whatever just arrived".
      if (isNew && dispL && dispR && dispN >= 2) {
        // L/R: one path per channel/lane. Mixdown: one path, mono sum, the single full-height lane.
        const paths = lanes.map(() => new Path2D());
        const valueAt = (i: number, lane: number): number => (mode === "mix" ? (dispL![i] + dispR![i]) / 2 : lane === 0 ? dispL![i] : dispR![i]);
        for (let lane = 0; lane < lanes.length; lane++) {
          const { cy, amp } = lanes[lane];
          const path = paths[lane];
          path.moveTo(0, cy - valueAt(0, lane) * amp);
          for (let i = 1; i < dispN; i++) path.lineTo((i / (dispN - 1)) * W, cy - valueAt(i, lane) * amp);
        }
        ctx.globalCompositeOperation = "lighter";
        ctx.lineWidth = Math.max(0.6, p.beam * (H / REF_SIZE));
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.strokeStyle = `rgba(${ar},${ag},${ab},${p.glow})`;
        for (const path of paths) ctx.stroke(path);
      }

      // 4) Hand the frame's trace to the accumulator — it decays the history and adds this on top.
      phos.commit(dt, p.trailTau, p.tail);

      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose(); // frees the GL textures/programs; a leaked context would survive the unmount
    };
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  type NumKey = "trailTau" | "tail" | "glow" | "beam" | "triggerFilterHz";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "tail", label: t("scope.tail"), min: 1, max: 64, step: 1 },
    { key: "glow", label: t("scope.glow"), min: 0.05, max: 1, step: 0.05 },
    { key: "beam", label: t("scope.beam"), min: 0.1, max: 8, step: 0.05 },
    { key: "triggerFilterHz", label: t("scope.trigFilter"), min: 40, max: 1000, step: 10 },
  ];
  const msPerDiv = MS_PER_DIV_STEPS[params.msPerDivIdx] ?? MS_PER_DIV_STEPS[0];

  return (
    <div className="timescope-wrap" ref={wrapRef}>
      {/* width/height here are CSS "100%" (matching the wrap exactly, sub-pixel precise) — the
          JS-measured `width`/`resW`/`resH` state feeds only the canvas backing-store *resolution*
          attributes below, never the display size. A JS-measured, floored pixel value here would
          drift by up to 1px from the wrap's true (CSS-computed) height and show up as exactly the
          kind of small persistent misalignment against EqChart's own height:auto SVG sizing. */}
      <div className="vs-screen" style={{ width: "100%", height: "100%" }}>
        <canvas
          ref={gridRef}
          className="vectorscope-canvas vs-grid"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <canvas
          ref={peakRef}
          className="vectorscope-canvas vs-peak"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <canvas
          ref={trailRef}
          className="vectorscope-canvas vs-trail"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <div className="vs-tools">
          <button
            type="button"
            className="vs-tool vs-tool-text"
            title={mode === "lr" ? t("scope.toMixTitle") : t("scope.toLrTitle")}
            onClick={() => setMode((m) => (m === "lr" ? "mix" : "lr"))}
          >
            {mode === "lr" ? t("scope.modeLr") : t("scope.modeMix")}
          </button>
          <button
            type="button"
            className={`vs-tool vs-tool-text${params.trigger ? " on" : ""}`}
            title={params.trigger ? t("scope.toFreeTitle") : t("scope.toTrigTitle")}
            aria-pressed={params.trigger}
            onClick={() => set("trigger", !params.trigger)}
          >
            {params.trigger ? t("scope.modeTrig") : t("scope.modeFree")}
          </button>
          <button
            type="button"
            className={`vs-tool${tuning ? " on" : ""}`}
            title={t("scope.tune")}
            aria-pressed={tuning}
            onClick={() => setTuning((v) => !v)}
          >
            ⚙
          </button>
        </div>
        {tuning && (
          <div className="vs-tuning">
            <div className="vs-tune-head">
              <span className="vs-tune-title">{t("scope.tune")}</span>
              <button type="button" className="vs-tune-reset" onClick={() => setParams(DEFAULTS)}>
                {t("scope.reset")}
              </button>
              <button type="button" className="vs-tune-close" title={t("scope.close")} aria-label={t("scope.close")} onClick={() => setTuning(false)}>
                ×
              </button>
            </div>
            <label className="vs-tune-row" title={t("scope.msPerDivHint", { total: (msPerDiv * DIVISIONS).toFixed(1) })}>
              <span className="vs-tune-label">{t("scope.msPerDiv")}</span>
              <input
                type="range"
                min={0}
                max={MS_PER_DIV_STEPS.length - 1}
                step={1}
                value={params.msPerDivIdx}
                onChange={(e) => set("msPerDivIdx", Number(e.currentTarget.value))}
              />
              <b>{msPerDiv < 1 ? msPerDiv.toFixed(1) : msPerDiv}</b>
            </label>
            {CONTROLS.map((cc) => (
              <label key={cc.key} className="vs-tune-row">
                <span className="vs-tune-label">{cc.label}</span>
                <input
                  type="range"
                  min={cc.min}
                  max={cc.max}
                  step={cc.step}
                  value={params[cc.key]}
                  onChange={(e) => set(cc.key, Number(e.currentTarget.value))}
                />
                <b>{params[cc.key].toFixed(cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : 2)}</b>
              </label>
            ))}
            <div className="vs-tune-sep" />
            <label className="vs-tune-row vs-tune-check" title={t("scope.undistortHint")}>
              <span className="vs-tune-label">{t("scope.undistort")}</span>
              <input type="checkbox" checked={params.undistort} onChange={(e) => set("undistort", e.currentTarget.checked)} />
            </label>
          </div>
        )}
      </div>
    </div>
  );
}
