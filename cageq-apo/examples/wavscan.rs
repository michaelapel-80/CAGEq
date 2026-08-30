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
