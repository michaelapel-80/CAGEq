/** The spectrum analyzer's window duration in milliseconds, for the window-size slider's readout.
 *
 *  The slider value is in samples at the ≤48 kHz base rate, but the real window the backend uses is
 *  that many samples times a power-of-two multiple that grows with the mix rate (see cageq-monitor's
 *  `Spectrum::window_params`: `mult = next_pow2(round(rate / 48 kHz))`, analysis size = value ×
 *  mult), and `rate` is the endpoint's rate capped at `CAPTURE_RATE_CAP` (96 kHz). At 48/96 kHz the
 *  duration works out identical (171 ms at the shortest setting), but at 44.1 kHz the multiple is
 *  still 1, so the same 8192 samples last 186 ms — a readout that divided by a fixed 48 would be
 *  wrong there. This mirrors that arithmetic for *display only*; it never changes the window.
 *
 *  `deviceRate` is the endpoint's true (uncapped) rate as the meter reports it; unknown → assume 48
 *  kHz. Keep the constants in sync with the backend's by hand, same as the other mirrored ones. */
const BASE_RATE = 48_000;
const CAPTURE_RATE_CAP = 96_000;

export function fftWindowMs(size: number, deviceRate: number | null | undefined): number {
  const rate = Math.min(deviceRate && deviceRate > 0 ? deviceRate : BASE_RATE, CAPTURE_RATE_CAP);
  const mult = 2 ** Math.ceil(Math.log2(Math.max(1, Math.round(rate / BASE_RATE))));
  return (size * mult * 1000) / rate;
}
