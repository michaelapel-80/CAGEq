A steadier oscilloscope trigger.

- **Improved:** the oscilloscope's trigger now locks exactly onto the waveform's zero crossing at
  every pitch. Before, its filter delayed the trigger by a pitch-dependent 2-10 ms, so the
  waveform sat off the trigger point and slid sideways as notes changed. The trigger filter
  now defaults to 120 Hz, which follows more bass lines, and goes down to 20 Hz. Saved
  oscilloscope settings keep their old cutoff until you reset them.
- **Changed:** the warping-corrected filter option no longer says "analog-matched" in the app,
  which read like an analog sound. It's about accuracy, not sound character.
