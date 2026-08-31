//! Find discontinuities in a recorded WAV.
//!
//! A click is a sample-to-sample jump much larger than the waveform's own slew, so that is
//! what this looks for — plus a block-RMS trace, which shows where a transition happened even
//! when it is smooth. Written for recordings made by `switchtest`/`CAGEQ_RECORD`, where the
//! interesting moment is known but the question is what the samples actually did there.
//!
//! ```text
//! wavscan switch.wav
//! ```

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: wavscan <file.wav>");
        std::process::exit(2);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("could not read {path}: {e}");
            std::process::exit(1);
        }
    };
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        eprintln!("not a WAV file");
        std::process::exit(1);
    }
    let fmt = u16::from_le_bytes([bytes[20], bytes[21]]);
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let bits = u16::from_le_bytes([bytes[34], bytes[35]]);
    if fmt != 3 || bits != 32 {
        eprintln!("expected 32-bit float (format 3), got format {fmt} / {bits} bits");
        std::process::exit(1);
    }

    // Left channel only: both carry the same tone, and one is enough to find an edge.
    let left: Vec<f32> = bytes[44..]
        .chunks_exact(4 * channels)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let n = left.len();
    println!("{path}: {rate} Hz, {channels} ch, {n} frames ({:.2} s)", n as f64 / rate as f64);

    // Per-sample jumps, and the median as the "natural slew" baseline — a median cannot be
    // inflated by the very outlier being looked for, which a mean or max could.
    let jumps: Vec<f32> = left.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let mut sorted = jumps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2].max(1e-12);
    println!("median per-sample step: {median:.6}");

    // The worst offenders, thinned so one event does not fill the list with its neighbours.
    let mut idx: Vec<usize> = (0..jumps.len()).collect();
    idx.sort_by(|&a, &b| jumps[b].partial_cmp(&jumps[a]).unwrap());
    let mut shown: Vec<usize> = Vec::new();
    println!("largest discontinuities:");
    for &i in &idx {
        if shown.iter().any(|&s| i.abs_diff(s) < rate as usize / 100) {
            continue;
        }
        shown.push(i);
        println!(
            "  {:8.1} ms  step {:.6}  = {:6.1}x median   ({:+.5} -> {:+.5})",
            i as f64 * 1000.0 / rate as f64,
            jumps[i],
            jumps[i] / median,
            left[i],
            left[i + 1],
        );
        if shown.len() == 8 {
            break;
        }
    }

    // Block RMS, to locate transitions that are smooth as well as those that are not.

    // Characterise each level transition: where it starts, how long it takes, and how far it
    // moves. This is what makes two recordings comparable — "the swell is worse" becomes a
    // duration in milliseconds, and a fade's own length is only one of the things that can
    // set it.
    println!("transitions (10%-90% of the level change):");
    // Envelope by RMS over a 20 ms window, stepped 5 ms. The window has to span at least one
    // period of the signal or it measures where in the cycle it lands rather than the level —
    // at 50 Hz a 1 ms window reports the tone's own oscillation as a 1 dB "transition" every
    // millisecond. 20 ms therefore resolves anything from about 20 ms upwards, which is the
    // range a *swell* lives in; a fade shorter than that shows up as a single step here, which
    // is exactly the distinction being drawn.
    const WIN_MS: usize = 20;
    const HOP_MS: usize = 5;
    let win = (rate as usize * WIN_MS / 1000).max(1);
    let hop = (rate as usize * HOP_MS / 1000).max(1);
    let env: Vec<f64> = (0..left.len().saturating_sub(win))
        .step_by(hop)
        .map(|start| {
            let c = &left[start..start + win];
            (c.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / c.len() as f64).sqrt()
        })
        .collect();
    let ms = HOP_MS as f64;
    // Anchored on the steepest point of the envelope, then measured outward. Triggering on
    // "the level differs from 200 ms hence" instead fires *before* the move starts, and then
    // reports its first millisecond as though it were the whole thing.
    let ctx = (100 / HOP_MS).max(2); // 100 ms of settled level either side
    let mut transition_points: Vec<usize> = Vec::new();
    let mut found = 0;
    let mut k = ctx + 1;
    while k + ctx < env.len() {
        let before = env[k - ctx];
        let after = env[k + ctx];
        let change_db = 20.0 * (after.max(1e-12) / before.max(1e-12)).log10();
        if change_db.abs() < 1.0 {
            k += 1;
            continue;
        }
        // The steepest hop in this neighbourhood is the transition itself.
        let steep = (k - ctx..k + ctx)
            .max_by(|&a, &b| {
                let da = (env[a + 1] - env[a]).abs();
                let db = (env[b + 1] - env[b]).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap_or(k);
        // Re-measure around the steepest point, not around wherever the scan first noticed a
        // difference — that is 100 ms early and reports the transition's first moments as
        // though they were the whole move.
        let before = env[steep.saturating_sub(ctx)];
        let after = env[(steep + ctx).min(env.len() - 1)];
        let change_db = 20.0 * (after.max(1e-12) / before.max(1e-12)).log10();
        let lo = before + (after - before) * 0.1;
        let hi = before + (after - before) * 0.9;
        let crossed = |v: f64, t: f64| if after > before { v >= t } else { v <= t };
        let start = (steep.saturating_sub(ctx)..(steep + ctx).min(env.len() - 1)).find(|&j| crossed(env[j], lo)).unwrap_or(steep);
        let end = (start..(steep + ctx).min(env.len() - 1))
            .find(|&j| crossed(env[j], hi))
            .unwrap_or(steep + ctx);
        println!(
            "  at {:6.0} ms: {:+.1} dB over {:3.0} ms   ({:.0} -> {:.0} dBFS)",
            start as f64 * ms,
            change_db,
            (end - start) as f64 * ms,
            20.0 * before.max(1e-12).log10(),
            20.0 * after.max(1e-12).log10(),
        );
        transition_points.push(steep * hop);
        found += 1;
        k = end + ctx * 2;
        if found == 6 {
            break;
        }
    }
    if found == 0 {
        println!("  (none: no sustained level change of 1 dB or more)");
    }

    // Spectrum around the first transition, so two recordings can be compared directly.
    //
    // A crossfade is an amplitude modulation, and modulation makes sidebands — a separate
    // mechanism from the phase cancellation measured elsewhere, and the one that actually
    // shows on a spectrum display. Absolute numbers here include the analysis window's own
    // leakage from the level step, which cannot be separated out; but the SAME analysis
    // applied to two files is directly comparable, and that is the question being asked.
    if let Some(&centre) = transition_points.first() {
        const N: usize = 8192; // 5.9 Hz bins at 48 kHz
        let start = centre.saturating_sub(N / 2);
        if start + N <= left.len() {
            let w: Vec<f64> = (0..N)
                // Hann: without a window the level step's leakage swamps everything and every
                // recording looks identical.
                .map(|i| {
                    0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos()
                })
                .collect();
            let seg: Vec<f64> =
                (0..N).map(|i| left[start + i] as f64 * w[i]).collect();
            let bin_at = |hz: f64| (hz * N as f64 / rate as f64).round() as usize;
            let mag = |k: usize| {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, &s) in seg.iter().enumerate() {
                    let a = 2.0 * std::f64::consts::PI * k as f64 * i as f64 / N as f64;
                    re += s * a.cos();
                    im -= s * a.sin();
                }
                (re * re + im * im).sqrt()
            };
            let fund = mag(bin_at(50.0)).max(1e-12);
            println!("spectrum around the transition (re 50 Hz, Hann {N}):");
            for hz in [100.0, 150.0, 200.0, 300.0, 500.0, 1000.0, 2000.0] {
                println!("  {hz:7.0} Hz  {:6.1} dB", 20.0 * (mag(bin_at(hz)) / fund).log10());
            }
        }
    }
    println!("level trace (10 ms blocks, dBFS):");
    let block = rate as usize / 100;
    let mut line = String::new();
    for (b, chunk) in left.chunks(block).enumerate() {
        let rms = (chunk.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>()
            / chunk.len() as f64)
            .sqrt();
        let db = 20.0 * rms.max(1e-12).log10();
        line.push_str(&format!("{db:.0} "));
        if (b + 1) % 20 == 0 {
            println!("  {:5.0} ms: {line}", (b as f64 - 19.0) * 10.0);
            line.clear();
        }
    }
    if !line.is_empty() {
        println!("  {line}");
    }
}
