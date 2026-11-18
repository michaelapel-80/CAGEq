Instruments that look the same at any refresh rate, and an optional frame-rate limit.

- **New:** a Frame rate setting at the bottom of every instrument's tuning panel caps how often the
  instruments draw — to see them at another refresh rate, or to save some CPU/GPU.
- **Fixed:** the scopes and the meter were brighter on slower displays and dimmer on faster ones:
  a 60 Hz display showed the time scope's trace about 1.5x as bright as a 240 Hz one. They now give
  the same light at any refresh rate, which makes them noticeably dimmer than before on a 60 Hz
  display; raise Glow if you prefer the old look.
- **Fixed:** the time scope's trace could tear into disconnected pieces whenever the display drew
  slower than the audio arrived, and the vectorscope and meter could miss short events then.
- **Fixed:** with no audio engine running on the selected device, or before the first apply to it,
  the instruments still removed the correction from what they showed and read badly wrong.
