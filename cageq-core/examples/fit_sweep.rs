//! Diagnostic: headphones fitted in both response models, reporting each fit's largest gain and
//! its error against the fit's own target (RMS below 10 kHz, mean error above) for three
//! variants — RBJ fit, the same RBJ bands realised warping-corrected, and the matched refit.
//!
//! `CAGEQ_CACHE_DIR=<catalogue cache> cargo run --release -p cageq-core --example fit_sweep [-- <headphone path>...]`
//!
//! Without paths: every measurement in the cache plus 40 seeded synthetic headphones (offline).
//! With paths (e.g. `measurements/oratory1990/data/over-ear/AKG K812.csv`): those, fetched into
//! the cache if missing. `CAGEQ_SWEEP_BANDS=1` also prints every band of both fits.
use std::sync::Arc;

use cageq_core::{filter_curve_db_in, BackendError, CalcRequest, Capabilities, Core, DeviceConfig, EqBackend, ResponseModel, StartupDecision};

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

fn main() {
    let dir = std::env::var("CAGEQ_CACHE_DIR").expect("set CAGEQ_CACHE_DIR");
    // The cached real measurement(s), plus seeded synthetic ones (offline — other real
    // measurements would have to be downloaded).
    let mut cases: Vec<(String, serde_json::Value)> = std::fs::read_dir(format!("{dir}/files")).unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("measurements__") && n.ends_with(".csv"))
        .map(|n| { let hp = n.replace("__", "/"); (hp.clone(), serde_json::Value::String(hp)) }).collect();
    // Headphone catalogue paths given on the command line are fitted instead of the synthetic set
    // (fetched into CAGEQ_CACHE_DIR if not there yet).
    let named: Vec<String> = std::env::args().skip(1).collect();
    if !named.is_empty() {
        cases = named.into_iter().map(|hp| (hp.clone(), serde_json::Value::String(hp))).collect();
    }
    let mut seed: u64 = 0x5eed_cafe;
    let mut rnd = move || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 11) as f64 / (1u64 << 53) as f64 };
    for k in 0..if std::env::args().len() > 1 { 0 } else { 40 } {
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
    println!("{:<14} {:>7} {:>7} | {:^14} | {:^14} | {:^14}", "", "", "", "RBJ fit/RBJ", "RBJ fit/match", "refit/match");
    println!("{:<14} {:>7} {:>7} | {:>6} {:>6} | {:>6} {:>6} | {:>6} {:>6}", "headphone", "max|g|R", "max|g|M", "rms<10k", "tail", "rms<10k", "tail", "rms<10k", "tail");
    for (hp, input) in cases {
        let target = if hp.contains("/in-ear/") || hp.contains("/earbud/") { "targets/Harman in-ear 2019.csv" } else { "targets/Harman over-ear 2018.csv" };
        let core = Core::start(Arc::new(Mem), None).unwrap();
        let mut fits = Vec::new();
        for model in [ResponseModel::Rbj, ResponseModel::AnalogMatched] {
            core.set_response_model(model);
            let mut req = CalcRequest::for_device("dev");
            req.inputs.insert(if input.is_string() { "headphone" } else { "measurement" }.into(), input.clone());
            req.inputs.insert("target".into(), target.into());
            match core.apply(req) { Ok(a) => fits.push(a), Err(e) => { println!("{hp}: {e}"); break; } }
        }
        if fits.len() < 2 { continue; }
        if std::env::var("CAGEQ_SWEEP_BANDS").is_ok() {
            for (m, a) in ["RBJ", "matched"].iter().zip(&fits) {
                println!("  {hp} — {m}");
                for f in &a.filters {
                    println!("     {:<9?} {:>8.1} Hz {:>+7.2} dB  Q {:.3}", f.kind, f.freq_hz, f.gain_db, f.q);
                }
            }
        }
        let maxg = |i: usize| fits[i].filters.iter().map(|f| f.gain_db.abs()).fold(0.0, f64::max);
        // Fit quality against the fit's own target (the AutoEq reference curve): RMS below 10 kHz
        // (where the loss matches shape) and the error of the mean above it (all it matches there).
        // Three variants: RBJ fit realised RBJ; the same RBJ bands realised matched (no refit);
        // the trust-region matched refit.
        let r = &fits[0].reference_curve;
        let quality = |bands: &[cageq_core::Filter], m: ResponseModel| {
            let (lo, hi): (Vec<_>, Vec<_>) = r.iter().partition(|p| p.f < 10_000.0);
            let got_lo = filter_curve_db_in(bands, &lo.iter().map(|p| p.f).collect::<Vec<_>>(), m);
            let got_hi = filter_curve_db_in(bands, &hi.iter().map(|p| p.f).collect::<Vec<_>>(), m);
            let rms = (got_lo.iter().zip(&lo).map(|(g, p)| (g - p.db).powi(2)).sum::<f64>() / lo.len() as f64).sqrt();
            let tail = got_hi.iter().sum::<f64>() / got_hi.len() as f64 - hi.iter().map(|p| p.db).sum::<f64>() / hi.len() as f64;
            (rms, tail)
        };
        let a = quality(&fits[0].filters, ResponseModel::Rbj);
        let b = quality(&fits[0].filters, ResponseModel::AnalogMatched);
        let c2 = quality(&fits[1].filters, ResponseModel::AnalogMatched);
        let short: String = hp.rsplit('/').next().unwrap().trim_end_matches(".csv").chars().take(14).collect();
        println!("{:<14} {:>7.2} {:>7.2} | {:>6.3} {:>+6.2} | {:>6.3} {:>+6.2} | {:>6.3} {:>+6.2}", short, maxg(0), maxg(1), a.0, a.1, b.0, b.1, c2.0, c2.1);
    }
}
