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
 * measured dramatically faster than the canvas accumulator besides. Note the accumulator is
 * deliberately *not* clamped to 1.0: half-float holds values above full brightness, so a sustained
 * dwell genuinely overexposes and takes a moment to fall back through the visible range, and only
 * the final present clamps. That's closer to a real tube than canvas's clamp-at-every-step.
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
 */

/** Shared surface of both backends — see the module comment for why the fallback exists. */
export type Phosphor = {
  /** Clear and return the 2D context for this frame's trace. Sized to the target canvas. */
  begin(): CanvasRenderingContext2D;
  /** Decay the history (time constant `tau` seconds over `dt` seconds), add this frame, present. */
  commit(dt: number, tau: number): void;
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
const FS_ACCUM = `precision highp float; varying vec2 vUv;
uniform sampler2D uPrev, uTrace; uniform float uKeep;
void main(){
  vec4 cur = texture2D(uTrace, vUv);
  cur.rgb *= cur.a;
  gl_FragColor = texture2D(uPrev, vUv) * uKeep + cur;
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
  const uPrev = gl.getUniformLocation(accum, "uPrev");
  const uTrace = gl.getUniformLocation(accum, "uTrace");
  const uTex = gl.getUniformLocation(blit, "uTex");

  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true); // one flip on upload, none on present — orientations agree
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
    commit(dt, tau) {
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
    commit(dt, tau) {
      const W = target.width;
      const H = target.height;
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${1 - Math.exp(-dt / tau)})`;
      ctx.fillRect(0, 0, W, H);
      ctx.globalCompositeOperation = "lighter";
      ctx.drawImage(scratch.cv, 0, 0);
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
