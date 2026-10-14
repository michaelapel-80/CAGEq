Impulse-response export, and keyboard fixes for number fields.

- **New:** Export can now save the correction as an impulse response (WAV, 32-bit float, mono)
  for a convolution engine such as Equalizer APO's `Convolution:`. It is exact, phase included,
  at a sample rate you choose from 44.1 to 192 kHz.
- **Fixed:** tabbing into a number field (Fc, Q, gain, …) didn't let you type a value; only a
  click did. Tab now selects the value ready to type over.
- **Changed:** a typed value now applies on Enter or when you leave the field, not with every
  keystroke, so the filter no longer jumps around while you type. Escape now restores the value
  from before the edit. Arrow keys and dragging still apply live.
- All changes are in the app itself; no engine update needed.
