//! The WebAssembly face of `cageq-biquad` — see this crate's `Cargo.toml` for why it exists.
//!
//! One export, [`biquad_design`]: a band and a model in, a pointer to five `f64`s out
//! (`b0, b1, b2, a1, a2`, `a0 = 1`, standard sign — `cageq_biquad::Coeffs`), which the caller
//! reads straight out of this module's linear memory. The result lives in a fixed buffer
//! rather than an allocation, so there is nothing to free and memory never grows (a grown
//! memory would detach the caller's views); each call overwrites the previous result, which
//! is safe because a WASM instance runs single-threaded.

use std::cell::UnsafeCell;

use cageq_biquad::{Band, Kind, ResponseModel};

struct Out(UnsafeCell<[f64; 5]>);
// SAFETY: wasm32-unknown-unknown without the atomics feature has exactly one thread.
unsafe impl Sync for Out {}
static OUT: Out = Out(UnsafeCell::new([0.0; 5]));

/// Design one band. `kind`: 0 peaking, 1 low shelf, 2 high shelf, 3 band-pass. `model`: 0 RBJ,
/// 1 analog-matched. Returns a pointer to `[b0, b1, b2, a1, a2]`, or null for a code it does
/// not know — refused rather than guessed at, like every other boundary in CAGEq.
#[unsafe(no_mangle)]
pub extern "C" fn biquad_design(kind: u32, freq_hz: f64, gain_db: f64, q: f64, fs: f64, model: u32) -> *const f64 {
    let kind = match kind {
        0 => Kind::Peaking,
        1 => Kind::LowShelf,
        2 => Kind::HighShelf,
        3 => Kind::Bandpass,
        _ => return std::ptr::null(),
    };
    let model = match model {
        0 => ResponseModel::Rbj,
        1 => ResponseModel::AnalogMatched,
        _ => return std::ptr::null(),
    };
    let c = cageq_biquad::design(&Band { kind, freq_hz, gain_db, q }, fs, model);
    let out = OUT.0.get();
    // SAFETY: single-threaded (see `Out`), and no reference to the buffer outlives this call.
    unsafe { *out = [c.b0, c.b1, c.b2, c.a1, c.a2] };
    out as *const f64
}
