Efficiency fix for the spectrum analyzer's FFT-size slider.

- **Fixed:** the Med/High window-size positions were computing a larger zero-padded transform
  than their actual resolution gain needed — padding was growing right along with the window
  (2x/4x the transform length), when its only job is interpolating a fixed display resolution.
  Padding now shares one fixed budget across all three tiers instead of multiplying with them:
  same display smoothness, meaningfully less CPU at Med/High, in every build (not just debug).
