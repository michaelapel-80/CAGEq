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
