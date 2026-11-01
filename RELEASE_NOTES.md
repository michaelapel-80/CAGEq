The instruments have a new look: their traces now behave like the light of a real CRT beam.

- **Your settings need a look:** saved instrument settings are converted automatically, but the
  picture has changed enough that they'll likely need a re-tune. Brightness settings (Glow, Bloom,
  Haze) move to the new defaults, while Trail, Tail, Beam and the rest stay as you had them. Reset
  in an instrument's tuning panel gives you the whole new default look.
- **Changed:** the oscilloscope, vectorscope, level meter and spectrum simulate the beam: its light
  depends on how long it spends in each place, so slow parts of a trace are bright and fast ones
  faint. All of them, and the spectrum behind the EQ chart, now work in real light rather than
  display values, so faint detail and long trails stay visible, and bright, dense parts can wash
  toward white. Machines whose graphics can't do this keep the previous drawing.
- **Changed:** the oscilloscope and vectorscope draw smooth curves between samples, without bright
  dots at every join.
- **Changed:** the level meter places high frequencies correctly. Their peaks often fall between
  samples, and the old drawing cut them off.
- **Changed:** the spectrum's steep flanks are dimmer than its flat stretches, replacing the old
  fade toward the floor.
- **New:** Saturation, Contrast and Headroom settings on all of them, to shape how bright parts
  roll off and how far they wash toward white. The oscilloscope, vectorscope and spectrum also get
  Bloom colour, from the beam's own hue to white.
- **New:** two oscilloscope settings for busy material. Speed dimming goes from the real beam to an
  evenly bright drawn line. BW limit smooths away detail too fine to show, and is off by default.
- All changes are in the app itself; no engine update needed.
