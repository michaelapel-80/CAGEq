Smooth, steady instruments at any refresh rate.

- **Changed:** the oscilloscope, vectorscope and level meter now play their audio out on a steady
  clock, in step with what you hear, instead of drawing it in bursts as it arrives. Motion is even
  from frame to frame on 60, 144 and 240 Hz displays alike.
- **Changed:** all instruments time their frames by the display's own refresh, so motion and
  brightness no longer wobble slightly from frame to frame.
- **Changed:** the vectorscope's and the meter's beam is drawn as one continuous path, without
  the tiny gap it used to leave every few milliseconds.
- **Note:** while you drag or resize the window, the scopes may pause for a moment and catch up.
  Windows holds back their data during a drag.
