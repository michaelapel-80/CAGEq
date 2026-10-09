High-pass filter against infrasound.

- **New:** a high-pass band type. Open headphones need a lot of bass boost to reach the Harman
  target, and a bass shelf keeps boosting all the way down, so infrasound in films or some music
  can push the drivers to their excursion limit. Click a band's type icon to cycle to High-pass
  (after Tilt): set its frequency and a slope of 12, 24, 36 or 48 dB/oct. It starts at 20 Hz,
  24 dB/oct. On the chart, drag its node sideways for the frequency and scroll to change the
  slope. One high-pass per slot.
- The spectrum, scope and vectorscope "undo EQ" views leave the high-pass in: what it removed
  can't be restored.
- Parametric export fits the other bands as before and adds the high-pass as `HPQ` lines.
- **Engine update:** CAGEq's own audio engine needs updating for the high-pass. The app flags
  the update after updating: Continue refreshes it, with a brief machine-wide audio interruption.
  Equalizer APO users need nothing.
