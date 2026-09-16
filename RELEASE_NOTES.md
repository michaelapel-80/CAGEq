A round of bug fixes from a full-codebase review — nothing new to learn, just more correct.

- **Fixed:** the EQ chart's K-weighted Tilt cursor readout showed a weighted number instead of the
  raw measurement.
- **Fixed:** the fail-safe watchdog could rarely hang instead of tripping, if the safe-state write
  itself was slow.
- **Fixed:** a custom filter with an invalid Q or frequency (e.g. dragged to zero) could silently
  corrupt the applied EQ instead of being rejected.
- **Fixed:** CAGEq's own APO could permanently leave "disable audio enhancements" turned on for an
  endpoint after being detached.
- **Fixed:** the DSP sidecar process wasn't reliably terminated when the app closed.
- **Fixed:** a rare race could lose a settings update if two changes landed at the same time.
- **Fixed:** Self-Test could measure an active solo/isolate audition instead of the real
  correction.
- Hardened the real-time control channel's cross-process memory safety, and improved screen-reader
  labels on the preset list's icon buttons.
