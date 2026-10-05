//! Spike: how should the engine move between two corrections? Compares, offline:
//!
//! - `real`      — today's engine (`Cascade::apply_coeffs`: linear coefficient ramp, distance-
//!                 scaled duration), as the reference for what ships;
//! - `coeff`     — the same linear coefficient ramp, fixed duration (sanity check vs `real`);
//! - `twin`      — coefficient ramp, but a slot that is empty on one side uses the other side's
//!                 *neutral twin* (same poles, numerator = denominator: an identity filter);
//! - `param/N`   — ramp the band *parameters* (Fc and Q in log, gain in dB; an empty side is the
//!                 same band at 0 dB; a type change fades to 0 dB, switches, fades back), with
//!                 coefficients recomputed every N samples and linearly interpolated in between.
//!
//! Metric: a linear filter maps a sine to a sine, so notching the test tone out of the output
//! leaves only what the transition added. Reported as the residual's peak around the change,
//! in dB relative to the tone, next to an *ideal* reference: the two steady-state outputs blended
//! over the same time, i.e. the intended level/phase change done as cleanly as possible. Lower is
//! better; at or below `ideal` means the transition adds nothing audible of its own.
//!
//! `cargo run --release -p cageq-apo --example rampcompare`

use cageq_apo::dsp::{coefficients, Band, Cascade, Coeffs, FilterKind};
use std::f64::consts::PI;

const FS: f64 = 48_000.0;
const T_MS: f64 = 8.0; // fixed ramp for the simulated engines (the real engine's floor)

type Slot = Option<Band>;

#[derive(Clone, Copy, Default)]
struct St { x1: f64, x2: f64, y1: f64, y2: f64 }
impl St {
    #[inline]
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x; self.y2 = self.y1; self.y1 = y;
        y
    }
}

fn coeffs_of(s: &Slot) -> Coeffs {
    s.map(|b| coefficients(&b, FS)).unwrap_or(Coeffs::PASSTHROUGH)
}
fn twin(c: &Coeffs) -> Coeffs {
    Coeffs { b0: 1.0, b1: c.a1, b2: c.a2, a1: c.a1, a2: c.a2 }
}
fn lerp(a: &Coeffs, b: &Coeffs, t: f64) -> Coeffs {
    Coeffs { b0: a.b0 + (b.b0 - a.b0) * t, b1: a.b1 + (b.b1 - a.b1) * t, b2: a.b2 + (b.b2 - a.b2) * t, a1: a.a1 + (b.a1 - a.a1) * t, a2: a.a2 + (b.a2 - a.a2) * t }
}

/// The band a slot is at, `t` of the way from `a` to `b`, in parameter space.
fn param_at(a: &Slot, b: &Slot, t: f64) -> Slot {
    let lg = |x: f64, y: f64| (x.ln() + (y.ln() - x.ln()) * t).exp();
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(Band { gain_db: a.gain_db * (1.0 - t), ..*a }),
        (None, Some(b)) => Some(Band { gain_db: b.gain_db * t, ..*b }),
        (Some(a), Some(b)) if a.kind == b.kind => {
            Some(Band { kind: a.kind, freq_hz: lg(a.freq_hz, b.freq_hz), q: lg(a.q, b.q), gain_db: a.gain_db + (b.gain_db - a.gain_db) * t })
        }
        // Different filter types: fade `a` to 0 dB, switch at 0 dB (identity either way), fade `b` in.
        (Some(a), Some(b)) => {
            if t < 0.5 { Some(Band { gain_db: a.gain_db * (1.0 - 2.0 * t), ..*a }) } else { Some(Band { gain_db: b.gain_db * (2.0 * t - 1.0), ..*b }) }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Method { Coeff, Twin, Param(usize) }

/// A simulated engine: per-slot DF1 state, per-slot current/target, one shared ramp clock.
struct Sim {
    method: Method,
    st: Vec<St>,
    from: Vec<Slot>,
    to: Vec<Slot>,
    c_from: Vec<Coeffs>,
    c_to: Vec<Coeffs>,
    cur: Vec<Coeffs>,
    pre_from: f64,
    pre_to: f64,
    left: usize,
    len: usize,
    // param engine: coefficients at the last and next block boundary
    blk_a: Vec<Coeffs>,
    blk_b: Vec<Coeffs>,
}

impl Sim {
    fn new(method: Method, slots: &[Slot], preamp_db: f64) -> Sim {
        let n = slots.len();
        let c: Vec<Coeffs> = slots.iter().map(coeffs_of).collect();
        Sim { method, st: vec![St::default(); n], from: slots.to_vec(), to: slots.to_vec(), c_from: c.clone(), c_to: c.clone(), cur: c.clone(),
              pre_from: preamp_db, pre_to: preamp_db, left: 0, len: 1, blk_a: c.clone(), blk_b: c }
    }
    /// The parameter state right now (mid-ramp included), for a retarget that interrupts a ramp.
    fn current_params(&self) -> Vec<Slot> {
        let t = 1.0 - self.left as f64 / self.len as f64;
        (0..self.from.len()).map(|i| if self.left == 0 { self.to[i] } else { param_at(&self.from[i], &self.to[i], t) }).collect()
    }
    fn retarget(&mut self, slots: &[Slot], preamp_db: f64, ms: f64) {
        let t = if self.left == 0 { 1.0 } else { 1.0 - self.left as f64 / self.len as f64 };
        self.pre_from = self.pre_from + (self.pre_to - self.pre_from) * t;
        self.pre_to = preamp_db;
        self.from = self.current_params();
        self.to = slots.to_vec();
        for i in 0..slots.len() {
            self.c_from[i] = self.cur[i];
            self.c_to[i] = coeffs_of(&slots[i]);
            if self.method == Method::Twin {
                let is_identity = |c: &Coeffs| c.b0 == 1.0 && c.b1 == c.a1 && c.b2 == c.a2;
                if slots[i].is_none() && !is_identity(&self.c_from[i]) {
                    self.c_to[i] = twin(&self.c_from[i]);
                } else if is_identity(&self.c_from[i]) && slots[i].is_some() {
                    // Seamless instant switch: both identity, and the slot's state already has y == x.
                    self.c_from[i] = twin(&self.c_to[i]);
                    self.cur[i] = self.c_from[i];
                }
            }
        }
        self.len = ((ms / 1000.0) * FS).round().max(1.0) as usize;
        self.left = self.len;
        if let Method::Param(_) = self.method {
            self.blk_a = self.cur.clone();
            self.blk_b = self.cur.clone();
        }
    }
    fn process(&mut self, x: f64) -> f64 {
        let mut pre_db = self.pre_to;
        if self.left > 0 {
            let done = self.len - self.left; // samples into the ramp
            let t = (done + 1) as f64 / self.len as f64;
            pre_db = self.pre_from + (self.pre_to - self.pre_from) * t;
            match self.method {
                Method::Coeff | Method::Twin => {
                    for i in 0..self.cur.len() { self.cur[i] = lerp(&self.c_from[i], &self.c_to[i], t); }
                }
                Method::Param(nb) => {
                    if done % nb == 0 {
                        // New block: design the band at the block's end, interpolate towards it.
                        let t_end = ((done + nb).min(self.len)) as f64 / self.len as f64;
                        self.blk_a = self.cur.clone();
                        self.blk_b = (0..self.cur.len()).map(|i| coeffs_of(&param_at(&self.from[i], &self.to[i], t_end))).collect();
                    }
                    let start = done - done % nb;
                    let span = (start + nb).min(self.len) - start;
                    let u = (done - start + 1) as f64 / span as f64;
                    for i in 0..self.cur.len() { self.cur[i] = lerp(&self.blk_a[i], &self.blk_b[i], u); }
                }
            }
            self.left -= 1;
            if self.left == 0 {
                // Land exactly: the real target (an identity twin stays — it is the same response).
                for i in 0..self.cur.len() {
                    if self.method != Method::Twin || self.to[i].is_some() { self.cur[i] = coeffs_of(&self.to[i]); }
                }
            }
        }
        let mut y = x * 10f64.powf(pre_db / 20.0);
        for i in 0..self.cur.len() { y = self.st[i].step(&self.cur[i], y); }
        y
    }
}

/// A scenario: a starting correction and a list of (time, correction, preamp) pushes. All
/// corrections are slot-aligned (index i is the same slot throughout), as `SlotAssignment` ensures.
struct Case { name: &'static str, start: Vec<Slot>, pre0: f64, pushes: Vec<(f64, Vec<Slot>, f64)> }

fn pk(f: f64, g: f64, q: f64) -> Slot { Some(Band { kind: FilterKind::Peaking, freq_hz: f, gain_db: g, q }) }
fn hs(f: f64, g: f64, q: f64) -> Slot { Some(Band { kind: FilterKind::HighShelf, freq_hz: f, gain_db: g, q }) }
fn ls(f: f64, g: f64, q: f64) -> Slot { Some(Band { kind: FilterKind::LowShelf, freq_hz: f, gain_db: g, q }) }

fn cases() -> Vec<Case> {
    let base = vec![ls(105.0, 2.0, 0.7), pk(280.0, -1.7, 0.8), hs(10_000.0, -1.5, 0.7)];
    let mut off = base.clone();
    off[1] = None;
    // Drag: the 280 Hz band pulled up to 560 Hz in 20 pushes ~17 ms apart.
    let drag: Vec<(f64, Vec<Slot>, f64)> = (1..=20).map(|k| {
        let mut s = base.clone();
        s[1] = pk(280.0 * 2f64.powf(k as f64 / 20.0), -1.7, 0.8);
        (0.4 + k as f64 * 0.017, s, -6.0)
    }).collect();
    let mut reuse = base.clone();
    reuse[1] = pk(450.0, -4.0, 2.4); // a different band landing in the same slot (within reuse ratios)
    let mut gain = base.clone();
    gain[1] = pk(280.0, -4.0, 0.8);
    let a = vec![pk(3000.0, 3.0, 1.0)];
    let b = vec![hs(3000.0, 3.0, 0.7)];
    // Two genuinely different 8-band corrections, slot-aligned by index.
    let ab_a: Vec<Slot> = (0..8).map(|i| pk(60.0 * 2.2f64.powi(i), if i % 2 == 0 { 3.0 } else { -4.0 }, 1.0 + 0.3 * i as f64)).collect();
    let ab_b: Vec<Slot> = (0..8).map(|i| pk(80.0 * 2.0f64.powi(i), if i % 2 == 0 { -2.0 } else { 4.5 }, 2.0 - 0.15 * i as f64)).collect();
    vec![
        Case { name: "toggle off (band -> empty)", start: base.clone(), pre0: -6.0, pushes: vec![(0.5, off.clone(), -6.4)] },
        Case { name: "toggle on (empty -> band)", start: off.clone(), pre0: -6.4, pushes: vec![(0.5, base.clone(), -6.0)] },
        Case { name: "gain edit (-1.7 -> -4 dB)", start: base.clone(), pre0: -6.0, pushes: vec![(0.5, gain, -5.5)] },
        Case { name: "slot reuse (280 Hz/Q0.8 -> 450 Hz/Q2.4)", start: base.clone(), pre0: -6.0, pushes: vec![(0.5, reuse, -6.0)] },
        Case { name: "type change (3k bell -> 3k shelf)", start: a.clone(), pre0: -6.0, pushes: vec![(0.5, b, -6.0)] },
        Case { name: "A/B (two 8-band corrections)", start: ab_a, pre0: -7.0, pushes: vec![(0.5, ab_b, -6.5)] },
        Case { name: "drag 280 -> 560 Hz (20 pushes)", start: base.clone(), pre0: -6.0, pushes: drag },
    ]
}

/// Notch the tone out (two cascaded RBJ notches, Q 10) and return the residual.
fn notch(x: &[f64], hz: f64) -> Vec<f64> {
    let w0 = 2.0 * PI * hz / FS;
    let al = w0.sin() / 20.0;
    let a0 = 1.0 + al;
    let c = Coeffs { b0: 1.0 / a0, b1: -2.0 * w0.cos() / a0, b2: 1.0 / a0, a1: -2.0 * w0.cos() / a0, a2: (1.0 - al) / a0 };
    let mut r = x.to_vec();
    for _ in 0..2 {
        let mut s = St::default();
        for v in r.iter_mut() { *v = s.step(&c, *v); }
    }
    r
}

fn main() {
    let tones = [100.0, 1000.0, 5000.0];
    let methods: [(&str, Option<Method>); 6] = [
        ("real", None), ("coeff", Some(Method::Coeff)), ("twin", Some(Method::Twin)),
        ("param/1", Some(Method::Param(1))), ("param/16", Some(Method::Param(16))), ("param/32", Some(Method::Param(32))),
    ];
    let n = (FS * 1.4) as usize;
    println!("residual peak in the 60 ms after each change, dB re tone (lower is better; `ideal` = steady states blended over 8 ms)\n");
    for case in cases() {
        println!("## {}", case.name);
        print!("{:>7}", "tone");
        for (m, _) in &methods { print!("{m:>10}"); }
        println!("{:>10}", "ideal");
        for &hz in &tones {
            let input: Vec<f64> = (0..n).map(|i| 0.25 * (2.0 * PI * hz * i as f64 / FS).sin()).collect();
            let push_at: Vec<usize> = case.pushes.iter().map(|(t, _, _)| (t * FS) as usize).collect();
            let first = push_at[0];
            let last = *push_at.last().unwrap();
            let win = (first, last + (FS * 0.06) as usize);
            let score = |out: &[f64]| -> f64 {
                let r = notch(out, hz);
                let tone = out[(FS * 0.25) as usize..(FS * 0.4) as usize].iter().fold(0.0f64, |m, v| m.max(v.abs()));
                20.0 * (r[win.0..win.1].iter().fold(0.0f64, |m, v| m.max(v.abs())) / tone).max(1e-12).log10()
            };
            print!("{hz:>7.0}");
            for (_, m) in &methods {
                let mut out = vec![0.0f64; n];
                match m {
                    None => {
                        let coeffs = |s: &[Slot]| -> Vec<Coeffs> { s.iter().map(coeffs_of).collect() };
                        let mut c = Cascade::new(1, FS);
                        c.apply_coeffs(&coeffs(&case.start), case.pre0);
                        let mut buf_in = [0.0f32; 1];
                        let mut buf_out = [0.0f32; 1];
                        for i in 0..n {
                            if let Some(k) = push_at.iter().position(|&p| p == i) {
                                let (_, s, pre) = &case.pushes[k];
                                c.apply_coeffs(&coeffs(s), *pre);
                            }
                            buf_in[0] = input[i] as f32;
                            c.process(&buf_in, &mut buf_out, 1);
                            out[i] = buf_out[0] as f64;
                        }
                    }
                    Some(m) => {
                        let mut s = Sim::new(*m, &case.start, case.pre0);
                        for i in 0..n {
                            if let Some(k) = push_at.iter().position(|&p| p == i) {
                                let (_, sl, pre) = &case.pushes[k];
                                s.retarget(sl, *pre, T_MS);
                            }
                            out[i] = s.process(input[i]);
                        }
                    }
                }
                print!("{:>10.1}", score(&out));
            }
            // Ideal: steady states of the start and final corrections, blended over 8 ms at the first
            // push (for the drag: start -> end over its whole length).
            let steady = |slots: &[Slot], pre: f64| -> Vec<f64> {
                let mut s = Sim::new(Method::Coeff, slots, pre);
                input.iter().map(|&x| s.process(x)).collect()
            };
            let (y0, y1) = (steady(&case.start, case.pre0), { let (_, s, p) = case.pushes.last().unwrap(); steady(s, *p) });
            let blend_len = if case.pushes.len() > 1 { last - first + (T_MS / 1000.0 * FS) as usize } else { (T_MS / 1000.0 * FS) as usize };
            let ideal: Vec<f64> = (0..n).map(|i| { let w = ((i as f64 - first as f64) / blend_len as f64).clamp(0.0, 1.0); y0[i] * (1.0 - w) + y1[i] * w }).collect();
            println!("{:>10.1}", score(&ideal));
        }
        println!();
    }

    // Cost of designing coefficients, for the parameter engine's per-block recompute.
    let b = Band { kind: FilterKind::HighShelf, freq_hz: 8000.0, gain_db: 4.0, q: 0.9 };
    for (name, model) in [("RBJ", cageq_apo::dsp::ResponseModel::Rbj), ("warping-corrected", cageq_apo::dsp::ResponseModel::AnalogMatched)] {
        let t = std::time::Instant::now();
        let mut acc = 0.0;
        for k in 0..200_000 {
            let c = cageq_apo::dsp::coefficients_in(&Band { gain_db: 4.0 + (k % 7) as f64 * 0.01, ..b }, FS, model);
            acc += c.b0;
        }
        let ns = t.elapsed().as_nanos() as f64 / 200_000.0;
        println!("design cost, {name} shelf: {ns:.0} ns per band  (checksum {acc:.1})");
    }
}
