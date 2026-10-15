//! Read a WAV file as 16 kHz mono f32 (for the bench and the `AGT_TEST_WAV` debug path).

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
