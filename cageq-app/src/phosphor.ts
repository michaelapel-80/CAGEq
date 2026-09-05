/**
 * Phosphor persistence accumulator — the decay half of the CRT look, shared by the scope views.
 *
 * The caller draws only *this frame's* trace, into a scratch 2D canvas handed back by `begin()`,
 * using whatever 2D drawing it likes (the scopes' velocity-bucketed strokes, splines, whatever).
 * `commit()` then decays the accumulated history, adds that trace to it, and presents the result.
 * So all the drawing code stays plain Canvas 2D; only the fade-and-composite step lives here.
 *
 * ## Why this exists
 * The obvious implementation — draw onto a canvas and fade it in place with `destination-out` — is
 * multiplicative, so it never reaches zero, and in 8-bit storage it stalls outright: alpha below
 * ~0.5/(1-keep) LSB rounds back to itself and freezes forever. That threshold scales with the decay
 * rate: invisible (~0.7%) at a 0.05 s trail, a permanent ~7% ghost at 0.6 s. It's why the scopes'
 * trail sliders were only usable at their short end.
 *
 * Several fixes were tried and rejected before this one (kept here so they aren't re-attempted):
 *   • an SVG alpha-floor filter via `ctx.filter = url(#id)` — silently inert in this WebView2 build;
 *   • a periodic full clear — bounded the residue but wiped live layers with it, build-up-then-pop;
 *   • redrawing the whole trail from timestamped history every frame (what SpectrumScope and
 *     EqChart's backdrop do, successfully) — correct, but a scope stamp is ~768 connected
 *     sample-pairs across up to 15 velocity buckets rather than 240 points and one stroke, and
 *     re-stroking the live window every frame dropped frames at default settings on a large tube;
 *   • a WebGL accumulator with 8-bit textures plus a linear drain term — measured a ~30 s tail and
 *     was abandoned unexplained. A standalone spike later found the cause: **the storage type, not
 *     the math.** Doing the decay in float in the shader achieves nothing when the result is
 *     rounded straight back into an `UNSIGNED_BYTE` texture on write — it lands in the identical
 *     8-bit trap. The drain was a workaround for a bug that didn't need one, and its visible
 *     flicker was that workaround's own quantisation dithering near zero.
 *
 * Hence: **half-float storage**, which removes the stall at its source with no drain hack — and
 * measured dramatically faster than the canvas accumulator besides.
 *
 * ## Getting the *look* back after removing the bug
 * Removing the stall turned out to remove something wanted along with it. The residue the 8-bit
 * accumulator parked on screen was, visually, a long faint afterglow — the objection was only ever
 * that it never left. A clean single exponential has no such tail, so the trail read thin and had
 * to be compensated with more glow. Two corrections restore it deliberately rather than by
 * accident: the accumulator is clamped to 1.0 (matching the old one, so bright dwells don't hold
 * clipped-white longer than they used to), and the decay rate is brightness-dependent — see
 * FS_ACCUM — so faint content lingers on its own slower constant while still terminating.
 *
 * ## Fallback
 * `EXT_color_buffer_half_float` is near-universal on desktop WebView2 but not guaranteed — a
 * blocklisted GPU falling back to SwiftShader is the realistic miss. Rather than leave the view
 * broken, `create()` transparently returns a Canvas2D-backed accumulator with the same API and the
 * old 8-bit behaviour (i.e. the faint long-trail ghost comes back, and nothing else changes).
 * Callers don't branch; `precise` reports which backend they got, for tuning copy or diagnostics.
 *
 * ## What must NOT be drawn through this
 * `commit()` is **additive** — that's what produces dwell saturation, where overlapping passes
 * stack toward white. Anything drawn at a constant alpha every frame (Vectorscope's resting spot,
 * TimeScope's peak-hold lines) must therefore live on its own non-accumulating canvas layer, or it
 * will pile up frame over frame instead of holding steady.
 *
 * This used to also offer an `over` blend (self-limiting, converges on its own colour instead of
 * stacking toward white) for EqChart's spectrum backdrop, a background element that needed to not
 * bloom and fight the curves drawn over it — and a `punch` dial partway between the two. Removed
 * once tuning alone (the same defaults-driven approach the meter already relied on) turned out
 * sufficient to keep that backdrop from blowing out on plain `add`, the same as every other caller
 * — see git history if the self-limiting behaviour is ever needed again.
 *
 * ## Bloom
 * Opt-in per call (`commit()`'s `bloomIntensity`/`bloomWide`, both default 0/off unless a caller
 * says otherwise) — EqChart's backdrop should never get it (same "don't fight the curves drawn
 * over it" reasoning as the removed `over` blend above). Confirmed live and shipped enabled, each
 * with its own tuned numbers rather than sharing one set: Vectorscope (a dwelling point/curve),
 * and — despite starting as the one shape this seemed least likely to suit, a log-frequency curve
 * rather than anything that dwells — SpectrumScope too, which fit better than expected. Wired into
 * TimeScope as well but left off there pending its own live check.
 *
 * Two tiers, modelling two different physical things, so each gets its own colour treatment AND
 * its own compositing operator (see `FS_COMPOSITE`'s own doc for the operators) — both whole-frame,
 * not per-primitive, see `commit()`'s own doc for why that specifically matters for a resting spot:
 *   1. **Tight** (`bloomIntensity`, white-valued — see `FS_BRIGHTPASS`): a bright core overloading
 *      and washing toward white, the same way a real CRT/camera bloom does (colour-tinted read as
 *      "saturates to the beam's own hue, not white" — reported live, wrong). Bright-pass (keep
 *      only what's above `BLOOM_THRESHOLD`) on a `BLOOM_SCALE`-downscaled copy of the accumulator,
 *      separable blur, composited in ADDITIVELY — an overload genuinely adds light on top.
 *   2. **Wide** (`bloomWide`, colour-preserving — see `FS_BRIGHTPASS_COLOR`): the beam's own light
 *      scattering into the surrounding dark, sampled fresh from the sharp accumulator (not chained
 *      from tier 1's grayscale result). Composited with `max()`, not addition — additive here was
 *      reported live as "brightens dense areas to white blobs" instead of doing what ambient
 *      scatter should: raise the FLOOR of the faint space around a bright feature without pushing
 *      the feature's own already-bright pixels any brighter.
 *
 *      Internally a *cascade*, not one blur: bright-pass into `BLOOM_WIDE_SIZE` (fixed, not a
 *      fraction of the canvas, so real trail shapes survive as shapes — see that constant's own
 *      "too small, not too generous" doc), a small safe blur there, then a further minification
 *      into `BLOOM_FAR_SIZE` and another small safe blur. The actual "covers roughly a third of
 *      the tube" reach comes from that second downsample, not from a wider single-stage blur —
 *      widening one stage's own tap spacing for more reach was tried and produced a visible grid
 *      (textbook under-sampling: sparse fixed taps on a still-detailed texture alias instead of
 *      blurring it), reverted in favour of this cascade, which only ever samples texels next to
 *      each other and gets its width from shrinking the canvas onto a tiny target instead.
 *
 * **Both tiers' bright-passes read a shared prefilter, not the accumulator directly** — see the
 * doc at its point of use in `commit()`. The accumulator is NEAREST-filtered (deliberately, for
 * its own 1:1 exactness), and sampling that straight into a much smaller target is point-sampling,
 * not averaging: it skips most source texels in a perfectly regular pattern, which produced the
 * same grid artefact as the wide tier's own first mistake, just faint enough at low `bloomIntensity`
 * to go unnoticed until it was turned up. The prefilter is one safe (LINEAR, exact-2x) halving that
 * both tiers then reduce further from, themselves already band-limited.
 *
 * Both tiers together are still only a handful of GL passes, all on small (or, for the wide tier's
 * far stage, tiny) targets, on top of the existing accumulate+blit — see `BLOOM_SCALE`/
 * `BLOOM_WIDE_SIZE`/`BLOOM_FAR_SIZE`'s own docs for the cost reasoning. 8-bit storage for the bloom
 * textures (not half-float): they're soft blurred layers, not the precise accumulator, and don't
 * need the precision or the render-to-half-float capability check.
 */

/** Brightness at which the decay has fully handed over to the fast (bright) rate; below it the
 *  slow tail rate blends in, taking over completely at zero. Raised from an initial 0.12, which
 *  confined the tail to a sliver at the very bottom of the curve and read as barely there. */
const TAIL_KNEE = 0.3;
/** Absolute decay floor, per second, on top of the multiply — guarantees the tail reaches true zero
 *  however slow it's set. Kept well under the faint end it's protecting: at an earlier 0.01 it
 *  dominated below ~2% brightness (removing twice what the multiply did at 1 LSB), so the mechanism
 *  guaranteeing termination was itself eating the tail it was meant to let exist. */
const TAIL_FLOOR_PER_SEC = 0.0015;

/** Reference frame rate the "dose" (this frame's contribution to the accumulator) is normalized
 *  against — see `commit()`'s `dt`-scaling of it below for why this exists at all. 240, not a
 *  rounder 60, because it's the refresh rate of the machine every glow/tau default in these views
 *  was actually tuned on this session — anchoring here means that machine's look is EXACTLY
 *  unchanged by this fix (dt/DOSE_REF_DT == 1 there), and it's every *other* refresh rate that gets
 *  compensated to match it, rather than the reverse.
 *
 *  THE BUG this fixes: decay is correctly time-integrated (`keep = exp(-dt/tau)`, below, scales
 *  properly with `dt`), but the dose ADDED each commit was a flat per-frame amount independent of
 *  `dt` — so its contribution *per second* scaled with how often `commit()` gets called, i.e. with
 *  the display's refresh rate. Reported live: the Spectrum view read much darker on a 60Hz machine
 *  than on a 240Hz one at the identical `glow` setting, and turning `glow` up to its 1.0 maximum on
 *  the slower machine still couldn't match it. The other three views (TimeScope/Vectorscope/
 *  EqChart's backdrop) share this same accumulator and are equally affected in principle, but their
 *  much higher default `glow` already pushes their steady-state brightness past the accumulator's
 *  1.0 clamp on both machines, so the same underlying refresh-rate sensitivity has no visible effect
 *  there — Spectrum's much lower default (tuned to sit well below the clamp, for headroom on real
 *  peaks) is the one view where it shows.
 *
 *  The fix: scale the dose by `dt / DOSE_REF_DT` before adding it. At steady state (dose added every
 *  `dt` seconds, decaying at `exp(-dt/tau)` between additions), the accumulated brightness is then
 *  `(glow * dt/DOSE_REF_DT) / (1 - exp(-dt/tau))` — for `dt` small relative to `tau` (true here at
 *  any plausible refresh rate), that's within a couple percent of `glow * tau / DOSE_REF_DT`,
 *  independent of `dt` — i.e. independent of refresh rate. Verified numerically before landing:
 *  at tau=0.15s, steady-state brightness units at glow=0.15 go from 5.48 (240Hz) vs. 1.43 (60Hz) —
 *  a 3.8x gap — under the OLD fixed-dose behaviour, to 5.48 vs. 5.71 under this fix. */
// Exported so a caller computing its own `doseMult` (see commit()'s doc) can derive a ratio against
// the same reference rather than hardcoding a second copy of 240 that could silently drift from
// this one.
export const DOSE_REF_FPS = 240;
const DOSE_REF_DT = 1 / DOSE_REF_FPS;

/** Shared surface of both backends — see the module comment for why the fallback exists. */
export type Phosphor = {
  /** Clear and return the 2D context for this frame's trace. Sized to the target canvas. */
  begin(): CanvasRenderingContext2D;
  /**
   * Decay the history (time constant `tau` seconds over `dt`), add this frame's trace, present.
   * `tail` multiplies `tau` for *faint* content only (1 = a plain single exponential), giving the
   * long low-level afterglow a real phosphor has — see FS_ACCUM. Ignored by the 2D fallback, which
   * can only apply one global fade.
   *
   * `doseMult` (default 1) is a further multiplier on top of the built-in `dt/DOSE_REF_DT` dose
   * normalization — for a caller that redraws more (or less) often per second than the update rate
   * its own per-trace alpha was tuned against (e.g. EqChart's spectrum backdrop, now redrawn every
   * animation frame instead of only when a fresh payload lands — DOSE_REF_DT alone only corrects for
   * the caller's own render rate, it has no notion of "how many of those frames actually carried new
   * content"). Deliberately a multiplier on `uDose` in the shader (full float precision), not
   * something the caller pre-applies via `ctx.globalAlpha` on the 2D trace: `begin()`'s scratch
   * canvas is always 8-bit regardless of this accumulator's own half-float storage, and a `doseMult`
   * well under 1 pushes a smooth gradient's own alpha down into a handful of discrete 8-bit levels —
   * exactly the regime Chromium's own anti-banding dither becomes visible in, which read live as a
   * stippled, almost-dithered texture across the backdrop. Scaling in the shader instead means the
   * canvas always draws its trace at its original, undiminished alpha (whatever precision that had
   * before), and the correction lands in the same lossless float multiply `uDose` already is.
   *
   * `bloomIntensity` (default 0 = off) adds a soft halo, tight-radius (`BLOOM_SCALE`), around
   * whatever in the accumulated frame is bright enough to clear `BLOOM_THRESHOLD` — a small extra
   * GL pass, not a different rendering stack (see the module doc's "Bloom" section). `bloomWide`
   * (default 0 = off) is a second, much broader and fainter tier at a fixed small size
   * (`BLOOM_WIDE_SIZE`) rather than a fraction of the canvas, downsampled further from the tight
   * tier's own result — the two read as genuinely different things (a crisp inner glow vs. a wide
   * ambient haze), which is why they're separate knobs rather than one radius slider. Both are
   * whole-frame post-processes, not a per-primitive glow baked into how the caller's own trace is
   * drawn: that keeps a stationary bright spot blooming by the same modest amount as a moving
   * trace, proportional to its own brightness, rather than ballooning the way a per-segment glow
   * quad sized for a moving line does when the segment collapses to near-zero length. Ignored by
   * the 2D fallback, which has no cheap way to blur.
   */
  commit(dt: number, tau: number, tail?: number, doseMult?: number, bloomIntensity?: number, bloomWide?: number): void;
  /** True for the half-float GL backend; false when running the 8-bit canvas fallback. */
  readonly precise: boolean;
  dispose(): void;
};

const VS_QUAD = `attribute vec2 aPos; varying vec2 vUv;
void main(){ vUv = aPos * 0.5 + 0.5; gl_Position = vec4(aPos, 0.0, 1.0); }`;

// Decay the history and add this frame's trace. `uKeep` is exp(-dt/tau).
//
// The trace arrives *unpremultiplied* and is premultiplied here rather than at upload: this build
// treats `UNPACK_PREMULTIPLY_ALPHA_WEBGL` as a no-op, and relying on it turned the composite
// additive in straight RGB — every frame stacking full-strength colour regardless of alpha until it
// clipped to white. Doing it in three characters of shader we control sidesteps that entirely.
// Clamped to 1.0, matching the 8-bit canvas accumulator this replaced. Half-float *can* hold
// values above full brightness, and letting it looked appealing on paper ("a real tube
// overexposes") — but it changes the dynamics asymmetrically: a dwell summing to 3.0 stores 3.0 and
// stays clipped-white until decay drags it back under 1.0, where the old accumulator clamped at 1.0
// on every write and began fading immediately. That made bright content bloom harder and hold
// longer, which raises the apparent floor and forces glow up before the faint end reads against it.
// The point of this module is a trail that reaches zero, not a different look.
const FS_ACCUM = `precision highp float; varying vec2 vUv;
uniform sampler2D uPrev, uTrace; uniform float uKeep, uKeepTail, uKnee, uFloor, uDose;
void main(){
  vec4 prev = texture2D(uPrev, vUv);
  // Brightness-dependent decay rate: bright content falls at uKeep, faint content at the slower
  // uKeepTail, blended across uKnee. A single exponential is the wrong model — real phosphors decay
  // with a long low-level tail, and *that tail is what the old 8-bit accumulator was accidentally
  // faking* by stalling. Removing the stall removed the tail with it, which read as a loss of
  // richness (and had to be compensated with more glow). This puts the tail back deliberately,
  // with a rate that still terminates instead of parking residue on screen forever.
  float v = max(max(prev.r, prev.g), prev.b);
  float k = mix(uKeepTail, uKeep, smoothstep(0.0, uKnee, v));
  // Tiny absolute floor so the tail is guaranteed to reach true zero however slow uKeepTail is.
  // This is the same idea as the "drain" that flickered in the 8-bit attempt — harmless here
  // because half-float has no quantisation left to dither against near zero.
  vec4 cur = texture2D(uTrace, vUv);
  // uDose (dt/DOSE_REF_DT, set in commit()) makes this frame's contribution frame-rate-independent
  // — see DOSE_REF_DT's own doc above for why: without it, a dose added once every dt seconds
  // contributes proportionally MORE per second on a higher-refresh display, purely because commit()
  // gets called more often, not because anything drawn is actually brighter.
  cur.a *= uDose;
  cur.rgb *= cur.a;
  vec4 decayed = max(prev * k - uFloor, 0.0);
  // Stacks toward saturation and has no hue ceiling — the overdraw every caller wants (see the
  // module doc's "What must NOT be drawn through this" for the one thing that rules out: anything
  // meant to hold a *constant* brightness needs its own non-accumulating layer instead).
  gl_FragColor = min(decayed + cur, 1.0);
}`;

const FS_BLIT = `precision highp float; varying vec2 vUv; uniform sampler2D uTex;
void main(){ gl_FragColor = texture2D(uTex, vUv); }`;

/** Bloom's small (tight-radius) target as a fraction of the caller's real canvas size — see the
 *  module doc's "Bloom" section for why this is what keeps the extra passes cheap regardless of
 *  tube size. */
const BLOOM_SCALE = 0.25;
/** The wide (broad, faint halo) tier's target, in *fixed* pixels, not a fraction of the canvas —
 *  deliberately: the point of this tier is maximum softness/spread, and a fraction of a large
 *  canvas can still be too many pixels wide to read as genuinely broad, where a small fixed size
 *  guarantees the downsample+upscale alone does most of the spreading, whatever the tube size.
 *  Allocated once (not on resize, unlike the tight tier) because it never depends on the target's
 *  own size.
 *
 *  **40 (this tier's first value) was too small, not too generous.** A phosphor trail commonly
 *  covers a real fraction of the tube (Trail persistence spreads it there on purpose), so
 *  downsampling it into a texture this tiny didn't spread a bright *feature's* light outward —
 *  it averaged away *where* the trail even was, degenerating toward one global brightness number
 *  reprojected everywhere. That reads as "accumulates" and blows out broad sections precisely
 *  because it stopped being spatial: everywhere lit, evenly, in proportion to how much of the
 *  screen the trail already covers, not to any one bright point. Raised so real trail shapes
 *  survive the downsample as actual shapes for the blur to spread — still tiny next to a real
 *  canvas, still cheap. */
const BLOOM_WIDE_SIZE = 112;
/** How many horizontal+vertical blur passes the wide tier runs at its own resolution, ping-
 *  ponging its own tiny textures. Small and safe on purpose — see `BLOOM_FAR_SIZE`'s own doc for
 *  why real reach comes from a second downsample stage, not from widening this one's taps. This
 *  pass just smooths the bright-pass output before that downsample happens. */
const BLOOM_WIDE_BLUR_PASSES = 2;
/** The wide tier's own tap spacing, in texels — kept at 1 (adjacent texels, no gaps) rather than
 *  widened for more reach.
 *
 *  **Widening this was tried and reverted.** Reach does scale with tap spacing, but a fixed 5-tap
 *  kernel with wide gaps between them is under-sampling, not blurring: it reads the source at only
 *  a handful of far-apart points and reconstructs everything between them by assumption, and any
 *  real structure at a finer scale than the gap aliases — reported live as a visible grid pattern,
 *  the textbook symptom. Real width has to come from *actually reducing resolution* first (which
 *  band-limits the content as a side effect of minification, the way a photograph blurs when
 *  shrunk) and blurring the now-safely-coarse result, not from sampling a still-detailed texture
 *  sparsely — see `BLOOM_FAR_SIZE`. */
const BLOOM_WIDE_BLUR_STEP = 1;
/** A second, further downsample of the wide tier's own (safely blurred) result, in fixed texels —
 *  this, not a wider blur on `BLOOM_WIDE_SIZE`'s own texture, is what actually reaches "covers
 *  roughly a third of the tube": a small texture stretched across the whole canvas is inherently
 *  that broad, and shrinking into it is a genuine minification (bilinear-averaging many source
 *  texels per destination one), which band-limits the content instead of skipping over it the way
 *  wide taps on a same-size texture do. Small enough that even a short, small-spacing blur here
 *  (`BLOOM_FAR_BLUR_PASSES`) reads as broad and soft once upscaled. */
const BLOOM_FAR_SIZE = 28;
/** Passes for the far tier's own blur — small and safe (1-texel spacing, like the wide tier's),
 *  since `BLOOM_FAR_SIZE` already does almost all of the actual spreading; this just rounds off
 *  any residual blockiness from the downsample itself. */
const BLOOM_FAR_BLUR_PASSES = 2;
/** Only content this bright (post-accumulation, 0..1 range) contributes to the bloom — a plain
 *  `max(c - threshold, 0)` subtractive knee, not a hard cutoff, so the transition isn't a visible
 *  edge. Keeps the halo tied to the beam/spot rather than smearing the whole faint trail outward.
 *  **Picked without live verification, unlike this file's other constants** — 0.6 in a 0..1
 *  accumulator that only reaches 1.0 at hard saturation left bloom invisible on real (non-blown-
 *  out) content at every intensity, reported live. Lowered to sit under typical steady-state
 *  trail brightness at default `glow` instead of only at the clamp — re-tune from here once seen
 *  live, not guessed again. */
const BLOOM_THRESHOLD = 0.2;

// Downsampling copy + threshold in one pass: sampling the full-res accumulator (NEAREST-filtered,
// deliberately — see its own texture setup) into a smaller target aliases slightly instead of
// box-filtering, but the blur immediately after erases it, so a second filter mode on the exact
// accumulator texture isn't worth adding just for this.
//
// Tight tier only. Collapses to a single brightness value (replicated across RGB) rather than
// keeping the source's own hue: real bloom/halation at a bright *core* washes out toward white as
// it saturates, it doesn't just intensify the same colour, and it's what "seems to not saturate to
// white but full saturation again" was reporting when this thresholded the coloured channels
// directly. Same `max(r,g,b)` brightness proxy FS_ACCUM's own decay math already uses, for
// consistency within this file. The wide tier keeps its own hue instead — see FS_BRIGHTPASS_COLOR.
const FS_BRIGHTPASS = `precision mediump float; varying vec2 vUv;
uniform sampler2D uTex; uniform float uThreshold;
void main(){
  vec3 c = texture2D(uTex, vUv).rgb;
  float v = max(c.r, max(c.g, c.b));
  float b = max(v - uThreshold, 0.0);
  gl_FragColor = vec4(b, b, b, 1.0);
}`;

// Wide tier's own bright-pass, sampled fresh from the sharp accumulator rather than chained from
// the tight tier's (grayscale) result — chaining would inherit that grayscale, and this tier is
// specifically meant to keep the beam's own colour as it bleeds into the surrounding dark (an
// ambient scatter, not a saturating overload — see FS_COMPOSITE's own doc for why that also means
// a different *compositing* operator, not just a different colour). Preserves hue by scaling the
// whole colour down by how much of its own brightness cleared the threshold, rather than reading
// off one brightness number and discarding the channel ratios the way FS_BRIGHTPASS does.
const FS_BRIGHTPASS_COLOR = `precision mediump float; varying vec2 vUv;
uniform sampler2D uTex; uniform float uThreshold;
void main(){
  vec3 c = texture2D(uTex, vUv).rgb;
  float v = max(c.r, max(c.g, c.b));
  float excess = max(v - uThreshold, 0.0);
  vec3 col = v > 0.0 ? c * (excess / v) : vec3(0.0);
  gl_FragColor = vec4(col, 1.0);
}`;

// Separable 5-tap approx-Gaussian (weights sum to 1): run once with a horizontal uOffset, once
// with a vertical one, ping-ponging the two small bloom targets. Two passes of this instead of one
// wide-kernel pass for the standard reason a separable blur exists at all — an NxN kernel's cost
// split into two 1D passes of N taps each, not one 2D pass of N².
const FS_BLUR = `precision mediump float; varying vec2 vUv;
uniform sampler2D uTex; uniform vec2 uOffset;
void main(){
  vec3 sum = texture2D(uTex, vUv).rgb * 0.4026;
  sum += texture2D(uTex, vUv + uOffset).rgb * 0.2442;
  sum += texture2D(uTex, vUv - uOffset).rgb * 0.2442;
  sum += texture2D(uTex, vUv + uOffset * 2.0).rgb * 0.0545;
  sum += texture2D(uTex, vUv - uOffset * 2.0).rgb * 0.0545;
  gl_FragColor = vec4(sum, 1.0);
}`;

// Replaces the plain FS_BLIT present pass when bloom is on. The two tiers are deliberately
// composited by different operators, not just different textures — they model different physical
// things:
//   * Tight (`uBloom`, white-valued — see FS_BRIGHTPASS): a bright core overloading and washing
//     toward white. ADDITIVE, same as FS_ACCUM's own stacking, because that overload genuinely
//     adds light on top of what's already there.
//   * Wide (`uBloomWide`, colour-preserving — see FS_BRIGHTPASS_COLOR): the beam's own light
//     scattering into the surrounding dark. Reported live that additive here "brightens dense
//     areas to white blobs" instead of doing what ambient scatter actually does — lift the FLOOR
//     of the faint space around a bright feature, without pushing the feature's own already-bright
//     pixels any brighter. `max()` (a lighten/screen-style blend) is exactly that: in a pixel
//     that's already brighter than the wide halo would be, `max` leaves it untouched; only in a
//     pixel darker than the halo does the halo actually raise it. Composited AFTER the tight
//     additive step (against `withTight`, not the bare `sharpC`) so it can't be undercut by
//     tight's own bloom pushing a pixel just high enough to dodge the comparison.
// Both bilinearly upscaled on sample — see the bloom textures' own LINEAR filtering. Carries the
// sharp texture's own alpha through unchanged — bloom is extra light, not extra coverage.
const FS_COMPOSITE = `precision highp float; varying vec2 vUv;
uniform sampler2D uSharp, uBloom, uBloomWide; uniform float uIntensity, uIntensityWide;
void main(){
  vec4 sharpC = texture2D(uSharp, vUv);
  vec3 withTight = min(sharpC.rgb + uIntensity * texture2D(uBloom, vUv).rgb, 1.0);
  vec3 result = max(withTight, uIntensityWide * texture2D(uBloomWide, vUv).rgb);
  gl_FragColor = vec4(result, sharpC.a);
}`;

// Logged on failure, not just returned null: every caller of link()/compile() otherwise fails
// completely silently (a program that never does anything, with zero trace of why) — fine for
// the original accum/blit pair, whose shaders never changed after they were first proven to
// compile, but the bloom passes are new and unverified on whatever GL/ANGLE build is actually
// running, so a compile or link error here needs to be visible, not just inferred from "bloom
// does nothing."
function compile(gl: WebGLRenderingContext, type: number, src: string): WebGLShader | null {
  const sh = gl.createShader(type);
  if (!sh) return null;
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (gl.getShaderParameter(sh, gl.COMPILE_STATUS)) return sh;
  console.warn("[phosphor] shader compile failed:", gl.getShaderInfoLog(sh));
  return null;
}

function link(gl: WebGLRenderingContext, fsSrc: string): WebGLProgram | null {
  const vs = compile(gl, gl.VERTEX_SHADER, VS_QUAD);
  const fs = compile(gl, gl.FRAGMENT_SHADER, fsSrc);
  const prog = gl.createProgram();
  if (!vs || !fs || !prog) return null;
  gl.attachShader(prog, vs);
  gl.attachShader(prog, fs);
  gl.bindAttribLocation(prog, 0, "aPos");
  gl.linkProgram(prog);
  if (gl.getProgramParameter(prog, gl.LINK_STATUS)) return prog;
  console.warn("[phosphor] program link failed:", gl.getProgramInfoLog(prog));
  return null;
}

/** The scratch canvas both backends hand out from `begin()`, kept in step with the target's size. */
function makeScratch(target: HTMLCanvasElement) {
  const cv = document.createElement("canvas");
  const ctx = cv.getContext("2d");
  const sync = () => {
    if (cv.width !== target.width || cv.height !== target.height) {
      cv.width = target.width; // also clears, which is what we want on a resize
      cv.height = target.height;
      return true;
    }
    return false;
  };
  return { cv, ctx, sync };
}

function createGl(target: HTMLCanvasElement): Phosphor | null {
  const gl = target.getContext("webgl", {
    alpha: true,
    premultipliedAlpha: true,
    antialias: false,
    depth: false,
    stencil: false,
  });
  if (!gl) return null;

  // Both are required: the first to *hold* half-float texels, the second to render into them.
  const half = gl.getExtension("OES_texture_half_float");
  const halfRender = gl.getExtension("EXT_color_buffer_half_float");
  if (!half || !halfRender) return null;

  const accum = link(gl, FS_ACCUM);
  const blit = link(gl, FS_BLIT);
  const quad = gl.createBuffer();
  if (!accum || !blit || !quad) return null;

  const uKeep = gl.getUniformLocation(accum, "uKeep");
  const uKeepTail = gl.getUniformLocation(accum, "uKeepTail");
  const uKnee = gl.getUniformLocation(accum, "uKnee");
  const uFloor = gl.getUniformLocation(accum, "uFloor");
  const uDose = gl.getUniformLocation(accum, "uDose");
  const uPrev = gl.getUniformLocation(accum, "uPrev");
  const uTrace = gl.getUniformLocation(accum, "uTrace");
  const uTex = gl.getUniformLocation(blit, "uTex");

  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true); // one flip on upload, none on present — orientations agree
  // Upload the canvas's values verbatim. The default is BROWSER_DEFAULT_WEBGL, which permits the
  // implementation to colour-convert canvas→texture — a silent brightness/saturation shift on
  // exactly the path this module depends on being exact.
  gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
  gl.disable(gl.BLEND); // every pass writes its whole target; compositing is the shader's job

  const scratch = makeScratch(target);
  if (!scratch.ctx) return null;

  const mkTexSized = (type: number, w: number, h: number, filter: number) => {
    const t = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, type, null);
    return t;
  };
  // NEAREST — 1:1 texel:pixel, exact, unlike the bloom textures' own LINEAR (see ensureBloom).
  const mkTex = (type: number) => mkTexSized(type, target.width, target.height, gl.NEAREST);

  let texTrace = mkTex(gl.UNSIGNED_BYTE); // uploads come from a 2D canvas, so always 8-bit
  let ping = [0, 1].map(() => ({ tex: mkTex(half.HALF_FLOAT_OES), fbo: gl.createFramebuffer() }));
  let src = 0;
  let sizedW = target.width;
  let sizedH = target.height;

  // Status must be read while the FBO in question is *bound* — checking after unbinding inspects
  // the default framebuffer, which is always COMPLETE, so the validation silently passes and the
  // renderer then draws into an incomplete FBO every frame. That took down the GPU process (a
  // Chromium crash page, nothing in the console, since the failure is below JS).
  const attach = () => {
    let ok = true;
    for (const pp of ping) {
      gl.bindFramebuffer(gl.FRAMEBUFFER, pp.fbo);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, pp.tex, 0);
      if (gl.checkFramebufferStatus(gl.FRAMEBUFFER) !== gl.FRAMEBUFFER_COMPLETE) ok = false;
    }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    return ok;
  };
  if (!attach()) {
    for (const pp of ping) {
      gl.deleteTexture(pp.tex);
      gl.deleteFramebuffer(pp.fbo);
    }
    return null;
  }

  // Set if a reallocation ever yields an incomplete FBO. `commit` then does nothing rather than
  // issuing draws against it — a frozen trail is a bad frame, but drawing into an incomplete
  // framebuffer every frame is a GPU-process crash.
  let dead = false;

  const resize = () => {
    if (target.width === sizedW && target.height === sizedH) return;
    if (target.width === 0 || target.height === 0) return; // mid-layout; nothing valid to allocate
    sizedW = target.width;
    sizedH = target.height;
    gl.deleteTexture(texTrace);
    for (const pp of ping) gl.deleteTexture(pp.tex);
    texTrace = mkTex(gl.UNSIGNED_BYTE);
    ping = ping.map((pp) => ({ tex: mkTex(half.HALF_FLOAT_OES), fbo: pp.fbo }));
    dead = !attach(); // fresh textures are zero-filled, so a resize also clears the trail
  };

  // Bloom's four programs (brightpass/blur are shared between both tiers) and two pairs of small
  // ping-pong targets — compiled/allocated lazily on first use, not up front, so a caller that
  // never passes bloomIntensity/bloomWide (every view but Vectorscope, for now) pays nothing extra
  // at all, not even the small textures.
  let bloomProg: {
    brightPass: WebGLProgram; uThreshold: WebGLUniformLocation | null; uBPTex: WebGLUniformLocation | null;
    brightPassColor: WebGLProgram; uThresholdColor: WebGLUniformLocation | null; uBPColorTex: WebGLUniformLocation | null;
    blur: WebGLProgram; uOffset: WebGLUniformLocation | null; uBlurTex: WebGLUniformLocation | null;
    composite: WebGLProgram;
    uSharp: WebGLUniformLocation | null; uBloom: WebGLUniformLocation | null; uBloomWide: WebGLUniformLocation | null;
    uIntensity: WebGLUniformLocation | null; uIntensityWide: WebGLUniformLocation | null;
  } | null = null;
  type BloomTarget = { tex: WebGLTexture; fbo: WebGLFramebuffer };
  type BloomPair = [BloomTarget, BloomTarget];
  let bloomTex: BloomPair | null = null; // tight tier — resized with the caller's own canvas
  let bloomWideTex: BloomPair | null = null; // wide tier — fixed BLOOM_WIDE_SIZE, allocated once
  let bloomFarTex: BloomPair | null = null; // wide tier's further downsample — fixed BLOOM_FAR_SIZE
  let bloomPrefilterTex: BloomTarget | null = null; // see its own doc at the point of use
  let bloomW = 0;
  let bloomH = 0;
  let prefilterW = 0;
  let prefilterH = 0;

  // LINEAR, not the main accumulator's NEAREST: every bloom texture is sampled both smaller (each
  // tier's own downsample step) and larger (the composite's upscale back to full res, and the wide
  // tier's own downsample *from* the tight tier's result) than its actual resolution, and LINEAR is
  // what makes all of that a cheap, smooth filter instead of a blocky one — exactly what a
  // *precise* 1:1 accumulator must NOT have, but a soft bloom layer wants.
  const mkBloomTex = (w: number, h: number): BloomTarget | null => {
    const tex = mkTexSized(gl.UNSIGNED_BYTE, w, h, gl.LINEAR);
    const fbo = gl.createFramebuffer();
    if (!fbo) return null;
    gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
    const status = gl.checkFramebufferStatus(gl.FRAMEBUFFER);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    if (status !== gl.FRAMEBUFFER_COMPLETE) {
      console.warn("[phosphor] bloom framebuffer incomplete:", status.toString(16));
      return null;
    }
    return { tex, fbo };
  };
  const mkBloomPair = (w: number, h: number): BloomPair | null => {
    const a = mkBloomTex(w, h);
    const b = mkBloomTex(w, h);
    return a && b ? [a, b] : null;
  };

  // `w`/`h` (the tight tier's own size) re-checked, and its targets reallocated if they've
  // drifted, on every call rather than hooked into `resize()`: the two are already independent —
  // bloom's own size is a fraction of whatever the caller's actual canvas is this frame, so it
  // self-corrects the next time this runs, with no need to coordinate with the main accumulator's
  // own resize logic. The wide tier never depends on the caller's size at all (see
  // BLOOM_WIDE_SIZE's own doc), so it's allocated once and left alone.
  const ensureBloom = (canvasW: number, canvasH: number): boolean => {
    const w = Math.max(1, Math.round(canvasW * BLOOM_SCALE));
    const h = Math.max(1, Math.round(canvasH * BLOOM_SCALE));
    if (!bloomProg) {
      const brightPass = link(gl, FS_BRIGHTPASS);
      const brightPassColor = link(gl, FS_BRIGHTPASS_COLOR);
      const blur = link(gl, FS_BLUR);
      const composite = link(gl, FS_COMPOSITE);
      if (!brightPass || !brightPassColor || !blur || !composite) return false;
      bloomProg = {
        brightPass,
        uThreshold: gl.getUniformLocation(brightPass, "uThreshold"),
        uBPTex: gl.getUniformLocation(brightPass, "uTex"),
        brightPassColor,
        uThresholdColor: gl.getUniformLocation(brightPassColor, "uThreshold"),
        uBPColorTex: gl.getUniformLocation(brightPassColor, "uTex"),
        blur,
        uOffset: gl.getUniformLocation(blur, "uOffset"),
        uBlurTex: gl.getUniformLocation(blur, "uTex"),
        composite,
        uSharp: gl.getUniformLocation(composite, "uSharp"),
        uBloom: gl.getUniformLocation(composite, "uBloom"),
        uBloomWide: gl.getUniformLocation(composite, "uBloomWide"),
        uIntensity: gl.getUniformLocation(composite, "uIntensity"),
        uIntensityWide: gl.getUniformLocation(composite, "uIntensityWide"),
      };
    }
    if (!bloomTex || bloomW !== w || bloomH !== h) {
      if (bloomTex) {
        for (const b of bloomTex) {
          gl.deleteTexture(b.tex);
          gl.deleteFramebuffer(b.fbo);
        }
      }
      bloomTex = mkBloomPair(w, h);
      if (!bloomTex) return false;
      bloomW = w;
      bloomH = h;
    }
    if (!bloomWideTex) {
      bloomWideTex = mkBloomPair(BLOOM_WIDE_SIZE, BLOOM_WIDE_SIZE);
      if (!bloomWideTex) return false;
    }
    if (!bloomFarTex) {
      bloomFarTex = mkBloomPair(BLOOM_FAR_SIZE, BLOOM_FAR_SIZE);
      if (!bloomFarTex) return false;
    }
    // Half the caller's actual canvas — see its own doc at the point of use (in commit()) for why
    // every bright-pass reads this instead of the accumulator directly.
    const pw = Math.max(1, Math.round(canvasW / 2));
    const ph = Math.max(1, Math.round(canvasH / 2));
    if (!bloomPrefilterTex || prefilterW !== pw || prefilterH !== ph) {
      if (bloomPrefilterTex) {
        gl.deleteTexture(bloomPrefilterTex.tex);
        gl.deleteFramebuffer(bloomPrefilterTex.fbo);
      }
      bloomPrefilterTex = mkBloomTex(pw, ph);
      if (!bloomPrefilterTex) return false;
      prefilterW = pw;
      prefilterH = ph;
    }
    return true;
  };

  return {
    precise: true,
    begin() {
      resize();
      scratch.sync();
      scratch.ctx!.clearRect(0, 0, target.width, target.height);
      return scratch.ctx!;
    },
    commit(dt, tau, tail = 1, doseMult = 1, bloomIntensity = 0, bloomWide = 0) {
      const W = target.width;
      const H = target.height;
      if (dead || W === 0 || H === 0 || gl.isContextLost()) return;
      const dst = 1 - src;

      gl.activeTexture(gl.TEXTURE1);
      gl.bindTexture(gl.TEXTURE_2D, texTrace);
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, gl.RGBA, gl.UNSIGNED_BYTE, scratch.cv);

      gl.bindBuffer(gl.ARRAY_BUFFER, quad);
      gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);

      gl.bindFramebuffer(gl.FRAMEBUFFER, ping[dst].fbo);
      gl.viewport(0, 0, W, H);
      gl.useProgram(accum);
      gl.uniform1f(uKeep, Math.exp(-dt / tau));
      gl.uniform1f(uKeepTail, Math.exp(-dt / (tau * Math.max(1, tail))));
      gl.uniform1f(uKnee, TAIL_KNEE);
      gl.uniform1f(uFloor, TAIL_FLOOR_PER_SEC * dt);
      gl.uniform1f(uDose, (dt / DOSE_REF_DT) * doseMult);
      gl.uniform1i(uPrev, 0);
      gl.uniform1i(uTrace, 1);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, ping[src].tex);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

      const bloomReady = (bloomIntensity > 0 || bloomWide > 0) && ensureBloom(W, H);

      if (bloomReady && bloomProg && bloomTex && bloomWideTex && bloomFarTex && bloomPrefilterTex) {
        const [a, b] = bloomTex;
        const [wa, wb] = bloomWideTex;
        const [fa, fb] = bloomFarTex;
        const pf = bloomPrefilterTex;

        // Prefilter: a *safe* (LINEAR, half-resolution) copy of the sharp accumulator, which both
        // tiers' bright-passes read instead of `ping[dst].tex` directly. That texture is NEAREST
        // (deliberately, for the accumulator's own exactness — see its setup), and sampling it at
        // a large reduction (tight: 4x via BLOOM_SCALE; wide: similar) is point-sampling, not
        // averaging — it skips most source texels in a perfectly regular pattern, which is exactly
        // what produced a visible grid at high bloom intensity (the same under-sampling family as
        // the wide tier's earlier one, just faint enough at low intensity to go unnoticed until
        // reported live). A single, safe 2x LINEAR halving here — exact for an even-sized target,
        // since bilinear correctly averages precisely the 4 source texels a 2x reduction maps each
        // destination texel to — band-limits the content once, before either tier's own further
        // (safe, LINEAR-sourced) reduction.
        gl.useProgram(blit);
        gl.uniform1i(uTex, 0);
        gl.bindFramebuffer(gl.FRAMEBUFFER, pf.fbo);
        gl.viewport(0, 0, prefilterW, prefilterH);
        gl.activeTexture(gl.TEXTURE0);
        gl.bindTexture(gl.TEXTURE_2D, ping[dst].tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        gl.bindFramebuffer(gl.FRAMEBUFFER, a.fbo);
        gl.viewport(0, 0, bloomW, bloomH);
        gl.useProgram(bloomProg.brightPass);
        gl.uniform1f(bloomProg.uThreshold, BLOOM_THRESHOLD);
        gl.uniform1i(bloomProg.uBPTex, 0);
        gl.activeTexture(gl.TEXTURE0);
        gl.bindTexture(gl.TEXTURE_2D, pf.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        // Separable blur, ping-ponging the two small targets (a -> b horizontally, b -> a
        // vertically) — cheap because the targets are small, not because the kernel is. `a` ends
        // up holding the tight tier's finished result, already white-valued (see FS_BRIGHTPASS).
        gl.useProgram(bloomProg.blur);
        gl.uniform1i(bloomProg.uBlurTex, 0);
        gl.bindFramebuffer(gl.FRAMEBUFFER, b.fbo);
        gl.uniform2f(bloomProg.uOffset, 1 / bloomW, 0);
        gl.bindTexture(gl.TEXTURE_2D, a.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        gl.bindFramebuffer(gl.FRAMEBUFFER, a.fbo);
        gl.uniform2f(bloomProg.uOffset, 0, 1 / bloomH);
        gl.bindTexture(gl.TEXTURE_2D, b.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        // Wide tier: its own bright-pass, also off the safe prefilter (NOT the sharp accumulator
        // directly — same aliasing reasoning as the tight tier above; NOT chained from `a` either,
        // which is already grayscale — see FS_BRIGHTPASS_COLOR's own doc for why this tier needs
        // its own colour-preserving pass instead).
        gl.useProgram(bloomProg.brightPassColor);
        gl.uniform1f(bloomProg.uThresholdColor, BLOOM_THRESHOLD);
        gl.uniform1i(bloomProg.uBPColorTex, 0);
        gl.bindFramebuffer(gl.FRAMEBUFFER, wa.fbo);
        gl.viewport(0, 0, BLOOM_WIDE_SIZE, BLOOM_WIDE_SIZE);
        gl.bindTexture(gl.TEXTURE_2D, pf.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        // BLOOM_WIDE_BLUR_PASSES horizontal+vertical pairs, ping-ponging wa/wb — small, safe
        // 1-texel spacing (BLOOM_WIDE_BLUR_STEP). This is a pre-smooth, not the reach control —
        // see BLOOM_FAR_SIZE's own doc for where the actual "covers a third of the tube" width
        // comes from.
        gl.useProgram(bloomProg.blur);
        gl.uniform1i(bloomProg.uBlurTex, 0);
        const wideStep = BLOOM_WIDE_BLUR_STEP / BLOOM_WIDE_SIZE;
        for (let i = 0; i < BLOOM_WIDE_BLUR_PASSES; i++) {
          gl.bindFramebuffer(gl.FRAMEBUFFER, wb.fbo);
          gl.uniform2f(bloomProg.uOffset, wideStep, 0);
          gl.bindTexture(gl.TEXTURE_2D, wa.tex);
          gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

          gl.bindFramebuffer(gl.FRAMEBUFFER, wa.fbo);
          gl.uniform2f(bloomProg.uOffset, 0, wideStep);
          gl.bindTexture(gl.TEXTURE_2D, wb.tex);
          gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        }

        // Far tier: a genuine minification of the wide tier's result into BLOOM_FAR_SIZE — this,
        // not a wider blur on the same-size texture, is where the real reach comes from (see that
        // constant's own doc). LINEAR filtering on `wa` means this downsample is itself a
        // bilinear box-average over many source texels per destination one, which band-limits the
        // content as a side effect rather than skipping over it.
        gl.useProgram(blit);
        gl.uniform1i(uTex, 0);
        gl.bindFramebuffer(gl.FRAMEBUFFER, fa.fbo);
        gl.viewport(0, 0, BLOOM_FAR_SIZE, BLOOM_FAR_SIZE);
        gl.bindTexture(gl.TEXTURE_2D, wa.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

        // BLOOM_FAR_BLUR_PASSES more, small and safe the same way, just rounding off residual
        // blockiness from the downsample rather than doing the spreading itself.
        gl.useProgram(bloomProg.blur);
        gl.uniform1i(bloomProg.uBlurTex, 0);
        const farStep = 1 / BLOOM_FAR_SIZE;
        for (let i = 0; i < BLOOM_FAR_BLUR_PASSES; i++) {
          gl.bindFramebuffer(gl.FRAMEBUFFER, fb.fbo);
          gl.uniform2f(bloomProg.uOffset, farStep, 0);
          gl.bindTexture(gl.TEXTURE_2D, fa.tex);
          gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

          gl.bindFramebuffer(gl.FRAMEBUFFER, fa.fbo);
          gl.uniform2f(bloomProg.uOffset, 0, farStep);
          gl.bindTexture(gl.TEXTURE_2D, fb.tex);
          gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        }

        // Composite straight to the visible canvas: sharp accumulator + both halos.
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        gl.viewport(0, 0, W, H);
        gl.useProgram(bloomProg.composite);
        gl.uniform1f(bloomProg.uIntensity, bloomIntensity);
        gl.uniform1f(bloomProg.uIntensityWide, bloomWide);
        gl.uniform1i(bloomProg.uSharp, 0);
        gl.uniform1i(bloomProg.uBloom, 1);
        gl.uniform1i(bloomProg.uBloomWide, 2);
        gl.activeTexture(gl.TEXTURE0);
        gl.bindTexture(gl.TEXTURE_2D, ping[dst].tex);
        gl.activeTexture(gl.TEXTURE1);
        gl.bindTexture(gl.TEXTURE_2D, a.tex);
        gl.activeTexture(gl.TEXTURE2);
        gl.bindTexture(gl.TEXTURE_2D, fa.tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        gl.activeTexture(gl.TEXTURE0); // restore the convention every other pass here assumes
      } else {
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        gl.viewport(0, 0, W, H);
        gl.useProgram(blit);
        gl.uniform1i(uTex, 0);
        gl.bindTexture(gl.TEXTURE_2D, ping[dst].tex);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      }
      src = dst;
    },
    dispose() {
      gl.deleteTexture(texTrace);
      for (const pp of ping) {
        gl.deleteTexture(pp.tex);
        gl.deleteFramebuffer(pp.fbo);
      }
      if (bloomTex) {
        for (const b of bloomTex) {
          gl.deleteTexture(b.tex);
          gl.deleteFramebuffer(b.fbo);
        }
      }
      if (bloomWideTex) {
        for (const b of bloomWideTex) {
          gl.deleteTexture(b.tex);
          gl.deleteFramebuffer(b.fbo);
        }
      }
      if (bloomFarTex) {
        for (const b of bloomFarTex) {
          gl.deleteTexture(b.tex);
          gl.deleteFramebuffer(b.fbo);
        }
      }
      if (bloomPrefilterTex) {
        gl.deleteTexture(bloomPrefilterTex.tex);
        gl.deleteFramebuffer(bloomPrefilterTex.fbo);
      }
      if (bloomProg) {
        gl.deleteProgram(bloomProg.brightPass);
        gl.deleteProgram(bloomProg.brightPassColor);
        gl.deleteProgram(bloomProg.blur);
        gl.deleteProgram(bloomProg.composite);
      }
      gl.deleteProgram(accum);
      gl.deleteProgram(blit);
      gl.deleteBuffer(quad);
      // Deliberately NOT `WEBGL_lose_context.loseContext()`: the canvas element outlives this
      // accumulator (React keeps it across a StrictMode double-mount, and across chart-view
      // switches), and `getContext` on a canvas whose context was lost hands back that same dead
      // context — so losing it here would poison every later mount on the same element.
    },
  };
}

/** Can this GPU actually *render into* half-float? Probed on a throwaway 2×2 canvas, deliberately
 *  not on the caller's: a canvas is bound to one context type for life, so attempting WebGL on the
 *  real canvas and then giving up would leave it unable to provide the 2D fallback either. Extension
 *  presence alone isn't sufficient — the FBO completeness check is the part that actually matters. */
function halfFloatRenderable(): boolean {
  try {
    const probe = document.createElement("canvas");
    probe.width = probe.height = 2;
    const gl = probe.getContext("webgl");
    if (!gl) return false;
    const half = gl.getExtension("OES_texture_half_float");
    if (!half || !gl.getExtension("EXT_color_buffer_half_float")) return false;
    const tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 2, 2, 0, gl.RGBA, half.HALF_FLOAT_OES, null);
    const fbo = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
    const ok = gl.checkFramebufferStatus(gl.FRAMEBUFFER) === gl.FRAMEBUFFER_COMPLETE;
    gl.getExtension("WEBGL_lose_context")?.loseContext(); // throwaway — release it now, not at GC
    return ok;
  } catch {
    return false;
  }
}

/** The 8-bit path — today's behaviour, including its long-trail ghost. Additive blitting is
 *  associative, so routing the trace through a scratch canvas and adding it in one go is
 *  equivalent to having stroked it straight onto the accumulator. */
function create2d(target: HTMLCanvasElement): Phosphor | null {
  const ctx = target.getContext("2d");
  const scratch = makeScratch(target);
  if (!ctx || !scratch.ctx) return null;
  return {
    precise: false,
    begin() {
      scratch.sync();
      scratch.ctx!.clearRect(0, 0, target.width, target.height);
      return scratch.ctx!;
    },
    // _bloomIntensity/_bloomWide ignored: no GL context here to blur with, and the module doc's
    // own "nothing else changes" philosophy for this fallback covers bloom the same as everything
    // else.
    commit(dt, tau, _tail = 1, doseMult = 1, _bloomIntensity = 0, _bloomWide = 0) {
      const W = target.width;
      const H = target.height;
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${1 - Math.exp(-dt / tau)})`;
      ctx.fillRect(0, 0, W, H);
      ctx.globalCompositeOperation = "lighter";
      // Same dt-normalized dose as the GL path (see DOSE_REF_DT's doc), further scaled by
      // `doseMult` (see commit()'s own doc) — `globalAlpha` can't exceed 1 (the spec ignores an
      // out-of-range assignment rather than clamping it), so a dose scale above 1 — any refresh rate
      // below DOSE_REF_FPS, or a caller-supplied doseMult above 1 — is spread across that many
      // additive draws of the same frame instead of one draw at an alpha it can't express.
      const doseScale = (dt / DOSE_REF_DT) * doseMult;
      const passes = Math.max(1, Math.ceil(doseScale));
      ctx.globalAlpha = doseScale / passes;
      for (let i = 0; i < passes; i++) ctx.drawImage(scratch.cv, 0, 0);
      ctx.globalAlpha = 1;
      ctx.globalCompositeOperation = "source-over";
    },
    dispose() {},
  };
}

/** Half-float GL accumulator, or the 8-bit canvas fallback when the GPU can't render half-float.
 *  Capability is settled on a throwaway canvas first (see `halfFloatRenderable`) so the caller's
 *  canvas is only ever bound to the context type actually being used — it keeps that type for life.
 *  Returns null only if even a 2D context is unobtainable. */
export function createPhosphor(target: HTMLCanvasElement): Phosphor | null {
  if (halfFloatRenderable()) {
    try {
      const gl = createGl(target);
      if (gl) return gl;
    } catch (e) {
      console.warn("[phosphor] half-float accumulator failed, using the 8-bit fallback", e);
    }
  }
  return create2d(target);
}
