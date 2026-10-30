The instruments are now drawn on the GPU — a fraction of the CPU load, with the same look.

- **Changed:** the spectrum, the spectrum behind the EQ chart, the oscilloscope, the vectorscope
  and the level meter draw their traces on the graphics card instead of the processor. Measured
  on a 240 Hz display with a maximised window, CPU load dropped to roughly a fifth (the spectrum
  from most of a CPU core to under a fifth of one), and the oscilloscope now keeps up at full
  frame rate where it used to drop frames. Lower-end machines should benefit most. Machines whose
  graphics can't do this keep the previous drawing.
- **Changed:** the vectorscope and the level meter now add up their beam's light wherever it
  passes, the way a real CRT beam deposits energy — busy material and overlapping paths read
  slightly brighter in the vectorscope, and the meter shows how often the level visits each height.
  The meter's Glow default is much lower to suit, and its slider now scales logarithmically. A Glow
  setting you saved for the meter is converted to the new scale automatically.
- **Changed:** the pop-out vectorscope now renders at full resolution on large windows instead of
  being stretched.
- **New:** the vectorscope's graticule also shows the side (S) diagonal when not rotated, and its
  L/R/M/S labels can be turned off in its settings.
- All changes are in the app itself; no engine update needed.
