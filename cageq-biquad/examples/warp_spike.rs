//! Stage 0 spike: how far is each design from its analog prototype, across CAGEq's actual
//! parameter ranges, and is it safe to run (stable, minimum phase, smooth in its parameters)?
//!
//! `cargo run --release -p cageq-biquad --example warp_spike [-- --csv <dir>]`
//!
//! Prints one table per sample rate: worst |error| in dB over 20 Hz–20 kHz against the analog
//! prototype, bucketed by corner frequency (warping is a function of fc/fs, so one global max
//! would just report the top bucket). With `--csv`, also writes a few named curves for
//! plotting against the thesis figures.

use std::f64::consts::PI;
use std::fmt::Write as _;

use cageq_biquad::matched::{self, Con};
use cageq_biquad::{analog, rbj, Band, Coeffs, Kind};

const RATES: [f64; 3] = [44_100.0, 48_000.0, 96_000.0];
const BUCKETS: [(f64, f64); 5] = [(20.0, 1_000.0), (1_000.0, 5_000.0), (5_000.0, 10_000.0), (10_000.0, 15_000.0), (15_000.0, 20_001.0)];

#[derive(Clone, Copy)]
enum Method {
    Rbj,
    Prescribed,
    /// Named constraint set for [`matched::constrained`], built per band.
    Cons(&'static str, fn(&Band) -> [Con; 3]),
    Ivantsov(f64),
    VicanekShelf,
}

impl Method {
    fn name(self) -> String {
        match self {
            Method::Rbj => "rbj".into(),
            Method::Prescribed => "prescribed".into(),
            Method::Cons(name, _) => name.into(),
            Method::Ivantsov(sigma) => format!("ivantsov{sigma:.2}"),
            Method::VicanekShelf => "vicanek-gen".into(),
        }
    }

    fn design(self, band: &Band, fs: f64) -> Result<Coeffs, matched::Failure> {
        match self {
            Method::Rbj => Ok(rbj::coefficients(band, fs)),
            Method::Prescribed => matched::prescribed(band, fs),
            Method::Cons(_, cons) => matched::constrained(band, fs, cons(band)),
            Method::Ivantsov(sigma) => matched::ivantsov(band, fs, sigma),
            Method::VicanekShelf => matched::vicanek_shelf(band, fs),
        }
    }
}

fn log_space(n: usize, lo: f64, hi: f64) -> Vec<f64> {
    (0..n).map(|i| lo * (hi / lo).powf(i as f64 / (n - 1) as f64)).collect()
}

fn param_grid(kind: Kind) -> Vec<(f64, f64, f64)> {
    let fcs = log_space(40, 20.0, 20_000.0);
    let gains: &[f64] = &[-20.0, -12.0, -6.0, -3.0, -1.0, -0.1, 0.1, 1.0, 3.0, 6.0, 12.0, 20.0];
    let qs: &[f64] = match kind {
        Kind::Peaking => &[0.1, 0.18, 0.3, 0.5, 0.7, 1.0, 1.41, 2.0, 3.0, 4.0, 6.0, 10.0, 20.0],
        Kind::LowShelf | Kind::HighShelf => &[0.1, 0.3, 0.4, 0.5, 0.7, 1.0, 1.41, 2.0, 4.0, 10.0, 20.0],
        Kind::Bandpass => &[0.5, 0.7, 1.0, 2.0, 4.0, 8.0],
    };
    let gains: &[f64] = if kind == Kind::Bandpass { &[0.0] } else { gains };
    let mut out = Vec::new();
    for &fc in &fcs {
        for &q in qs {
            for &g in gains {
                out.push((fc, g, q));
            }
        }
    }
    out
}

#[derive(Default, Clone)]
struct Cell {
    max_err: f64,
    worst: Option<(f64, f64, f64, f64)>, // fc, gain, q, at Hz
}

#[derive(Default)]
struct Row {
    cells: [Cell; 5],
    designs: usize,
    failed: usize,
    unstable: usize,
    non_min_phase: usize,
}

fn bucket_of(fc: f64) -> usize {
    BUCKETS.iter().position(|&(lo, hi)| fc >= lo && fc < hi).unwrap()
}

fn evaluate(kind: Kind, method: Method, fs: f64, freqs: &[f64]) -> Row {
    let mut row = Row::default();
    for (fc, gain, q) in param_grid(kind) {
        let band = Band { kind, freq_hz: fc, gain_db: gain, q };
        row.designs += 1;
        let c = match method.design(&band, fs) {
            Ok(c) => c,
            Err(matched::Failure::Unsupported) => return Row::default(),
            Err(_) => {
                row.failed += 1;
                continue;
            }
        };
        if c.pole_radius() >= 1.0 {
            row.unstable += 1;
            continue;
        }
        // A band-pass has an inherent zero at DC (radius exactly 1) — minimum phase is only
        // meaningful for the shapes the undistort views invert.
        if kind != Kind::Bandpass && c.zero_radius() >= 1.0 - 1e-12 {
            row.non_min_phase += 1;
        }
        let cell = &mut row.cells[bucket_of(fc)];
        for &f in freqs {
            let w = 2.0 * PI * f / fs;
            let err = c.db(w) - analog::db(&band, fs, w);
            if err.abs() > cell.max_err {
                cell.max_err = err.abs();
                cell.worst = Some((fc, gain, q, f));
            }
        }
    }
    row
}

fn methods_for(kind: Kind) -> Vec<Method> {
    use Con::{Slope as S, Value as V};
    let mut m = vec![Method::Rbj];
    match kind {
        Kind::Peaking | Kind::Bandpass => m.push(Method::Prescribed),
        Kind::LowShelf | Kind::HighShelf => {
            m.push(Method::Cons("v1s1/v.5", |_| [V(1.0), S(1.0), V(0.5)]));
            m.push(Method::VicanekShelf);
        }
    }
    m.push(Method::Ivantsov(2.0));
    m.push(Method::Ivantsov(PI * (2.0f64 / 3.0).sqrt()));
    m
}

fn report(out: &mut String) {
    let freqs = log_space(2_000, 20.0, 20_000.0);
    for fs in RATES {
        writeln!(out, "\n=== fs = {fs} Hz — worst |error| vs analog over 20 Hz–20 kHz, dB, by corner frequency ===").unwrap();
        writeln!(out, "{:<4} {:<14} {:>8} {:>8} {:>8} {:>8} {:>8}   fail unst nminph", "", "method", "<1k", "1-5k", "5-10k", "10-15k", "15-20k").unwrap();
        for kind in Kind::ALL {
            for method in methods_for(kind) {
                let row = evaluate(kind, method, fs, &freqs);
                write!(out, "{:<4} {:<14}", kind.token(), method.name()).unwrap();
                for c in &row.cells {
                    write!(out, " {:>8.3}", c.max_err).unwrap();
                }
                writeln!(out, "   {:>4} {:>4} {:>6}", row.failed, row.unstable, row.non_min_phase).unwrap();
            }
        }
        writeln!(out, "\n  worst case per bucket (fc, gain, Q -> at Hz):").unwrap();
        for kind in Kind::ALL {
            for method in methods_for(kind) {
                let row = evaluate(kind, method, fs, &freqs);
                for b in 0..5 {
                    if let Some((fc, g, q, at)) = row.cells[b].worst {
                        writeln!(out, "  {:<4} {:<14} {:>6.0} Hz {:+5.1} dB Q{:<5} -> {:>6.3} dB at {:>6.0} Hz", kind.token(), method.name(), fc, g, q, row.cells[b].max_err, at).unwrap();
                    }
                }
            }
        }
    }
}

/// Is the design smooth in gain around 0 dB, at the optimizer's own finite-difference scale?
/// SLSQP's gradient is a central difference (`GRAD_EPS = 1e-6`), and the fit spends much of
/// its time on bands passing near 0 dB, where the prescribed peaking formula is 0/0. Compare
/// the derivative d(response dB)/d(gain dB) at a coarse and at the optimizer's step size: a
/// smooth design gives the same number.
fn smoothness(out: &mut String) {
    writeln!(out, "\n=== smoothness near 0 dB: |d(resp)/d(gain) at h=1e-6 − at h=1e-3|, worst over gain∈{{±1e-7..±0.1}} and 8 probe freqs ===").unwrap();
    let fs = 48_000.0;
    let probes = log_space(8, 50.0, 20_000.0);
    for kind in [Kind::Peaking, Kind::LowShelf, Kind::HighShelf] {
        for method in methods_for(kind) {
            let mut worst: f64 = 0.0;
            let mut fails = 0;
            for &(fc, q) in &[(12_000.0, 1.0), (10_000.0, 0.7), (3_000.0, 2.0), (200.0, 0.5), (16_000.0, 6.0)] {
                for &g in &[1e-7, 1e-5, 1e-3, 0.1, -1e-7, -1e-5, -1e-3, -0.1] {
                    let resp = |gain: f64, f: f64| -> Option<f64> {
                        let b = Band { kind, freq_hz: fc, gain_db: gain, q };
                        method.design(&b, fs).ok().map(|c| c.db(2.0 * PI * f / fs))
                    };
                    for &f in &probes {
                        let d = |h: f64| Some((resp(g + h, f)? - resp(g - h, f)?) / (2.0 * h));
                        match (d(1e-6), d(1e-3)) {
                            (Some(fine), Some(coarse)) => worst = worst.max((fine - coarse).abs()),
                            _ => fails += 1,
                        }
                    }
                }
            }
            writeln!(out, "{:<4} {:<14} worst derivative mismatch {:.2e}  (design failures: {fails})", kind.token(), method.name(), worst).unwrap();
        }
    }
}

/// Named curves: the thesis's own figure parameters (its Q is `Q_t = Q_rbj·√g`, so these are
/// converted), plus the CAGEq bands that motivated the spike.
fn named_cases() -> Vec<(&'static str, Band, f64)> {
    let a = |db: f64| 10f64.powf(db / 40.0);
    vec![
        ("thesis_fig3_1_pk_15k", Band { kind: Kind::Peaking, freq_hz: 15_000.0, gain_db: 10.0, q: 1.0 / a(10.0) }, 40_000.0),
        ("thesis_fig3_3_pk_5k", Band { kind: Kind::Peaking, freq_hz: 5_000.0, gain_db: -10.0, q: 2.0 / a(-10.0) }, 40_000.0),
        ("thesis_fig3_5_pk_18k", Band { kind: Kind::Peaking, freq_hz: 18_000.0, gain_db: -10.0, q: 0.5 / a(-10.0) }, 40_000.0),
        ("thesis_fig3_6_bp_15k", Band { kind: Kind::Bandpass, freq_hz: 15_000.0, gain_db: 0.0, q: 1.0 }, 40_000.0),
        ("cageq_hs_10k_48k", Band { kind: Kind::HighShelf, freq_hz: 10_000.0, gain_db: 6.0, q: 0.7 }, 48_000.0),
        ("cageq_hs_10k_44k", Band { kind: Kind::HighShelf, freq_hz: 10_000.0, gain_db: -6.0, q: 0.7 }, 44_100.0),
        ("cageq_pk_12k_48k", Band { kind: Kind::Peaking, freq_hz: 12_000.0, gain_db: 6.0, q: 1.0 }, 48_000.0),
        ("cageq_hs_4k_q2_48k", Band { kind: Kind::HighShelf, freq_hz: 4_000.0, gain_db: 12.0, q: 2.0 }, 48_000.0),
    ]
}

fn write_csv(dir: &str) {
    std::fs::create_dir_all(dir).unwrap();
    for (name, band, fs) in named_cases() {
        let methods = methods_for(band.kind);
        let mut s = String::from("f,analog");
        for m in &methods {
            write!(s, ",{}", m.name()).unwrap();
        }
        s.push('\n');
        let designs: Vec<_> = methods.iter().map(|m| m.design(&band, fs).ok()).collect();
        for f in log_space(1_000, 20.0, fs / 2.0 * 0.999) {
            let w = 2.0 * PI * f / fs;
            write!(s, "{f},{}", analog::db(&band, fs, w)).unwrap();
            for d in &designs {
                write!(s, ",{}", d.map(|c| c.db(w)).unwrap_or(f64::NAN)).unwrap();
            }
            s.push('\n');
        }
        std::fs::write(format!("{dir}/{name}.csv"), s).unwrap();
    }
}

/// Shelves broken down by Q instead of fc: resonant shelves (Q ≳ 1) are a different problem
/// from the Q 0.4–0.7 ones CAGEq's fit and macro bands actually use, and one max over both
/// hides which regime a method fails in.
fn shelf_by_q(out: &mut String) {
    let freqs = log_space(1_000, 20.0, 20_000.0);
    for fs in [44_100.0, 48_000.0] {
        writeln!(out, "
=== high shelf at {fs} Hz, by Q: worst |error| dB (fc 20 Hz–20 kHz, gain ±20) [failures/{}] ===", 40 * 12).unwrap();
        let qs = [0.1, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 1.0, 1.41, 2.0, 4.0, 10.0, 20.0];
        write!(out, "{:<14}", "method").unwrap();
        for q in qs {
            write!(out, " {:>9}", format!("Q{q}")).unwrap();
        }
        writeln!(out).unwrap();
        for method in methods_for(Kind::HighShelf) {
            write!(out, "{:<14}", method.name()).unwrap();
            for q in qs {
                let (mut worst, mut fails) = (0.0f64, 0);
                for fc in log_space(40, 20.0, 20_000.0) {
                    for g in [-20.0, -12.0, -6.0, -3.0, -1.0, -0.1, 0.1, 1.0, 3.0, 6.0, 12.0, 20.0] {
                        let b = Band { kind: Kind::HighShelf, freq_hz: fc, gain_db: g, q };
                        match method.design(&b, fs) {
                            Ok(c) => {
                                for &f in &freqs {
                                    let w = 2.0 * PI * f / fs;
                                    worst = worst.max((c.db(w) - analog::db(&b, fs, w)).abs());
                                }
                            }
                            Err(_) => fails += 1,
                        }
                    }
                }
                write!(out, " {:>9}", format!("{worst:.2}[{fails}]")).unwrap();
            }
            writeln!(out).unwrap();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out = String::new();
    if args.iter().any(|a| a == "--shelf-q") {
        shelf_by_q(&mut out);
        print!("{out}");
        return;
    }
    report(&mut out);
    shelf_by_q(&mut out);
    smoothness(&mut out);
    print!("{out}");
    if let Some(i) = args.iter().position(|a| a == "--csv") {
        write_csv(&args[i + 1]);
    }
}
