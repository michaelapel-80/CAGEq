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
//! ## Layout
//! * [`dsp`] — the biquad cascade and preamp, verified against a ground-truth harness
//!   (impulse response → DFT vs. the analytic magnitude) rather than by ear inside audiodg.
//! * [`config`] — the persistent per-endpoint correction, loaded at lock time so the APO
//!   keeps working with CAGEq not running, exactly as EqualizerAPO does from `config.txt`.
//! * this module — the C ABI the shim drives: create/destroy, set bands and preamp, process.
//!
//! Still to come (stage C3b): the shared-memory control channel, which layers live edits on
//! top of the persistent config. It is about *edit latency*, not state-carry — the cascade
//! carries its delay registers across any coefficient change by construction, so even a
//! reload from disk is already free of Equalizer APO's cold-start bloom.

pub mod config;
pub mod dsp;

use std::ffi::c_void;

/// Per-instance state. One of these exists per APO instance (per endpoint, per mode),
/// created at `LockForProcess` and destroyed at `UnlockForProcess`.
///
/// Deliberately holds the format it was locked to: `APOProcess` is handed a frame count
/// but not a format, and the shim guarantees these stay in step by tearing the instance
/// down and rebuilding it whenever the connection format changes.
pub struct CageqApo {
    /// The filter engine (see [`dsp`]). Owns the coefficients, the per-channel delay
    /// registers and the preamp; all of its storage is allocated here, at lock time, so the
    /// real-time path never does.
    cascade: dsp::Cascade,
    /// Samples per frame (2 for stereo) — kept alongside the cascade because the RT path
    /// needs it to size each buffer, and reading it back through the cascade would be an
    /// indirection for nothing.
    channels: u32,
    /// Frames processed since creation. Not used for DSP — it is the liveness signal that
    /// proves the RT callback is genuinely running, which is what settled stage B after
    /// "it loads and audio still plays" turned out to be true of a completely dead APO.
    frames_processed: u64,
}

/// One filter band across the C ABI. Mirrors [`dsp::Band`] with `kind` as a plain integer,
/// so the C++ shim (and, from stage C3, the shared-memory control block) can describe a
/// correction without either side owning the other's type.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CageqBand {
    /// 0 = peaking, 1 = low shelf, 2 = high shelf, 3 = band-pass. Anything else is rejected
    /// rather than guessed at — see [`cageq_apo_set_bands`].
    pub kind: u32,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

impl CageqBand {
    fn to_band(self) -> Option<dsp::Band> {
        let kind = match self.kind {
            0 => dsp::FilterKind::Peaking,
            1 => dsp::FilterKind::LowShelf,
            2 => dsp::FilterKind::HighShelf,
            3 => dsp::FilterKind::Bandpass,
            _ => return None,
        };
        // A band with a non-finite or nonsensical parameter would produce NaN coefficients,
        // and NaN in a biquad's delay registers is permanent: it poisons every subsequent
        // sample until the state is reset. Refuse it at the boundary instead.
        if !(self.freq_hz.is_finite() && self.freq_hz > 0.0)
            || !self.gain_db.is_finite()
            || !(self.q.is_finite() && self.q > 0.0)
        {
            return None;
        }
        Some(dsp::Band { kind, freq_hz: self.freq_hz, gain_db: self.gain_db, q: self.q })
    }
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
    let apo = Box::new(CageqApo {
        cascade: dsp::Cascade::new(channels as usize, sample_rate as f64),
        channels,
        frames_processed: 0,
    });
    Box::into_raw(apo) as *mut c_void
}

/// Replace the filter set. Returns `false` and changes nothing if any band is malformed or
/// there are more than [`dsp::MAX_BANDS`] — a correction that silently lost or corrupted a
/// band would be worse than one that visibly failed to apply.
///
/// **Filter state is deliberately preserved**, which is the entire point of this APO: a
/// retuned band inherits its predecessor's delay registers, so a live edit produces a small
/// decaying discontinuity rather than Equalizer APO's cold-start bloom (filter.md §5.3c).
///
/// Computes coefficients (trig, per band), so it is not free. Call it from the
/// configuration path, not from inside the audio callback.
///
/// # Safety
/// `handle` must be live; `bands` must address at least `count` [`CageqBand`]s (or `count`
/// may be 0, in which case `bands` is ignored and the cascade becomes a passthrough).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_set_bands(
    handle: *mut c_void,
    bands: *const CageqBand,
    count: u32,
) -> bool {
    if handle.is_null() {
        return false;
    }
    let apo = unsafe { &mut *(handle as *mut CageqApo) };

    let n = count as usize;
    if n > dsp::MAX_BANDS {
        return false;
    }
    if n == 0 {
        return apo.cascade.set_bands(&[]);
    }
    if bands.is_null() {
        return false;
    }

    // Validate every band BEFORE touching the running cascade, so a bad one at the end
    // cannot leave a half-applied correction playing.
    let raw = unsafe { std::slice::from_raw_parts(bands, n) };
    let mut checked = [dsp::Band {
        kind: dsp::FilterKind::Peaking,
        freq_hz: 1000.0,
        gain_db: 0.0,
        q: 1.0,
    }; dsp::MAX_BANDS];
    for (slot, band) in checked.iter_mut().zip(raw) {
        match band.to_band() {
            Some(b) => *slot = b,
            None => return false,
        }
    }
    apo.cascade.set_bands(&checked[..n])
}

/// Outcome of [`cageq_apo_load_config`]. Distinguishes "nothing configured" from "something
/// configured but unusable", because they mean different things to whoever is listening: the
/// first is a device CAGEq has never been pointed at, the second is a correction that exists
/// and is being refused.
pub const CAGEQ_CONFIG_NONE: i32 = 0;
pub const CAGEQ_CONFIG_APPLIED: i32 = 1;
pub const CAGEQ_CONFIG_BAD_ARGS: i32 = -1;
/// The file exists but was rejected — malformed, out of bounds, oversized, or a reparse
/// point. The caller keeps whatever was already running.
pub const CAGEQ_CONFIG_REJECTED: i32 = -2;
/// Parsed and in range, but not applicable to *this* connection — in practice a band at or
/// above Nyquist for the rate we locked at (see `dsp::Cascade::set_bands`).
pub const CAGEQ_CONFIG_UNSUITABLE: i32 = -3;

/// Load and apply this endpoint's persistent configuration (see [`config`]).
///
/// The whole reason the APO can work with CAGEq not running. Called at lock time, off the
/// real-time thread. `endpoint_id` is UTF-16, as Windows hands it to the shim; conversion and
/// path construction happen here so every rule about what that string may contain lives in
/// one place with the code that enforces it.
///
/// Deliberately returns a *code*, never a message: the file is written by a lower-privileged
/// account and read by a service one, so nothing derived from its contents may flow back out
/// (see [`config::load_from`]).
///
/// # Safety
/// `handle` must be live; `endpoint_id` must address at least `len` UTF-16 code units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_load_config(
    handle: *mut c_void,
    endpoint_id: *const u16,
    len: u32,
) -> i32 {
    if handle.is_null() || endpoint_id.is_null() || len == 0 || len > 256 {
        return CAGEQ_CONFIG_BAD_ARGS;
    }
    let apo = unsafe { &mut *(handle as *mut CageqApo) };
    let wide = unsafe { std::slice::from_raw_parts(endpoint_id, len as usize) };
    // `from_utf16_lossy`, not a fallible decode: an unpaired surrogate should fail the
    // *id validation* below (it cannot be a GUID), not produce a distinct error path.
    let id = String::from_utf16_lossy(wide);

    match config::load(&id) {
        Ok(None) => CAGEQ_CONFIG_NONE,
        Ok(Some(cfg)) => {
            // Bands first: if they are refused the preamp must not be applied either, or the
            // endpoint would run at a level chosen to compensate for filters that aren't there.
            if !apo.cascade.set_bands(&cfg.bands) {
                return CAGEQ_CONFIG_UNSUITABLE;
            }
            apo.cascade.set_preamp_db(cfg.preamp_db);
            CAGEQ_CONFIG_APPLIED
        }
        Err(_) => CAGEQ_CONFIG_REJECTED,
    }
}

/// Set the preamp in dB (negative attenuates), as EqAPO's `Preamp:` line does. A
/// non-finite value is ignored rather than turning the whole stream into NaN.
///
/// # Safety
/// `handle` must be live or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cageq_apo_set_preamp_db(handle: *mut c_void, db: f64) -> bool {
    if handle.is_null() || !db.is_finite() {
        return false;
    }
    unsafe { (*(handle as *mut CageqApo)).cascade.set_preamp_db(db) };
    true
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

    // `input` and `output` may be the same buffer (APO_FLAG_INPLACE), so these are built as
    // separate slices over possibly-identical memory. That would be aliasing UB for two
    // Rust references, which is why `process` takes `&[f32]`/`&mut [f32]` that are only
    // ever indexed at the same position — each sample is read before its own slot is
    // written, and nothing looks ahead. Constructing them here (rather than passing raw
    // pointers down) keeps every bounds-relevant fact in one place.
    let src = unsafe { std::slice::from_raw_parts(input, count) };
    let dst = unsafe { std::slice::from_raw_parts_mut(output, count) };
    apo.cascade.process(src, dst, frames as usize);

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
    unsafe { (*(handle as *mut CageqApo)).cascade.sample_rate() as f32 }
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

        // No bands and no preamp yet: still exact identity, which matters because it is the
        // state the APO holds between locking and CAGEq pushing a correction.
        let input: Vec<f32> = (0..8).map(|i| i as f32 * 0.125).collect();
        let mut output = vec![0.0f32; 8];
        unsafe { cageq_apo_process(h, input.as_ptr(), output.as_mut_ptr(), 4) };
        assert_eq!(output, input, "an unconfigured cascade must pass through bit-exact");
        assert_eq!(unsafe { cageq_apo_frames_processed(h) }, 4);

        unsafe { cageq_apo_destroy(h) };
    }

    /// The configuration surface the C++ shim (and, from C3, the control channel) drives.
    #[test]
    fn bands_and_preamp_reach_the_engine() {
        let h = cageq_apo_create(2, 48_000.0);
        let bands = [
            CageqBand { kind: 0, freq_hz: 1000.0, gain_db: 6.0, q: 1.4 },
            CageqBand { kind: 1, freq_hz: 105.0, gain_db: -3.0, q: 0.7 },
        ];
        assert!(unsafe { cageq_apo_set_bands(h, bands.as_ptr(), 2) });
        assert!(unsafe { cageq_apo_set_preamp_db(h, -6.0) });

        // A signal now actually changes, i.e. the cascade is in the path.
        let input: Vec<f32> = (0..64).map(|i| ((i as f64 * 0.2).sin() * 0.5) as f32).collect();
        let mut output = vec![0.0f32; 64];
        unsafe { cageq_apo_process(h, input.as_ptr(), output.as_mut_ptr(), 32) };
        assert!(output.iter().zip(&input).any(|(o, i)| (o - i).abs() > 1e-6), "EQ had no effect");

        // Clearing the bands returns it to a passthrough (preamp still applies).
        assert!(unsafe { cageq_apo_set_bands(h, std::ptr::null(), 0) });
        unsafe { cageq_apo_destroy(h) };
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// The load path's argument handling. The file-level behaviour is covered in
    /// `config::tests`; what matters here is that nothing malformed reaches it and that the
    /// distinct outcomes stay distinguishable.
    #[test]
    fn loading_config_reports_distinct_outcomes() {
        let h = cageq_apo_create(2, 48_000.0);

        // An endpoint nobody has configured: "nothing here", not a failure.
        let unconfigured = utf16("{00000000-0000-0000-0000-00000000dead}");
        assert_eq!(
            unsafe { cageq_apo_load_config(h, unconfigured.as_ptr(), unconfigured.len() as u32) },
            CAGEQ_CONFIG_NONE,
        );

        // Anything that could escape the config directory is refused before it becomes a path.
        for bad in ["../escape", r"..\escape", "a/b"] {
            let w = utf16(bad);
            assert_eq!(
                unsafe { cageq_apo_load_config(h, w.as_ptr(), w.len() as u32) },
                CAGEQ_CONFIG_REJECTED,
                "accepted a traversal-shaped id: {bad}",
            );
        }

        // Null / empty / absurd length are argument errors, not file errors.
        assert_eq!(unsafe { cageq_apo_load_config(h, std::ptr::null(), 4) }, CAGEQ_CONFIG_BAD_ARGS);
        assert_eq!(
            unsafe { cageq_apo_load_config(h, unconfigured.as_ptr(), 0) },
            CAGEQ_CONFIG_BAD_ARGS,
        );
        assert_eq!(
            unsafe { cageq_apo_load_config(h, unconfigured.as_ptr(), 9999) },
            CAGEQ_CONFIG_BAD_ARGS,
        );
        assert_eq!(
            unsafe { cageq_apo_load_config(std::ptr::null_mut(), unconfigured.as_ptr(), 4) },
            CAGEQ_CONFIG_BAD_ARGS,
        );

        unsafe { cageq_apo_destroy(h) };
    }

    /// Malformed input must be refused at the boundary, leaving the running correction
    /// alone. NaN matters more than it looks: a NaN in a biquad's delay registers is
    /// permanent — it poisons every later sample until the state is reset.
    #[test]
    fn malformed_bands_are_refused_whole() {
        let h = cageq_apo_create(2, 48_000.0);
        let good = CageqBand { kind: 0, freq_hz: 1000.0, gain_db: 6.0, q: 1.4 };
        assert!(unsafe { cageq_apo_set_bands(h, &good, 1) });

        for bad in [
            CageqBand { kind: 9, freq_hz: 1000.0, gain_db: 0.0, q: 1.0 },       // unknown kind
            CageqBand { kind: 0, freq_hz: f64::NAN, gain_db: 0.0, q: 1.0 },     // NaN frequency
            CageqBand { kind: 0, freq_hz: 0.0, gain_db: 0.0, q: 1.0 },          // DC
            CageqBand { kind: 0, freq_hz: 1000.0, gain_db: f64::INFINITY, q: 1.0 },
            CageqBand { kind: 0, freq_hz: 1000.0, gain_db: 0.0, q: 0.0 },       // zero Q
        ] {
            // Second in the list, so a naive implementation would already have applied the
            // first before noticing.
            let pair = [good, bad];
            assert!(!unsafe { cageq_apo_set_bands(h, pair.as_ptr(), 2) }, "accepted {bad:?}");
        }

        assert!(!unsafe { cageq_apo_set_preamp_db(h, f64::NAN) });
        assert!(!unsafe { cageq_apo_set_bands(h, std::ptr::null(), 3) }, "null with count>0");

        // Still exactly the one good band, and still producing finite audio.
        let input = vec![0.25f32; 32];
        let mut output = vec![0.0f32; 32];
        unsafe { cageq_apo_process(h, input.as_ptr(), output.as_mut_ptr(), 16) };
        assert!(output.iter().all(|v| v.is_finite()), "engine was poisoned by a rejected band");
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
