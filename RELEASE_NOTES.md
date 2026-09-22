A round of frontend performance fixes for the scope views and level meter.

- **Fixed:** the time scope, vectorscope, and level meter each allocated fresh buffers (typed
  arrays, or a `Path2D` per beam-velocity bucket) on every incoming audio window, tens of times a
  second — a steady stream of short-lived allocations that could trigger full "Major GC" pauses
  well above the frame-time budget. They now reuse the same buffers across windows instead.
- **Fixed:** the level meter re-rendered its whole React component on every single backend update
  (~60/s) because the update always arrived as a freshly-parsed object, defeating React's normal
  same-value skip. Its peak/RMS/LUFS marks now update directly instead of through a full re-render.
