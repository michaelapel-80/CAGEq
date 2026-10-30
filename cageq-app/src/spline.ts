/**
 * Monotone cubic Hermite spline through arbitrarily-spaced points — shared by SpectrumScope's own
 * trace and EqChart's spectrum backdrop, both of which draw the same underlying FFT data
 * (cageq-monitor's log-frequency bins) and hit the identical reason to want it smoothed: a plain
 * per-bin polyline visibly shows the individual bins as a jagged staircase, most noticeably where
 * bins are widest in Hz but densest in log-x pixels (the top octaves).
 */

/** The tangent at point `i` for a *monotone* non-uniform cubic Hermite spline (Fritsch-Carlson in
 *  spirit: constrain the tangent so the curve can't leave the range its neighbours bound it to).
 *  Each secant weighted by the *opposite* segment's width, so a short adjacent segment pulls the
 *  tangent toward its own slope instead of the far one's — matters here because the backend's
 *  per-bin Gaussian reduction (`cageq-monitor`'s `gaussian_power`) doesn't put bin centres at even
 *  Hz spacing on this log axis in general. Endpoints just use the one available secant.
 *
 *  A spline was tried in SpectrumScope before this module existed, over three live-tested rounds,
 *  and thrown out (a plain polyline replaced it — see git history) once it became clear the actual
 *  bug wasn't the curve shape at all: the backend's OLD `max`-based reduction let many adjacent
 *  display bins share the *exact* same value (oversampling a coarse linear FFT grid), and the dedup
 *  built to collapse those literal ties kept discarding or reinventing shape across gaps in a way a
 *  spline's tangent math couldn't cleanly recover from. `gaussian_power` replaced that reduction
 *  entirely — a smooth function of each bin's own (never-repeating) fractional range, so adjacent
 *  bins essentially never produce bit-for-bit identical values any more. With no ties, there's
 *  nothing to dedup: every bin gets its own spline control point, and the monotone tangent here
 *  exists only for what's left — a plain sharp residual bump (the backend fix's own case history
 *  notes one, ~2.7 dB, 41 dB down — see `gaussian_power`'s doc) can't make the curve swing past its
 *  own true height on the way through. */
function hermiteTangent(xs: Float64Array, ys: Float64Array, n: number, i: number): number {
  if (i <= 0) return (ys[1] - ys[0]) / (xs[1] - xs[0]);
  if (i >= n - 1) return (ys[n - 1] - ys[n - 2]) / (xs[n - 1] - xs[n - 2]);
  const hL = xs[i] - xs[i - 1];
  const hR = xs[i + 1] - xs[i];
  const sL = (ys[i] - ys[i - 1]) / hL;
  const sR = (ys[i + 1] - ys[i]) / hR;
  if (sL === 0 || sR === 0 || sL > 0 !== sR > 0) return 0; // local extremum — flatten, don't overshoot it
  const avg = (hR * sL + hL * sR) / (hL + hR);
  const cap = Math.min(Math.abs(sL), Math.abs(sR));
  return Math.sign(avg) * Math.min(Math.abs(avg), cap);
}

/** Trace a smooth cubic Hermite spline through `n` points `(xs[i], ys[i])`, tangents from
 *  `hermiteTangent` — passes exactly through every point, correct for arbitrarily-spaced x.
 *  Converted to cubic Bezier per segment (tangent scaled by a third of the segment's own width —
 *  the standard Hermite-to-Bezier conversion, and *why* it needs the true per-segment width rather
 *  than assuming a uniform one) since canvas has no native spline primitive. Starts with its own
 *  `moveTo(xs[0], ys[0])`, so call it as (or right after) the first thing inside a `beginPath()` —
 *  the path's current point afterward is `(xs[n-1], ys[n-1])`, ready for the caller to `stroke()`
 *  directly or extend further (e.g. down to a baseline) before `fill()`. */
export function traceSmooth(ctx: CanvasRenderingContext2D, xs: Float64Array, ys: Float64Array, n: number) {
  ctx.moveTo(xs[0], ys[0]);
  for (let i = 0; i < n - 1; i++) {
    const h = xs[i + 1] - xs[i];
    const m0 = hermiteTangent(xs, ys, n, i);
    const m1 = hermiteTangent(xs, ys, n, i + 1);
    const cp1x = xs[i] + h / 3;
    const cp1y = ys[i] + (m0 * h) / 3;
    const cp2x = xs[i + 1] - h / 3;
    const cp2y = ys[i + 1] - (m1 * h) / 3;
    ctx.bezierCurveTo(cp1x, cp1y, cp2x, cp2y, xs[i + 1], ys[i + 1]);
  }
}

/** Most pieces `tessellateSmooth` cuts one segment into. */
const MAX_PIECES = 16;
/** How far (px) a tessellated chord may stray from the true curve — well under a pixel, so the
 *  polyline is indistinguishable from what `traceSmooth` strokes. */
const FLATNESS_PX = 0.2;

/** Points `tessellateSmooth` can write for `n` input points, at most — size its `out` for this. */
export function tessellatedCapacity(n: number): number {
  return Math.max(1, (n - 1) * MAX_PIECES + 1);
}

/** The curve `traceSmooth` draws, as a polyline for the GPU trace (phosphor.ts's
 *  `strokeTrace`): the same Bezier per segment, cut into as many straight pieces as its curvature
 *  needs to stay within `FLATNESS_PX` of the curve (a chord's error is at most an eighth of the
 *  second derivative's bound over the square of the piece count). Writes interleaved x, y into
 *  `out` (at least `tessellatedCapacity(n) * 2` long) and returns the number of points. */
export function tessellateSmooth(xs: Float64Array, ys: Float64Array, n: number, out: Float32Array): number {
  out[0] = xs[0];
  out[1] = ys[0];
  let k = 1;
  for (let i = 0; i < n - 1; i++) {
    const h = xs[i + 1] - xs[i];
    const x0 = xs[i];
    const y0 = ys[i];
    const x1 = x0 + h / 3;
    const y1 = y0 + (hermiteTangent(xs, ys, n, i) * h) / 3;
    const x3 = xs[i + 1];
    const y3 = ys[i + 1];
    const x2 = x3 - h / 3;
    const y2 = y3 - (hermiteTangent(xs, ys, n, i + 1) * h) / 3;
    const dd = Math.max(Math.hypot(x0 - 2 * x1 + x2, y0 - 2 * y1 + y2), Math.hypot(x1 - 2 * x2 + x3, y1 - 2 * y2 + y3));
    const pieces = Math.min(MAX_PIECES, Math.max(1, Math.ceil(Math.sqrt((6 * dd) / (8 * FLATNESS_PX)))));
    for (let j = 1; j <= pieces; j++) {
      const t = j / pieces;
      const u = 1 - t;
      const a = u * u * u;
      const b = 3 * u * u * t;
      const c = 3 * u * t * t;
      const d = t * t * t;
      out[2 * k] = a * x0 + b * x1 + c * x2 + d * x3;
      out[2 * k + 1] = a * y0 + b * y1 + c * y2 + d * y3;
      k++;
    }
  }
  return k;
}
