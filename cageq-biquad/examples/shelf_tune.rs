//! Stage 0b: re-run Vicanek's "thorough numerical analysis" for the generalised shelf, per Q.
//!
//! His match points `f_i = fc / √(p_i + r_i·fc²)` (fc, f in Nyquist units) were tuned for the
//! Butterworth shelf only. For each Q this searches `(p1, r1, p2, r2)` by Nelder–Mead (in log
//! space, so they stay positive) to minimise the worst |error| against the analog prototype
//! over fc 20 Hz–20 kHz × gain ±20 dB at 44.1 and 48 kHz, with a heavy penalty per design
//! that is not realisable. Prints the tuned parameters and their worst error next to the
//! paper's defaults.
//!
//! `cargo run --release -p cageq-biquad --example shelf_tune`

use std::f64::consts::PI;

use cageq_biquad::{analog, matched, Band, Kind};

const PAPER: [f64; 4] = [0.160, 1.543, 0.947, 3.806];
const FAIL_PENALTY: f64 = 100.0;

fn log_space(n: usize, lo: f64, hi: f64) -> Vec<f64> {
    (0..n).map(|i| lo * (hi / lo).powf(i as f64 / (n - 1) as f64)).collect()
}

struct Grid {
    fcs: Vec<f64>,
    gains: Vec<f64>,
    freqs: Vec<f64>,
}

/// (worst |error| dB, failed designs) for match-point parameters `p` at shelf `q`.
fn score(p: &[f64; 4], q: f64, grid: &Grid) -> (f64, usize) {
    let mut worst: f64 = 0.0;
    let mut fails = 0;
    for fs in [44_100.0, 48_000.0] {
        for &fc_hz in &grid.fcs {
            let fc = fc_hz / (fs / 2.0);
            let f1 = fc / (p[0] + p[1] * fc * fc).sqrt();
            let f2 = fc / (p[2] + p[3] * fc * fc).sqrt();
            for &g in &grid.gains {
                let b = Band { kind: Kind::HighShelf, freq_hz: fc_hz, gain_db: g, q };
                match matched::vicanek_shelf_at(&b, fs, f1, f2) {
                    Ok(c) if c.pole_radius() < 1.0 && c.zero_radius() < 1.0 => {
                        for &f in &grid.freqs {
                            let w = 2.0 * PI * f / fs;
                            worst = worst.max((c.db(w) - analog::db(&b, fs, w)).abs());
                        }
                    }
                    _ => fails += 1,
                }
            }
        }
    }
    (worst, fails)
}

fn objective(logp: &[f64; 4], q: f64, grid: &Grid) -> f64 {
    let p = logp.map(f64::exp);
    let (worst, fails) = score(&p, q, grid);
    worst + FAIL_PENALTY * fails as f64
}

/// Plain Nelder–Mead on 4 parameters — enough for a smooth-ish 4-D offline search, and no
/// dependency.
fn nelder_mead(start: [f64; 4], f: impl Fn(&[f64; 4]) -> f64, iters: usize) -> ([f64; 4], f64) {
    let mut simplex: Vec<([f64; 4], f64)> = (0..5)
        .map(|i| {
            let mut x = start;
            if i > 0 {
                x[i - 1] += 0.4;
            }
            (x, f(&x))
        })
        .collect();
    for _ in 0..iters {
        simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut centroid = [0.0; 4];
        for (x, _) in &simplex[..4] {
            for k in 0..4 {
                centroid[k] += x[k] / 4.0;
            }
        }
        let worst = simplex[4];
        let along = |t: f64| -> [f64; 4] { std::array::from_fn(|k| centroid[k] + t * (worst.0[k] - centroid[k])) };
        let xr = along(-1.0);
        let fr = f(&xr);
        if fr < simplex[0].1 {
            let xe = along(-2.0);
            let fe = f(&xe);
            simplex[4] = if fe < fr { (xe, fe) } else { (xr, fr) };
        } else if fr < simplex[3].1 {
            simplex[4] = (xr, fr);
        } else {
            let xc = along(0.5);
            let fc = f(&xc);
            if fc < worst.1 {
                simplex[4] = (xc, fc);
            } else {
                let best = simplex[0].0;
                for s in simplex.iter_mut().skip(1) {
                    s.0 = std::array::from_fn(|k| best[k] + 0.5 * (s.0[k] - best[k]));
                    s.1 = f(&s.0);
                }
            }
        }
    }
    simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
    simplex[0]
}

/// `--verify`: the composite [`matched::shelf`] over a dense grid — both shelf kinds, 44.1/48/96
/// kHz, 200 Q values 0.1–20, 60 fc, 12 gains. Failures and stability/minimum-phase violations
/// must be zero; accuracy is reported per Q range next to RBJ and Ivantsov; continuity is the
/// largest response jump for a 0.1 % step in Q across each blend-window edge, next to RBJ's jump
/// for the same step (RBJ is analytic in Q, so its jump is the "nothing switched" baseline).
fn verify() {
    let fcs = log_space(60, 20.0, 20_000.0);
    let gains = [-20.0, -12.0, -6.0, -3.0, -1.0, -0.1, 0.1, 1.0, 3.0, 6.0, 12.0, 20.0];
    let freqs = log_space(200, 20.0, 20_000.0);
    let ranges = [(0.1, 0.38), (0.38, 0.42), (0.42, 0.655), (0.655, 0.69), (0.69, 1.5), (1.5, 2.8), (2.8, 3.3), (3.3, 20.01)];
    let mut worst = [[0.0f64; 3]; 8]; // per range: shelf, rbj, ivantsov
    let (mut fails, mut unsafe_) = (0usize, 0usize);
    let db_err = |c: &cageq_biquad::Coeffs, b: &Band, fs: f64| {
        freqs.iter().map(|&f| { let w = 2.0 * PI * f / fs; (c.db(w) - analog::db(b, fs, w)).abs() }).fold(0.0, f64::max)
    };
    for kind in [Kind::HighShelf, Kind::LowShelf] {
        for fs in [44_100.0, 48_000.0, 96_000.0] {
            for i in 0..200 {
                let q = 0.1 * 200f64.powf(i as f64 / 199.0);
                let r = ranges.iter().position(|&(lo, hi)| q >= lo && q < hi).unwrap();
                for &fc in &fcs {
                    for &g in &gains {
                        let b = Band { kind, freq_hz: fc, gain_db: g, q };
                        match matched::shelf(&b, fs) {
                            Ok(c) => {
                                if !(c.pole_radius() < 1.0 && c.zero_radius() < 1.0) {
                                    unsafe_ += 1;
                                }
                                worst[r][0] = worst[r][0].max(db_err(&c, &b, fs));
                            }
                            Err(_) => fails += 1,
                        }
                        worst[r][1] = worst[r][1].max(db_err(&cageq_biquad::rbj::coefficients(&b, fs), &b, fs));
                        worst[r][2] = worst[r][2].max(db_err(&matched::ivantsov(&b, fs, 2.0).unwrap(), &b, fs));
                    }
                }
            }
        }
    }
    println!("composite shelf: {fails} failures, {unsafe_} unstable / non-minimum-phase designs");
    println!("{:>14} {:>9} {:>9} {:>9}   (worst |error| dB, both kinds, 44.1/48/96 kHz)", "Q range", "shelf", "rbj", "ivantsov");
    for (r, &(lo, hi)) in ranges.iter().enumerate() {
        println!("{:>14} {:>9.2} {:>9.2} {:>9.2}", format!("{lo}–{}", hi.min(20.0)), worst[r][0], worst[r][1], worst[r][2]);
    }

    println!("\ncontinuity: largest |Δ response| dB for Q → Q·1.001, at each window edge (shelf | rbj)");
    for edge in [0.38, 0.40, 0.42, 0.655, 0.67, 0.69, 2.8, 3.0, 3.3] {
        let (mut js, mut jr) = (0.0f64, 0.0f64);
        for fs in [44_100.0, 48_000.0] {
            for &fc in &fcs {
                for &g in &gains {
                    let at = |q: f64| Band { kind: Kind::HighShelf, freq_hz: fc, gain_db: g, q };
                    let (a, b) = (at(edge), at(edge * 1.001));
                    let (sa, sb) = (matched::shelf(&a, fs).unwrap(), matched::shelf(&b, fs).unwrap());
                    let (ra, rb) = (cageq_biquad::rbj::coefficients(&a, fs), cageq_biquad::rbj::coefficients(&b, fs));
                    for &f in &freqs {
                        let w = 2.0 * PI * f / fs;
                        js = js.max((sa.db(w) - sb.db(w)).abs());
                        jr = jr.max((ra.db(w) - rb.db(w)).abs());
                    }
                }
            }
        }
        println!("  Q {edge:>5}: {js:.4} | {jr:.4}");
    }
}

fn main() {
    let grid = Grid {
        fcs: log_space(30, 20.0, 20_000.0),
        gains: vec![-20.0, -12.0, -6.0, -3.0, -1.0, -0.1, 0.1, 1.0, 3.0, 6.0, 12.0, 20.0],
        freqs: log_space(300, 20.0, 20_000.0),
    };
    // `--fixed`: how does each per-Q optimum hold up across *all* Q? One parameter set for
    // every Q is trivially continuous in Q; this measures what that costs.
    if std::env::args().any(|a| a == "--fixed") {
        let sets: [(&str, [f64; 4]); 7] = [
            ("paper", PAPER),
            ("Q0.4", [0.042, 1.245, 0.392, 2.160]),
            ("Q0.7", [0.085, 1.401, 0.748, 3.052]),
            ("Q1", [0.076, 1.350, 0.961, 2.122]),
            ("Q1.41", [0.223, 1.125, 0.805, 2.175]),
            ("Q2", [0.236, 1.668, 0.898, 1.683]),
            ("Q3", [0.356, 1.458, 0.994, 0.995]),
        ];
        let qs = [0.1, 0.3, 0.4, 0.45, 0.5, 0.55, 0.6, 0.65, 0.7, 1.0, 1.41, 2.0, 3.0, 4.0, 10.0, 20.0];
        print!("{:>7}", "set\\Q");
        for q in qs {
            print!(" {:>10}", q);
        }
        println!();
        for (name, p) in sets {
            print!("{name:>7}");
            for q in qs {
                let (w, f) = score(&p, q, &grid);
                print!(" {:>10}", format!("{w:.2}[{f}]"));
            }
            println!();
        }
        return;
    }
    // `--schedule`: the anchor sets interpolated in log Q, scanned densely — where is the
    // schedule feasible for every fc/gain, and how accurate is it there?
    if std::env::args().any(|a| a == "--schedule") {
        // `--schedule lo hi` narrows the scan (e.g. to find a window edge precisely).
        let args: Vec<f64> = std::env::args().filter_map(|a| a.parse().ok()).collect();
        let (lo, hi) = if args.len() >= 2 { (args[0], args[1]) } else { (0.1, 20.0) };
        for i in 0..=48 {
            let q = lo * (hi / lo).powf(i as f64 / 48.0);
            let (w, f) = score(&matched::shelf_match_points(q), q, &grid);
            println!("Q {q:>7.3}: {w:>6.2} dB [{f}]");
        }
        return;
    }
    if std::env::args().any(|a| a == "--verify") {
        verify();
        return;
    }
    let designs = 2 * grid.fcs.len() * grid.gains.len();
    println!("{designs} designs per Q; worst |error| dB [failures]");
    println!("{:>6} {:>14} {:>14}   tuned (p1, r1, p2, r2)", "Q", "paper", "tuned");
    let mut start = PAPER.map(f64::ln);
    for q in [0.3, 0.4, 0.45, 0.55, 0.6, 0.65, 0.7, 0.8, 1.0, 1.2, 1.41, 1.7, 2.0, 2.5, 3.0, 4.0] {
        let (pw, pf) = score(&PAPER, q, &grid);
        // Warm-start from the previous Q's optimum (neighbouring Qs want similar points), but
        // also try the paper's; keep the better.
        let a = nelder_mead(start, |x| objective(x, q, &grid), 300);
        let b = nelder_mead(PAPER.map(f64::ln), |x| objective(x, q, &grid), 300);
        let (best, _) = if a.1 <= b.1 { a } else { b };
        start = best;
        let p = best.map(f64::exp);
        let (tw, tf) = score(&p, q, &grid);
        println!(
            "{q:>6} {:>14} {:>14}   ({:.3}, {:.3}, {:.3}, {:.3})",
            format!("{pw:.3}[{pf}]"),
            format!("{tw:.3}[{tf}]"),
            p[0], p[1], p[2], p[3]
        );
    }
}
