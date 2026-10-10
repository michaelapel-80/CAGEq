//! Diagnostic: what does the main fit do below 20 Hz, and would fitting differently stop it?
//!
//! The fit (AutoEq's) runs on 20 Hz-20 kHz and always includes a low shelf at 105 Hz. Below 20 Hz
//! nothing constrains it, so the shelf's boost runs on unchanged to DC — infrasound gets the full
//! bass lift, which on an open headphone can drive the driver to its excursion limit. Compared,
//! for a few real open-back measurements against the Harman over-ear 2018 target:
//!
//! - band sets: today's (low shelf 105 Hz + high shelf 10 kHz + 8 peaking), no low shelf (high
//!   shelf + 9 peaking), and a low shelf with a free corner (+ high shelf + 8 peaking);
//! - targets: today's (20 Hz-20 kHz), and the same equalization extended to 5 Hz by holding its
//!   20 Hz value and rolling it off at 12 or 24 dB/oct below 20 Hz, with the fit error taken
//!   from 5 Hz;
//! - for reference, today's fit with a 24 dB/oct Butterworth high-pass at 20 Hz appended.
//!
//! Reported: RMS error against the equalization over 20 Hz-10 kHz (above that the fit only
//! matches the mean) and over 20-100 Hz, the correction's gain at 20, 16 and 10 Hz, and its peak
//! gain (what the preamp has to make room for); then how far each alternative moves the audible
//! curve from today's fit, by region, with the bands side by side.
//!
//! Finding (2026-10, HD 6XX / HD 800 S / K812): **the fit stays as it is.**
//! - Today's fit carries +4 to +6 dB to 10 Hz (the shelf runs on to DC).
//! - Extending the target with a -12 dB/oct roll-off below 20 Hz halves the 16 Hz boost at a
//!   near-equal fit error, but only by degenerating: the fixed 105 Hz shelf pins at -20 dB and
//!   +15-18 dB bells at 20-50 Hz rebuild the bass — exactly the cancelling pair the solver's
//!   cancellation penalty exists to prevent — and the audible curve moves by up to 1.3-2.1 dB in
//!   the treble. -24 dB/oct doubles the fit error.
//! - Dropping the shelf keeps sane bands and moves the curve mostly < 0.2 dB (up to 1.1 dB in
//!   1-10 kHz where bands get reshuffled), but still passes about +4 dB at 16 Hz: the lowest bell
//!   cannot sit below 20 Hz.
//! - A free-corner shelf fits best and is worst for infrasound.
//! Changing the fit would also break parity with AutoEq and move every saved preset under
//! stages tuned on top of it. The high-pass band (12-48 dB/oct) is the targeted tool instead:
//! today's fit plus 24 dB/oct at 20 Hz cuts 16 Hz by 3-4 dB and costs -3 dB at 20 Hz.
//!
//! `cargo run --release -p cageq-core --example bass_fit` (measurements from
//! `tests/fixtures_real`, plus any `measurements__*.csv` in `$CAGEQ_CACHE_DIR/files`).
use std::path::{Path, PathBuf};

use cageq_peq_solver::grid::{biquad_optimization_grid, generate_frequencies, linear_interp_log, standard_grid};
use cageq_peq_solver::{equalize, prepare, Band, BandKind, Solver};

const FS: f64 = 48_000.0;
const MAX_SLOPE: f64 = 18.0;
const MAX_GAIN: f64 = 6.0;

fn read_csv(path: &Path) -> (Vec<f64>, Vec<f64>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split(',');
            Some((it.next()?.trim().parse::<f64>().ok()?, it.next()?.trim().parse::<f64>().ok()?))
        })
        .unzip()
}

#[derive(Clone, Copy)]
enum Set {
    Today,
    NoLowShelf,
    FreeLowShelf,
}

fn bands(set: Set) -> Vec<Band> {
    let mut v = match set {
        Set::Today => vec![Band::fixed_fc_q(BandKind::LowShelf, 105.0, 0.7)],
        Set::NoLowShelf => vec![Band::free(BandKind::Peaking)],
        Set::FreeLowShelf => vec![Band::free(BandKind::LowShelf)],
    };
    v.push(Band::fixed_fc_q(BandKind::HighShelf, 10_000.0, 0.7));
    v.extend((0..8).map(|_| Band::free(BandKind::Peaking)));
    v
}

/// Response in dB of `bands` (plus `extra` biquads, already designed) on `f`.
fn response(bands: &[Band], extra: &[cageq_biquad::Coeffs], f: &[f64]) -> Vec<f64> {
    let mut total = vec![0.0; f.len()];
    for b in bands {
        for (t, v) in total.iter_mut().zip(b.fr(f, FS)) {
            *t += v;
        }
    }
    for c in extra {
        for (t, &hz) in total.iter_mut().zip(f) {
            *t += c.db(2.0 * std::f64::consts::PI * hz / FS);
        }
    }
    total
}

fn rms_between(f: &[f64], a: &[f64], b: &[f64], lo: f64, hi: f64) -> f64 {
    let (mut s, mut n) = (0.0, 0);
    for i in 0..f.len() {
        if f[i] >= lo && f[i] <= hi {
            s += (a[i] - b[i]).powi(2);
            n += 1;
        }
    }
    (s / n as f64).sqrt()
}

fn main() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures_real/files");
    let target = read_csv(&fixtures.join("targets__Harman over-ear 2018.csv"));
    let mut sources: Vec<PathBuf> = Vec::new();
    for dir in [Some(fixtures.clone()), std::env::var("CAGEQ_CACHE_DIR").ok().map(|d| PathBuf::from(d).join("files"))].into_iter().flatten() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("measurements__") && name.ends_with(".csv") && !sources.iter().any(|p| p.file_name() == e.path().file_name()) {
                    sources.push(e.path());
                }
            }
        }
    }
    sources.sort();

    let hp24: Vec<cageq_biquad::Coeffs> = [0.5412, 1.3066]
        .iter()
        .map(|&q| cageq_biquad::design(&cageq_biquad::Band { kind: cageq_biquad::Kind::HighPass, freq_hz: 20.0, gain_db: 0.0, q }, FS, cageq_biquad::ResponseModel::Rbj))
        .collect();

    let eval_f = generate_frequencies(5.0, 20_000.0, 1.01);
    println!("RMS error vs the equalization (dB) | correction gain (dB) at 20 / 16 / 10 Hz | peak gain\n");
    for src in &sources {
        let name = src.file_name().unwrap().to_string_lossy().rsplit("__").next().unwrap().trim_end_matches(".csv").to_string();
        let (mf, mr) = read_csv(src);
        let prepped = prepare(&mf, &mr, &target.0, &target.1);
        let eq = equalize(&prepped.f, &prepped.error_smoothed, MAX_SLOPE, MAX_GAIN);
        let std_f = standard_grid();
        let eq_std = linear_interp_log(&prepped.f, &eq, &std_f);
        let eq20 = eq_std[0];
        println!("## {name}   (equalization at 20 Hz: {eq20:+.1} dB)");
        println!("{:<34}{:>9}{:>9}{:>8}{:>8}{:>8}{:>8}", "variant", "20-10k", "20-100", "@20", "@16", "@10", "peak");

        let opt_f = biquad_optimization_grid();
        let report = |label: &str, bs: &[Band], extra: &[cageq_biquad::Coeffs]| {
            let on_std = response(bs, extra, &std_f);
            let on_eval = response(bs, extra, &eval_f);
            let at = |hz: f64| on_eval[eval_f.iter().position(|&f| f >= hz).unwrap()];
            let peak = on_eval.iter().copied().fold(f64::MIN, f64::max);
            println!(
                "{label:<34}{:>9.2}{:>9.2}{:>8.1}{:>8.1}{:>8.1}{:>8.1}",
                rms_between(&std_f, &on_std, &eq_std, 20.0, 10_000.0),
                rms_between(&std_f, &on_std, &eq_std, 20.0, 100.0),
                at(20.0),
                at(16.0),
                at(10.0),
                peak
            );
        };

        let (mut today, mut extended, mut no_shelf): (Option<Vec<Band>>, Option<Vec<Band>>, Option<Vec<Band>>) = (None, None, None);
        for (set, set_name) in [(Set::Today, "LS105 + HS + 8 PK"), (Set::NoLowShelf, "HS + 9 PK"), (Set::FreeLowShelf, "free LS + HS + 8 PK")] {
            for rolloff in [None, Some(12.0), Some(24.0)] {
                let (f, t) = match rolloff {
                    None => (opt_f.clone(), linear_interp_log(&prepped.f, &eq, &opt_f)),
                    Some(slope) => {
                        let mut f = generate_frequencies(5.0, 20.0, 1.02);
                        f.pop(); // 20 Hz itself comes from the standard part
                        let mut t: Vec<f64> = f.iter().map(|&hz| eq20 - slope * (20.0 / hz).log2()).collect();
                        f.extend(&opt_f);
                        t.extend(linear_interp_log(&prepped.f, &eq, &opt_f));
                        (f, t)
                    }
                };
                let mut solver = Solver::new(f, FS, bands(set), t);
                if rolloff.is_some() {
                    solver.set_min_f(5.0);
                }
                if let Err(e) = solver.optimize() {
                    println!("{set_name}: {e}");
                    continue;
                }
                let label = match rolloff {
                    None => set_name.to_string(),
                    Some(s) => format!("{set_name}, -{s:.0} dB/oct < 20 Hz"),
                };
                report(&label, &solver.bands, &[]);
                if matches!(set, Set::NoLowShelf) && rolloff.is_none() {
                    no_shelf = Some(solver.bands.clone());
                }
                if matches!(set, Set::Today) {
                    match rolloff {
                        None => today = Some(solver.bands.clone()),
                        Some(s) if s == 12.0 => extended = Some(solver.bands.clone()),
                        _ => {}
                    }
                }
                if matches!(set, Set::Today) && rolloff.is_none() {
                    report("  + HP 24 dB/oct @ 20 Hz", &solver.bands, &hp24);
                }
            }
        }

        // How far the -12 dB/oct fit moves the *audible* curve from today's — what a hand-tuned
        // stage on top of it would notice — by region, and the bands themselves side by side.
        for (what, other) in [("-12 dB/oct fit", &extended), ("no low shelf", &no_shelf)] {
            let (Some(a), Some(b)) = (&today, other) else { continue };
            let (ca, cb) = (response(a, &[], &std_f), response(b, &[], &std_f));
            let d: Vec<f64> = cb.iter().zip(&ca).map(|(x, y)| x - y).collect();
            print!("today -> {what}, curve change (RMS / max |dB|):");
            for (lo, hi) in [(20.0, 40.0), (40.0, 100.0), (100.0, 1000.0), (1000.0, 10_000.0), (10_000.0, 20_000.0)] {
                let sel: Vec<f64> = std_f.iter().zip(&d).filter(|(f, _)| **f >= lo && **f < hi).map(|(_, v)| *v).collect();
                let rms = (sel.iter().map(|v| v * v).sum::<f64>() / sel.len() as f64).sqrt();
                let max = sel.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                print!("  {lo:.0}-{hi:.0}: {rms:.2}/{max:.2}");
            }
            println!();
            let fmt = |b: &Band| format!("{:?} {:.0} Hz {:+.1} dB Q{:.2}", b.kind, b.fc, b.gain, b.q);
            let mut sa: Vec<&Band> = a.iter().collect();
            let mut sb: Vec<&Band> = b.iter().collect();
            sa.sort_by(|x, y| x.fc.total_cmp(&y.fc));
            sb.sort_by(|x, y| x.fc.total_cmp(&y.fc));
            for (x, y) in sa.iter().zip(&sb) {
                println!("    {:<36} {}", fmt(x), fmt(y));
            }
        }
        println!();
    }
}
