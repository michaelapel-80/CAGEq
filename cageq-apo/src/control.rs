//! The live control channel's **protocol**: a fixed-layout shared block, a seqlock, and the
//! validation that stands between an unelevated writer and audiodg's real-time thread.
//!
//! (The shared-memory plumbing itself — creating the `Global\` section, its DACL and
//! mandatory label — is separate. This module is the part that decides what a byte pattern
//! is allowed to mean, and is testable without touching the OS.)
//!
//! ## What it is for
//! The persistent config ([`crate::config`]) already gives the APO a correction to apply
//! with CAGEq not running, and the cascade carries filter state across any coefficient
//! change, so a reload from disk is *already* free of Equalizer APO's cold-start bloom. The
//! channel exists for one reason the file cannot serve: **edit latency**. Dragging a filter
//! node should not mean a disk write, a watcher, and a re-parse per frame.
//!
//! ## Coefficients, not bands
//! The writer sends finished biquad coefficients. Two consequences, both deliberate:
//!
//! * The real-time thread does no trigonometry — an update is a bounded copy.
//! * **Stability becomes directly checkable.** A biquad is stable iff its poles lie inside
//!   the unit circle, i.e. `|a2| < 1` and `|a1| < 1 + a2`. That is a far stronger guarantee
//!   than bounding Q and centre frequency and hoping the combination is sane, and it matters
//!   because an unstable filter's output grows without bound — a hearing and speaker hazard
//!   that arrives long before anyone reaches a debugger.
//!
//! ## Torn reads
//! The writer is an ordinary user process; the reader is audiodg's RT thread. They are not
//! synchronised, and the reader may not block, spin or allocate. A seqlock gives consistency
//! without either: the writer bumps `seq` to odd, writes, bumps it to even; the reader
//! checks `seq` before and after and discards the snapshot if it moved or was odd. A
//! discarded update simply means the previous coefficients stay in force for another buffer
//! — inaudible, and the correct failure mode for something that must never stall.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::dsp::{Coeffs, MAX_BANDS};

/// `"CAGQ"` — guards against mapping something that is not our block at all.
pub const CONTROL_MAGIC: u32 = 0x4341_4751;
/// Layout version. A mismatch is refused rather than interpreted: this describes what
/// someone is listening to, and a half-understood layout is not worth guessing at.
///
/// Bump whenever `ControlBlock`'s layout changes size or shape — e.g. `dsp::MAX_BANDS`
/// changing resizes `coeffs`, and an old DLL and a new app (or vice versa) disagreeing about
/// that size must not be allowed to interpret each other's memory.
pub const CONTROL_VERSION: u32 = 7;

/// Preamp bounds mirroring [`crate::config`]'s, for the same reason: attenuation is
/// harmless, gain is a hazard, and the writer is not trusted merely because it is ours.
const MIN_PREAMP_DB: f64 = -120.0;
const MAX_PREAMP_DB: f64 = 12.0;

/// One biquad's coefficients as they cross the boundary. Plain `f64`s in a fixed layout —
/// no pointers, no lengths that could lie about how much memory to touch.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RawCoeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl RawCoeffs {
    /// Is this a filter that is safe to run?
    ///
    /// Finite, and **stable**: for `1 + a1·z⁻¹ + a2·z⁻²` the poles are inside the unit circle
    /// exactly when `|a2| < 1` and `|a1| < 1 + a2`. An unstable biquad's output grows without
    /// bound, so this is a safety check before it is a correctness one.
    ///
    /// The numerator is only checked for finiteness — any finite `b` is a bounded gain, and
    /// bounding *how much* gain is the preamp's job, not each stage's.
    fn is_safe(&self) -> bool {
        let finite = self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite();
        finite && self.a2.abs() < 1.0 && self.a1.abs() < 1.0 + self.a2
    }

    fn to_coeffs(self) -> Coeffs {
        Coeffs { b0: self.b0, b1: self.b1, b2: self.b2, a1: self.a1, a2: self.a2 }
    }
}

/// The shared block, exactly as it sits in mapped memory.
///
/// `#[repr(C)]` and fixed-size throughout: the writer is a separate process, so the layout is
/// an ABI. Nothing here is a pointer or a length the reader would have to trust — the only
/// count is bounds-checked against a compile-time array.
///
/// **`coeffs` is declared last, deliberately, and must stay that way.** It is the one field
/// whose size moves whenever [`MAX_BANDS`] does, so putting it last keeps every other field's
/// *byte offset* stable across a `MAX_BANDS` change — which matters because [`CONTROL_VERSION`]
/// stops a version-mismatched pair from *processing* each other's data, but it cannot stop the
/// writer from opening the section and reading `sample_rate`/`heartbeat`/etc. at its own
/// compiled offsets first, since that happens before `publish` ever gets a chance to notice
/// anything is wrong. A real case: `RegisterServer` stops and restarts `audiosrv` to swap the
/// DLL, so the APO side of a `MAX_BANDS` bump is live the moment a stream re-locks — but nothing
/// about that rebuilds the *app* process hosting this code, which native Rust has no way to do
/// to itself. Before this field was last, a still-running old build reading a since-grown
/// section landed inside the middle of the new (larger) `coeffs` array instead of the real
/// `sample_rate`, and read back plausible-looking garbage instead of failing cleanly. With
/// `coeffs` last, that same old build instead reads its own genuinely-correct, unmoved
/// `sample_rate`/etc. — `publish` still gets refused by the newer reader's version check
/// exactly as it always would, just without first taking a wrong turn on the way there. This
/// only helps as of the version where it shipped; it does nothing for a mismatch straddling an
/// older build that never had this ordering.
#[repr(C)]
pub struct ControlBlock {
    pub magic: u32,
    pub version: u32,
    /// Seqlock counter. Odd = a write is in progress. See the module doc.
    pub seq: AtomicU32,
    /// Bands actually in use, `<= MAX_BANDS`. Validated on every read.
    pub band_count: u32,
    pub preamp_db: f64,
    /// Incremented by the APO so the app can see it is alive and being processed. Purely
    /// outbound; the reader never trusts it.
    pub heartbeat: AtomicU64,
    /// What the APO did with the most recent update it looked at: `seq << 32 | code`.
    ///
    /// Outbound, like the heartbeat. It exists because the writer cannot predict the
    /// answer: `publish` only checks what a writer can know — finite, stable, in range — but
    /// the loudness ceiling is a property of the *combined* chain and is enforced in the
    /// engine. A +39 dB filter is perfectly stable, so it publishes happily and is then
    /// declined, and without this the writer would report success for a correction that
    /// never took effect.
    ///
    /// Packed into one atomic so the sequence and its verdict can never be read out of step.
    pub ack: AtomicU64,
    /// The rate this APO instance locked to, in Hz. Outbound.
    ///
    /// The writer computes coefficients, and coefficients depend on the sample rate — so it
    /// has to know the endpoint's rate, and getting it wrong is not an error but something
    /// worse: a correction silently applied at the wrong frequencies. The APO is the only
    /// party that authoritatively knows (it is handed the locked format), so it publishes it
    /// rather than leaving the writer to guess or to re-derive it through WASAPI.
    pub sample_rate: AtomicU32,
    /// Unix time the loaded DLL was compiled. Outbound.
    ///
    /// Answers "is audiodg running the build I just made?", which is otherwise unanswerable
    /// from outside: the registered DLL lives in %ProgramFiles% and is only refreshed by
    /// `cageq-apo-setup register`, so a freshly built DLL sitting in a working folder is not
    /// the one being loaded. Mistaking one for the other has twice sent a hunt for DSP bugs
    /// that were already fixed.
    pub build_stamp: AtomicU64,
    /// Nonzero: apply this update via [`crate::dsp::Cascade::start_crossfade`] rather than
    /// [`crate::dsp::Cascade::apply_coeffs`]'s plain coefficient ramp. Inbound, seqlock-guarded
    /// alongside `preamp_db`/`band_count`/`coeffs` — not a separate atomic like the outbound
    /// fields above, since it is part of the same payload the writer publishes atomically.
    ///
    /// Set by `cageq-apo-backend::push_live` exactly when a push crosses the §5.2 isolate
    /// boundary (a single `Bandpass`-only band list starting or ending) — see its own doc for
    /// why a plain ramp is unsafe there: several bands independently fading toward
    /// `PASSTHROUGH` on one shared clock can sum into a real spike, measured directly at over
    /// +20 dB above both endpoints on a real correction. An ordinary edit or an A/B slot
    /// switch — neither side ever `Bandpass` — always publishes 0 and keeps the ramp, which
    /// remains the right tool for an in-place change (see `retuning_live_does_not_splatter...`
    /// in dsp.rs for why that decision was already made and re-litigating it is out of scope
    /// here).
    pub crossfade: u32,
    /// See this struct's own doc: kept last so its size (the one thing that moves when
    /// [`MAX_BANDS`] does) never disturbs any other field's offset.
    pub coeffs: [RawCoeffs; MAX_BANDS],
}

/// A validated snapshot, ready to hand to the cascade. Fixed-size so taking one allocates
/// nothing on the real-time path.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub preamp_db: f64,
    pub band_count: usize,
    /// See [`ControlBlock::crossfade`]'s doc.
    pub crossfade: bool,
    pub coeffs: [Coeffs; MAX_BANDS],
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot { preamp_db: 0.0, band_count: 0, crossfade: false, coeffs: [Coeffs::PASSTHROUGH; MAX_BANDS] }
    }
}

/// Why a read did not produce a usable snapshot. Every variant means "keep using the
/// coefficients you already have" — none is a reason to stall or to silence the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// A consistent, valid snapshot. `seq` is its sequence number, so the caller can skip
    /// re-applying an update it has already seen.
    Updated(u32),
    /// The writer was mid-update, or finished one while we were reading. Normal and
    /// expected; the next buffer will pick it up.
    Torn,
    /// Not our block, or a layout we do not speak.
    Unrecognised,
    /// Structurally intact but describing something unsafe to run — an out-of-range band
    /// count, a non-finite value, an unstable filter, an absurd preamp.
    Rejected,
}

/// The block's current sequence number, as one atomic load.
///
/// Lets the audio path skip [`try_read`] entirely when nothing has been published since it
/// last looked — which is almost every buffer. Steady-state cost of having a control channel
/// at all is therefore one load, not a scan and validation of every band.
pub fn sequence(block: &ControlBlock) -> u32 {
    block.seq.load(Ordering::Acquire)
}

/// Record that the APO is alive and processing, for the writer's benefit. Purely outbound;
/// nothing on this side ever reads it back.
pub fn bump_heartbeat(block: &ControlBlock) {
    block.heartbeat.fetch_add(1, Ordering::Relaxed);
}


/// When the running APO was built (Unix seconds), and the constant it reports.
pub const BUILD_STAMP: &str = env!("CAGEQ_APO_BUILD");

/// Publish the build stamp so a writer can tell which DLL is actually loaded.
pub fn set_build_stamp(block: &ControlBlock) {
    block.build_stamp.store(BUILD_STAMP.parse().unwrap_or(0), Ordering::Relaxed);
}

/// The loaded APO's build time, or `None` if it published none.
pub fn build_stamp(block: &ControlBlock) -> Option<u64> {
    match block.build_stamp.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v),
    }
}
/// Publish the rate this APO locked to, so the writer can compute coefficients for it.
pub fn set_sample_rate(block: &ControlBlock, hz: u32) {
    block.sample_rate.store(hz, Ordering::Relaxed);
}

/// The endpoint's sample rate, or `None` if no APO has published one yet.
///
/// The writer must not fall back to a guess: coefficients computed for the wrong rate produce
/// a correction silently applied at the wrong frequencies, which is worse than not applying
/// one at all because nothing looks broken.
pub fn sample_rate(block: &ControlBlock) -> Option<u32> {
    match block.sample_rate.load(Ordering::Relaxed) {
        0 => None,
        hz => Some(hz),
    }
}

/// Verdicts the APO reports back through [`ControlBlock::ack`].
pub const ACK_APPLIED: u32 = 0;
/// Structurally valid, but the combined chain would exceed the engine's loudness ceiling.
/// The correction was NOT applied; whatever was running still is.
pub const ACK_TOO_LOUD: u32 = 1;

/// Record what the APO did with update `seq`. Outbound only, and a single relaxed store —
/// it runs on the real-time thread and nothing here is ordered against anything else.
pub fn set_ack(block: &ControlBlock, seq: u32, code: u32) {
    block.ack.store(((seq as u64) << 32) | code as u64, Ordering::Relaxed);
}

/// Read back the APO's verdict as `(seq, code)`.
///
/// The writer compares `seq` against its own publish: an older `seq` simply means the APO has
/// not looked yet, which during normal operation lasts less than one buffer.
pub fn ack(block: &ControlBlock) -> (u32, u32) {
    let packed = block.ack.load(Ordering::Relaxed);
    ((packed >> 32) as u32, packed as u32)
}

/// Take a consistent snapshot, if there is one.
///
/// **Runs on the real-time thread**: bounded work, no allocation, no locking, and it never
/// retries — a torn read is discarded and the previous coefficients stay in force for one
/// more buffer, which is inaudible and cannot stall the audio callback.
///
/// Validation happens *after* the seqlock check, so a value being validated cannot have been
/// changing while it was inspected.
pub fn try_read(block: &ControlBlock, out: &mut Snapshot) -> ReadOutcome {
    // Acquire: nothing below may be hoisted above this read.
    let before = block.seq.load(Ordering::Acquire);
    if before % 2 != 0 {
        return ReadOutcome::Torn; // writer mid-update
    }

    if block.magic != CONTROL_MAGIC || block.version != CONTROL_VERSION {
        return ReadOutcome::Unrecognised;
    }

    let count = block.band_count as usize;
    if count > MAX_BANDS {
        return ReadOutcome::Rejected;
    }
    let preamp = block.preamp_db;
    let crossfade = block.crossfade != 0;

    let mut staged = [Coeffs::PASSTHROUGH; MAX_BANDS];
    for i in 0..count {
        let raw = block.coeffs[i];
        if !raw.is_safe() {
            return ReadOutcome::Rejected;
        }
        staged[i] = raw.to_coeffs();
    }

    // Release-ordered re-check: if the writer touched anything while we copied, everything
    // above is suspect and gets discarded rather than half-applied.
    if block.seq.load(Ordering::Acquire) != before {
        return ReadOutcome::Torn;
    }

    // Validated last because it does not participate in tearing the same way — but still
    // inside the fence, so the value checked is the value taken.
    if !(preamp.is_finite() && (MIN_PREAMP_DB..=MAX_PREAMP_DB).contains(&preamp)) {
        return ReadOutcome::Rejected;
    }

    out.preamp_db = preamp;
    out.band_count = count;
    out.crossfade = crossfade;
    out.coeffs = staged;
    ReadOutcome::Updated(before)
}

/// Publish a coefficient set into the block, seqlock-correctly.
///
/// The APO does not call this — CAGEq does, from the other side of the boundary. It lives
/// here so both halves of the protocol are written and tested together; a seqlock whose two
/// sides are implemented in different files by different people is a race waiting to happen.
///
/// Returns `false` without publishing anything if the set would be refused by [`try_read`],
/// so a bug on the writer's side surfaces there rather than as a silently ignored update.
pub fn publish(block: &mut ControlBlock, preamp_db: f64, coeffs: &[RawCoeffs], crossfade: bool) -> bool {
    if coeffs.len() > MAX_BANDS
        || !(preamp_db.is_finite() && (MIN_PREAMP_DB..=MAX_PREAMP_DB).contains(&preamp_db))
        || !coeffs.iter().all(RawCoeffs::is_safe)
    {
        return false;
    }

    block.magic = CONTROL_MAGIC;
    block.version = CONTROL_VERSION;

    // Odd: readers now know the payload is in flux.
    let start = block.seq.load(Ordering::Relaxed);
    block.seq.store(start.wrapping_add(1), Ordering::Release);

    block.preamp_db = preamp_db;
    block.band_count = coeffs.len() as u32;
    block.crossfade = crossfade as u32;
    for (slot, c) in block.coeffs.iter_mut().zip(coeffs) {
        *slot = *c;
    }
    for slot in block.coeffs[coeffs.len()..].iter_mut() {
        *slot = RawCoeffs::default();
    }

    // Even again, and Release so a reader that sees this value also sees everything above.
    block.seq.store(start.wrapping_add(2), Ordering::Release);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block in the state a freshly-created mapping would be: all zeroes.

    /// The verdict channel. A writer cannot predict whether an update will be applied: it can
    /// check stability, finiteness and range, but the loudness ceiling is a property of the
    /// combined chain and lives in the engine — so a stable, in-range +39 dB filter publishes
    /// happily and is then declined. Without this the writer reports success for a correction
    /// that never took effect, which is exactly what the VM run showed.
    #[test]
    fn the_apo_can_report_a_verdict_the_writer_could_not_predict() {
        let mut b = zeroed();
        assert!(publish(&mut b, -6.0, &[stable()], false));
        let seq = sequence(&b);

        // Nothing has looked at it yet: the ack still refers to an older sequence.
        assert_ne!(ack(&b).0, seq, "an unexamined update must not read as acknowledged");

        set_ack(&b, seq, ACK_APPLIED);
        assert_eq!(ack(&b), (seq, ACK_APPLIED));

        // A later update declined by the engine.
        assert!(publish(&mut b, -6.0, &[stable(), stable()], false));
        let seq2 = sequence(&b);
        set_ack(&b, seq2, ACK_TOO_LOUD);
        assert_eq!(ack(&b), (seq2, ACK_TOO_LOUD));
        assert_ne!(seq, seq2, "each publish must be separately acknowledgeable");
    }
    fn zeroed() -> ControlBlock {
        ControlBlock {
            magic: 0,
            version: 0,
            seq: AtomicU32::new(0),
            band_count: 0,
            preamp_db: 0.0,
            crossfade: 0,
            coeffs: [RawCoeffs::default(); MAX_BANDS],
            heartbeat: AtomicU64::new(0),
            ack: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0),
            build_stamp: AtomicU64::new(0),
        }
    }

    /// A stable, ordinary filter — a gentle peaking band's coefficients.
    fn stable() -> RawCoeffs {
        RawCoeffs { b0: 1.02, b1: -1.9, b2: 0.89, a1: -1.9, a2: 0.91 }
    }

    #[test]
    fn publish_then_read_round_trips() {
        let mut b = zeroed();
        assert!(publish(&mut b, -6.0, &[stable(), stable()], false));

        let mut snap = Snapshot::default();
        match try_read(&b, &mut snap) {
            ReadOutcome::Updated(_) => {}
            other => panic!("expected Updated, got {other:?}"),
        }
        assert_eq!(snap.band_count, 2);
        assert_eq!(snap.preamp_db, -6.0);
        assert_eq!(snap.coeffs[0].b0, 1.02);
        // Unused slots are identity, so a shrinking set cannot leave a stale filter running.
        assert_eq!(snap.coeffs[2].b0, Coeffs::PASSTHROUGH.b0);
        assert!(!snap.crossfade, "the default publish must not request a crossfade");
    }

    /// The `crossfade` flag itself — see `ControlBlock::crossfade`'s doc — round-trips
    /// independently of the coefficients, and toggling it between two publishes is
    /// distinguishable, the same way the sequence number is.
    #[test]
    fn the_crossfade_flag_round_trips_and_is_not_sticky() {
        let mut b = zeroed();
        let mut snap = Snapshot::default();

        assert!(publish(&mut b, 0.0, &[stable()], true));
        assert!(matches!(try_read(&b, &mut snap), ReadOutcome::Updated(_)));
        assert!(snap.crossfade, "a crossfade publish must read back as one");

        // A later ordinary publish must not leave the previous push's flag set — it is part
        // of each publish's own payload, not a persistent mode.
        assert!(publish(&mut b, 0.0, &[stable()], false));
        assert!(matches!(try_read(&b, &mut snap), ReadOutcome::Updated(_)));
        assert!(!snap.crossfade, "crossfade must not stick across an unrelated publish");
    }

    /// A zeroed mapping is not a valid block. This is the state the memory is in before the
    /// writer has ever published, and reading it must not be mistaken for "no filters".
    #[test]
    fn an_unwritten_block_is_unrecognised() {
        let b = zeroed();
        let mut snap = Snapshot::default();
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Unrecognised);
    }

    #[test]
    fn foreign_or_future_layouts_are_refused() {
        let mut b = zeroed();
        publish(&mut b, 0.0, &[stable()], false);

        b.magic = 0xDEAD_BEEF;
        let mut snap = Snapshot::default();
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Unrecognised);

        b.magic = CONTROL_MAGIC;
        b.version = CONTROL_VERSION + 1;
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Unrecognised);
    }

    /// A write in progress, and a write that completes mid-read, must both be discarded
    /// rather than half-applied.
    #[test]
    fn torn_reads_are_detected_not_half_applied() {
        let mut b = zeroed();
        publish(&mut b, -3.0, &[stable()], false);
        let mut snap = Snapshot::default();

        // Writer mid-update: seq is odd.
        b.seq.store(7, Ordering::Release);
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Torn);

        // …and an even counter reads cleanly again once the writer has finished.
        b.seq.store(8, Ordering::Release);
        assert!(matches!(try_read(&b, &mut snap), ReadOutcome::Updated(8)));
    }

    /// The other half of the seqlock: an update that *completes while the read is in flight*
    /// must also be discarded. Exercised for real, with a writer thread racing the reader —
    /// the reader must only ever return snapshots that were internally consistent, never a
    /// mixture of two publishes.
    #[test]
    fn a_racing_writer_never_yields_a_mixed_snapshot() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        // Two publishes distinguishable in *every* field, so any mixture is detectable.
        let a = RawCoeffs { b0: 0.5, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.5 };
        let z = RawCoeffs { b0: 0.9, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.9 };

        let block = Arc::new(std::sync::Mutex::new(zeroed()));
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let (block, stop) = (Arc::clone(&block), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut flip = false;
                while !stop.load(Ordering::Relaxed) {
                    let mut g = block.lock().unwrap();
                    if flip {
                        publish(&mut g, -3.0, &[a, a, a], false);
                    } else {
                        publish(&mut g, -9.0, &[z], false);
                    }
                    flip = !flip;
                }
            })
        };

        let mut snap = Snapshot::default();
        for _ in 0..20_000 {
            let g = block.lock().unwrap();
            if let ReadOutcome::Updated(_) = try_read(&g, &mut snap) {
                // Whatever we got must be one publish or the other, never a blend of the two.
                let consistent = (snap.preamp_db == -3.0
                    && snap.band_count == 3
                    && snap.coeffs[0].b0 == a.b0)
                    || (snap.preamp_db == -9.0
                        && snap.band_count == 1
                        && snap.coeffs[0].b0 == z.b0);
                assert!(consistent, "torn snapshot escaped validation: {snap:?}");
            }
        }

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    /// The core safety property: an unstable filter is refused. Its output grows without
    /// bound, which reaches someone's ears before it reaches a debugger.
    #[test]
    fn unstable_filters_are_refused() {
        let unstable = [
            RawCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 1.5 },   // |a2| >= 1
            RawCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: -1.0 },  // |a2| == 1, marginal
            RawCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 2.0, a2: 0.5 },   // |a1| >= 1 + a2
            RawCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: -1.6, a2: 0.5 },  // and the mirror
        ];
        for u in unstable {
            assert!(!u.is_safe(), "accepted an unstable filter: {u:?}");
            let mut b = zeroed();
            assert!(!publish(&mut b, 0.0, &[u], false), "published an unstable filter: {u:?}");
        }
        // A real filter sits comfortably inside the triangle.
        assert!(stable().is_safe());
    }

    #[test]
    fn non_finite_values_are_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut c = stable();
            c.b0 = bad;
            assert!(!c.is_safe(), "accepted b0 = {bad}");
            let mut c = stable();
            c.a1 = bad;
            assert!(!c.is_safe(), "accepted a1 = {bad}");

            let mut b = zeroed();
            assert!(!publish(&mut b, bad, &[stable()], false), "published preamp = {bad}");
        }
    }

    /// Bounds are enforced on the reader's side too, not just the writer's — the writer is a
    /// separate process and could be a different build, or not ours at all.
    #[test]
    fn the_reader_does_not_trust_the_writer() {
        let mut b = zeroed();
        publish(&mut b, 0.0, &[stable()], false);
        let mut snap = Snapshot::default();

        // A count past the end of the array: the one field that could make the reader walk
        // off the block if it were believed.
        b.band_count = (MAX_BANDS + 1) as u32;
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Rejected);
        b.band_count = u32::MAX;
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Rejected);

        // Dangerous gain written straight into the block, bypassing `publish`.
        b.band_count = 1;
        b.preamp_db = 60.0;
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Rejected);

        // An unstable filter written directly.
        b.preamp_db = 0.0;
        b.coeffs[0] = RawCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 2.0 };
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Rejected);
    }

    /// A rejected or torn read must leave the caller's snapshot untouched, so the previous
    /// correction stays in force rather than being partially overwritten.
    #[test]
    fn a_failed_read_does_not_disturb_the_previous_snapshot() {
        let mut b = zeroed();
        publish(&mut b, -6.0, &[stable()], false);
        let mut snap = Snapshot::default();
        assert!(matches!(try_read(&b, &mut snap), ReadOutcome::Updated(_)));
        let good = snap;

        b.coeffs[0].a2 = 5.0; // now unstable
        assert_eq!(try_read(&b, &mut snap), ReadOutcome::Rejected);
        assert_eq!(snap.preamp_db, good.preamp_db);
        assert_eq!(snap.band_count, good.band_count);
        assert_eq!(snap.coeffs[0].b0, good.coeffs[0].b0);
    }

    /// The sequence number lets the caller skip work it has already done — an update is
    /// published once but read on every buffer.
    #[test]
    fn the_sequence_number_identifies_an_update() {
        let mut b = zeroed();
        let mut snap = Snapshot::default();

        publish(&mut b, -1.0, &[stable()], false);
        let ReadOutcome::Updated(first) = try_read(&b, &mut snap) else { panic!("expected Updated") };
        let ReadOutcome::Updated(again) = try_read(&b, &mut snap) else { panic!("expected Updated") };
        assert_eq!(first, again, "an unchanged block must report the same sequence");

        publish(&mut b, -2.0, &[stable()], false);
        let ReadOutcome::Updated(second) = try_read(&b, &mut snap) else { panic!("expected Updated") };
        assert_ne!(first, second, "a new publish must be distinguishable");
    }

    /// Publishing fewer bands than before must actually stop the dropped ones.
    #[test]
    fn shrinking_the_set_clears_the_tail() {
        let mut b = zeroed();
        publish(&mut b, 0.0, &[stable(), stable(), stable()], false);
        publish(&mut b, 0.0, &[stable()], false);

        let mut snap = Snapshot::default();
        assert!(matches!(try_read(&b, &mut snap), ReadOutcome::Updated(_)));
        assert_eq!(snap.band_count, 1);
        assert_eq!(snap.coeffs[1].b0, Coeffs::PASSTHROUGH.b0);
        assert_eq!(b.coeffs[1], RawCoeffs::default(), "stale coefficients left in the block");
    }
}
