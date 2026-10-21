Small refinements to the spectrum, oscilloscope and impulse-response export.

- **New:** the spectrum's window setting now shows the frequency resolution under the window
  length, so you can see both sides of the trade-off as you tune it: a shorter window follows
  the music more closely, a longer one separates closer tones.
- **Changed:** the oscilloscope's Fast preset now draws the waveform without a trail. With the
  steadier trigger from 0.17.5 there is no smear left to hide.
- **Fixed:** the impulse-response export's preview showed the impulse chart, whose display
  cut-off read as the end of the file. It now shows the curve and phase the file reproduces
  exactly; the file's length is in the table below.
- All changes are in the app itself; no engine update needed.
