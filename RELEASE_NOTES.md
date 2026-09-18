CAGEq no longer depends on Python at all — the headphone-correction fitting engine and the
measurement/target catalogue browsing, previously a Python (NumPy/SciPy/AutoEq) sidecar process,
now run natively in Rust, checked against the original implementation across its full ~6800-file
measurement corpus rather than assumed equivalent.

- **Changed:** fitting a headphone correction and browsing the AutoEq catalogue no longer spawns a
  Python process — both are native Rust now. In practice this means a smaller installer (no bundled
  Python interpreter/NumPy/SciPy) and no per-launch Python startup cost.
- **Fixed:** two subtle bugs in the new Rust solver's objective function (an extra penalty term and
  a missing normalization step) that could very occasionally make a fit converge slightly
  differently than the reference algorithm; fits now match exactly.
- **Fixed:** a failed configuration write no longer risks leaving whatever was last on disk playing
  silently — CAGEq falls back to its defined safe (silent) state on that failure, same as before
  this change.
