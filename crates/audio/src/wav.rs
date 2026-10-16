//! WAV files: read as 16 kHz mono f32 (for the bench and the `AGT_TEST_WAV` debug path), and
//! write a recording (for the optional dictation history).

use crate::resample::resample;
use std::path::Path;

pub fn read_16k_mono(path: &Path) -> Result<Vec<f32>, String> {
    let fail = |e: hound::Error| format!("{}: {e}", path.display());
    let mut reader = hound::WavReader::open(path).map_err(fail)?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(fail)?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 * scale))
                .collect::<Result<_, _>>()
                .map_err(fail)?
        }
    };
    let mono = crate::downmix(&samples, spec.channels as usize);
    Ok(resample(
        &mono,
        spec.sample_rate,
        agt_core::stt::SAMPLE_RATE,
    ))
}

/// Writes 16 kHz mono samples (-1..1) as 16-bit PCM: half the size of f32, and what every
/// tool reads.
pub fn write_16k_mono(path: &Path, samples: &[f32]) -> Result<(), String> {
    let fail = |e: hound::Error| format!("{}: {e}", path.display());
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: agt_core::stt::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(fail)?;
    let mut w = writer.get_i16_writer(samples.len() as u32);
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
    }
    w.flush().map_err(fail)?;
    writer.finalize().map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_wav_reads_back() {
        let path = std::env::temp_dir().join(format!("agt-wav-test-{}.wav", std::process::id()));
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        write_16k_mono(&path, &[samples.as_slice(), &[2.0, -2.0]].concat()).unwrap();
        let back = read_16k_mono(&path).unwrap();
        assert_eq!(back.len(), samples.len() + 2);
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        // Out-of-range samples are clipped, not wrapped.
        assert!(back[1600] > 0.99 && back[1601] < -0.99);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 44 + 2 * 1602);
        std::fs::remove_file(&path).unwrap();
    }
}
