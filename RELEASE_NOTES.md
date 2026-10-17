A steadier oscilloscope trigger.

- **Fixed:** the oscilloscope's trigger could stop locking when the waveform sat off-centre, as
  in oscilloscope music, which uses near-DC content to position its image. It now triggers on
  the waveform's own centre line wherever that sits.
- **Changed:** the trigger filter now defaults to 60 Hz (was 120 Hz) for a steadier lock. Low
  settings no longer stop the scope triggering. If you have used the scope before, your own
  setting is kept.
- The warping-corrected filters' description no longer says "up to 20 kHz": it describes the
  whole audible range.
- All changes are in the app itself; no engine update needed.
