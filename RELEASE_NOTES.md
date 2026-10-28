A sharper, more honest spectrum view.

- **Fixed:** on a large or maximised window, peaks in the upper part of the spectrum looked
  misshapen. The curve is now drawn from four times as many points, so it shows its real shape;
  the levels it reads are unchanged.
- **Changed:** anything quieter than the bottom of the spectrum's level range now drops off the
  plot instead of lying flat along it, where it read as a real flat section.
- **New:** the bottom of that range is adjustable — Floor in the spectrum's settings (−150 to
  −30 dBFS; the default stays at −90).
- All changes are in the app itself; no engine update needed.
