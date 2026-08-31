//! Run the dry switch through the engine offline and write it to a WAV, so it can be measured
//! with **exactly** the analysis used on the recordings.
//!
//! Recorded evidence puts our transition 25 dB above Equalizer APO's at 200 Hz, and the
//! obvious cause (ours being quicker) measured out as a non-cause. This decides where to look
//! next: if the engine's own output shows the same spread, the mechanism is in the DSP; if it
//! comes out clean, something between the engine and the endpoint is adding it.
//!
//! Absolute numbers from a windowed DFT of a level step include the window's own leakage and
//! cannot be trusted on their own — but the same analysis applied to three files is
//! comparable, which is the whole point of dumping this one.
//!
//! ```text
//! enginedump engine.wav          # 50 Hz, 21-band correction, dry at ~1290 ms and back
//! ```

use std::io::Write;

use cageq_apo::dsp::{Band, Cascade, FilterKind, coefficients};

const FS: f64 = 48_000.0;
const CH: usize = 2;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "engine.wav".into());

    // Shaped like the correction in the recording it is being compared against: 21 bands,
    // alternating boost and cut, wet preamp -7.7 dB going to a dry preamp of -9.0 dB.
    let bands: Vec<Band> = (0..21)
        .map(|i| Band {
            kind: FilterKind::Peaking,
            freq_hz: 40.0 * 1.35_f64.powi(i),
            // The low band carries more gain than the rest so the level change at 50 Hz matches
            // the recordings' -4.5 dB. Comparing sideband spread across different step sizes
            // would flatter whichever moved least.
            gain_db: if i == 0 { 6.0 } else if i % 2 == 0 { 4.0 } else { -3.0 },
            q: 1.4,
        })
        .collect();
    let wet: Vec<_> = bands.iter().map(|b| coefficients(b, FS)).collect();

    let mut c = Cascade::new(CH, FS);
    assert!(c.apply_coeffs(&wet, -7.7), "correction refused");
    c.settle();

    // Same shape as `switchtest`: settled, dry at ~1290 ms, back at ~1890 ms, out at 2.5 s.
    let total = (2.5 * FS) as usize;
    let at_dry = (1.29 * FS) as usize;
    let at_wet = (1.89 * FS) as usize;

    let mut pcm: Vec<f32> = Vec::with_capacity(total * CH);
    let mut frame = vec![0.0f32; CH];
    for n in 0..total {
        if n == at_dry {
            assert!(c.apply_coeffs(&[], -9.0));
        }
        if n == at_wet {
            assert!(c.apply_coeffs(&wet, -7.7));
        }
        // A continuous 50 Hz tone at the same level the recordings used.
        let s = ((2.0 * std::f64::consts::PI * 50.0 * n as f64 / FS).sin() * 0.5) as f32;
        let input = [s; CH];
        c.process(&input, &mut frame, 1);
        pcm.extend_from_slice(&frame);
    }

    write_wav(&out, &pcm).expect("write");
    println!("wrote {out}: {:.2} s, dry at 1290 ms, back at 1890 ms", total as f64 / FS);
    println!("compare with:  wavscan {out}   (and against eqapo.wav / cageqapo.wav)");
}

/// 32-bit float WAV, matching what the loopback recorder writes so the two are analysed
/// identically.
fn write_wav(path: &str, pcm: &[f32]) -> std::io::Result<()> {
    let bytes = (pcm.len() * 4) as u32;
    let block_align = (CH * 4) as u16;
    let rate = FS as u32;
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?; // IEEE float
    f.write_all(&(CH as u16).to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * block_align as u32).to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&bytes.to_le_bytes())?;
    for s in pcm {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}
