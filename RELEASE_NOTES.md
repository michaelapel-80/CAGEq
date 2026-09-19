Two spectrum analyzer fixes: responsiveness at the larger FFT window sizes, and the RTA/K-weighted
Tilt reading at low frequencies.

- **Fixed:** the "+3 dB" (RTA) and K-weighted Tilt modes read pink noise progressively too high
  toward low frequencies — about +9 dB at 20 Hz and +4-5 dB at 50 Hz — instead of flat. They now
  read flat down to 20 Hz. Trade-off: below roughly 150 Hz, an isolated tone reads lower on the
  drawn curve (the display bins there are finer than the window can resolve); peak markers and the
  hover readout still report its true level.
- **Fixed:** at the Medium and High FFT window sizes, the spectrum only refreshed about 6-12 times
  per second (High waited ~171 ms between updates) instead of the ~23 the default size gets. The
  update interval is now the same at every window size, so the larger sizes feel as responsive as
  the default while keeping their finer frequency resolution — and the phosphor "Distribution"
  preset now has a steady stream of frames to integrate at every size. The default size is
  unchanged.
