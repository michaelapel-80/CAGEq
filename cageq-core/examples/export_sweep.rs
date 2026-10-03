//! Diagnostic: how faithfully does the parametric export reproduce the desktop curve, under
//! AutoEq's treble rule (match only the mean above 10 kHz, no band above 10 kHz) versus a
//! full-range fit with a raised band ceiling? The export's target is the slot's own composed
//! filter curve — exact, no measurement noise — so the rule protects nothing there.
//!
//! `cargo run --release -p cageq-core --example export_sweep`
//!
//! `NOTCH_HZ=15000` moves the hand-placed notch (default 12 kHz), `NOTCH_ONLY=1` skips the
//! as-fitted curves.
//!
//! Offline: the real measurements in `tests/fixtures_real` plus seeded synthetic headphones.
//! Each desktop curve is tried as fitted and with a hand-placed notch added. Errors are
//! the exported (RBJ) bands against the desktop curve: RMS below 10 kHz, RMS and max above, and
//! the RMS above 10 kHz again with the exported bands run at 44.1 kHz (a 44.1 kHz phone).
use std::sync::Arc;

use cageq_core::{filter_curve_db_in, BackendError, CalcRequest, Capabilities, Core, DeviceConfig, EqBackend, Filter, FilterType, ResponseModel, StartupDecision};
use cageq_peq_solver::{bands_to_filters, cageq_default_bands_in, grid::standard_grid, BandKind, Solver};

struct Mem;
impl EqBackend for Mem {
    fn capabilities(&self) -> Capabilities {
        Capabilities { min_write_spacing: std::time::Duration::ZERO, owns_transitions: true, manages_foreign_config: false, analog_matched: true }
    }
    fn apply(&self, _: &[DeviceConfig]) -> Result<String, BackendError> { Ok("h".into()) }
    fn write_safe_state(&self) -> Result<(), BackendError> { Ok(()) }
    fn startup_decision(&self, _: Option<&str>) -> Result<StartupDecision, BackendError> { Ok(StartupDecision::FirstRun) }
    fn drives_endpoint(&self, _: &str) -> bool { true }
    fn location(&self) -> String { "mem".into() }
}

/// RBJ realisation of `filters` at an arbitrary sample rate (the core's curve helper is 48 kHz).
fn curve_at(filters: &[Filter], freqs: &[f64], fs: f64) -> Vec<f64> {
    let coeffs: Vec<_> = filters.iter().map(|f| {
        let kind = match f.kind {
            FilterType::Peaking => cageq_biquad::Kind::Peaking,
            FilterType::LowShelf => cageq_biquad::Kind::LowShelf,
            FilterType::HighShelf => cageq_biquad::Kind::HighShelf,
            _ => unreachable!("fits produce only peaking and shelves"),
        };
        cageq_biquad::design(&cageq_biquad::Band { kind, freq_hz: f.freq_hz, gain_db: f.gain_db, q: f.q }, fs, cageq_biquad::ResponseModel::Rbj)
    }).collect();
    freqs.iter().map(|&hz| { let w = 2.0 * std::f64::consts::PI * hz / fs; coeffs.iter().map(|c| c.db(w)).sum() }).collect()
}

struct Variant { name: &'static str, tail_mean: bool, max_fc: f64 }
const VARIANTS: [Variant; 5] = [
    Variant { name: "AutoEq rule (old export)", tail_mean: true, max_fc: 10_000.0 },
    Variant { name: "full range, fc<=10k", tail_mean: false, max_fc: 10_000.0 },
    Variant { name: "full range, fc<=14k", tail_mean: false, max_fc: 14_000.0 },
    Variant { name: "full, fc<=16k (export now)", tail_mean: false, max_fc: 16_000.0 },
    Variant { name: "full range, fc<=18k", tail_mean: false, max_fc: 18_000.0 },
];

fn export(target_filters: &[Filter], band_count: usize, v: &Variant) -> Vec<Filter> {
    let f = standard_grid();
    let target = filter_curve_db_in(target_filters, &f, ResponseModel::Rbj);
    let mut bands = cageq_default_bands_in(band_count - 2, cageq_biquad::ResponseModel::Rbj);
    for b in &mut bands {
        if b.kind == BandKind::Peaking {
            b.max_fc = v.max_fc;
        }
    }
    let mut solver = Solver::new(f, 48_000.0, bands, target);
    solver.tail_mean = v.tail_mean;
    solver.optimize().expect("export fit");
    bands_to_filters(&solver.bands)
}

fn main() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures_real");
    unsafe { std::env::set_var("CAGEQ_CACHE_DIR", fixtures) };
    let mut cases: Vec<(String, serde_json::Value)> = ["Sennheiser HD 800 S", "AKG K812"].iter()
        .map(|n| (n.to_string(), serde_json::Value::String(format!("measurements/oratory1990/data/over-ear/{n}.csv")))).collect();
    let mut seed: u64 = 0x5eed_cafe;
    let mut rnd = move || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 11) as f64 / (1u64 << 53) as f64 };
    for k in 0..8 {
        let bumps: Vec<(f64, f64, f64)> = (0..5).map(|_| ((30f64.ln() + rnd() * (18_000f64 / 30.0).ln()).exp(), 0.08 + rnd() * 0.9, (rnd() - 0.5) * 10.0)).collect();
        let tilt = (rnd() - 0.5) * 6.0;
        let mut pts = Vec::new();
        let mut f: f64 = 20.0;
        while f <= 20_000.0 {
            let lf = f.log2();
            let raw: f64 = bumps.iter().map(|(c, w, g)| g * (-((lf - c.log2()).powi(2)) / (2.0 * w * w)).exp()).sum::<f64>() + tilt * (f / 1000.0).log10();
            pts.push(serde_json::json!({"frequency": f, "raw_db": raw}));
            f *= 1.03;
        }
        cases.push((format!("synthetic {k:02}"), serde_json::Value::Array(pts)));
    }

    // Desktop curves: each case's RBJ fit, as fitted and with a hand-placed 12 kHz notch.
    let mut desktops: Vec<(String, bool, Vec<Filter>)> = Vec::new();
    for (name, input) in &cases {
        let core = Core::start(Arc::new(Mem), None).unwrap();
        let mut req = CalcRequest::for_device("dev");
        req.inputs.insert(if input.is_string() { "headphone" } else { "measurement" }.into(), input.clone());
        req.inputs.insert("target".into(), "targets/Harman over-ear 2018.csv".into());
        let fit = core.apply(req).unwrap_or_else(|e| panic!("{name}: {e}")).filters;
        let mut notched = fit.clone();
        let notch_hz: f64 = std::env::var("NOTCH_HZ").ok().and_then(|v| v.parse().ok()).unwrap_or(12_000.0);
        notched.push(Filter { kind: FilterType::Peaking, freq_hz: notch_hz, gain_db: -6.0, q: 4.0 });
        desktops.push((name.clone(), false, fit));
        desktops.push((name.clone(), true, notched));
    }

    let f: Vec<f64> = { let mut v = Vec::new(); let mut x: f64 = 20.0; while x <= 20_000.0 { v.push(x); x *= 1.005; } v };
    let (lo, hi): (Vec<usize>, Vec<usize>) = (0..f.len()).partition(|&i| f[i] < 10_000.0);
    let rms = |a: &[f64], b: &[f64], ix: &[usize]| (ix.iter().map(|&i| (a[i] - b[i]).powi(2)).sum::<f64>() / ix.len() as f64).sqrt();
    let maxe = |a: &[f64], b: &[f64], ix: &[usize]| ix.iter().map(|&i| (a[i] - b[i]).abs()).fold(0.0, f64::max);

    let notch_only = std::env::var("NOTCH_ONLY").is_ok();
    for notch in [false, true] {
        if notch_only && !notch { continue; }
        for band_count in [6usize, 8, 12, 16] {
            println!("\n## {} — {band_count} bands  (mean over {} desktop curves; dB)", if notch { "with notch" } else { "as fitted" }, cases.len());
            println!("{:<28} {:>8} {:>9} {:>9} {:>12} {:>8}", "variant", "rms<10k", "rms>10k", "max>10k", "rms>10k@44k", "top fc");
            for v in &VARIANTS {
                let mut acc = [0.0f64; 4];
                let mut top_fc = 0.0f64;
                for (_, n, desk) in desktops.iter().filter(|d| d.1 == notch) {
                    let _ = n;
                    let ex = export(desk, band_count, v);
                    top_fc = top_fc.max(ex.iter().filter(|b| b.kind == FilterType::Peaking).map(|b| b.freq_hz).fold(0.0, f64::max));
                    let want = curve_at(desk, &f, 48_000.0);
                    let got48 = curve_at(&ex, &f, 48_000.0);
                    let got44 = curve_at(&ex, &f, 44_100.0);
                    acc[0] += rms(&got48, &want, &lo);
                    acc[1] += rms(&got48, &want, &hi);
                    acc[2] += maxe(&got48, &want, &hi);
                    acc[3] += rms(&got44, &want, &hi);
                }
                let n = cases.len() as f64;
                println!("{:<28} {:>8.3} {:>9.3} {:>9.3} {:>12.3} {:>8.0}", v.name, acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n, top_fc);
            }
        }
    }
}
