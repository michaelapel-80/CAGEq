New test signals, a Tilt display mode for the spectrum views, and exporting a slot's correction to a phone.

- **New:** the test-tone generator gains a **frequency sweep (chirp)** signal, log or linear, and
  **AM/FM** signals — useful for checking frequency resolution, sidebands, and envelope/vibrato
  behavior beyond a steady tone.
- **New:** a **Tilt** toggle on SpectrumScope and EqChart's spectrum backdrop — the conventional
  RTA reading instead of the density-correct one: pink noise reads flat and a tone reads at its
  true level, at the cost of no longer being a true spectral density. Independent per view; on by
  default.
- **New:** **export a slot's correction for a mobile EQ app** — a new Export button next to Save
  Preset. Choose a free low-band-count parametric fit (with a live preview and an Fc/Gain/Q table
  for manual entry), or AutoEq's own standard 10-/31-band graphic EQ; copy the result straight to
  the clipboard.
