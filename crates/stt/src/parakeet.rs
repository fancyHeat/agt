//! NVIDIA Parakeet TDT 0.6B (v2 English / v3 multilingual) on ONNX Runtime.
//!
//! Uses the ONNX export from `istupakov/parakeet-tdt-0.6b-v{2,3}-onnx` (the one `onnx-asr`
//! uses), which ships three graphs:
//! - `nemo128.onnx`: NeMo's log-mel preprocessor (128 bins), so features match training
//!   exactly and we need no FFT code;
//! - `encoder-model*.onnx`: the FastConformer encoder (the expensive part; 8x subsampling,
//!   one frame per 80 ms);
//! - `decoder_joint-model*.onnx`: LSTM prediction network + joint, run once per decoding step.
//!
//! Only the encoder runs on the configured device; the preprocessor and the per-step
//! decoder are tiny and run on the CPU (a GPU round-trip per decoding step costs more than
//! it saves).

use crate::{
    models::{self, ModelFile, Progress},
    tdt::{self, Joint},
};
use agt_core::{
    config::{Device, Precision, SttConfig},
    stt::{Result, SAMPLE_RATE, SttEngine, TranscribeOpts, Transcript},
};
use ort::{
    environment::Environment,
    session::{Session, builder::GraphOptimizationLevel},
    value::TensorRef,
};
use std::{path::Path, time::Instant};

macro_rules! hf {
    ($repo:literal, $rev:literal, $name:literal, $size:literal, $sha:literal) => {
        ModelFile {
            name: $name,
            url: concat!(
                "https://huggingface.co/",
                $repo,
                "/resolve/",
                $rev,
                "/",
                $name
            ),
            size: $size,
            sha256: $sha,
        }
    };
}

macro_rules! parakeet_files {
    ($repo:literal, $rev:literal,
     vocab: ($vsize:literal, $vsha:literal),
     decoder_int8: ($ds:literal, $dh:literal),
     encoder_int8: ($es8:literal, $eh8:literal),
     encoder_fp32: ($es:literal, $eh:literal),
     encoder_fp32_data: ($eds:literal, $edh:literal) $(,)?) => {
        (
            [
                hf!(
                    $repo,
                    $rev,
                    "nemo128.onnx",
                    139764,
                    "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f"
                ),
                hf!($repo, $rev, "vocab.txt", $vsize, $vsha),
                hf!($repo, $rev, "decoder_joint-model.int8.onnx", $ds, $dh),
                hf!($repo, $rev, "encoder-model.int8.onnx", $es8, $eh8),
            ],
            [
                hf!(
                    $repo,
                    $rev,
                    "nemo128.onnx",
                    139764,
                    "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f"
                ),
                hf!($repo, $rev, "vocab.txt", $vsize, $vsha),
                hf!($repo, $rev, "decoder_joint-model.int8.onnx", $ds, $dh),
                hf!($repo, $rev, "encoder-model.onnx", $es, $eh),
                hf!($repo, $rev, "encoder-model.onnx.data", $eds, $edh),
            ],
        )
    };
}

// Pinned to a repo revision, so URLs and hashes never change under us. The decoder is always
// the int8 export: it runs once per output step and is memory-bound (the fp32 one is 36-73 MB,
// doesn't fit in cache, and was ~5x slower per step with no accuracy gain in our tests).
#[rustfmt::skip]
const V2: ([ModelFile; 4], [ModelFile; 5]) = parakeet_files!(
    "istupakov/parakeet-tdt-0.6b-v2-onnx", "0bbb45a3365852604aef28b538a8f066f4ccaa85",
    vocab: (9384, "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d"),
    decoder_int8: (8998286, "a449f49acd68979d418651dd2dcb737cc0f1bf0225e009e29ee326354edbf7d3"),
    encoder_int8: (652184014, "3e0581fda6ab843888b51e56d7ee78b6d5bc3237ec113af1f732d1d5286aa155"),
    encoder_fp32: (41770866, "3987bcd28175d829d12888a996a84e8f62a0e374d9ffd640662c1515adc679d3"),
    encoder_fp32_data: (2435420160, "4dab7362d4874d85965045b1e41b2d61dd2cc0fb25671a7f6b3dc47bf120cc41"),
);

#[rustfmt::skip]
const V3: ([ModelFile; 4], [ModelFile; 5]) = parakeet_files!(
    "istupakov/parakeet-tdt-0.6b-v3-onnx", "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce",
    vocab: (93939, "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"),
    decoder_int8: (18202004, "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70"),
    encoder_int8: (652183999, "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09"),
    encoder_fp32: (41770866, "98a74b21b4cc0017c1e7030319a4a96f4a9506e50f0708f3a516d02a77c96bb1"),
    encoder_fp32_data: (2435420160, "9a22d372c51455c34f13405da2520baefb7125bd16981397561423ed32d24f36"),
);

/// The files `cfg` needs, or an error for an unknown model name.
pub fn files(cfg: &SttConfig) -> Result<&'static [ModelFile]> {
    let set = match cfg.model.as_str() {
        "parakeet-tdt-0.6b-v2" => &V2,
        "parakeet-tdt-0.6b-v3" => &V3,
        other => {
            return Err(format!(
                "unknown parakeet model {other:?}; expected \"parakeet-tdt-0.6b-v2\" or \"parakeet-tdt-0.6b-v3\""
            )
            .into());
        }
    };
    Ok(match cfg.precision {
        Precision::Int8 => &set.0,
        Precision::Fp32 => &set.1,
    })
}

pub struct Parakeet {
    name: String,
    preprocessor: Session,
    encoder: Session,
    decoder: Session,
    vocab: Vec<String>,
    blank: usize,
    /// Prediction-network LSTM state shape, e.g. `[2, 1, 640]`.
    state_shape: [usize; 3],
}

impl Parakeet {
    /// Downloads missing files, then loads the three sessions.
    pub fn load(cfg: &SttConfig, progress: &mut dyn FnMut(Progress)) -> Result<Self> {
        let files = files(cfg)?;
        let dir = cfg.models_path().join(&cfg.model);
        models::ensure(&dir, files, progress)?;

        let start = Instant::now();
        let vocab = load_vocab(&dir.join("vocab.txt"))?;
        let blank = vocab
            .iter()
            .position(|t| t == "<blk>")
            .ok_or("vocab.txt has no <blk> token")?;
        let enc_file = match cfg.precision {
            Precision::Int8 => "encoder-model.int8.onnx",
            Precision::Fp32 => "encoder-model.onnx",
        };
        let env = ort::init().with_name("agt").build()?;
        let preprocessor = session(&env, &dir.join("nemo128.onnx"), Device::Cpu, cfg.threads)?;
        let encoder = session(&env, &dir.join(enc_file), cfg.device, cfg.threads)?;
        let decoder = session(
            &env,
            &dir.join("decoder_joint-model.int8.onnx"),
            Device::Cpu,
            1,
        )?;

        let state_dims = decoder
            .inputs()
            .iter()
            .find(|i| i.name() == "input_states_1")
            .and_then(|i| i.dtype().tensor_shape().map(|s| s.to_vec()))
            .ok_or("decoder has no input_states_1 input")?;
        let state_shape = match state_dims[..] {
            [layers, _, hidden] if layers > 0 && hidden > 0 => {
                [layers as usize, 1, hidden as usize]
            }
            _ => return Err(format!("unexpected decoder state shape {state_dims:?}").into()),
        };

        let name = format!(
            "{} {} ({})",
            cfg.model,
            match cfg.precision {
                Precision::Int8 => "int8",
                Precision::Fp32 => "fp32",
            },
            match cfg.device {
                Device::Cpu => "cpu",
                Device::DirectMl => "directml",
            }
        );
        tracing::info!("loaded {name} in {:.2} s", start.elapsed().as_secs_f64());
        Ok(Self {
            name,
            preprocessor,
            encoder,
            decoder,
            vocab,
            blank,
            state_shape,
        })
    }
}

fn session(env: &Environment, path: &Path, device: Device, threads: usize) -> Result<Session> {
    let mut b = Session::builder(env)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| e.to_string())?;
    if threads > 0 {
        b = b.with_intra_threads(threads).map_err(|e| e.to_string())?;
    }
    match device {
        Device::Cpu => {}
        #[cfg(windows)]
        Device::DirectMl => {
            // DirectML requires sequential execution and no memory-pattern optimization.
            let dml = ort::ep::DirectML::default().build().error_on_failure();
            b = b.with_memory_pattern(false).map_err(|e| e.to_string())?;
            b = b
                .with_parallel_execution(false)
                .map_err(|e| e.to_string())?;
            b = b
                .with_execution_providers([dml])
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(windows))]
        Device::DirectMl => return Err("device \"directml\" is only available on Windows".into()),
    }
    b.commit_from_file(path)
        .map_err(|e| format!("cannot load {}: {e}", path.display()).into())
}

fn load_vocab(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut vocab = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let (piece, id) = line
            .rsplit_once(' ')
            .ok_or_else(|| format!("bad vocab line {line:?}"))?;
        let id: usize = id.parse().map_err(|_| format!("bad vocab line {line:?}"))?;
        if vocab.len() <= id {
            vocab.resize(id + 1, String::new());
        }
        vocab[id] = piece.to_owned();
    }
    Ok(vocab)
}

impl SttEngine for Parakeet {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn warmup(&mut self) -> Result<()> {
        // Two seconds of faint noise: exercises every graph, including a few decoder steps.
        let mut x = 0x1234_5678u32;
        let audio: Vec<f32> = (0..2 * SAMPLE_RATE)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 - 0.5) * 1e-3
            })
            .collect();
        self.transcribe(&audio, &TranscribeOpts::default())?;
        Ok(())
    }

    fn transcribe(&mut self, audio: &[f32], _opts: &TranscribeOpts) -> Result<Transcript> {
        if audio.is_empty() {
            return Ok(Transcript::default());
        }
        let t0 = Instant::now();
        // 1. Log-mel features [1, 128, T].
        let pre = self.preprocessor.run(ort::inputs![
            "waveforms" => TensorRef::from_array_view(([1usize, audio.len()], audio))?,
            "waveforms_lens" => TensorRef::from_array_view(([1usize], &[audio.len() as i64][..]))?,
        ])?;
        let (feat_shape, feats) = pre["features"].try_extract_tensor::<f32>()?;
        let (_, feat_lens) = pre["features_lens"].try_extract_tensor::<i64>()?;
        let t1 = Instant::now();

        // 2. Encoder: [1, D, T'] (channels first).
        let enc = self.encoder.run(ort::inputs![
            "audio_signal" => TensorRef::from_array_view((feat_shape.clone(), feats))?,
            "length" => TensorRef::from_array_view(([1usize], feat_lens))?,
        ])?;
        let (enc_shape, enc_out) = enc["outputs"].try_extract_tensor::<f32>()?;
        let (_, enc_lens) = enc["encoded_lengths"].try_extract_tensor::<i64>()?;
        let (dim, frames) = (enc_shape[1] as usize, enc_shape[2] as usize);
        let valid = (enc_lens[0].max(0) as usize).min(frames);
        // Frame-major copy so each decoder step reads one contiguous [D] slice.
        let mut by_frame = vec![0f32; frames * dim];
        for d in 0..dim {
            for t in 0..frames {
                by_frame[t * dim + d] = enc_out[d * frames + t];
            }
        }
        let t2 = Instant::now();

        // 3. Greedy TDT decoding.
        let state_len = self.state_shape.iter().product();
        let mut joint = OnnxJoint {
            session: &mut self.decoder,
            frames: &by_frame,
            dim,
            vocab_size: self.vocab.len(),
            blank: self.blank,
            state_shape: self.state_shape,
            state: [vec![0.0; state_len], vec![0.0; state_len]],
            next_state: [vec![0.0; state_len], vec![0.0; state_len]],
            steps: 0,
        };
        let tokens = tdt::greedy_decode(valid, self.blank, &mut joint)?;
        let steps = joint.steps;
        let text = tdt::detokenize(tokens.iter().map(|&t| self.vocab[t].as_str()));
        let t3 = Instant::now();
        tracing::debug!(
            "stt: preprocess {:.1} ms, encoder {:.1} ms ({frames} frames), decode {:.1} ms ({steps} steps)",
            ms(t1 - t0),
            ms(t2 - t1),
            ms(t3 - t2)
        );
        Ok(Transcript { text })
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

struct OnnxJoint<'a> {
    session: &'a mut Session,
    /// Encoder output, frame-major `[T', D]`.
    frames: &'a [f32],
    dim: usize,
    vocab_size: usize,
    blank: usize,
    state_shape: [usize; 3],
    state: [Vec<f32>; 2],
    next_state: [Vec<f32>; 2],
    steps: usize,
}

impl Joint for OnnxJoint<'_> {
    fn step(&mut self, t: usize, last_token: usize) -> Result<(usize, usize)> {
        self.steps += 1;
        let frame = &self.frames[t * self.dim..(t + 1) * self.dim];
        let target = [last_token as i32];
        let out = self.session.run(ort::inputs![
            "encoder_outputs" => TensorRef::from_array_view(([1usize, self.dim, 1], frame))?,
            "targets" => TensorRef::from_array_view(([1usize, 1], &target[..]))?,
            "target_length" => TensorRef::from_array_view(([1usize], &[1i32][..]))?,
            "input_states_1" => TensorRef::from_array_view((self.state_shape, &self.state[0][..]))?,
            "input_states_2" => TensorRef::from_array_view((self.state_shape, &self.state[1][..]))?,
        ])?;
        let (_, logits) = out["outputs"].try_extract_tensor::<f32>()?;
        if logits.len() <= self.vocab_size {
            return Err(format!(
                "decoder output has {} logits, vocab is {}",
                logits.len(),
                self.vocab_size
            )
            .into());
        }
        let token = tdt::argmax(&logits[..self.vocab_size]);
        let duration = tdt::argmax(&logits[self.vocab_size..]);
        if token != self.blank {
            let (_, s1) = out["output_states_1"].try_extract_tensor::<f32>()?;
            let (_, s2) = out["output_states_2"].try_extract_tensor::<f32>()?;
            self.next_state[0].copy_from_slice(s1);
            self.next_state[1].copy_from_slice(s2);
        }
        Ok((token, duration))
    }

    fn accept(&mut self) {
        std::mem::swap(&mut self.state, &mut self.next_state);
    }
}
