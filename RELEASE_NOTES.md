Bluetooth headset support for CAGEq's own engine, a clearer self-test, and chart and scope refinements.

- **Fixed (needs testing):** CAGEq's own engine never ran on some Bluetooth headsets that
  Windows 11 shows as a combined device (stereo and hands-free in one), while Equalizer APO
  worked on them. Windows doesn't run the effect slot CAGEq used there; CAGEq now attaches
  those devices in the slot that does run, as Equalizer APO's installer does. If yours was
  affected, the Engine panel offers to re-attach it. We couldn't test this on such a headset
  ourselves, so please report whether it works for yours.
- **New:** the self-test now checks that CAGEq's engine is actually running on the device
  before comparing the sound, and says so directly when it isn't, instead of reporting that no
  correction (or something unexpected) reaches the output.
- **Changed:** the "Ideal EQ" curve is now "Target EQ" and is drawn level-matched to your
  correction, the way auto-gain matches what you hear, so a mostly cutting correction no longer
  sits far below it and only the shapes differ.
- **Changed:** the oscilloscope now starts with the Fast preset's instant trace; the former
  default is available as the new Smooth preset.
- **New:** a saved scope default can now be deleted: with the Saved preset selected, the
  "Save as default" button becomes "Delete default".
- No engine update needed. Attaching a Bluetooth device in its new slot asks for administrator
  rights once, like attaching any device.
