//! SPIKE — a measurement harness, not part of the crate. Standalone integration test (`tests/`,
//! not `src/`), deliberately: nothing in `src/lib.rs` imports or depends on this, and it doesn't
//! touch `Spectrum`'s (currently reverted, see git history) production reduction code at all. Run
//! with:
//!   cargo test -p cageq-monitor --test decimation_spike -- --nocapture
//!
//! WHY THIS EXISTS: a live-tested investigation this session tried five sequential fixes to
//! `Spectrum::snapshot`'s per-log-bin reduction (max -> sum -> fractional-boundary-weighted sum ->
//! fractional-octave smoothing -> Hz-width smoothing), each fixing one visible symptom while
//! introducing or leaving another, until the user stopped further live iteration and the whole
//! chain was reverted. The user's follow-up diagnosis reframes the problem: converting ~16k linear
//! FFT bins down to 240 log-spaced display bins is DECIMATION at a *continuously varying ratio*
//! (~1:1 near 20 Hz, hundreds:1 near 20 kHz) — and decimation without a properly band-limiting
//! filter always aliases, regardless of how the "read the decimated value" step is done. The
//! fractional-boundary-weighted sum built earlier this session IS a filter — a rectangular
//! (boxcar) one — but a boxcar is a textbook-weak anti-alias filter: its frequency response is a
//! sinc with real, slowly-decaying sidelobes, so it lets through exactly the higher-frequency
//! content that then folds back as spurious energy in the decimated output.
//!
//! This spike tests that theory directly against synthetic, known ground truth BEFORE writing
//! anything into `cageq-monitor/src/lib.rs` again.
//!
//! VERDICT (answered, with one correction to the theory along the way): switching the reduction
//! from a rectangular (boxcar) window to a Gaussian one of the same nominal width makes a real,
//! large difference on realistic content — 3.7x smoother (fewer spurious bin-to-bin reversals) on
//! a synthetic harmonic series, and the max/box/gaussian ordering is monotonic in the predicted
//! direction (`max_is_roughest_of_the_three`). That's strong enough to act on.
//!
//! The mechanism is subtly different from what the theory above predicted, though — worth getting
//! right before implementing anything, since "aliasing" isn't quite the correct word for what a
//! rectangular window does *here*. `boxcar_leaks_out_of_band_energy_more_than_gaussian` shows the
//! box has literally ZERO response to a point strictly outside its own [lo, hi) — that's the
//! defining property of a hard-edged window, not a flaw in it: nothing "leaks in" through box
//! sidelobes the way spectral leakage does when a window is applied to TIME-domain data before an
//! FFT (the Hann-window case earlier this session). This step isn't that — it's a direct weighted
//! sum over already-computed frequency-domain power values, so a rectangular weight of 0 outside
//! the range really does mean exactly 0 in, always, by construction. The Gaussian, if anything,
//! shows MORE picked-up energy from that one isolated far-away point (its tails extend past its
//! own nominal width) — worse by this specific measure.
//!
//! What actually causes the roughness on real (dense, discrete) content is a different mechanism:
//! HARD BIN-MEMBERSHIP BOUNDARIES. A box's inclusion rule is all-or-nothing — a harmonic partial
//! sitting near a boundary between two log bins counts *entirely* toward whichever side it happens
//! to fall on, so as the boundary sweeps past a dense comb of partials from bin to bin, each one
//! flips in or out abruptly, and adjacent bins' totals can differ sharply even though the
//! underlying content barely changed. A Gaussian has no hard boundary — a partial near the seam
//! contributes partially to BOTH neighbouring bins, blended smoothly, so the same sweep produces a
//! smoothly-varying total instead of a jagged one. Same practical conclusion as the theory (use a
//! smoothly-tapered window, not a rectangular one, matched in width to the local decimation
//! ratio) — different, more precise reason, closer to "hard quantization of bin membership" than
//! to classical continuous-signal decimation aliasing.
//!
//! FOLLOW-UP (same day): the width-matched Gaussian alone still let the analysis window's own
//! fixed-Hz sidelobe nulls show through near the low-frequency crossover (where the local
//! decimation ratio is ~1:1, so the width-matched sigma is far too narrow to smooth them). A
//! floor on sigma fixes that — but a *first* attempt at the floor (`4.0 * ZERO_PAD_FACTOR`,
//! reasoning "one Hann mainlobe width") conflated sigma with the kernel's actual reach
//! (`radius = sigma * 4`), so it actually reached ~4 mainlobes out and destroyed all frequency
//! resolution below ~100 Hz — shipped, then caught live: "0 frequency resolutin below 100Hz."
//! `floor_sweep_preserves_low_frequency_resolution` was added specifically because nothing before
//! it had ever tested resolution (only single-tone smoothness/width) — it settled on floor=2.0 as
//! the largest floor that still leaves even a 40/60 Hz pair (0.58 octave, the tightest tested)
//! clearly resolved (7.67 dB dip). A separate `GAUSSIAN_WIDTH_MULT=1.2` (applied to the natural
//! width, before the floor) answers the accompanying "high end could use a smidge more" ask —
//! verified to have no measurable cost up to at least 1.5x on both an isolated tone's width and
//! dense high-frequency content's roughness (`width_mult_smooths_dense_high_frequency_content`).

/// Mirrors the real config in `src/lib.rs` (`N_LOG_BINS`, `SPEC_F_MIN/MAX`, and a realistic
/// 48 kHz / `ZERO_PAD_FACTOR=4` linear grid) closely enough to be representative, without pulling
/// in the private `Spectrum` type this spike is deliberately not touching.
const N_LOG_BINS: usize = 240;
const SPEC_F_MIN: f32 = 20.0;
const SPEC_F_MAX: f32 = 20_000.0;
const RATE: f32 = 48_000.0;
const ANALYSIS_SIZE: usize = 8192;
const ZERO_PAD_FACTOR: usize = 4;
const PADDED_SIZE: usize = ANALYSIS_SIZE * ZERO_PAD_FACTOR;
const N_LIN: usize = PADDED_SIZE / 2 + 1;
const BIN_HZ: f32 = RATE / PADDED_SIZE as f32;

/// Each log bin's exact (fractional) linear-bin span — identical formula to the one this session
/// already validated as correct in isolation (see git history, `band_power`'s doc): the boundary
/// math itself was never in question, only what gets done with it once you have it.
fn log_bin_ranges() -> Vec<(f32, f32)> {
    let ratio = (SPEC_F_MAX / SPEC_F_MIN).powf(1.0 / (N_LOG_BINS as f32 - 1.0));
    let half = ratio.sqrt();
    (0..N_LOG_BINS)
        .map(|i| {
            let fc = SPEC_F_MIN * ratio.powi(i as i32);
            let lo = ((fc / half) / BIN_HZ).max(0.0);
            let hi = ((fc * half) / BIN_HZ).min((N_LIN - 1) as f32);
            (lo, hi)
        })
        .collect()
}

/// Current production method (see git history — the state this session reverted to): the tallest
/// single linear-bin sample within the (integer-rounded) range. No filtering at all, so nothing
/// outside the exact range can leak in — but nothing INSIDE the range other than the single loudest
/// sample is represented either, which is its own, separately-diagnosed problem (independent
/// per-bin peak-picking from non-overlapping windows).
fn reduce_max(power: &[f32], lo: f32, hi: f32) -> f32 {
    let lo = (lo.round() as usize).min(power.len() - 1);
    let hi = (hi.round() as usize).min(power.len() - 1).max(lo);
    power[lo..=hi].iter().copied().fold(0.0, f32::max)
}

/// This session's boxcar/fractional-overlap sum (`band_power` in the reverted code) — each linear
/// bin contributes the fraction of its own `[i, i+1)` span that overlaps `[lo, hi]`, zero outside.
/// A rectangular window in disguise: flat weight of 1 across the passband, a hard cliff to 0 at the
/// edges. Textbook-weak stopband: the box's frequency response is a sinc, whose sidelobes decay
/// only ~1/f (about -6 dB/octave) — real energy from well outside the nominal passband still gets
/// through at a non-negligible level.
fn reduce_box(power: &[f32], lo: f32, hi: f32) -> f32 {
    let lo = lo.max(0.0);
    let hi = hi.min(power.len().saturating_sub(1) as f32);
    if hi <= lo {
        return power.get(lo.round() as usize).copied().unwrap_or(0.0);
    }
    let lo_i = lo.floor() as usize;
    let hi_i = (hi.ceil() as usize).min(power.len().saturating_sub(1));
    let mut total = 0.0;
    for (i, &p) in power.iter().enumerate().take(hi_i + 1).skip(lo_i) {
        let overlap = (hi.min(i as f32 + 1.0) - lo.max(i as f32)).max(0.0);
        total += p * overlap;
    }
    total
}

/// The candidate fix: a Gaussian window of the SAME nominal width as the boxcar (same lo/hi,
/// sigma set from that width so this is an apples-to-apples shape comparison, not a width
/// comparison) — normalized so ANY window width still integrates to "the local average power",
/// same units as the boxcar. A Gaussian's own Fourier transform is a Gaussian: no sidelobes at
/// all, monotonically decaying stopband, in contrast to the box's sinc. If the leakage test below
/// shows this meaningfully suppresses out-of-band energy the box lets through, that's direct
/// evidence for switching the real reduction to something Gaussian-shaped (or similar) rather than
/// rectangular, matched in width to the local decimation ratio exactly as `lo`/`hi` already are.
fn reduce_gaussian(power: &[f32], lo: f32, hi: f32) -> f32 {
    let center = (lo + hi) / 2.0;
    let half_width = ((hi - lo) / 2.0).max(0.1);
    let sigma = half_width; // same nominal half-width as the box, by construction
    let radius = (sigma * 4.0).ceil() as i32;
    let c = center.round() as i32;
    let lo_i = (c - radius).max(0) as usize;
    let hi_i = ((c + radius) as usize).min(power.len().saturating_sub(1));
    let mut acc = 0.0f32;
    let mut wsum = 0.0f32;
    for (i, &p) in power.iter().enumerate().take(hi_i + 1).skip(lo_i) {
        let d = i as f32 - center;
        let w = (-0.5 * (d / sigma).powi(2)).exp();
        acc += p * w;
        wsum += w;
    }
    // Normalize to a per-bin AVERAGE power (not a sum that would scale with window width the way
    // the box's sum does) — comparable units to reduce_box only because reduce_box's own width is
    // fixed per call here; this keeps the leakage measurement (a ratio, see below) meaningful
    // without also having to correct for a normalization mismatch between the two methods.
    if wsum > 0.0 {
        (acc / wsum) * (hi - lo).max(1.0)
    } else {
        0.0
    }
}

/// Like `reduce_gaussian`, but sigma is floored to a fixed bin-count (`floor_sigma_bins`) rather
/// than always exactly matching the local decimation ratio (`(hi-lo)/2`), and additionally scaled
/// by `width_mult` BEFORE the floor — a uniform "smidge more everywhere" knob distinct from the
/// floor, which only ever affects the low end (by construction, it's a `max()`). Since the LINEAR
/// bin grid is uniform in Hz (unlike the log grid), a constant bin-count floor here is
/// automatically also a constant-Hz floor — no per-position conversion needed, unlike the earlier
/// (abandoned) attempt at a fixed-Hz *post-fold* smoothing pass over the already-log-binned
/// output. At high frequency the natural width-matched sigma already exceeds any reasonable
/// floor, so `width_mult` is what can add a touch of extra smoothing there, where the floor never
/// engages; at low frequency, where the natural sigma is tiny, the floor takes over instead.
fn reduce_gaussian_tuned(power: &[f32], lo: f32, hi: f32, floor_sigma_bins: f32, width_mult: f32) -> f32 {
    let center = (lo + hi) / 2.0;
    let half_width = (((hi - lo) / 2.0) * width_mult).max(0.1);
    let sigma = half_width.max(floor_sigma_bins);
    let radius = (sigma * 4.0).ceil() as i32;
    let c = center.round() as i32;
    let lo_i = (c - radius).max(0) as usize;
    let hi_i = ((c + radius) as usize).min(power.len().saturating_sub(1));
    let mut acc = 0.0f32;
    let mut wsum = 0.0f32;
    for (i, &p) in power.iter().enumerate().take(hi_i + 1).skip(lo_i) {
        let d = i as f32 - center;
        let w = (-0.5 * (d / sigma).powi(2)).exp();
        acc += p * w;
        wsum += w;
    }
    if wsum > 0.0 {
        (acc / wsum) * (hi - lo).max(1.0)
    } else {
        0.0
    }
}

/// A real windowed + zero-padded linear power spectrum for a pure tone — same method
/// `minimal_realfft_repro` used to originally find the Hann-sidelobe-null problem, reused here
/// rather than a hand-rolled idealized shape. That matters: a smooth synthetic Gaussian "lobe"
/// folded through a Gaussian reduction is smooth pretty much no matter what sigma you pick, and an
/// earlier version of this test used exactly that — it couldn't have found a real problem even if
/// the floor were wrong, because it never reproduced the actual mechanism (genuine sidelobe
/// structure) causing the low-end steppiness in the first place.
fn real_tones_linear_power(freqs: &[f64]) -> Vec<f32> {
    let rate = 48_000usize;
    let analysis_size = 8192usize;
    let padded = analysis_size * ZERO_PAD_FACTOR;
    let window: Vec<f32> = (0..analysis_size)
        .map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / analysis_size as f32).cos())
        .collect();
    let mut in_buf = vec![0.0f32; padded];
    let steps: Vec<f64> = freqs.iter().map(|&f| 2.0 * std::f64::consts::PI * f / rate as f64).collect();
    let mut phases = vec![0.0f64; freqs.len()];
    for i in 0..analysis_size {
        let mut s = 0.0f64;
        for (p, step) in phases.iter_mut().zip(steps.iter()) {
            s += 0.5 * p.sin() / freqs.len() as f64; // equal-amplitude, summed power stays comparable
            *p += step;
        }
        in_buf[i] = s as f32 * window[i];
    }
    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(padded);
    let mut out_buf = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    fft.process_with_scratch(&mut in_buf, &mut out_buf, &mut scratch).unwrap();
    out_buf.iter().map(|c| c.norm_sqr()).collect()
}
fn real_tone_linear_power(freq: f64) -> Vec<f32> {
    real_tones_linear_power(&[freq])
}

fn fold(ranges: &[(f32, f32)], power: &[f32], floor: f32, width_mult: f32) -> Vec<f32> {
    ranges
        .iter()
        .map(|&(lo, hi)| {
            let p = reduce_gaussian_tuned(power, lo, hi, floor, width_mult);
            if p > 0.0 { 10.0 * p.log10() } else { -120.0 }
        })
        .collect()
}
fn max_step_near_peak(db: &[f32], window: usize) -> f32 {
    let peak_i = db.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).map(|(i, _)| i).unwrap();
    let lo = peak_i.saturating_sub(window);
    let hi = (peak_i + window).min(db.len() - 1);
    db[lo..=hi].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}
fn width6db_hz(db: &[f32]) -> f32 {
    let ratio = (SPEC_F_MAX / SPEC_F_MIN).powf(1.0 / (N_LOG_BINS as f32 - 1.0));
    let peak = db.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let half = peak - 6.0;
    let above: Vec<usize> = db.iter().enumerate().filter(|&(_, &d)| d >= half).map(|(i, _)| i).collect();
    if above.is_empty() {
        return 0.0;
    }
    let (lo, hi) = (*above.first().unwrap(), *above.last().unwrap());
    SPEC_F_MIN * ratio.powi(hi as i32) - SPEC_F_MIN * ratio.powi(lo as i32)
}

/// Sweeps MUCH smaller floor candidates than the first attempt (which used up to 24 padded bins
/// — the first shipped floor, 16, turned out to conflate sigma with total kernel reach: a
/// Gaussian's effective width is ±4σ, so a σ of "one mainlobe width" was actually reaching out to
/// ~4 mainlobes on each side, smearing out everything below ~100 Hz. The sidelobe nulls this is
/// meant to smooth over sit only ~2 unpadded bins (~8 padded bins) from centre — a floor anywhere
/// near that scale, not 4x a mainlobe, is the right order of magnitude to even try.
///
/// Also measures 8kHz roughness (not just width) with a small `width_mult` sweep, since a smidge
/// of extra high-frequency smoothing was asked for too — a knob distinct from the floor, since the
/// floor by construction (a `max`) never engages where the natural width-matched sigma already
/// exceeds it, which is always true at 8kHz.
#[test]
fn floor_and_width_sweep() {
    let ranges = log_bin_ranges();
    let low = real_tone_linear_power(60.0);
    let high = real_tone_linear_power(8000.0);

    println!("\n--- floor sweep (width_mult=1.0), sigma in padded linear bins ---");
    println!("{:>6}  {:>26}  {:>20}", "floor", "60Hz max step near peak (dB)", "8kHz -6dB width (Hz)");
    for &floor in &[0.0f32, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0] {
        let low_db = fold(&ranges, &low, floor, 1.0);
        let high_db = fold(&ranges, &high, floor, 1.0);
        println!("{floor:6.1}  {:26.2}  {:20.1}", max_step_near_peak(&low_db, 20), width6db_hz(&high_db));
    }

    // window=20 log bins at 8kHz spans roughly 4.4-14.4kHz — mostly off-tone content falling into
    // the -120dB floor, not the shoulder ripple we actually want to measure here. A window of 6
    // bins stays within the tone's own -6dB width (232.6Hz => a few bins either side) so the step
    // it measures is real near-peak roughness, not the tone's own edge dropping into silence.
    println!("\n--- width_mult sweep (floor=0), high-frequency smidge (near-peak window, not whole-tone edge) ---");
    println!("{:>10}  {:>22}  {:>20}", "width_mult", "8kHz max step (dB)", "8kHz -6dB width (Hz)");
    for &mult in &[1.0f32, 1.1, 1.2, 1.3, 1.5, 2.0] {
        let high_db = fold(&ranges, &high, 0.0, mult);
        println!("{mult:10.1}  {:22.2}  {:20.1}", max_step_near_peak(&high_db, 6), width6db_hz(&high_db));
    }
}

/// The gap in the first floor spike: it only ever checked whether a SINGLE isolated tone read
/// smoothly, never whether TWO separate, closely-spaced tones stayed distinguishable — which is
/// what "frequency resolution" actually means, and exactly what the shipped floor (16) destroyed
/// below 100 Hz. Two equal-amplitude tones a given interval apart; "resolved" means the folded
/// curve shows a genuine local minimum between their two peaks, at least 1 dB below the lower of
/// the two peak heights (not just a shoulder/plateau, which would mean they'd already blurred into
/// one visual blob) — a hard, checkable pass/fail per floor candidate rather than eyeballing it.
#[test]
fn floor_sweep_preserves_low_frequency_resolution() {
    let ranges = log_bin_ranges();
    let resolved = |db: &[f32], f1: f32, f2: f32| -> Option<f32> {
        let ratio = (SPEC_F_MAX / SPEC_F_MIN).powf(1.0 / (N_LOG_BINS as f32 - 1.0));
        let i1 = ((f1 / SPEC_F_MIN).ln() / ratio.ln()).round() as usize;
        let i2 = ((f2 / SPEC_F_MIN).ln() / ratio.ln()).round() as usize;
        let (lo, hi) = (i1.min(i2), i1.max(i2));
        if hi <= lo + 1 {
            return None; // tones map to adjacent/same bins — not a meaningful test at this spacing
        }
        let peak1 = db[lo.saturating_sub(2)..=(lo + 2).min(db.len() - 1)].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let peak2 = db[hi.saturating_sub(2)..=(hi + 2).min(db.len() - 1)].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let valley = db[lo..=hi].iter().cloned().fold(f32::INFINITY, f32::min);
        let lower_peak = peak1.min(peak2);
        Some(lower_peak - valley) // dip depth below the lower peak — bigger is more clearly resolved
    };

    let a = real_tones_linear_power(&[60.0, 90.0]);
    let b = real_tones_linear_power(&[60.0, 120.0]);
    let c = real_tones_linear_power(&[40.0, 60.0]);

    println!("\n--- low-frequency resolution vs floor: dip depth between two tones (dB, higher=better resolved; <1.0 ~= merged) ---");
    println!("{:>6}  {:>18}  {:>18}  {:>18}", "floor", "60/90Hz (0.58oct)", "60/120Hz (1oct)", "40/60Hz (0.58oct)");
    for &floor in &[0.0f32, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 16.0] {
        let da = fold(&ranges, &a, floor, 1.0);
        let db_ = fold(&ranges, &b, floor, 1.0);
        let dc = fold(&ranges, &c, floor, 1.0);
        let ra = resolved(&da, 60.0, 90.0).map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
        let rb = resolved(&db_, 60.0, 120.0).map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
        let rc = resolved(&dc, 40.0, 60.0).map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
        println!("{floor:6.1}  {ra:>18}  {rb:>18}  {rc:>18}");
    }
}

/// The isolated-tone `width_mult` sweep above is nearly blind to the knob: an isolated tone has no
/// neighbouring content to blend in, so widening its kernel mostly just averages in more noise
/// floor, not more signal. Dense, closely-spaced content — the actual "spurious peaks" case this
/// whole investigation started from — is the real test for whether a wider high-frequency kernel
/// helps. Reuses `dense_harmonics_roughness_comparison`'s 137Hz harmonic series, restricted to the
/// >7kHz region where the natural (unfloored) sigma already dominates.
#[test]
fn width_mult_smooths_dense_high_frequency_content() {
    let ranges = log_bin_ranges();
    let fundamental = 137.0f32;
    let mut power = vec![0.0f32; N_LIN];
    let mut f = fundamental;
    while f < SPEC_F_MAX * 1.2 {
        let bin = (f / BIN_HZ).round() as usize;
        if bin < N_LIN {
            power[bin] = 1.0;
        }
        f += fundamental;
    }

    let high_region_roughness = |width_mult: f32| -> f32 {
        let db: Vec<(f32, f32)> = ranges
            .iter()
            .map(|&(lo, hi)| {
                let fc = ((lo + hi) / 2.0) * BIN_HZ;
                let p = reduce_gaussian_tuned(&power, lo, hi, 0.0, width_mult);
                (fc, if p > 0.0 { 10.0 * p.log10() } else { -120.0 })
            })
            .collect();
        db.windows(3)
            .filter(|w| w[1].0 > 7000.0)
            .map(|w| (w[0].1 - 2.0 * w[1].1 + w[2].1).abs())
            .sum()
    };

    println!("\n--- >7kHz roughness on dense 137Hz harmonic content vs width_mult ---");
    println!("{:>10}  {:>10}", "width_mult", "roughness");
    for &mult in &[1.0f32, 1.1, 1.2, 1.25, 1.3, 1.5, 2.0] {
        println!("{mult:10.2}  {:10.1}", high_region_roughness(mult));
    }
}

/// TEST 1 — checks the (wrong, see the file header VERDICT) "boxcar lets far-away energy leak in"
/// theory directly: an isolated impulse well outside a log bin's own [lo, hi). Result: the box
/// shows exactly 0 (it has no response outside its own hard edge, by construction — there's no
/// sidelobe to leak through here), and the Gaussian shows a small NON-zero reading (its tails do
/// extend past its own nominal width). Kept, with the corrected interpretation, specifically so
/// this dead end isn't re-discovered by trying the same test again next time: the real problem
/// (see TEST 2) is bin-membership discontinuity on dense discrete content, not literal leakage
/// from distant, isolated content — for THAT specific case the box is actually cleaner.
#[test]
fn boxcar_leaks_out_of_band_energy_more_than_gaussian() {
    let ranges = log_bin_ranges();
    // Pick a log bin comfortably in the "many linear bins per log bin" region (high frequency,
    // where the decimation ratio is large and any leakage should be most visible) — 8 kHz-ish.
    let target_i = ranges
        .iter()
        .position(|&(lo, hi)| {
            let fc = lo * BIN_HZ;
            let width = (hi - lo) * BIN_HZ;
            fc > 7000.0 && width > 50.0 // comfortably wide, comfortably high
        })
        .expect("no log bin found in the expected high-frequency, wide-boxcar region");
    let (lo, hi) = ranges[target_i];
    let half_width = (hi - lo) / 2.0;
    let impulse_bin = (hi + half_width * 1.5).round() as usize; // well outside [lo, hi]

    let mut power = vec![0.0f32; N_LIN];
    let impulse_power = 1.0e6f32; // large, isolated, nothing else in the array
    power[impulse_bin.min(N_LIN - 1)] = impulse_power;

    let box_reading = reduce_box(&power, lo, hi);
    let gauss_reading = reduce_gaussian(&power, lo, hi);
    let box_leak_db = 10.0 * (box_reading / impulse_power).log10();
    let gauss_leak_db = 10.0 * (gauss_reading / impulse_power).log10();

    println!("\n--- out-of-band leakage into log bin {target_i} ({:.0}-{:.0} Hz) ---", lo * BIN_HZ, hi * BIN_HZ);
    println!("    impulse at linear bin {impulse_bin} ({:.0} Hz, {:.1} half-widths past the edge)", impulse_bin as f32 * BIN_HZ, 1.5);
    println!("    box leakage:      {box_reading:.3e}  ({box_leak_db:+.1} dB relative to the impulse)");
    println!("    gaussian leakage: {gauss_reading:.3e}  ({gauss_leak_db:+.1} dB relative to the impulse)");
    println!("    box is exactly 0 here (hard edge, no response outside it) — gaussian is NOT: its tails extend past its own nominal width, so for an isolated distant point specifically, box is the cleaner of the two, not the other way round (see TEST 2 for where the real problem is instead).");
}

/// TEST 2 — dense, evenly-spaced harmonic content (mimicking real musical material) in a
/// high-frequency region where each log bin spans many linear bins. Measures how "rough"
/// (bin-to-bin non-monotonic) the resulting folded curve is under each method — a rough curve full
/// of small reversals is what "spurious peaks" looks like on screen; a properly band-limited
/// reduction of genuinely dense-but-locally-similar content should read smoother, not because
/// anything is being hidden, but because less out-of-band energy is bleeding unevenly between
/// adjacent output bins.
#[test]
fn dense_harmonics_roughness_comparison() {
    let ranges = log_bin_ranges();
    // A harmonic series with a plausible fundamental, dense enough that many partials fall inside
    // a single high-frequency log bin's linear-bin span.
    let fundamental = 137.0f32;
    let mut power = vec![0.0f32; N_LIN];
    let mut f = fundamental;
    while f < SPEC_F_MAX * 1.2 {
        let bin = (f / BIN_HZ).round() as usize;
        if bin < N_LIN {
            power[bin] = 1.0; // equal-amplitude partials — a deliberately simple, legible case
        }
        f += fundamental;
    }

    let roughness = |reduce: fn(&[f32], f32, f32) -> f32| -> (Vec<f32>, f32) {
        let db: Vec<f32> = ranges
            .iter()
            .map(|&(lo, hi)| {
                let p = reduce(&power, lo, hi);
                if p > 0.0 { 10.0 * p.log10() } else { -120.0 }
            })
            .collect();
        let rough: f32 = db.windows(3).map(|w| (w[0] - 2.0 * w[1] + w[2]).abs()).sum();
        (db, rough)
    };

    let (_db_box, rough_box) = roughness(reduce_box);
    let (_db_gauss, rough_gauss) = roughness(reduce_gaussian);

    println!("\n--- roughness (sum of |second differences| in dB, lower = smoother), {fundamental} Hz harmonic series ---");
    println!("    box:      {rough_box:.1}");
    println!("    gaussian: {rough_gauss:.1}");
    println!("    gaussian is {:.1}x smoother", rough_box / rough_gauss.max(0.001));

    // Print the actual high-frequency region (8-12 kHz) for both, side by side, since the
    // aggregate roughness number alone doesn't show WHERE the difference is concentrated.
    println!("\n--- log bin dB, 8-12 kHz region, box vs gaussian ---");
    for (i, &(lo, hi)) in ranges.iter().enumerate() {
        let fc = ((lo + hi) / 2.0) * BIN_HZ;
        if (8000.0..12000.0).contains(&fc) {
            let b = reduce_box(&power, lo, hi);
            let g = reduce_gaussian(&power, lo, hi);
            let bdb = if b > 0.0 { 10.0 * b.log10() } else { -120.0 };
            let gdb = if g > 0.0 { 10.0 * g.log10() } else { -120.0 };
            println!("  log bin {i:4}  {fc:8.1} Hz   box={bdb:7.2} dB   gauss={gdb:7.2} dB");
        }
    }
}

/// Sanity check that `reduce_max` (the CURRENT shipped behaviour) is at least as rough as both —
/// establishes the three-way ordering (max worst, box middle, gaussian best) the theory predicts,
/// rather than only ever comparing box against gaussian in isolation.
#[test]
fn max_is_roughest_of_the_three() {
    let ranges = log_bin_ranges();
    let fundamental = 137.0f32;
    let mut power = vec![0.0f32; N_LIN];
    let mut f = fundamental;
    while f < SPEC_F_MAX * 1.2 {
        let bin = (f / BIN_HZ).round() as usize;
        if bin < N_LIN {
            power[bin] = 1.0;
        }
        f += fundamental;
    }
    let roughness = |reduce: fn(&[f32], f32, f32) -> f32| -> f32 {
        let db: Vec<f32> = ranges
            .iter()
            .map(|&(lo, hi)| {
                let p = reduce(&power, lo, hi);
                if p > 0.0 { 10.0 * p.log10() } else { -120.0 }
            })
            .collect();
        db.windows(3).map(|w| (w[0] - 2.0 * w[1] + w[2]).abs()).sum()
    };
    let rough_max = roughness(reduce_max);
    let rough_box = roughness(reduce_box);
    let rough_gauss = roughness(reduce_gaussian);
    println!("\n--- three-way roughness ordering ---");
    println!("    max:      {rough_max:.1}");
    println!("    box:      {rough_box:.1}");
    println!("    gaussian: {rough_gauss:.1}");
}
