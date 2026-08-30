//! CAGEq's own APO — the **Rust half**: everything that is not COM plumbing.
//!
//! ## Why this crate is split from a C++ shim
//! filter.md §5.3c moves CAGEq off Equalizer APO onto its own APO, so it owns the filter
//! lifecycle (state-carrying coefficient updates, arbitrary-length transitions, a live
//! control channel) instead of driving EqAPO's file-reload model from outside.
//!
//! An APO is a COM object loaded into `audiodg.exe`. An earlier spike hand-wrote the
//! `IAudioProcessingObject{,RT,Configuration}` vtables: it CoCreated fine but audiodg
//! silently discarded it during the load handshake. The thing that actually survives that
//! handshake is `CBaseAudioProcessingObject` from the Windows SDK — a C++ base class
//! implementing format negotiation, connection validation and buffer bookkeeping, and the
//! one Equalizer APO itself inherits. **Rust cannot inherit a C++ class**, so a minimal
//! C++ translation unit (`shim/cageq_apo.cpp`) does that and nothing else, forwarding
//! across the C ABI below. Everything with any judgement in it — the DSP, the control
//! channel, filter state — lives here.
//!
//! (Note for the record: that base class ships in the plain Windows **SDK**
//! (`Include/<ver>/um/baseaudioprocessingobject.h`, `Lib/<ver>/um/x64/
//! AudioBaseProcessingObjectV140.lib`), not the WDK as the spike's notes assumed. No
//! driver kit is needed to build this.)
//!
//! ## Real-time contract
//! [`cageq_apo_process`] runs on audiodg's real-time thread, inside the audio callback.
//! It must not allocate, lock, block, log, or panic — a stall here is a dropout for the
//! whole machine's audio, not just this app. Everything it touches is allocated in
//! [`cageq_apo_create`] and only read afterwards. `extern "C"` functions abort rather than
//! unwind across the FFI boundary, so a panic would take audiodg down: the code on this
//! path is written to have no panicking operations at all (no indexing that can go out of
//! bounds, no `unwrap`), rather than relying on a catch.
//!
//! ## Stage B scope
//! This is deliberately an **identity passthrough**. Stage B answers one question — does
//! our APO load into audiodg and pass audio? — and adding DSP before that is answered
//! would only make a failure harder to localise. The biquad cascade, preamp and shared
//! memory control channel land in stage C, against a synthetic ground-truth harness rather
//! than by ear inside audiodg.

pub mod dsp;

use std::ffi::c_void;

/// Per-instance state. One of these exists per APO instance (per endpoint, per mode),
/// created at `LockForProcess` and destroyed at `UnlockForProcess`.
///
/// Deliberately holds the format it was locked to: `APOProcess` is handed a frame count
/// but not a format, and the shim guarantees these stay in step by tearing the instance
/// down and rebuilding it whenever the connection format changes.
pub struct CageqApo {
    /// Samples per frame (2 for stereo). The engine is channel-count-agnostic; the value
    /// is kept for stage C's per-channel filter state and for validation here.
    channels: u32,
    /// Locked sample rate, in Hz. Stage C derives biquad coefficients from it.
    sample_rate: f32,
    /// Frames processed since creation. Not used for DSP — it is the liveness signal the
    /// stage-B bring-up reads to prove the RT callback is genuinely running (the spike's
    /// "heartbeat"), and the cheapest possible thing to compute on the RT path.
    frames_processed: u64,
}

/// Create an instance for a connection locked at `channels` × `sample_rate`.
///
/// Returns null on implausible parameters rather than trusting the caller: this runs
/// inside audiodg, and a bad format slipping through to the RT path is far worse than
/// refusing to lock. The shim maps null to a failed `LockForProcess`, which makes Windows
/// drop the APO cleanly instead of running it mis-configured.
///
/// # Safety
/// The returned pointer must be released exactly once with [`cageq_apo_destroy`] and not
/// used afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn cageq_apo_create(channels: u32, sample_rate: f32) -> *mut c_void {
    // Bounds are sanity limits, not policy: anything outside them means the shim and
    // audiodg disagree about the format, which is a bug, not a stream to try to play.
    if channels == 0 || channels > 32 || !(sample_rate.is_finite() && sample_rate > 0.0) {
        return std::ptr::null_mut();
    }
    let apo = Box::new(CageqApo { channels, sample_rate, frames_processed: 0 });
    Box::into_raw(apo) as *mut c_void
}

/// Destroy an instance from [`cageq_apo_create`]. Null is a no-op.
///
/// # Safety
/// `handle` must have come from [`cageq_apo_create`] and not been destroyed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut CageqApo) });
}

/// Process one buffer: `frames` × `channels` interleaved `f32` samples, `input` → `output`.
///
/// **Runs on audiodg's real-time thread** — see the module doc's real-time contract.
/// `input` and `output` may alias (the APO declares `APO_FLAG_INPLACE`, so Windows is
/// free to hand over the same buffer), which is why this is written as a forward-only
/// element-wise pass rather than anything that reads ahead.
///
/// # Safety
/// `handle` must be live; `input`/`output` must each address at least
/// `frames * channels` `f32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_process(
    handle: *mut c_void,
    input: *const f32,
    output: *mut f32,
    frames: u32,
) {
    if handle.is_null() || input.is_null() || output.is_null() {
        return;
    }
    let apo = unsafe { &mut *(handle as *mut CageqApo) };
    let count = (frames as usize).saturating_mul(apo.channels as usize);

    // Stage B: identity. Written as an explicit copy rather than skipped entirely so the
    // RT path, the pointer handling and the aliasing case are all genuinely exercised —
    // "it loads but we never touched the buffer" would prove much less. `copy` (memmove
    // semantics) rather than `copy_nonoverlapping` precisely because of APO_FLAG_INPLACE.
    unsafe { std::ptr::copy(input, output, count) };

    apo.frames_processed = apo.frames_processed.wrapping_add(frames as u64);
}

/// Frames processed since creation — the bring-up liveness probe (see
/// [`CageqApo::frames_processed`]). Zero for a null handle.
///
/// # Safety
/// `handle` must be live or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_frames_processed(handle: *mut c_void) -> u64 {
    if handle.is_null() {
        return 0;
    }
    unsafe { (*(handle as *mut CageqApo)).frames_processed }
}

/// The sample rate an instance was locked at — lets the shim assert it and Rust agree
/// about the format, which is the class of mismatch that silently breaks an APO.
///
/// # Safety
/// `handle` must be live or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_sample_rate(handle: *mut c_void) -> f32 {
    if handle.is_null() {
        return 0.0;
    }
    unsafe { (*(handle as *mut CageqApo)).sample_rate }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI contract, exercised the way the shim actually drives it.
    #[test]
    fn create_process_destroy_round_trip() {
        let h = cageq_apo_create(2, 48_000.0);
        assert!(!h.is_null());
        assert_eq!(unsafe { cageq_apo_sample_rate(h) }, 48_000.0);

        let input: Vec<f32> = (0..8).map(|i| i as f32 * 0.125).collect();
        let mut output = vec![0.0f32; 8];
        unsafe { cageq_apo_process(h, input.as_ptr(), output.as_mut_ptr(), 4) };
        assert_eq!(output, input, "stage B is an identity passthrough");
        assert_eq!(unsafe { cageq_apo_frames_processed(h) }, 4);

        unsafe { cageq_apo_destroy(h) };
    }

    /// Windows may hand the same buffer in and out (`APO_FLAG_INPLACE`), so the copy has
    /// to be memmove-safe rather than `copy_nonoverlapping`.
    #[test]
    fn processing_in_place_is_safe() {
        let h = cageq_apo_create(2, 44_100.0);
        let mut buf: Vec<f32> = vec![0.25, -0.5, 0.75, -1.0];
        let before = buf.clone();
        unsafe { cageq_apo_process(h, buf.as_ptr(), buf.as_mut_ptr(), 2) };
        assert_eq!(buf, before);
        unsafe { cageq_apo_destroy(h) };
    }

    /// A format the shim and audiodg disagree about must fail the lock, not reach the RT
    /// path. Null handles and pointers are no-ops everywhere for the same reason.
    #[test]
    fn implausible_formats_refuse_to_lock() {
        assert!(cageq_apo_create(0, 48_000.0).is_null());
        assert!(cageq_apo_create(64, 48_000.0).is_null());
        assert!(cageq_apo_create(2, 0.0).is_null());
        assert!(cageq_apo_create(2, f32::NAN).is_null());

        // Null-tolerant: nothing here may fault, whatever the shim hands over.
        unsafe {
            cageq_apo_process(std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut(), 128);
            cageq_apo_destroy(std::ptr::null_mut());
            assert_eq!(cageq_apo_frames_processed(std::ptr::null_mut()), 0);
            assert_eq!(cageq_apo_sample_rate(std::ptr::null_mut()), 0.0);
        }
    }
}
