A perceptual K-weighted Tilt mode, a way to skip AutoEq entirely, and a full LUFS meter.

- **New:** a third **K-weighted** option on the spectrum Tilt toggle — layers the project's own
  ITU-R BS.1770-4 K-weighting curve on top of the RTA reading, a real perceptual-loudness tilt
  instead of an arbitrary slope. The Tilt toggle now shows small slope glyphs instead of
  easily-truncated text labels.
- **New:** **skip AutoEq's fit entirely** for a headphone it has no good measurement for — a "flat
  start" option right in the measurement picker. Build the whole correction from your own
  Fit/Content/Tone bands instead, with nothing from AutoEq mixed in.
- **New:** the level meter gains a full BS.1770 loudness readout — **Integrated**, **Loudness
  Range** (EBU Tech 3342), and a **Peak Max** high-water mark, alongside the existing
  momentary/short-term numbers — with a one-click restart, since those three otherwise keep
  accumulating for as long as the meter stays open.
- **Fixed:** the spectrum analyzer's peak detector no longer reports peaks outside the displayed
  20 Hz–20 kHz range.
- Setup diagnostics catch two more ways the APO can silently fail to load: a missing
  processing-modes registry value, and the installed DLL not being readable by the account
  `audiodg` actually runs as.
