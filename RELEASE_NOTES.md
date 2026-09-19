A responsiveness fix for the spectrum analyzer's larger FFT window sizes.

- **Fixed:** at the Medium and High FFT window sizes, the spectrum only refreshed about 6-12 times
  per second (High waited ~171 ms between updates) instead of the ~23 the default size gets. The
  update interval is now the same at every window size, so the larger sizes feel as responsive as
  the default while keeping their finer frequency resolution — and the phosphor "Distribution"
  preset now has a steady stream of frames to integrate at every size. The default size is
  unchanged.
