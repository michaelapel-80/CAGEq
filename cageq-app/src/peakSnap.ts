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
 *
 * Only while Shift is held: a cursor that jumps onto every peak it passes is distracting when
 * you are not reading one. Where it could snap but Shift is up, the cursor label shows a faint
 * "⇧" — the one hint, and only where it applies.
 */

// Whether Shift is held, for every view in this window: one listener set, read each frame. Mouse
// events also report it (`noteShift`), which corrects it after a key-up the window missed while
// another window had focus.
let shiftHeld = false;
if (typeof window !== "undefined") {
  window.addEventListener("keydown", (e) => {
    if (e.key === "Shift") shiftHeld = true;
  });
  window.addEventListener("keyup", (e) => {
    if (e.key === "Shift") shiftHeld = false;
  });
  window.addEventListener("blur", () => {
    shiftHeld = false;
  });
}

/** Is a snap asked for right now (Shift held)? */
export function snapRequested(): boolean {
  return shiftHeld;
}

/** Take Shift's state from a mouse event over a spectrum view. */
export function noteShift(e: { shiftKey: boolean }): void {
  shiftHeld = e.shiftKey;
}

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

/** A snapped peak's frequency for the readout: a tenth of a hertz at every frequency, the
 *  backend's own rounding of `peak_hz`. Measured (cageq-monitor's
 *  `peak_hz_accuracy_across_frequency_window_and_noise`), the refinement is within ~0.1 Hz while
 *  the tone clearly stands out, and within ~0.5 Hz where it barely does — there the last digit
 *  flickers from frame to frame, which shows that uncertainty in the live readout. Above 1 kHz
 *  that is four decimals in kHz, keeping the unsnapped cursor's unit ("15.6234 kHz"); below it
 *  hertz with one decimal ("100.4 Hz"). An unsnapped cursor keeps its coarser format:
 *  there the position is only as precise as a pixel (~170 Hz near 19 kHz). */
export function fmtSnappedHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(4)} kHz` : `${hz.toFixed(1)} Hz`;
}
