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
  const byWidth = vw / APP_MAX_WIDTH;
  let zoom = base;
  if (byWidth > base) {
    // At exactly `byWidth` the layout is the full design width, so its height is the design's
    // height at that width. Any smaller zoom keeps the same layout width (capped by max-width),
    // so that height stays valid: measure once there, then fit the height too.
    app.style.zoom = String(byWidth);
    const designHeight = app.getBoundingClientRect().height / byWidth;
    zoom = Math.max(base, Math.min(byWidth, vh / designHeight, MAX_ZOOM));
  }
  app.style.zoom = String(zoom);
  current = zoom;
  if (app.style.zoom !== before) window.dispatchEvent(new Event(APP_ZOOM_EVENT));
}
