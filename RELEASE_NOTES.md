Spectrum analyzer accuracy pass, plus a couple of real bugs found while chasing it down.

- **Fixed:** the spectrum analyzer's peak readout was badly wrong at the top of the range (off by
  hundreds of Hz). Peak-finding now runs on the real, raw spectrum instead of the already-smoothed
  display curve.
- **Fixed:** the test-tone generator itself was playing the wrong pitch at high frequencies (a
  15 kHz+ tone could be audibly off, or silent altogether near the top of the range) — unrelated
  bug, found while tracking down the one above.
- **Fixed:** the spectrum/scope views could come up blank on launch if Dry was the last-active A/B
  slot.
- **New:** optional linear-frequency mode for the spectrum analyzer — reads a harmonic series
  (hum, motor noise, a test tone's own partials) as an evenly-spaced comb instead of bunched at
  the low end.
- **New:** the old high-res checkbox is now a 3-position window-size slider (Base/Med/High).
