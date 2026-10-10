//! Spike: why did toggling a high-pass thump, and what would not? Offline, like `rampcompare`:
//! a correction (a +6 dB bass shelf) with a 20 Hz Butterworth high-pass toggled on or off, each
//! of its sections in its own slot, moved over a range of ramp lengths by:
//!
//! - `real`      — the engine itself (`Cascade::apply_coeffs`, its own ramp length);
//! - `df1 twin`  — what the engine does, at a fixed length: Direct Form I, an entering/leaving
//!                 section ramped to and from its neutral twin;
//! - `df2 twin`  — the same in Direct Form II, whose state is the input through the poles alone:
//!                 with the poles fixed, a numerator ramp is then exactly an output crossfade. A
//!                 section entering starts from zero state;
//! - `Fc sweep`  — ramp the high-pass's Fc up from 1 Hz (or down to it, then drop it), Direct
//!                 Form I, coefficients designed every sample;
//! - `cold`      — the dry signal blended into a high-pass switched on at the push;
//! - `ideal`     — the two steady states blended (for a toggle *on*, a high-pass that was already
//!                 running, which no causal engine has).
//!
//! Metric as in `rampcompare`: the tone notched out of the output leaves what the transition
//! added; reported as its peak in the 400 ms after the push (the 20 Hz poles ring for a while),
//! in dB re the tone.
//!
//! Finding (2026-10): the ramp's *length* is what matters — every ~3x longer buys 9-10 dB, for
//! every method and for the ideal alike, and at equal length the twin ramp sits within ~1 dB of
//! the ideal wherever it is audible (Direct Form II only gains at 1 kHz, already below -45 dB; the
//! Fc sweep is worse throughout). A high-pass barely changes the level above its corner but
//! rotates the phase there a long way, and the engine sized ramps by level alone, so a toggle
//! landed in the 8 ms floor: a 100 Hz tone's residual around 0 dB re the tone at 48 dB/oct, where
//! even the ideal 8 ms blend is -3 dB. Sizing by the complex change (`Cascade::ramp_distance_db`)
//! gives such a toggle 0.1-0.3 s and brings `real` to about -27 dB there.
//!
//! `cargo run --release -p cageq-apo --example hpcompare`

use cageq_apo::dsp::{coefficients, Band, Cascade, Coeffs, FilterKind};
use std::f64::consts::PI;

const FS: f64 = 48_000.0;
const HP_FC: f64 = 20.0;
const SWEEP_FLOOR: f64 = 1.0;

fn butterworth_qs(slope: u32) -> Vec<f64> {
    let n = slope / 6;
    (1..=n / 2).map(|k| 1.0 / (2.0 * ((2 * k - 1) as f64 * PI / (2 * n) as f64).cos())).collect()
}

fn hp(fc: f64, q: f64) -> Band {
    Band { kind: FilterKind::HighPass, freq_hz: fc, gain_db: 0.0, q }
}
fn shelf() -> Band {
    Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 6.0, q: 0.7 }
}

fn twin(c: &Coeffs) -> Coeffs {
    Coeffs { b0: 1.0, b1: c.a1, b2: c.a2, a1: c.a1, a2: c.a2 }
}
fn lerp(a: &Coeffs, b: &Coeffs, t: f64) -> Coeffs {
    Coeffs { b0: a.b0 + (b.b0 - a.b0) * t, b1: a.b1 + (b.b1 - a.b1) * t, b2: a.b2 + (b.b2 - a.b2) * t, a1: a.a1 + (b.a1 - a.a1) * t, a2: a.a2 + (b.a2 - a.a2) * t }
}

#[derive(Clone, Copy, Default)]
struct Df1 { x1: f64, x2: f64, y1: f64, y2: f64 }
impl Df1 {
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x; self.y2 = self.y1; self.y1 = y;
        y
    }
}
#[derive(Clone, Copy, Default)]
struct Df2 { w1: f64, w2: f64 }
impl Df2 {
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let w = x - c.a1 * self.w1 - c.a2 * self.w2;
        let y = c.b0 * w + c.b1 * self.w1 + c.b2 * self.w2;
        self.w2 = self.w1; self.w1 = w;
        y
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Method { Real, Df1(f64), Df2(f64), Sweep(f64), Cold(f64), Ideal(f64) }

/// Run `input` through the shelf plus a `slope` dB/oct high-pass that is present before the push
/// at `at` iff `on_before`, and after it iff not.
fn run(method: Method, input: &[f64], at: usize, slope: u32, on_before: bool) -> Vec<f64> {
    let qs = butterworth_qs(slope);
    let shelf_c = coefficients(&shelf(), FS);
    let hp_c: Vec<Coeffs> = qs.iter().map(|&q| coefficients(&hp(HP_FC, q), FS)).collect();
    match method {
        Method::Real => {
            let set = |on: bool| -> Vec<Coeffs> {
                let mut v = vec![shelf_c];
                if on { v.extend(&hp_c); }
                v
            };
            let mut c = Cascade::new(1, FS);
            c.apply_coeffs(&set(on_before), -6.0);
            let (mut i_buf, mut o_buf) = ([0.0f32; 1], [0.0f32; 1]);
            input.iter().enumerate().map(|(i, &x)| {
                if i == at { c.apply_coeffs(&set(!on_before), -6.0); }
                i_buf[0] = x as f32;
                c.process(&i_buf, &mut o_buf, 1);
                o_buf[0] as f64
            }).collect()
        }
        Method::Df1(ms) | Method::Df2(ms) => {
            // The shelf is untouched throughout; only the high-pass slots move.
            let n_ramp = (ms / 1000.0 * FS) as usize;
            let df2 = matches!(method, Method::Df2(_));
            let mut s0 = Df1::default();
            let mut st = vec![Df2::default(); qs.len()];
            let mut st1 = vec![Df1::default(); qs.len()];
            input.iter().enumerate().map(|(i, &x)| {
                let mut y = s0.step(&shelf_c, x * 0.5);
                let t = if i < at { 0.0 } else { ((i - at + 1) as f64 / n_ramp as f64).min(1.0) };
                for k in 0..qs.len() {
                    let (from, to) = if on_before { (hp_c[k], twin(&hp_c[k])) } else { (twin(&hp_c[k]), hp_c[k]) };
                    let c = lerp(&from, &to, t);
                    if !on_before && i < at { continue; } // not yet there; starts from zero state
                    y = if df2 { st[k].step(&c, y) } else { st1[k].step(&c, y) };
                }
                y
            }).collect()
        }
        Method::Sweep(ms) => {
            let len = (ms / 1000.0 * FS) as usize;
            let mut s0 = Df1::default();
            let mut st = vec![Df1::default(); qs.len()];
            input.iter().enumerate().map(|(i, &x)| {
                let mut y = s0.step(&shelf_c, x * 0.5);
                let t = if i < at { 0.0 } else { ((i - at + 1) as f64 / len as f64).min(1.0) };
                // Position along the sweep: 1 = at Fc, 0 = at the floor.
                let pos = if on_before { 1.0 - t } else { t };
                let present = if on_before { !(i >= at && t >= 1.0) } else { i >= at };
                if !present { return y; }
                let fc = SWEEP_FLOOR * (HP_FC / SWEEP_FLOOR).powf(pos);
                for (k, s) in st.iter_mut().enumerate() {
                    y = s.step(&coefficients(&hp(fc, qs[k]), FS), y);
                }
                y
            }).collect()
        }
        Method::Cold(ms) | Method::Ideal(ms) => {
            let n_ramp = (ms / 1000.0 * FS) as usize;
            let cold_m = matches!(method, Method::Cold(_));
            // Dry (shelf only) and wet (shelf + high-pass) as separate steady chains, blended over
            // 8 ms. `Ideal`: the wet chain has run all along. `Cold`: it starts at the push.
            let mut s_dry = Df1::default();
            let mut s_wet = Df1::default();
            let mut hp_st = vec![Df1::default(); qs.len()];
            input.iter().enumerate().map(|(i, &x)| {
                let dry = s_dry.step(&shelf_c, x * 0.5);
                let mut wet = s_wet.step(&shelf_c, x * 0.5);
                let cold = cold_m && !on_before;
                if cold && i < at {
                    // not running yet
                } else {
                    for (k, s) in hp_st.iter_mut().enumerate() { wet = s.step(&hp_c[k], wet); }
                }
                let t = if i < at { 0.0 } else { ((i - at + 1) as f64 / n_ramp as f64).min(1.0) };
                let w = if on_before { 1.0 - t } else { t };
                dry * (1.0 - w) + wet * w
            }).collect()
        }
    }
}

/// Notch the tone out (two cascaded RBJ notches, Q 10) and return the residual.
fn notch(x: &[f64], hz: f64) -> Vec<f64> {
    let w0 = 2.0 * PI * hz / FS;
    let al = w0.sin() / 20.0;
    let a0 = 1.0 + al;
    let c = Coeffs { b0: 1.0 / a0, b1: -2.0 * w0.cos() / a0, b2: 1.0 / a0, a1: -2.0 * w0.cos() / a0, a2: (1.0 - al) / a0 };
    let mut r = x.to_vec();
    for _ in 0..2 {
        let mut s = Df1::default();
        for v in r.iter_mut() { *v = s.step(&c, *v); }
    }
    r
}

fn main() {
    let n = (FS * 2.0) as usize;
    let at = (FS * 1.0) as usize; // late enough for every 20 Hz pole to have settled
    let durations = [8.0, 30.0, 100.0, 300.0];
    println!("residual peak in the 400 ms after the push, dB re tone (lower is better), by ramp length
");
    println!("(today's engine, its own ramp length, for reference)");
    for on_before in [false, true] {
        for slope in [24, 48] {
            for hz in [40.0, 100.0, 1000.0] {
                let input: Vec<f64> = (0..n).map(|i| 0.25 * (2.0 * PI * hz * i as f64 / FS).sin()).collect();
                let score = |m: Method| -> f64 {
                    let out = run(m, &input, at, slope, on_before);
                    let r = notch(&out, hz);
                    let tone = out[at - (FS * 0.2) as usize..at].iter().fold(0.0f64, |a, v| a.max(v.abs()));
                    let peak = r[at..at + (FS * 0.4) as usize].iter().fold(0.0f64, |a, v| a.max(v.abs()));
                    20.0 * (peak / tone).max(1e-12).log10()
                };
                println!("## {slope} dB/oct, toggled {}, {hz} Hz tone: real {:.1}", if on_before { "off" } else { "on" }, score(Method::Real));
                print!("{:>8}", "ms");
                for m in ["df1 twin", "df2 twin", "Fc sweep", "cold", "ideal"] { print!("{m:>10}"); }
                println!();
                for &ms in &durations {
                    print!("{ms:>8.0}");
                    for m in [Method::Df1(ms), Method::Df2(ms), Method::Sweep(ms), Method::Cold(ms), Method::Ideal(ms)] {
                        print!("{:>10.1}", score(m));
                    }
                    println!();
                }
            }
            println!();
        }
    }
}
