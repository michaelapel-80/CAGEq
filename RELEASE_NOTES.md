A fit-quality fix for the Rust port introduced in 0.14.0, plus a UI responsiveness fix.

- **Fixed:** the AutoEq fit could occasionally place one or two parametric bands at odd
  very-low frequencies (near 20-30 Hz) that real AutoEq never produces, on some
  headphone/target combinations. The Rust solver was running its band search on a finer
  frequency grid than AutoEq actually uses for that step, letting sub-audible noise in the
  measurement register as a "peak" worth fitting; it now matches AutoEq's own grid exactly.
- **Fixed:** applying a correction or generating a mobile export could briefly freeze the
  whole window - these now run in the background instead of blocking the UI thread.
