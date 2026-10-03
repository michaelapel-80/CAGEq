A more faithful mobile export.

- **New:** the Graphic EQ export now offers **Curve**: the exact correction as AutoEq's 127-point
  GraphicEQ format, for apps that import it (Equalizer APO, Wavelet, Poweramp, JamesDSP,
  EasyEffects).
- **Changed:** the 10- and 31-band values are now the curve's average over each band. They used
  to be fitted for a slider design most EQ apps don't actually use (Wavelet's bands don't overlap,
  for example), so they were often off in exactly the apps people use.
- **Improved:** the Parametric export now matches the treble above 10 kHz, not just its average
  level, and can place bands up to 16 kHz. A hand-placed treble band, like a notch at 12-15 kHz,
  now comes through the export intact.
- **Changed:** on a fresh install the spectrum analyzer starts with a slightly longer window
  (240 ms) for finer frequency resolution. Existing settings are kept.
- **Fixed:** the arrow keys on a band's frequency got stuck between 20 and 25 Hz.
