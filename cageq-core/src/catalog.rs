//! Replaces the sidecar's `list_headphones`/`list_targets`/`measurement_curves` RPCs —
//! thin wrappers over [`cageq_catalog`]'s index build and curve fetch, plus (for
//! `measurement_curves`) the same interpolate/center/subsample sequence `fit.rs`'s main
//! path already uses, just applied to a raw measurement/target for the §5.2 nerd
//! overlays rather than feeding an optimizer.

pub use cageq_catalog::index::{HeadphoneEntry, TargetEntry};
use cageq_peq_solver::grid::{center_diff, interpolate, standard_grid};

use crate::fit::subsample_curve;
use crate::{CoreError, CurvePoint};

/// `list_headphones` (`sidecar_dsp.py:155-195`, via `cageq_catalog::index::build_index`).
pub(crate) fn list_headphones(refresh: bool) -> Result<Vec<HeadphoneEntry>, CoreError> {
    Ok(cageq_catalog::index::build_index(refresh)?)
}

/// `list_targets` (`sidecar_dsp.py:198-213`, via `cageq_catalog::index::list_targets`).
pub(crate) fn list_targets(refresh: bool) -> Result<Vec<TargetEntry>, CoreError> {
    Ok(cageq_catalog::index::list_targets(refresh)?)
}

/// `measurement_curves` (`sidecar_dsp.py:443-459`): the raw headphone measurement and
/// (if given) the named target, both interpolated onto the standard grid and centred
/// the same way so they share one relative-dB reference — UI-only overlays, never
/// written to EqAPO.
pub(crate) fn measurement_curves(headphone: &str, target: Option<&str>) -> Result<(Vec<CurvePoint>, Vec<CurvePoint>), CoreError> {
    let f = standard_grid();

    let (mf, mr) = cageq_catalog::fetch_curve(headphone)?;
    let mut raw = interpolate(&mf, &mr, &f);
    let diff = center_diff(&f, &raw);
    raw.iter_mut().for_each(|v| *v -= diff);
    let raw_curve = subsample_curve(&f, &raw, 140);

    let target_curve = match target {
        Some(t) => {
            let (tf, tr) = cageq_catalog::fetch_curve(t)?;
            let mut tgt = interpolate(&tf, &tr, &f);
            let tdiff = center_diff(&f, &tgt);
            tgt.iter_mut().for_each(|v| *v -= tdiff);
            subsample_curve(&f, &tgt, 140)
        }
        None => Vec::new(),
    };

    Ok((raw_curve, target_curve))
}
