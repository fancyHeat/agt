//! WAV files: read as 16 kHz mono f32 (for the bench and the `AGT_TEST_WAV` debug path), and
//! write a recording, whole (for the optional dictation history) or as it arrives
//! ([`WavWriter`], for meeting notes).

use crate::resample::resample;
use std::{fs::File, io, io::BufWriter, path::Path};

/// 16 kHz mono 16-bit PCM: half the size of f32, and what every tool reads.
const SPEC: hound::WavSpec = hound::WavSpec {
    channels: 1,
    sample_rate: agt_core::stt::SAMPLE_RATE,
    bits_per_sample: 16,
    sample_format: hound::SampleFormat::Int,
};

/// -1..1 to 16-bit PCM, clipped (not wrapped) when out of range.
fn pcm16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

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

/// Writes 16 kHz mono samples (-1..1) as 16-bit PCM (see [`SPEC`]).
pub fn write_16k_mono(path: &Path, samples: &[f32]) -> Result<(), String> {
    let fail = |e: hound::Error| format!("{}: {e}", path.display());
    let mut writer = hound::WavWriter::create(path, SPEC).map_err(fail)?;
    let mut w = writer.get_i16_writer(samples.len() as u32);
    for &s in samples {
        w.write_sample(pcm16(s));
    }
    w.flush().map_err(fail)?;
    writer.finalize().map_err(fail)
}

/// A 16 kHz mono 16-bit PCM WAV written as the audio arrives, for recordings too long to
/// keep in memory (a meeting hour is 230 MB as f32, 115 MB in the file). The 44-byte header
/// goes out first with zero sizes; [`finish`](Self::finish) seeks back and patches the RIFF
/// and data sizes, and so does dropping it (best effort, errors ignored), so a session that
/// ends on an error still leaves a readable file. Only a crash or a kill leaves the sizes
/// at zero (the samples are there, but readers see none). Writes are buffered.
pub struct WavWriter(hound::WavWriter<BufWriter<File>>);

/// WAV sizes are 32-bit: 37 hours of 16 kHz 16-bit audio (header included).
const MAX_SAMPLES: u64 = (u32::MAX as u64 - 36) / 2;

impl WavWriter {
    /// Creates (or truncates) `path`.
    pub fn create(path: &Path) -> io::Result<WavWriter> {
        hound::WavWriter::create(path, SPEC)
            .map(WavWriter)
            .map_err(io_error)
    }

    /// Appends 16 kHz mono samples (-1..1, clipped beyond). Fails once the file is full
    /// ([`MAX_SAMPLES`]), rather than wrapping the sizes.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        if u64::from(self.0.len()) + samples.len() as u64 > MAX_SAMPLES {
            return Err(io::Error::other("WAV file full (37 hours)"));
        }
        let mut w = self.0.get_i16_writer(samples.len() as u32);
        for &s in samples {
            w.write_sample(pcm16(s));
        }
        w.flush().map_err(io_error)
    }

    /// Patches the header and flushes, reporting any error (dropping can't).
    pub fn finish(self) -> io::Result<()> {
        self.0.finalize().map_err(io_error)
    }
}

/// hound's errors as `io::Error` (I/O ones unwrapped, the rest can't happen with [`SPEC`]).
fn io_error(e: hound::Error) -> io::Error {
    match e {
        hound::Error::IoError(e) => e,
        e => io::Error::other(e),
    }
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

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("agt-{name}-{}.wav", std::process::id()))
    }

    #[test]
    fn wav_writer_round_trip() {
        let path = temp("wav-writer");
        // At most 0.5: then 16-bit PCM is within 1/32768 (write scales by 32767, read by 32768).
        let samples: Vec<f32> = (0..4000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let mut w = WavWriter::create(&path).unwrap();
        for chunk in [
            &samples[..1000],
            &[],
            &samples[1000..1001],
            &samples[1001..],
        ] {
            w.write(chunk).unwrap();
        }
        w.finish().unwrap();
        let back = read_16k_mono(&path).unwrap();
        assert_eq!(back.len(), samples.len());
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() <= 1.0 / 32768.0, "{a} vs {b}");
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 44 + 2 * 4000);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn wav_writer_patches_on_drop() {
        let path = temp("wav-writer-drop");
        let mut w = WavWriter::create(&path).unwrap();
        w.write(&[0.25; 1600]).unwrap();
        w.write(&[-0.25; 160]).unwrap();
        drop(w);
        let back = read_16k_mono(&path).unwrap();
        assert_eq!(back.len(), 1760);
        assert!(back[1600] < -0.24);
        std::fs::remove_file(&path).unwrap();
    }
}
