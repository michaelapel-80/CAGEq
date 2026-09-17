A small tuning-quality-of-life update for the scope views (Spectrum/Time/Vectorscope), plus routine
dependency security bumps.

- **Added:** curated "Fast"/"Distribution" render-tuning presets, plus a "Saved" preset for your
  own saved default — all three sit next to the existing Save/Reset controls.
- **Added:** the Trail slider's usable range is no longer artificially capped short.
- **Changed:** each scope view now remembers exactly where you left its tuning sliders across
  restarts, independently of your saved default (which is now a preset you load on demand, not
  something that's silently reapplied on launch).
- Updated a few frontend build dependencies (postcss, browserslist, baseline-browser-mapping) to
  patch known vulnerabilities.
