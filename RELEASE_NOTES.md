A stepless spectrum window, plus two small UI fixes.

- **Added:** the spectrum analyzer's window size is now a stepless slider (~171–683 ms) instead of
  three fixed steps, and the readout shows the window's real duration at your device's sample rate
  (it was off by ~8% at 44.1 kHz).
- **Added:** "Hi-res" peak detection is now its own checkbox in the Spectrum tuning panel instead of
  being implied by a longer window. If you'd already picked a longer window, it starts enabled.
- **Added:** the mobile export dialog shows a spinner while the solver runs, so a high band count no
  longer looks frozen until the result pops in. The stale result is dimmed and Copy is disabled
  until the new fit lands.
- **Fixed:** the "Emergency clipping protection" notice appearing or disappearing shifted the layout
  and could fight a band's gain fader mid-drag. It's now overlaid on the chart's top-left corner and
  takes no space.
