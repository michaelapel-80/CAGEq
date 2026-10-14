//! A minimal RIFF/WAVE writer for the impulse-response export: one channel of 32-bit IEEE
//! float samples. Float rather than 24-bit integer so a filter's decay tail, which the export
//! keeps down to -120 dB, stays exact. Equalizer APO reads float WAVs (through libsndfile), as do
//! convolution engines generally.

/// `WAVE_FORMAT_IEEE_FLOAT`.
const FORMAT_IEEE_FLOAT: u16 = 3;

/// The complete file: `RIFF` header, a `fmt ` chunk, the `fact` chunk the WAVE spec requires
/// for every non-PCM format (some readers reject a float file without one), and the samples.
pub fn float_mono(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    const CHANNELS: u16 = 1;
    const BYTES_PER_SAMPLE: u16 = 4;
    let data_len = (samples.len() * BYTES_PER_SAMPLE as usize) as u32;
    // fmt (8 + 18) + fact (8 + 4) + data (8 + len), plus the 4-byte "WAVE" tag.
    let riff_len = 4 + (8 + 18) + (8 + 4) + (8 + data_len);

    let mut out = Vec::with_capacity(8 + riff_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&18u32.to_le_bytes());
    out.extend_from_slice(&FORMAT_IEEE_FLOAT.to_le_bytes());
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(CHANNELS * BYTES_PER_SAMPLE)).to_le_bytes()); // byte rate
    out.extend_from_slice(&(CHANNELS * BYTES_PER_SAMPLE).to_le_bytes()); // block align
    out.extend_from_slice(&(BYTES_PER_SAMPLE * 8).to_le_bytes()); // bits per sample
    out.extend_from_slice(&0u16.to_le_bytes()); // cbSize: no extension

    out.extend_from_slice(b"fact");
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes()); // frames per channel

    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }
    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
    }

    #[test]
    fn writes_a_well_formed_float_wav() {
        let samples = [1.0f32, -0.5, 0.25];
        let b = float_mono(&samples, 48_000);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32_at(&b, 4) as usize, b.len() - 8, "RIFF size covers everything after it");
        assert_eq!(&b[8..12], b"WAVE");
        assert_eq!(&b[12..16], b"fmt ");
        assert_eq!(u32_at(&b, 16), 18);
        assert_eq!(u16_at(&b, 20), FORMAT_IEEE_FLOAT);
        assert_eq!(u16_at(&b, 22), 1, "mono");
        assert_eq!(u32_at(&b, 24), 48_000);
        assert_eq!(u32_at(&b, 28), 192_000, "byte rate");
        assert_eq!(u16_at(&b, 32), 4, "block align");
        assert_eq!(u16_at(&b, 34), 32, "bits per sample");
        assert_eq!(&b[38..42], b"fact");
        assert_eq!(u32_at(&b, 46), 3, "frame count");
        assert_eq!(&b[50..54], b"data");
        assert_eq!(u32_at(&b, 54), 12);
        let read: Vec<f32> = b[58..].chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(read, samples);
    }
}
