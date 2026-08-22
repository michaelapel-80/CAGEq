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
 * At the default `add` blend, `commit()` is **additive** — that's what produces dwell saturation,
 * where overlapping passes stack toward white. Anything drawn at a constant alpha every frame
 * (Vectorscope's resting spot, TimeScope's peak-hold lines) must therefore live on its own
 * non-accumulating canvas layer, or it will pile up frame over frame instead of holding steady.
 * The `over` blend has no such hazard — it converges rather than stacking — which is why the
 * spectrum *backdrop* uses it: blooming would fight the EQ curves drawn over the top.
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
const DOSE_REF_FPS = 240;
const DOSE_REF_DT = 1 / DOSE_REF_FPS;

/** How this frame's trace lands on the decayed history. `add` (default) is the scopes' additive
 *  beam; `over` is the plain over-operator for backdrop-style layers that must not bloom. */
export type PhosphorBlend = "add" | "over";

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
   * `punch` (0..1, default 0) only matters for the `over` blend: 0 is the plain over-operator
   * (converges on its own colour, never blooms — see the module doc's "What must NOT be drawn
   * through this"); 1 makes `over` behave exactly like `add` (stacks toward white, uncapped). Values
   * in between let content that keeps landing in the same place build up real brightness without
   * going all the way to `add`'s full saturation — for a backdrop that wants a *little* pop without
   * competing with what's drawn over it. No effect on `add` blend (already maximally additive) or
   * the 2D fallback (ignored, like `tail`).
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
   */
  commit(dt: number, tau: number, tail?: number, punch?: number, doseMult?: number): void;
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
uniform sampler2D uPrev, uTrace; uniform float uKeep, uKeepTail, uKnee, uFloor, uOver, uDose, uPunch;
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
  // "add" stacks toward saturation and has no hue ceiling — the beam overdraw the scopes want.
  // "over" is the plain over-operator, so repeated identical content converges on its own colour
  // instead of blooming: what a dim *backdrop* (EqChart's spectrum) wants, where blowing out would
  // fight the curves drawn on top of it. uPunch (0..1, 0 for every existing caller) partially
  // undoes "over"'s own self-limiting discount of the existing trail — at 0 it's the plain
  // (1.0 - cur.a) factor (today's behaviour, unchanged); at 1 the discount vanishes entirely and
  // the over-branch reduces to exactly the add-branch's formula. A dial between "converges, never
  // blooms" and "stacks toward white", rather than only the two ends of it.
  float overKeep = 1.0 - cur.a * (1.0 - uPunch);
  gl_FragColor = min(uOver > 0.5 ? cur + decayed * overKeep : decayed + cur, 1.0);
}`;

const FS_BLIT = `precision highp float; varying vec2 vUv; uniform sampler2D uTex;
void main(){ gl_FragColor = texture2D(uTex, vUv); }`;

function compile(gl: WebGLRenderingContext, type: number, src: string): WebGLShader | null {
  const sh = gl.createShader(type);
  if (!sh) return null;
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  return gl.getShaderParameter(sh, gl.COMPILE_STATUS) ? sh : null;
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
  return gl.getProgramParameter(prog, gl.LINK_STATUS) ? prog : null;
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

function createGl(target: HTMLCanvasElement, blend: PhosphorBlend): Phosphor | null {
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
  const uOver = gl.getUniformLocation(accum, "uOver");
  const uDose = gl.getUniformLocation(accum, "uDose");
  const uPunch = gl.getUniformLocation(accum, "uPunch");
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

  const mkTex = (type: number) => {
    const t = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST); // 1:1 texel:pixel
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, target.width, target.height, 0, gl.RGBA, type, null);
    return t;
  };

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

  return {
    precise: true,
    begin() {
      resize();
      scratch.sync();
      scratch.ctx!.clearRect(0, 0, target.width, target.height);
      return scratch.ctx!;
    },
    commit(dt, tau, tail = 1, punch = 0, doseMult = 1) {
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
      gl.uniform1f(uOver, blend === "over" ? 1 : 0);
      gl.uniform1f(uDose, (dt / DOSE_REF_DT) * doseMult);
      gl.uniform1f(uPunch, punch);
      gl.uniform1i(uPrev, 0);
      gl.uniform1i(uTrace, 1);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, ping[src].tex);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      gl.viewport(0, 0, W, H);
      gl.useProgram(blit);
      gl.uniform1i(uTex, 0);
      gl.bindTexture(gl.TEXTURE_2D, ping[dst].tex);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      src = dst;
    },
    dispose() {
      gl.deleteTexture(texTrace);
      for (const pp of ping) {
        gl.deleteTexture(pp.tex);
        gl.deleteFramebuffer(pp.fbo);
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
function create2d(target: HTMLCanvasElement, blend: PhosphorBlend): Phosphor | null {
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
    commit(dt, tau, _tail = 1, _punch = 0, doseMult = 1) {
      const W = target.width;
      const H = target.height;
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${1 - Math.exp(-dt / tau)})`;
      ctx.fillRect(0, 0, W, H);
      ctx.globalCompositeOperation = blend === "over" ? "source-over" : "lighter";
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
export function createPhosphor(target: HTMLCanvasElement, blend: PhosphorBlend = "add"): Phosphor | null {
  if (halfFloatRenderable()) {
    try {
      const gl = createGl(target, blend);
      if (gl) return gl;
    } catch (e) {
      console.warn("[phosphor] half-float accumulator failed, using the 8-bit fallback", e);
    }
  }
  return create2d(target, blend);
}
