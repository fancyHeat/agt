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
//!
//! A GPU encoder can be built for a fixed input window ([`fixed_window`]): static shapes let
//! the GPU compile the graph once instead of on every call. The features are zero-padded to
//! the window (the encoder masks by `length`) and the engine reports the window as its
//! [`SttEngine::max_input_secs`], so `agt-stt`'s chunker never passes more.
//!
//! ## Core ML (macOS)
//!
//! With dynamic input shapes, ONNX Runtime's Core ML EP can only take about half of the
//! encoder (291 partitions) and the rest bounces to the CPU: no faster than the CPU alone. With
//! the time axis fixed to [`COREML_WINDOW_SECS`] the whole graph compiles to one ML Program on
//! the GPU. Core ML needs the fp32 encoder (the int8 one uses ops it doesn't support). The
//! compiled model is cached next to the model (`coreml-<frames>/`), so only the first load pays
//! the ~20 s compile.

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
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

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
    Ok(match precision(cfg) {
        Precision::Int8 => &set.0,
        Precision::Fp32 => &set.1,
    })
}

pub struct Parakeet {
    name: String,
    /// Model and precision, e.g. `parakeet-tdt-0.6b-v2 int8`.
    label: String,
    preprocessor: Session,
    encoder: Encoder,
    decoder: Session,
    vocab: Vec<String>,
    blank: usize,
    /// Prediction-network LSTM state shape, e.g. `[2, 1, 640]`.
    state_shape: [usize; 3],
    /// What a CPU fallback needs to rebuild the encoder.
    env: Environment,
    encoder_path: PathBuf,
    threads: usize,
    /// `stt.device` named a device (not "auto"): a fallback is shown to the user.
    explicit: bool,
    /// The device the encoder fell back to the CPU from, if it did.
    fell_back_from: Option<Device>,
    notice: Option<String>,
}

/// The encoder session and how to feed it.
struct Encoder {
    session: Session,
    device: Device,
    /// Fixed input length in feature frames, if the session was built for a fixed window
    /// (see [`fixed_window`]): shorter features are zero-padded to it.
    fixed_frames: Option<usize>,
}

impl Encoder {
    fn new(env: &Environment, path: &Path, device: Device, threads: usize) -> Result<Self> {
        let fixed_frames = fixed_window(device);
        Ok(Self {
            session: session(env, path, device, threads, fixed_frames)?,
            device,
            fixed_frames,
        })
    }
}

/// What `stt.device = "auto"` tries first, kept only if it isn't slower than the CPU (see
/// [`Parakeet::load`]). Windows: DirectML. macOS: the CPU (int8). Core ML is only used when
/// asked for: on an M4 with real speech and a few seconds between dictations, the CPU's int8
/// encoder was ~2x faster for short clips (113 vs 215 ms p50) and as fast for 6-20 s ones,
/// more accurate (the 10 s window splits longer dictations), needs a quarter of the download
/// and no ~20 s compile. A warm, back-to-back GPU (what a start-up probe measures) hides the
/// ~90 ms Core ML pays after the GPU idles for a second or more. See docs/benchmarks.md.
fn auto_device() -> Device {
    if cfg!(windows) {
        Device::DirectMl
    } else {
        Device::Cpu
    }
}

/// The device `cfg` asks for, with `auto` resolved (before any fallback).
fn wanted_device(cfg: &SttConfig) -> Device {
    match cfg.device {
        Device::Auto => auto_device(),
        d => d,
    }
}

/// The encoder precision actually used: Core ML always runs the fp32 encoder.
fn precision(cfg: &SttConfig) -> Precision {
    match wanted_device(cfg) {
        Device::CoreMl => Precision::Fp32,
        _ => cfg.precision,
    }
}

/// Core ML's fixed encoder window (see the module docs): every chunk is padded to it, so it's
/// a trade-off. Measured on an M4 (GPU): 5 s ≈ 51 ms, 10 s ≈ 76 ms, 15 s ≈ 104 ms, 25 s ≈
/// 170 ms per encoder call, vs. ~100 ms (3 s) to ~330 ms (10 s) on the CPU. The padding
/// doesn't change the output; the chunk cuts do: with 10 s, dictations of 10-15 s are cut in
/// two and real-speech WER is ~1.3 points worse than the CPU's, with 15 s it equals the CPU's
/// (docs/benchmarks.md, window sweep).
pub const COREML_WINDOW_SECS: f32 = 10.0;

/// Under `auto`, the GPU is kept if its time for [`PROBE_SECS`] of audio is at most this
/// many times the CPU's. A short clip is the CPU's best case: a fixed-window GPU encoder costs
/// the same for any length up to its window, the CPU's cost grows with the length (3.4 s:
/// 142 vs 152 ms; 10.7 s: 162 vs 407 ms on an RTX 2080 Ti and an i9-9900K). So near-parity
/// on 3 s still means the GPU wins for everything longer; an integrated GPU that pays a 25 s
/// window for every dictation shows up as several times slower and loses.
const AUTO_GPU_TOLERANCE: f64 = 1.25;
/// Length of the warmup/probe audio (the typical short dictation).
const PROBE_SECS: f32 = 3.0;

const ENCODER_INT8: &str = "encoder-model.int8.onnx";
const ENCODER_FP32: &str = "encoder-model.onnx";

/// Feature frames the preprocessor makes from `secs` of audio, rounded up: one per 10 ms hop,
/// plus one (centered frames).
pub const fn window_frames(secs: f32) -> usize {
    let hops = secs * 100.0;
    let whole = hops as usize;
    if (whole as f32) < hops {
        whole + 2
    } else {
        whole + 1
    }
}

/// Seconds of audio that fit in `frames` feature frames (inverse of [`window_frames`]).
fn window_secs(frames: usize) -> f32 {
    (frames - 1) as f32 / 100.0
}

/// DirectML encoder window: the longest chunk [`crate::load`] passes plus 10 frames (2511
/// in all). The extra 0.1 s is what the benchmarks were measured with: on the CPU the padded
/// length doesn't change the output at all, but on DirectML it shifts the numerics slightly
/// (2501 frames: same latency, AMI WER 11.5 instead of 9.9%, i.e. within noise either way).
pub const DML_FRAMES: usize = window_frames(crate::CHUNK_SECS) + 10;
// A full chunk fits (the chunker cuts at `(CHUNK_SECS * SAMPLE_RATE) as usize` samples; the
// preprocessor makes one frame per 160 samples, plus one).
const _: () = assert!((crate::CHUNK_SECS * SAMPLE_RATE as f32) as usize / 160 < DML_FRAMES);

/// The fixed encoder input window (feature frames) a session on `device` is built for, if
/// any. Static shapes let a GPU compile the graph once; the features are zero-padded to the
/// window and `length` masks the padding. The engine reports the window as its
/// `max_input_secs`, so the chunker never passes more.
fn fixed_window(device: Device) -> Option<usize> {
    match device {
        Device::DirectMl => Some(DML_FRAMES),
        Device::CoreMl => Some(window_frames(COREML_WINDOW_SECS)),
        _ => None,
    }
}

/// A device this build can't run the encoder on at all (as opposed to one that failed).
#[derive(Debug)]
struct Unsupported(&'static str);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Unsupported {}

/// The encoder session failed while running (not the CPU preprocessor or decoder).
#[derive(Debug)]
struct EncoderFailed(String);

impl std::fmt::Display for EncoderFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EncoderFailed {}

fn device_name(d: Device) -> &'static str {
    match d {
        Device::Auto => "auto",
        Device::Cpu => "cpu",
        Device::DirectMl => "directml",
        Device::Cuda => "cuda",
        Device::CoreMl => "coreml",
        Device::Vulkan => "vulkan",
        Device::Metal => "metal",
    }
}

fn device_title(d: Device) -> &'static str {
    match d {
        Device::DirectMl => "DirectML",
        Device::Cuda => "CUDA",
        Device::CoreMl => "Core ML",
        _ => "the GPU",
    }
}

/// Faint noise: exercises every graph, including a few decoder steps.
fn probe_audio(secs: f32) -> Vec<f32> {
    let mut x = 0x1234_5678u32;
    (0..(secs * SAMPLE_RATE as f32) as usize)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 1e-3
        })
        .collect()
}

impl Parakeet {
    /// Downloads missing files, then loads the three sessions. A GPU encoder is run once
    /// here (which also compiles it), so a GPU that can create a session but not run it
    /// (driver bug, device removed, out of VRAM) falls back to the CPU instead of failing.
    /// Under `stt.device = "auto"` the GPU is only kept if it isn't slower than the CPU on
    /// a short clip (see [`AUTO_GPU_TOLERANCE`]).
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
        let precision = precision(cfg);
        if precision != cfg.precision {
            tracing::info!("Core ML uses the fp32 encoder (stt.precision is ignored)");
        }
        let encoder_path = dir.join(match precision {
            Precision::Int8 => ENCODER_INT8,
            Precision::Fp32 => ENCODER_FP32,
        });
        let env = ort::init().with_name("agt").build()?;
        let preprocessor = session(
            &env,
            &dir.join("nemo128.onnx"),
            Device::Cpu,
            cfg.threads,
            None,
        )?;
        let wanted = wanted_device(cfg);
        let explicit = cfg.device != Device::Auto;
        let mut notice = None;
        let mut fell_back_from = None;
        let encoder = match Encoder::new(&env, &encoder_path, wanted, cfg.threads) {
            Ok(e) => e,
            // No usable GPU (no D3D12 adapter, some RDP/VM sessions, broken driver) or a
            // device this build doesn't have: run on the CPU rather than fail.
            Err(e) if wanted != Device::Cpu => {
                tracing::warn!("{e}; running the encoder on the CPU instead");
                if explicit {
                    notice = Some(match e.downcast_ref::<Unsupported>() {
                        Some(why) => format!("on the CPU: {why}"),
                        None => format!("on the CPU, {} unavailable", device_title(wanted)),
                    });
                }
                fell_back_from = Some(wanted);
                Encoder::new(&env, &encoder_path, Device::Cpu, cfg.threads)?
            }
            Err(e) => return Err(e),
        };
        let decoder = session(
            &env,
            &dir.join("decoder_joint-model.int8.onnx"),
            Device::Cpu,
            1,
            None,
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

        let mut p = Self {
            name: String::new(),
            label: format!(
                "{} {}",
                cfg.model,
                match precision {
                    Precision::Int8 => "int8",
                    Precision::Fp32 => "fp32",
                }
            ),
            preprocessor,
            encoder,
            decoder,
            vocab,
            blank,
            state_shape,
            env,
            encoder_path,
            threads: cfg.threads,
            explicit,
            fell_back_from,
            notice,
        };
        p.update_name();
        if p.encoder.device != Device::Cpu {
            p.check_gpu(!explicit)?;
        }
        tracing::info!(
            "loaded {} in {:.2} s",
            p.name,
            start.elapsed().as_secs_f64()
        );
        Ok(p)
    }

    /// Runs the GPU encoder once (falling back to the CPU if that fails) and, if `auto`,
    /// keeps it only if it is not slower than the CPU.
    fn check_gpu(&mut self, auto: bool) -> Result<()> {
        let probe = probe_audio(PROBE_SECS);
        let gpu = self.encoder.device;
        let gpu_ms = match self.time(&probe) {
            Ok(t) => t,
            Err(e) => {
                return self.fall_back(&format!(
                    "the encoder failed its first run on {}: {e}",
                    device_title(gpu)
                ));
            }
        };
        if !auto {
            tracing::info!(
                "{}: {gpu_ms:.0} ms for {PROBE_SECS} s of audio",
                device_title(gpu)
            );
            return Ok(());
        }
        let cpu = match Encoder::new(&self.env, &self.encoder_path, Device::Cpu, self.threads) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("stt.device \"auto\": can't compare with the CPU ({e})");
                return Ok(());
            }
        };
        let gpu_encoder = std::mem::replace(&mut self.encoder, cpu);
        let cpu_ms = self.time(&probe)?;
        let keep_gpu = gpu_ms <= cpu_ms * AUTO_GPU_TOLERANCE;
        tracing::info!(
            "stt.device \"auto\": {} {gpu_ms:.0} ms, CPU {cpu_ms:.0} ms for {PROBE_SECS} s of audio; using {}",
            device_title(gpu),
            if keep_gpu {
                device_title(gpu)
            } else {
                "the CPU"
            }
        );
        if keep_gpu {
            self.encoder = gpu_encoder; // drops the CPU session
        } else {
            drop(gpu_encoder); // frees the VRAM
            self.update_name();
        }
        Ok(())
    }

    /// Best of two runs on `audio`, in ms, after one untimed run (which compiles a GPU
    /// encoder's kernels).
    fn time(&mut self, audio: &[f32]) -> Result<f64> {
        self.run(audio)?;
        let mut best = f64::MAX;
        for _ in 0..2 {
            let t = Instant::now();
            self.run(audio)?;
            best = best.min(ms(t.elapsed()));
        }
        Ok(best)
    }

    /// Rebuilds the encoder on the CPU after the GPU one failed.
    fn fall_back(&mut self, why: &str) -> Result<()> {
        let from = self.encoder.device;
        tracing::warn!("{why}; running the encoder on the CPU from now on");
        self.encoder = Encoder::new(&self.env, &self.encoder_path, Device::Cpu, self.threads)?;
        self.fell_back_from = Some(from);
        if self.explicit {
            self.notice = Some(format!("on the CPU, {} unavailable", device_title(from)));
        }
        self.update_name();
        Ok(())
    }

    fn update_name(&mut self) {
        self.name = match (self.encoder.device, self.fell_back_from) {
            (Device::Cpu, Some(from)) => {
                format!("{} (cpu; {} unavailable)", self.label, device_name(from))
            }
            (d, _) => format!("{} ({})", self.label, device_name(d)),
        };
    }
}

fn session(
    env: &Environment,
    path: &Path,
    device: Device,
    threads: usize,
    fixed_frames: Option<usize>,
) -> Result<Session> {
    let mut b = Session::builder(env)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| e.to_string())?;
    if threads > 0 {
        b = b.with_intra_threads(threads).map_err(|e| e.to_string())?;
    }
    if let Some(frames) = fixed_frames {
        // Fixed encoder input shape (see `fixed_window`). The names are the export's dynamic
        // axes: batch (both inputs) and time.
        for (axis, size) in [
            ("audio_signal_dynamic_axes_1", 1),
            ("length_dynamic_axes_1", 1),
            ("audio_signal_dynamic_axes_2", frames as i64),
        ] {
            b = b
                .with_dimension_override(axis, size)
                .map_err(|e| e.to_string())?;
        }
    }
    match device {
        Device::Cpu => {
            // `ort` sets ONNX Runtime's automatic device selection to `MaxEfficiency` (= prefer
            // an NPU) on every session, which on a Mac put a few hundred encoder nodes, and some
            // of the per-step decoder's, on Core ML (the Neural Engine) even for "cpu". Machines
            // without an NPU get the CPU either way.
            b = b
                .with_auto_device(ort::session::builder::AutoDevicePolicy::PreferCPU)
                .map_err(|e| e.to_string())?;
        }
        #[cfg(windows)]
        Device::DirectMl => {
            use ort::ep::directml::PerformancePreference;
            // The high-performance adapter: the discrete GPU on a hybrid laptop (the default
            // preference lets Windows pick, usually the integrated one).
            let dml = ort::ep::DirectML::default()
                .with_performance_preference(PerformancePreference::HighPerformance)
                .build()
                .error_on_failure();
            if fixed_frames.is_some() {
                // With dynamic shapes DirectML re-creates its operators on every run: a
                // ~250 ms floor per call, however short the audio. The fixed window lets it
                // compile them once. DirectML's whole-graph fusion of the static graph gives
                // wrong output (empty transcripts), so it's off; op by op it's still ~2.5x
                // faster than dynamic shapes. See docs/benchmarks.md.
                b = b
                    .with_config_entry("ep.dml.disable_graph_fusion", "1")
                    .map_err(|e| e.to_string())?;
            }
            // DirectML requires sequential execution and no memory-pattern optimization.
            b = b.with_memory_pattern(false).map_err(|e| e.to_string())?;
            b = b
                .with_parallel_execution(false)
                .map_err(|e| e.to_string())?;
            b = b
                .with_execution_providers([dml])
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(windows))]
        Device::DirectMl => return Err(Unsupported("DirectML is only available on Windows").into()),
        #[cfg(feature = "cuda")]
        Device::Cuda => {
            let cuda = ort::ep::CUDA::default().build().error_on_failure();
            b = b
                .with_execution_providers([cuda])
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(feature = "cuda"))]
        Device::Cuda => return Err(Unsupported("this build doesn't include CUDA support").into()),
        #[cfg(target_os = "macos")]
        Device::CoreMl => {
            use ort::ep::coreml::{ComputeUnits, ModelFormat};
            // The cache is keyed by the model path only: the window size goes in the folder
            // name so a different window never picks up a stale compiled model.
            let frames = fixed_frames.ok_or("Core ML needs a fixed encoder window")?;
            let cache = path
                .with_file_name(format!("coreml-{frames}"))
                .display()
                .to_string();
            // GPU: the Neural Engine took 5x longer on this fp32 graph, and "all" adds nothing.
            let coreml = ort::ep::CoreML::default()
                .with_model_format(ModelFormat::MLProgram)
                .with_compute_units(ComputeUnits::CPUAndGPU)
                .with_model_cache_dir(cache)
                .build()
                .error_on_failure();
            b = b
                .with_execution_providers([coreml])
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(target_os = "macos"))]
        Device::CoreMl => return Err(Unsupported("Core ML is only available on macOS").into()),
        Device::Auto | Device::Vulkan | Device::Metal => {
            return Err(Unsupported("the parakeet engine can't use this device").into());
        }
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
        self.transcribe(&probe_audio(PROBE_SECS), &TranscribeOpts::default())?;
        Ok(())
    }

    fn max_input_secs(&self) -> Option<f32> {
        self.encoder.fixed_frames.map(window_secs)
    }

    fn notice(&self) -> Option<String> {
        self.notice.clone()
    }

    fn transcribe(&mut self, audio: &[f32], _opts: &TranscribeOpts) -> Result<Transcript> {
        match self.run(audio) {
            // A GPU that worked at load can still fail later (driver reset/TDR, device
            // removed, out of VRAM): switch to the CPU for good and retry this audio once.
            Err(e) if self.encoder.device != Device::Cpu && e.is::<EncoderFailed>() => {
                self.fall_back(&format!(
                    "the encoder failed on {}: {e}",
                    device_title(self.encoder.device)
                ))?;
                self.run(audio)
            }
            r => r,
        }
    }
}

impl Parakeet {
    fn run(&mut self, audio: &[f32]) -> Result<Transcript> {
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
        // A fixed-window encoder takes exactly `fixed_frames` frames: zero-pad.
        let padded;
        let (feat_shape, feats) = match self.encoder.fixed_frames {
            Some(n) => {
                let (c, t) = (feat_shape[1] as usize, feat_shape[2] as usize);
                if t > n {
                    return Err(format!(
                        "{t} feature frames exceed the encoder's fixed {n} (chunking bug)"
                    )
                    .into());
                }
                let mut p = vec![0f32; c * n];
                for (dst, src) in p.chunks_exact_mut(n).zip(feats.chunks_exact(t)) {
                    dst[..t].copy_from_slice(src);
                }
                padded = p;
                (
                    ort::value::Shape::new([1i64, c as i64, n as i64]),
                    &padded[..],
                )
            }
            None => (feat_shape.clone(), feats),
        };
        let enc = self
            .encoder
            .session
            .run(ort::inputs![
                "audio_signal" => TensorRef::from_array_view((feat_shape, feats))?,
                "length" => TensorRef::from_array_view(([1usize], feat_lens))?,
            ])
            .map_err(|e| EncoderFailed(e.to_string()))?;
        let (enc_shape, enc_out) = enc["outputs"].try_extract_tensor::<f32>()?;
        let (_, enc_lens) = enc["encoded_lengths"].try_extract_tensor::<i64>()?;
        let (dim, frames) = (enc_shape[1] as usize, enc_shape[2] as usize);
        let valid = (enc_lens[0].max(0) as usize).min(frames);
        // Frame-major copy (of the valid frames only: a padded encoder has many more) so each
        // decoder step reads one contiguous [D] slice.
        let mut by_frame = vec![0f32; valid * dim];
        for d in 0..dim {
            for t in 0..valid {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_frames_round_up() {
        assert_eq!(window_frames(25.0), 2501);
        assert_eq!(window_frames(10.0), 1001);
        assert_eq!(window_frames(22.5), 2251);
        assert_eq!(window_frames(22.505), 2252);
        assert_eq!(window_secs(window_frames(25.0)), 25.0);
        assert_eq!(window_secs(window_frames(10.0)), 10.0);
    }

    /// Zero-padding the features to a fixed window (what a static-shape GPU encoder gets) must
    /// not change the transcript: `length` masks the padding. Runs on the CPU, with the v2 int8
    /// model (in `AGT_TEST_MODELS`, else the default models folder) and a real speech clip
    /// (`AGT_TEST_CLIP`, else `eval-data/clips/ls-clean-short-01.wav`; see
    /// `scripts/prepare_eval.py`). Skipped when either is missing (CI): nothing is downloaded.
    #[test]
    fn padding_does_not_change_the_transcript() {
        let mut cfg = SttConfig {
            device: Device::Cpu,
            ..SttConfig::default()
        };
        if let Some(d) = std::env::var_os("AGT_TEST_MODELS") {
            cfg.models_dir = d.to_string_lossy().into_owned();
        }
        let dir = cfg.models_path().join(&cfg.model);
        if let Some(f) = files(&cfg)
            .unwrap()
            .iter()
            .find(|f| !dir.join(f.name).exists())
        {
            eprintln!("skipped: {} not found", dir.join(f.name).display());
            return;
        }
        let clip = std::env::var_os("AGT_TEST_CLIP").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../eval-data/clips/ls-clean-short-01.wav")
            },
            PathBuf::from,
        );
        let Ok(mut wav) = hound::WavReader::open(&clip) else {
            eprintln!("skipped: {} not found", clip.display());
            return;
        };
        let spec = wav.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (SAMPLE_RATE, 1, 16),
            "16 kHz mono 16-bit clip expected"
        );
        let audio: Vec<f32> = wav
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        let mut p = Parakeet::load(&cfg, &mut |_| {}).unwrap();
        assert_eq!(p.max_input_secs(), None);
        let opts = TranscribeOpts::default();
        let plain = p.transcribe(&audio, &opts).unwrap().text;
        assert!(plain.split_whitespace().count() >= 3, "{plain:?}");

        // The same CPU encoder with the DirectML window: features zero-padded to 25 s.
        p.encoder = Encoder {
            session: session(
                &p.env,
                &p.encoder_path,
                Device::Cpu,
                p.threads,
                Some(DML_FRAMES),
            )
            .unwrap(),
            device: Device::Cpu,
            fixed_frames: Some(DML_FRAMES),
        };
        assert_eq!(p.max_input_secs(), Some(25.1));
        assert_eq!(p.transcribe(&audio, &opts).unwrap().text, plain);

        // A full chunk (the longest audio the chunker passes) fits the window. (Not the same
        // text: trailing silence in the *audio* changes the per-utterance feature
        // normalization, unlike padding the features.)
        let mut full = audio.clone();
        full.resize((crate::CHUNK_SECS * SAMPLE_RATE as f32) as usize, 0.0);
        let text = p.transcribe(&full, &opts).unwrap().text;
        assert!(text.split_whitespace().count() >= 3, "{text:?}");
    }
}
