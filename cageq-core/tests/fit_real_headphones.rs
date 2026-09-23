//! The AutoEq fit on real measurements, against Python AutoEq 4.1.2's own result for the same
//! inputs (`cageq-sidecar/.venv`, `sidecar_dsp.calculate_filters`) — offline, from the fixtures
//! in `tests/fixtures_real` (see its README).
//!
//! The Sennheiser HD 800 S is the case that exposed the problem: fully converged, the port's
//! solve used to settle on a self-cancelling +20 dB / −18 dB pair of Q 0.39 bells 150 Hz apart
//! (which even scores better than Python's fit by AutoEq's own loss — Python only avoids it by
//! stopping early). The solver's cancellation penalty makes the solve itself prefer sane sets;
//! this pins that, in both response models, without giving up fit quality versus Python.
//!
//! Its own test binary (so its own process): it sets `CAGEQ_CACHE_DIR` for the whole run.

use std::sync::Arc;

use cageq_core::{
    filter_curve_db_in, BackendError, CalcRequest, Capabilities, Core, DeviceConfig, EqBackend, ResponseModel,
    StartupDecision,
};

struct Mem;
impl EqBackend for Mem {
    fn capabilities(&self) -> Capabilities {
        Capabilities { min_write_spacing: std::time::Duration::ZERO, owns_transitions: true, manages_foreign_config: false, analog_matched: true }
    }
    fn apply(&self, _: &[DeviceConfig]) -> Result<String, BackendError> {
        Ok("h".into())
    }
    fn write_safe_state(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn startup_decision(&self, _: Option<&str>) -> Result<StartupDecision, BackendError> {
        Ok(StartupDecision::FirstRun)
    }
    fn drives_endpoint(&self, _: &str) -> bool {
        true
    }
    fn location(&self) -> String {
        "mem".into()
    }
}

/// (headphone, Python AutoEq's largest |gain|, Python's RMS error vs the fit's own target below
/// 10 kHz) — measured with the reference on these exact fixtures.
const CASES: [(&str, f64, f64); 2] = [
    ("measurements/oratory1990/data/over-ear/Sennheiser HD 800 S.csv", 5.24, 0.403),
    ("measurements/oratory1990/data/over-ear/AKG K812.csv", 6.20, 0.261),
];

#[test]
fn real_fits_are_sane_and_at_least_as_good_as_python_autoeq() {
    // SAFETY-free: this test binary has no other tests running concurrently with it.
    unsafe { std::env::set_var("CAGEQ_CACHE_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures_real")) };
    for (hp, py_max_gain, py_rms) in CASES {
        let core = Core::start(Arc::new(Mem), None).unwrap();
        for model in [ResponseModel::Rbj, ResponseModel::AnalogMatched] {
            core.set_response_model(model);
            let mut req = CalcRequest::for_device("dev");
            req.inputs.insert("headphone".into(), hp.into());
            req.inputs.insert("target".into(), "targets/Harman over-ear 2018.csv".into());
            let fit = core.apply(req).unwrap();

            let max_gain = fit.filters.iter().map(|f| f.gain_db.abs()).fold(0.0, f64::max);
            assert!(max_gain <= 10.0, "{hp} {model:?}: largest gain {max_gain:.2} dB (Python: {py_max_gain}) — {:?}", fit.filters);

            let lo: Vec<_> = fit.reference_curve.iter().filter(|p| p.f < 10_000.0).collect();
            let got = filter_curve_db_in(&fit.filters, &lo.iter().map(|p| p.f).collect::<Vec<_>>(), model);
            let rms = (got.iter().zip(&lo).map(|(g, p)| (g - p.db).powi(2)).sum::<f64>() / lo.len() as f64).sqrt();
            assert!(rms <= py_rms * 1.1, "{hp} {model:?}: error {rms:.3} dB vs Python's {py_rms} (+10% allowed)");
        }
    }
}
