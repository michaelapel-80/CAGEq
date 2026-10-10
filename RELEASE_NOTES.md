Smoother high-pass and band toggles.

- **Fixed:** switching the high-pass on or off could thump audibly on bass, more so at steeper
  slopes. CAGEq's own audio engine now takes as long over a change as the change needs — it
  measures how far the bass's timing shifts, not only how much the level changes — so the
  high-pass fades in and out over a fraction of a second instead of snapping.
- **Fixed:** toggling a band again before its previous fade had finished cut the new fade short
  and could click. Toggles always get their full fade now; dragging stays as immediate as before.
- **Fixed:** the first band toggle after starting the app could click, while later ones were
  clean. The app now checks which filter the engine is actually running in each position
  before changing anything.
- Other changes fade a little more gradually too (about a third longer), for the same reason.
- **Engine update:** the high-pass and re-toggle fixes are in CAGEq's own audio engine. The app
  flags the update after updating: Continue refreshes it, with a brief machine-wide audio
  interruption. The first-toggle fix is in the app itself. Equalizer APO users need nothing.
