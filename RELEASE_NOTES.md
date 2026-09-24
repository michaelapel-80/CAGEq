Warping-corrected (analog-matched) filters.

- **New:** an optional warping-corrected filter design, next to the loudness controls. Filters
  keep their analog shape up to 20 kHz instead of being squeezed toward Nyquist by the standard
  (RBJ) design, and sound the same at 44.1, 48 or 96 kHz. It is mostly audible on treble bands,
  and turning it on redoes the AutoEq fit for the new design. It needs CAGEq's own audio engine,
  because Equalizer APO always designs standard filters.
- **New:** the chart, the scopes and the export preview now use the same filter code as the
  audio engine (compiled to WebAssembly), so what you see can't drift from what you hear.
- **New:** export has an "App uses analog-matched filters" option. Leave it off for nearly every
  EQ app. The export is fitted so standard filters reproduce what you hear in CAGEq.
- **Engine update:** CAGEq's own audio engine has a new version that reads the new configuration
  format. The app flags it after updating: Continue refreshes it, with a brief machine-wide
  audio interruption.
- **Fixed:** the third-party license file was out of date and now credits NLopt (LGPL-2.1).
