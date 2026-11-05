//! True-peak detection for the level meter: an 8x polyphase interpolator with a sharp passband,
//! plus a parabolic fit around each oversampled local maximum.
//!
//! Replaces `ebur128`'s own `TRUE_PEAK` mode, which implements BS.1770 faithfully and so
//! inherits its short 48-tap filter. That filter droops near the top of the band, and on
//! content there it under-reads by far more than the standard's few tenths of a dB:
//! `signal::ISP16_PATTERN` at 0 dBTP read -1.78 dBTP at 48 kHz. An over that reads under 0 is a
//! missed over, which is the one thing this meter must not do. How far over it reads matters much
//! less.
//!
//! Measured against synthetic ground truth (periodic band-limited signals whose true peak comes
//! from a 256x FFT reconstruction: sines up to the passband edge at random phase, band-limited
//! noise, near-edge two-tones and the ISP patterns), this design under-reads by at most 0.004 dB
//! and over-reads by at most 0.017 dB. For comparison, 4x without the parabola under-reads by up
//! to 0.36 dB, and 4x with it by up to 0.03 dB. Content between the passband edge and Nyquist
//! (20 kHz–22.05 kHz at 44.1 kHz) is attenuated and can under-read. Nothing finite can avoid
//! that: the overshoot there depends on the DAC's own filter, and for an ideal one is unbounded.
//!
//! The filter is defined relative to the sample rate, so one coefficient table serves every rate.

/// Oversampling factor.
const FACTOR: usize = 8;
/// Taps per polyphase branch (`FACTOR * TAPS` = 312 in the prototype filter).
const TAPS: usize = 39;
/// Passband edge, as a fraction of the sample rate: 20 kHz at 44.1 kHz. The stopband edge mirrors
/// it about Nyquist, so an image of in-band content always lands in the stopband.
const PASSBAND_EDGE: f64 = 0.4535;
/// Kaiser β for 60 dB of stopband attenuation (`0.1102·(60 − 8.7)`). Images at -60 dB move a
/// reading by under 0.01 dB; more attenuation costs taps without changing the readings.
const KAISER_BETA: f64 = 5.653;

pub struct TruePeak {
    channels: usize,
    /// Per channel, `2 * TAPS` samples written twice (at `pos` and `pos + TAPS`) so the newest
    /// `TAPS` are always one contiguous slice, newest first.
    history: Vec<f32>,
    pos: usize,
    /// `coeffs[i][phase]` multiplies the sample `i` steps back. Tap-major, so each input sample
    /// updates all eight phases with one 8-wide multiply-add: that vectorizes, where eight
    /// separate dot products each end in a sequential reduction.
    coeffs: [[f32; FACTOR]; TAPS],
    /// Last two oversampled magnitudes per channel, oldest first, for the parabola.
    tail: Vec<[f32; 2]>,
    /// Input frames still to go before the filter's history is all real signal. Until then only
    /// raw sample peaks count: a history of zeros ahead of a loud stream is a step the stream
    /// never had, and the filter would ring on it.
    warmup: usize,
    block: f32,
    max: f32,
}

impl TruePeak {
    pub fn new(channels: usize) -> Self {
        TruePeak {
            channels,
            history: vec![0.0; channels * 2 * TAPS],
            pos: 0,
            coeffs: design(),
            tail: vec![[0.0; 2]; channels],
            warmup: TAPS,
            block: 0.0,
            max: 0.0,
        }
    }

    /// Feed interleaved frames (a trailing partial frame is ignored).
    pub fn push(&mut self, frames: &[f32]) {
        for frame in frames.chunks_exact(self.channels) {
            self.pos = if self.pos == 0 { TAPS - 1 } else { self.pos - 1 };
            for (c, &s) in frame.iter().enumerate() {
                self.block = self.block.max(s.abs());
                let h = &mut self.history[c * 2 * TAPS..(c + 1) * 2 * TAPS];
                h[self.pos] = s;
                h[self.pos + TAPS] = s;
                if self.warmup > 0 {
                    continue;
                }
                let recent: &[f32; TAPS] = h[self.pos..self.pos + TAPS].try_into().unwrap();
                let mut acc = [0.0f32; FACTOR];
                for (taps, &x) in self.coeffs.iter().zip(recent) {
                    for (a, &t) in acc.iter_mut().zip(taps) {
                        *a += t * x;
                    }
                }
                let [mut y0, mut y1] = self.tail[c];
                for &y in &acc {
                    let y2 = y.abs();
                    // The vertex only matters when it could raise the reading, and it can't sit
                    // far above its middle point, so most frames skip the fit altogether.
                    if y1 > self.block * PARABOLA_GATE {
                        self.block = self.block.max(parabola_peak(y0, y1, y2));
                    }
                    self.block = self.block.max(y2);
                    (y0, y1) = (y1, y2);
                }
                self.tail[c] = [y0, y1];
            }
            self.warmup = self.warmup.saturating_sub(1);
        }
        self.max = self.max.max(self.block);
    }

    /// Highest true peak (linear, can exceed 1.0) since the last call, across all channels.
    pub fn take_block(&mut self) -> f32 {
        std::mem::take(&mut self.block)
    }

    /// Highest true peak since construction or the last [`reset_max`](Self::reset_max).
    pub fn max(&self) -> f32 {
        self.max
    }

    /// Restart the high-water mark. The filter's history is deliberately kept: clearing it
    /// mid-stream would make the filter ring on a step the signal never had, a false over.
    pub fn reset_max(&mut self) {
        self.max = 0.0;
    }
}

/// A peak can fall at most half an oversampled step from the nearest point, so for in-band content
/// the vertex sits at most `1/cos(π·PASSBAND_EDGE/FACTOR)` ≈ 1.016× (0.14 dB) above its middle
/// point. A middle point below this fraction of the running peak can't produce a new one.
const PARABOLA_GATE: f32 = 0.98;

/// Vertex of the parabola through three equally spaced magnitudes, when the middle one is a local
/// maximum. Catches a peak landing between oversampled points; `0.0` otherwise.
fn parabola_peak(y0: f32, y1: f32, y2: f32) -> f32 {
    let curvature = y0 - 2.0 * y1 + y2;
    if y1 >= y0 && y1 >= y2 && curvature < 0.0 {
        y1 - (y0 - y2) * (y0 - y2) / (8.0 * curvature)
    } else {
        0.0
    }
}

/// Kaiser-windowed sinc lowpass at Nyquist, at the oversampled rate, split into polyphase
/// branches. Each branch is normalized to unity DC gain, so a constant input reads exactly itself.
fn design() -> [[f32; FACTOR]; TAPS] {
    let len = FACTOR * TAPS;
    let centre = (len - 1) as f64 / 2.0;
    let cutoff = (PASSBAND_EDGE + (1.0 - PASSBAND_EDGE)) / 2.0; // = 0.5: halfway through the transition
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut proto = vec![0.0f64; len];
    for (n, h) in proto.iter_mut().enumerate() {
        let m = n as f64 - centre;
        let x = 2.0 * cutoff * m / FACTOR as f64;
        let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
        let r = m / centre;
        *h = sinc * bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
    }
    let mut coeffs = [[0.0f32; FACTOR]; TAPS];
    for phase in 0..FACTOR {
        let sum: f64 = (0..TAPS).map(|i| proto[i * FACTOR + phase]).sum();
        for (i, taps) in coeffs.iter_mut().enumerate() {
            taps[phase] = (proto[i * FACTOR + phase] / sum) as f32;
        }
    }
    coeffs
}

/// Modified Bessel function of the first kind, order 0, by its power series (converges quickly
/// for the β range a Kaiser window uses).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0f64, 1.0f64, 1.0f64);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{build_wavetable, Signal, ISP16_MAX_OVER_DB};

    fn db(x: f32) -> f64 {
        20.0 * (x as f64).log10()
    }

    /// Steady-state reading of a periodic mono signal, skipping the first periods so the result
    /// is the meter's, not its start-up.
    fn read_periodic(period: &[f32], channels: usize) -> f32 {
        let mut tp = TruePeak::new(channels);
        let warm: Vec<f32> = period.iter().cycle().take(period.len() * (200 / period.len() + 1)).flat_map(|&s| std::iter::repeat_n(s, channels)).collect();
        tp.push(&warm);
        tp.take_block();
        let run: Vec<f32> = period.iter().flat_map(|&s| std::iter::repeat_n(s, channels)).collect();
        tp.push(&run);
        tp.take_block()
    }

    /// The case that motivated this module: the 16-sample ISP pattern at a 0 dBTP target must not
    /// read below 0 dBTP (`ebur128` read -1.78).
    #[test]
    fn isp16_at_zero_dbtp_reads_zero_dbtp() {
        let table = build_wavetable(Signal::Isp16 { db_over: 0.0 }, 48_000);
        let read = db(read_periodic(&table, 2));
        assert!((-0.01..=0.02).contains(&read), "read {read:.4} dBTP for a 0 dBTP signal");
        // And full scale reads the full overshoot.
        let full: Vec<f32> = table.iter().map(|&s| s * 10f64.powf(ISP16_MAX_OVER_DB / 20.0) as f32).collect();
        let read = db(read_periodic(&full, 1));
        assert!((read - ISP16_MAX_OVER_DB).abs() < 0.02, "read {read:.4}, expected {ISP16_MAX_OVER_DB}");
    }

    /// Unit sines from low frequencies up to the passband edge, each at several phases, must all
    /// read 0 dBTP to within a few thousandths of a dB. Frequencies are whole cycles per 4096
    /// samples so each is exactly periodic.
    #[test]
    fn in_band_sines_read_their_true_peak_at_any_phase() {
        let n = 4096usize;
        let mut worst = (0.0f64, 0.0f64);
        for k in (40..=(PASSBAND_EDGE * n as f64) as usize).step_by(23) {
            for p in 0..5 {
                let phase = p as f64 * 1.2345;
                let x: Vec<f32> = (0..n).map(|i| (std::f64::consts::TAU * (k * i) as f64 / n as f64 + phase).cos() as f32).collect();
                let read = db(read_periodic(&x, 1));
                worst = (worst.0.min(read), worst.1.max(read));
            }
        }
        assert!(worst.0 > -0.01, "worst under-read {:.4} dB", worst.0);
        assert!(worst.1 < 0.02, "worst over-read {:.4} dB", worst.1);
    }

    /// EBU Tech 3341 (2023) true-peak minimum-requirement signals 15–19: stereo sines with an
    /// expected reading and a +0.2/-0.4 dBTP tolerance. This meter has to land inside that window
    /// and, being closer to the exact true peak than the standard requires, within 0.02 dB of it.
    #[test]
    fn meets_the_tech_3341_true_peak_signals() {
        // (case, cycles per sample, phase in degrees, amplitude, expected dBTP)
        for (case, freq, phase_deg, amp, expected) in [
            (15, 1.0 / 4.0, 0.0f64, 0.5f64, -6.0f64),
            (16, 1.0 / 4.0, 45.0, 0.5, -6.0),
            (17, 1.0 / 6.0, 60.0, 0.5, -6.0),
            (18, 1.0 / 8.0, 67.5, 0.5, -6.0),
            (19, 1.0 / 4.0, 45.0, 1.41, 3.0),
        ] {
            let period = (1.0 / freq) as usize;
            let x: Vec<f32> = (0..period)
                .map(|i| (amp * (std::f64::consts::TAU * freq * i as f64 + phase_deg.to_radians()).sin()) as f32)
                .collect();
            let read = db(read_periodic(&x, 2));
            assert!((expected - 0.4..=expected + 0.2).contains(&read), "case {case}: read {read:.3}, expected {expected} +0.2/-0.4");
            let exact = 20.0 * amp.log10();
            assert!((read - exact).abs() < 0.02, "case {case}: read {read:.4}, exact {exact:.4}");
        }
    }

    /// A raw sample can never read higher than the meter: low-frequency content, where the
    /// reconstruction adds nothing, still reads its sample peak.
    #[test]
    fn never_reads_below_the_sample_peak() {
        let x: Vec<f32> = (0..4800).map(|i| (std::f64::consts::TAU * 50.0 * i as f64 / 48_000.0).sin() as f32 * 0.5).collect();
        let mut tp = TruePeak::new(1);
        tp.push(&x);
        assert!(tp.take_block() >= 0.5 - 1e-6);
    }

    /// Loud audio arriving into a fresh meter must not read a false over from the filter ringing
    /// on its zero-filled history.
    #[test]
    fn a_loud_start_reads_no_false_over() {
        let x: Vec<f32> = (0..4800).map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0 + 1.0).sin() as f32 * 0.99).collect();
        let mut tp = TruePeak::new(1);
        tp.push(&x);
        assert!(tp.max() < 1.0, "read {:.4} dBTP for a -0.09 dBFS sine", db(tp.max()));
    }

    /// Channels are measured independently and the loudest one wins.
    #[test]
    fn the_loudest_channel_sets_the_reading() {
        let x: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32;
                [s * 0.1, s * 0.8]
            })
            .collect();
        let mut tp = TruePeak::new(2);
        tp.push(&x);
        assert!((db(tp.take_block()) - db(0.8)).abs() < 0.02);
    }

    #[test]
    fn take_block_clears_the_block_and_reset_clears_the_max() {
        let x: Vec<f32> = (0..4800).map(|i| (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5).collect();
        let mut tp = TruePeak::new(1);
        tp.push(&x);
        assert!(tp.take_block() > 0.49);
        assert_eq!(tp.take_block(), 0.0);
        assert!(tp.max() > 0.49);
        tp.reset_max();
        assert_eq!(tp.max(), 0.0);
    }
}
