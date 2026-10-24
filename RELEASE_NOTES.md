Precise peak readings in the spectrum, and a few smaller fixes.

- **New:** hold Shift while hovering a spectrum to snap the cursor onto the nearest peak and
  read its exact frequency (to the hertz) and level, also for quiet lines the peak list never
  shows. A faint ⇧ in the cursor label marks where a snap is possible.
- **Fixed:** switching the filter model or the loudness mode ended a frequency audition
  (middle mouse button or an isolated band), but the app still showed it as active. It now
  shows it as ended.
- **Changed:** the loudness mode texts are shorter, with the full explanations in their
  tooltips; the Loudness and Compare panels no longer repeat each other.
- Updated dependencies, including the build tool Vite (its security advisories concern only the
  development server, not the app).
- All changes are in the app itself; no engine update needed.
