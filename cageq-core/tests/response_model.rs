//! The core's handling of the [`ResponseModel`] (warping-correction plan, Stage 3): the user's
//! preference, gated by the backend, drives the fit, the loudness/preamp quantities and what
//! reaches the backend — and a change of the *effective* model re-fits stale slots on its own,
//! including when the backend is swapped mid-session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cageq_core::{
    BackendError, CalcRequest, Capabilities, Core, DeviceConfig, EqBackend, Filter, FilterType, ResponseModel, Slot,
    StartupDecision,
};
use serde_json::json;

/// Records every applied config; `analog_matched` can be flipped to stand in for the app
/// swapping CAGEq's APO for Equalizer APO (or back) under a running core.
#[derive(Default)]
struct Recording {
    analog_matched: AtomicBool,
    applied: Mutex<Vec<DeviceConfig>>,
}

impl Recording {
    fn last(&self) -> DeviceConfig {
        self.applied.lock().unwrap().last().cloned().expect("something was applied")
    }
}

impl EqBackend for Recording {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            min_write_spacing: std::time::Duration::ZERO,
            owns_transitions: true,
            manages_foreign_config: false,
            analog_matched: self.analog_matched.load(Ordering::SeqCst),
        }
    }
    fn apply(&self, configs: &[DeviceConfig]) -> Result<String, BackendError> {
        let mut applied = self.applied.lock().unwrap();
        applied.extend_from_slice(configs);
        Ok(format!("h{}", applied.len()))
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
        "memory".into()
    }
}

/// A measurement with real treble content (a peak near 8 kHz), inline so no network is
/// needed — enough for the fit to have high-frequency bands whose best values depend on
/// the model.
fn request() -> CalcRequest {
    let mut points = Vec::new();
    let mut f: f64 = 20.0;
    while f <= 20_000.0 {
        let lf = f.log2();
        let raw = 4.0 * (-((lf - 80f64.log2()).powi(2)) / 0.18).exp() - 3.0 * (-((lf - 3000f64.log2()).powi(2)) / 0.125).exp()
            + 5.0 * (-((lf - 9000f64.log2()).powi(2)) / 0.08).exp();
        points.push(json!({"frequency": f, "raw_db": raw}));
        f *= 1.03;
    }
    let mut req = CalcRequest::for_device("DAC");
    req.inputs.insert("measurement".into(), serde_json::Value::Array(points));
    req.inputs.insert(
        "custom_filters".into(),
        json!([{"kind": "HighShelf", "freq_hz": 12000.0, "gain_db": 4.0, "q": 0.7}]),
    );
    req
}

fn start(analog_matched: bool) -> (Core, Arc<Recording>) {
    let backend = Arc::new(Recording::default());
    backend.analog_matched.store(analog_matched, Ordering::SeqCst);
    (Core::start(backend.clone(), None).unwrap(), backend)
}

#[test]
fn rbj_is_the_default_and_what_is_applied() {
    let (core, backend) = start(true);
    assert_eq!(core.response_model(), ResponseModel::Rbj);
    let applied = core.apply(request()).unwrap();
    assert_eq!(applied.model, ResponseModel::Rbj);
    assert_eq!(backend.last().model, ResponseModel::Rbj);
}

/// On a backend that can realise it, the preference is applied — refitted for it: the
/// fitted bands and the loudness quantities differ from the RBJ fit, and the backend is
/// told which model to realise them with.
#[test]
fn analog_matched_refits_and_reaches_the_backend() {
    let (core, backend) = start(true);
    let rbj = core.apply(request()).unwrap();

    let matched = core.update_response_model(ResponseModel::AnalogMatched).expect("a slot is active").unwrap();
    assert_eq!(matched.model, ResponseModel::AnalogMatched);
    assert_eq!(backend.last().model, ResponseModel::AnalogMatched);
    assert_eq!(core.effective_response_model(), ResponseModel::AnalogMatched);
    assert!(
        matched.filters.iter().zip(&rbj.filters).any(|(m, r)| (m.gain_db - r.gain_db).abs() > 1e-6 || (m.freq_hz - r.freq_hz).abs() > 1e-6),
        "the fit must be redone for the new model, not reused"
    );
    assert_ne!(matched.g_max_peak_db, rbj.g_max_peak_db, "curve quantities must describe the matched curve");

    // And back: the RBJ fit comes back exactly (from the fit cache, as before).
    let back = core.update_response_model(ResponseModel::Rbj).unwrap().unwrap();
    assert_eq!(back.model, ResponseModel::Rbj);
    assert_eq!(back.filters.len(), rbj.filters.len());
    for (a, b) in back.filters.iter().zip(&rbj.filters) {
        assert_eq!((a.freq_hz, a.gain_db, a.q), (b.freq_hz, b.gain_db, b.q));
    }
    assert_eq!(back.g_target_db, rbj.g_target_db);
}

/// A backend without the capability (Equalizer APO) never gets the matched model, whatever
/// the preference says — the preference is remembered, the effective model is RBJ.
#[test]
fn a_backend_without_the_capability_stays_rbj() {
    let (core, backend) = start(false);
    core.set_response_model(ResponseModel::AnalogMatched);
    let applied = core.apply(request()).unwrap();
    assert_eq!(core.response_model(), ResponseModel::AnalogMatched);
    assert_eq!(core.effective_response_model(), ResponseModel::Rbj);
    assert!(!core.analog_matched_available());
    assert_eq!(applied.model, ResponseModel::Rbj);
    assert_eq!(backend.last().model, ResponseModel::Rbj);
}

/// The mid-session backend swap: the capability disappears under a running core and the app
/// calls `reapply`. Both slots must come back fitted for RBJ — not the matched fit written to
/// a backend that would realise it as RBJ — and when the capability returns, so does the model.
#[test]
fn a_backend_swap_refits_every_slot_for_the_new_effective_model() {
    let (core, backend) = start(true);
    core.set_response_model(ResponseModel::AnalogMatched);
    let a = core.apply_to_slot(Slot::A, request()).unwrap();
    let mut other = request();
    other.inputs.insert("custom_filters".into(), json!([{"kind": "Peaking", "freq_hz": 15000.0, "gain_db": -3.0, "q": 2.0}]));
    core.apply_to_slot(Slot::B, other).unwrap();
    assert_eq!(a.model, ResponseModel::AnalogMatched);

    backend.analog_matched.store(false, Ordering::SeqCst); // swapped to Equalizer APO
    let after = core.reapply().unwrap().unwrap();
    assert_eq!(after.model, ResponseModel::Rbj);
    assert_eq!(backend.last().model, ResponseModel::Rbj);
    let a_rbj = core.activate_slot(Slot::A).unwrap();
    assert_eq!(a_rbj.model, ResponseModel::Rbj, "the inactive slot was refitted too");

    backend.analog_matched.store(true, Ordering::SeqCst); // and back to CAGEq's APO
    let restored = core.reapply().unwrap().unwrap();
    assert_eq!(restored.model, ResponseModel::AnalogMatched);
    for (x, y) in restored.filters.iter().zip(&a.filters) {
        assert_eq!((x.freq_hz, x.gain_db, x.q), (y.freq_hz, y.gain_db, y.q), "same matched fit as before the swap");
    }
}

/// A slot restored from the launch cache (§3.5) has no request to re-fit from. Its bands are
/// kept, but its curve quantities are recomputed for the model now in effect, so the preamp
/// is composed for what actually plays.
#[test]
fn a_seeded_slot_gets_its_quantities_recomputed_for_the_effective_model() {
    let (core, backend) = start(true);
    core.set_response_model(ResponseModel::AnalogMatched);
    let bands = vec![Filter { kind: FilterType::HighShelf, freq_hz: 12_000.0, gain_db: 8.0, q: 0.7 }];
    core.seed_slot(Slot::A, "DAC".into(), bands.clone(), 0.0, 0.0, Vec::new(), ResponseModel::Rbj).unwrap();
    let applied = core.activate_slot(Slot::A).unwrap();
    assert_eq!(applied.model, ResponseModel::AnalogMatched);
    assert_eq!(backend.last().model, ResponseModel::AnalogMatched);
    assert_eq!(applied.filters.len(), 1);
    assert_eq!((applied.filters[0].freq_hz, applied.filters[0].gain_db), (12_000.0, 8.0), "bands are kept");
    assert!(applied.g_max_peak_db > 7.0, "quantities recomputed (not the seeded 0.0): {}", applied.g_max_peak_db);
}

fn differ(a: &[Filter], b: &[Filter]) -> bool {
    a.iter().zip(b).any(|(x, y)| (x.gain_db - y.gain_db).abs() > 1e-3 || (x.freq_hz - y.freq_hz).abs() > 1e-3)
}

/// The export fit aims at the curve the slot's bands actually produce in the effective model.
#[test]
fn export_fits_the_effective_models_curve() {
    let (core, _backend) = start(true);
    let bands = [Filter { kind: FilterType::Peaking, freq_hz: 14_000.0, gain_db: 6.0, q: 1.0 }];
    let (rbj_export, _) = core.fit_export_eq(&bands, 5, ResponseModel::Rbj).unwrap();
    core.set_response_model(ResponseModel::AnalogMatched);
    let (matched_export, _) = core.fit_export_eq(&bands, 5, ResponseModel::Rbj).unwrap();
    assert!(differ(&rbj_export, &matched_export), "a different target curve must give a different export fit (and not a cache hit)");
}

/// The receiving app's filter design is a separate choice from CAGEq's own playback model: the
/// same curve, exported for an app that realises its bands warping-corrected, needs different
/// bands than for an RBJ app — and the export cache must not hand one the other's.
#[test]
fn the_export_band_model_is_independent_of_the_playback_model() {
    let (core, _backend) = start(true);
    // A treble bell: where the two designs differ most, and a curve the fixed-band (graphic) fit
    // handles (it currently returns all-zero gains for a lone shelf, in either design — a
    // separate, pre-existing issue).
    let bands = [Filter { kind: FilterType::Peaking, freq_hz: 12_000.0, gain_db: 6.0, q: 1.0 }];
    let (for_rbj_app, _) = core.fit_export_eq(&bands, 5, ResponseModel::Rbj).unwrap();
    let (for_matched_app, _) = core.fit_export_eq(&bands, 5, ResponseModel::AnalogMatched).unwrap();
    assert!(differ(&for_rbj_app, &for_matched_app), "different app designs must give different export bands");
    let (graphic_rbj, _) = core.fit_fixed_band_eq(&bands, "10", ResponseModel::Rbj).unwrap();
    let (graphic_matched, _) = core.fit_fixed_band_eq(&bands, "10", ResponseModel::AnalogMatched).unwrap();
    assert!(differ(&graphic_rbj, &graphic_matched), "the graphic-EQ export honours the app design too");
}

/// No self-cancelling band sets in either model: every fit's largest gain stays sane on seeded
/// synthetic headphones (offline), and no two bands oppose each other at double-digit gains
/// within an octave. The cold warping-corrected refit, and the fully converged RBJ fit of the
/// Sennheiser HD 800 S (`tests/fit_real_headphones.rs`), used to produce exactly that.
#[test]
fn fits_have_no_self_cancelling_band_pairs() {
    let mut seed: u64 = 0x5eed_cafe;
    let mut rnd = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    for case in 0..4 {
        let bumps: Vec<(f64, f64, f64)> =
            (0..5).map(|_| ((30f64.ln() + rnd() * (18_000f64 / 30.0).ln()).exp(), 0.08 + rnd() * 0.9, (rnd() - 0.5) * 10.0)).collect();
        let tilt = (rnd() - 0.5) * 6.0;
        let mut points = Vec::new();
        let mut f: f64 = 20.0;
        while f <= 20_000.0 {
            let lf = f.log2();
            let raw: f64 = bumps.iter().map(|(c, w, g)| g * (-((lf - c.log2()).powi(2)) / (2.0 * w * w)).exp()).sum::<f64>() + tilt * (f / 1000.0).log10();
            points.push(json!({"frequency": f, "raw_db": raw}));
            f *= 1.03;
        }
        let mut req = CalcRequest::for_device("DAC");
        req.inputs.insert("measurement".into(), serde_json::Value::Array(points));

        let (core, _backend) = start(true);
        let rbj = core.apply(req).unwrap();
        let matched = core.update_response_model(ResponseModel::AnalogMatched).unwrap().unwrap();
        for (name, fit) in [("rbj", &rbj.filters), ("matched", &matched.filters)] {
            for a in fit.iter() {
                for b in fit.iter() {
                    let opposed = a.gain_db > 10.0 && b.gain_db < -10.0 && (a.freq_hz / b.freq_hz).log2().abs() < 1.0;
                    assert!(!opposed, "case {case} {name}: self-cancelling pair {a:?} / {b:?}");
                }
            }
        }
    }
}
