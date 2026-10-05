Click-free band toggles.

- **Fixed:** switching a band on or off (in a stage, or by A/B switching between slots that
  differ by a band) could click, most audibly on a steady tone. CAGEq's own audio engine now
  fades a band in and out without the brief ringing that caused it. This also removes a larger
  spike that could appear when many bands faded out at once.
- **Fixed:** sweeping the frequency finder (middle mouse button) back and forth hard enough
  could end in "the correction ... was refused by CAGEq's own engine".
- **Engine update:** both fixes are in CAGEq's own audio engine. The app flags the update after
  updating: Continue refreshes it, with a brief machine-wide audio interruption.
- Small wording fixes in the export dialog's tooltips.
