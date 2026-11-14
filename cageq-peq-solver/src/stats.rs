//! The small numeric helpers the solver's modules share, in one place: each used to keep its own
//! copy, and a fix to one missed the others.

/// Index of the smallest value. `total_cmp`, so a NaN can't panic it (it sorts last).
pub(crate) fn argmin(xs: &[f64]) -> usize {
    xs.iter().enumerate().min_by(|(_, a), (_, b)| a.total_cmp(b)).map(|(i, _)| i).unwrap()
}

/// Index of the value closest to `target`.
pub(crate) fn argmin_abs(xs: &[f64], target: f64) -> usize {
    xs.iter().enumerate().min_by(|(_, a), (_, b)| (*a - target).abs().total_cmp(&(*b - target).abs())).map(|(i, _)| i).unwrap()
}

pub(crate) fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}
