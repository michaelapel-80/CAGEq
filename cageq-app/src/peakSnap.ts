/**
 * Hover-cursor peak snapping for the spectrum views: when the cursor sits near a real peak, the
 * readout jumps to it and reports its exact frequency and level instead of the slope under the
 * pointer. Meant for reading quiet lines the peak finder never lists (it keeps only the few most
 * prominent): mains hum harmonics, a video line at 15.625 kHz, a converter or reverb artefact.
 *
 * Searches the display bins' per-bin peak levels (`peak_db` / `peak_db_lin`, the true level of
 * the strongest FFT bin in each span) around the cursor, and takes the frequency from the
 * matching `peak_hz` / `peak_hz_lin`, which the backend refines from the raw FFT. The display
 * bins alone would only place it to within a bin (~550 Hz near 19 kHz on the log axis).
 */

/** How far from the cursor a peak may be, as a fraction of the plot width (~10 px on a typical
 *  chart): close enough that the snap reads as "the peak I'm pointing at", not a jump. */
const SNAP_FRAC = 0.012;
/** How far the peak must stand above the lowest level within the search span. Below this, the
 *  cursor is on noise or a smooth slope, and snapping would only make the readout jitter. */
const MIN_PROMINENCE_DB = 3;

/**
 * The display bin to snap to for a cursor at fractional bin index `fi`, or null to read the
 * cursor position as before. `levels` is the per-bin peak level in dB, already corrected the way
 * the view reads it (undistort). Only a true local maximum strictly inside the search span
 * counts — one at its edge means the real peak lies further out, and the cursor is on its slope.
 */
export function snapBin(levels: ArrayLike<number>, fi: number): number | null {
  const n = levels.length;
  if (n < 3) return null;
  const half = Math.max(1, Math.round(SNAP_FRAC * (n - 1)));
  const lo = Math.max(0, Math.floor(fi) - half);
  const hi = Math.min(n - 1, Math.ceil(fi) + half);
  let best = lo;
  let floor = levels[lo];
  for (let i = lo; i <= hi; i++) {
    if (levels[i] > levels[best]) best = i;
    if (levels[i] < floor) floor = levels[i];
  }
  if (best === lo || best === hi) return null;
  if (!(levels[best] > levels[best - 1] && levels[best] >= levels[best + 1])) return null;
  return levels[best] - floor >= MIN_PROMINENCE_DB ? best : null;
}

/** A snapped peak's frequency for the readout: whole hertz at every frequency, the precision the
 *  backend's refinement actually delivers (to about a hertz, independent of frequency). Above
 *  1 kHz that is three decimals in kHz ("18.812 kHz"); below it plain hertz, since a fraction of a
 *  hertz would claim more than the FFT resolves. An unsnapped cursor keeps its coarser format:
 *  there the position is only as precise as a pixel (~170 Hz near 19 kHz). */
export function fmtSnappedHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(3)} kHz` : `${Math.round(hz)} Hz`;
}
