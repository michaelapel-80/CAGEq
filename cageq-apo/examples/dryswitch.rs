//! Spike: is switching to Dry cleaner as a crossfade or as an ordinary ramp?
//!
//! Dry reaches the engine as a correction with no filters. It used to become a 10 ms
//! raised-cosine crossfade between the corrected and the untouched signal (`Cascade::set_dry`,
//! Equalizer APO's own transition); the alternative is what an A/B switch to a slot without
//! filters already did: every band ramps to its neutral twin, over a time sized by how far the
//! response moves (`Cascade::ramp_distance_db`, level and phase). The crossfade column drives
//! `set_dry` directly, so the comparison stays reproducible now that Dry is the ramp.
//!
//! Finding (2026-10): the ramp wins every case, by 4-25 dB — a typical fit's switch to Dry left a
//! 100 Hz tone's residual at -4.7 dB re the tone as a crossfade and -23.4 dB as a ramp: a fixed
//! 10 ms blend of two signals that differ in phase is itself the artefact (the same finding as
//! `hpcompare`). `apply_coeffs` now ramps to Dry; the crossfade only carries the fail-safe mute.
//!
//! Metric as in `rampcompare`/`hpcompare`: the tone notched out of the output leaves what the
//! transition added; peak in the 400 ms after each switch, dB re the tone. Both directions.
//!
//! `cargo run --release -p cageq-apo --example dryswitch`

use cageq_apo::dsp::{coefficients, Band, Cascade, Coeffs, FilterKind};
use std::f64::consts::PI;

const FS: f64 = 48_000.0;

fn pk(f: f64, g: f64, q: f64) -> Band {
    Band { kind: FilterKind::Peaking, freq_hz: f, gain_db: g, q }
}

#[derive(Clone, Copy, Default)]
struct St { x1: f64, x2: f64, y1: f64, y2: f64 }
impl St {
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x; self.y2 = self.y1; self.y1 = y;
        y
    }
}

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
    let hp = |q: f64| Band { kind: FilterKind::HighPass, freq_hz: 20.0, gain_db: 0.0, q };
    let cases: Vec<(&str, Vec<Band>, f64)> = vec![
        ("small: -1.7 dB bell 280 Hz", vec![pk(280.0, -1.7, 0.8)], -0.5),
        ("typical fit (shelf + bells)", vec![
            Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 5.5, q: 0.7 },
            pk(210.0, -2.2, 0.8), pk(1050.0, -2.0, 1.8), pk(1530.0, 3.5, 1.0), pk(3470.0, 1.6, 3.8), pk(5610.0, -4.0, 6.0),
            Band { kind: FilterKind::HighShelf, freq_hz: 10_000.0, gain_db: -4.0, q: 0.7 },
        ], -6.0),
        ("fit + 24 dB/oct high-pass", vec![
            Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 5.5, q: 0.7 },
            pk(1530.0, 3.5, 1.0), pk(5610.0, -4.0, 6.0), hp(0.5412), hp(1.3066),
        ], -5.0),
    ];
    let n = (FS * 2.6) as usize;
    let (to_dry, back) = ((FS * 1.0) as usize, (FS * 1.8) as usize);
    println!("residual peak in the 400 ms after the switch, dB re tone (lower is better)\n");
    println!("{:<30}{:>7}{:>14}{:>12}{:>14}{:>12}", "correction", "tone", "xfade to dry", "ramp to dry", "xfade back", "ramp back");
    for (name, bands, pre) in &cases {
        let wet: Vec<Coeffs> = bands.iter().map(|b| coefficients(b, FS)).collect();
        let identity = vec![Coeffs::PASSTHROUGH; wet.len()];
        for hz in [100.0, 1000.0] {
            let input: Vec<f32> = (0..n).map(|i| (0.25 * (2.0 * PI * hz * i as f64 / FS).sin()) as f32).collect();
            let run = |crossfade: bool| -> Vec<f64> {
                let mut c = Cascade::new(1, FS);
                c.apply_coeffs(&wet, *pre);
                let mut out = vec![0.0f32; n];
                for start in (0..n).step_by(480) {
                    if start == to_dry {
                        if crossfade { c.set_dry(true, 0.0); } else { c.apply_coeffs(&identity, 0.0); }
                    }
                    if start == back { c.apply_coeffs(&wet, *pre); }
                    let m = 480.min(n - start);
                    c.process(&input[start..start + m], &mut out[start..start + m], m);
                }
                out.iter().map(|&v| v as f64).collect()
            };
            let score = |out: &[f64], at: usize| -> f64 {
                let r = notch(out, hz);
                let tone = out[at - (FS * 0.25) as usize..at].iter().fold(0.0f64, |m, v| m.max(v.abs()));
                let pk = r[at..at + (FS * 0.4) as usize].iter().fold(0.0f64, |m, v| m.max(v.abs()));
                20.0 * (pk / tone).log10()
            };
            let (xf, rp) = (run(true), run(false));
            println!("{:<30}{:>7.0}{:>14.1}{:>12.1}{:>14.1}{:>12.1}", name, hz, score(&xf, to_dry), score(&rp, to_dry), score(&xf, back), score(&rp, back));
        }
    }
}
