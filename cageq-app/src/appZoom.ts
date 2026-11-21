/**
 * Fit-to-window scaling for the main window. The layout is designed at a maximum width
 * (`.app`'s `max-width`, mirrored as `APP_MAX_WIDTH`) and its height follows from that width
 * (the chart keeps a fixed 720:215 shape, everything else takes its natural height). So a window
 * larger than that design left the content at its natural size with empty space around it.
 *
 * Instead the whole app is scaled uniformly, with CSS `zoom` on `.app`: text, chart, faders and
 * spacing all grow together, so the design stays as it is, just bigger, until the window's width
 * or height runs out, whichever comes first. Capped at `MAX_ZOOM` so a very large monitor does not
 * get comically large text. Windows no wider than the design keep the former rules: 100%, or 90%
 * on a short window (`SHORT_VIEWPORT_PX`, e.g. a 1080p laptop at 125% scaling), where the content
 * may still scroll.
 *
 * Canvases size their pixel buffers from `getBoundingClientRect`, which does report zoomed sizes,
 * but a `ResizeObserver` does not fire when only the zoom changes (measured in Chromium 152). So
 * every zoom change is announced with `APP_ZOOM_EVENT`, and the canvas views re-measure on it too.
 */

/** `.app`'s `max-width` in App.css. */
export const APP_MAX_WIDTH = 1500;
/** The most the app is scaled up on a large window. */
const MAX_ZOOM = 1.5;
/** Below this window height the app is shown at `SHORT_ZOOM` (and never scaled up). */
const SHORT_VIEWPORT_PX = 900;
const SHORT_ZOOM = 0.9;

/** CSS pixels the fit leaves free below the content: an exact fit can round a fraction of a pixel
 *  over and bring up a scroll bar with next to no range. */
const FIT_SLACK_PX = 2;

/** Fired on `window` after the app's zoom changed. */
export const APP_ZOOM_EVENT = "cageq-app-zoom";

let current = 1;

/** The zoom currently applied to `.app`. Divide a `getBoundingClientRect` size of anything inside
 *  it by this before using it as a CSS length: the rect is in on-screen pixels, CSS lengths inside
 *  `.app` are in its own, unzoomed units. */
export function appZoom(): number {
  return current;
}

/** Scale `app` to fit the window (see the module doc), and announce it if it changed. */
export function fitAppToWindow(app: HTMLElement): void {
  const before = app.style.zoom;
  const body = getComputedStyle(document.body);
  const vw = document.documentElement.clientWidth - parseFloat(body.marginLeft) - parseFloat(body.marginRight);
  const vh = window.innerHeight - parseFloat(body.marginTop) - parseFloat(body.marginBottom);
  const base = window.innerHeight < SHORT_VIEWPORT_PX ? SHORT_ZOOM : 1;
  const byWidth = (vw - FIT_SLACK_PX) / APP_MAX_WIDTH; // the same headroom against snapping, sideways
  let zoom = base;
  if (byWidth > base) {
    // At exactly `byWidth` the layout is the full design width, so its height is the design's
    // height at that width. Any smaller zoom keeps the same layout width (capped by max-width),
    // so that height stays valid: measure once there, then fit the height too.
    app.style.zoom = String(byWidth);
    const designHeight = app.getBoundingClientRect().height / byWidth;
    zoom = Math.max(base, Math.min(byWidth, (vh - FIT_SLACK_PX) / designHeight, MAX_ZOOM));
    // Zoomed layout is not exactly linear: Chromium snaps borders, line boxes and rounded lengths
    // to device pixels at each zoom, so the height predicted from one zoom can miss the real one at
    // another by a few pixels (measured 3.6 px at 141%, a scroll bar with 1-2 px of range). So
    // check the real height and scale down by what is still over; it settles in a step or two.
    for (let i = 0; i < 4; i++) {
      app.style.zoom = String(zoom);
      const h = app.getBoundingClientRect().height;
      if (h <= vh - FIT_SLACK_PX || zoom <= base) break;
      zoom = Math.max(base, (zoom * (vh - FIT_SLACK_PX)) / h);
    }
  }
  app.style.zoom = String(zoom);
  current = zoom;
  if (app.style.zoom !== before) window.dispatchEvent(new Event(APP_ZOOM_EVENT));
}

/** localStorage key: the inline scopes' tube height, in `.app`'s own units (see `noteScopeTube`). */
const SCOPE_TUBE_KEY = "cageq-scope-tube-px";
/** Until the inline scopes have measured one: roughly their tube at the full design width. */
const SCOPE_TUBE_DEFAULT_PX = 270;

function readScopeTube(): number {
  try {
    const v = Number(localStorage.getItem(SCOPE_TUBE_KEY));
    return Number.isFinite(v) && v > 0 ? v : SCOPE_TUBE_DEFAULT_PX;
  } catch {
    return SCOPE_TUBE_DEFAULT_PX;
  }
}

let scopeTube = readScopeTube();
// The pop-out windows share localStorage with the main window that writes it.
window.addEventListener("storage", (e) => {
  if (e.key === SCOPE_TUBE_KEY) scopeTube = readScopeTube();
});

/** The inline scopes' tube height as measured on screen (CSS px), noted in `.app`'s own units —
 *  what a detached scope sizes its beam against (`beamScale`). */
export function noteScopeTube(onScreenPx: number): void {
  const px = onScreenPx / current;
  if (!(px > 0) || Math.abs(px - scopeTube) < 0.5) return;
  scopeTube = px;
  try {
    localStorage.setItem(SCOPE_TUBE_KEY, px.toFixed(1));
  } catch {
    // Not shared, still used in this window.
  }
}

/**
 * What a scope's beam width takes on top of its tube's own size (`beam × tube / REF_SIZE`), so the
 * beam keeps the on-screen width it has inline at the design size: a real beam's spot doesn't grow
 * with the screen. Inline, the app's zoom divided back out — a larger window scales the whole app up,
 * tube included. In a detached window (`fill`), the inline tube's height over this one's
 * (`tubeOnScreenPx`): a full-screen pop-out is a much bigger tube, and scaled with it the beam read
 * as several times too thick (~5x for the time scope full screen).
 */
export function beamScale(fill: boolean, tubeOnScreenPx: number): number {
  return fill ? scopeTube / Math.max(1, tubeOnScreenPx) : 1 / current;
}
