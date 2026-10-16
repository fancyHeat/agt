//! OpenAI Whisper via whisper.cpp (`whisper-rs`), behind the `whisper` cargo feature.
//!
//! GPU backends are compile-time: `whisper-vulkan` (Windows/Linux) or `whisper-metal`
//! (macOS). With one compiled in, `stt.device = "vulkan" | "metal"` runs on the GPU
//! (whisper.cpp falls back to the CPU by itself if no device is usable).
//!
//! Windows build notes: bindgen needs libclang (`LIBCLANG_PATH`), and with CMake's default
//! Visual Studio generator the `cmake` crate drops the `/O2` release flags, leaving ggml
//! unoptimized (~7x slower); set `CMAKE_GENERATOR` (Ninja, or "Visual Studio 17 2022"
//! explicitly). `whisper-vulkan` also needs `VULKAN_SDK` (headers, `vulkan-1.lib`, `glslc`).
//!
//! Audio is always given to whisper in one call of at most 25 s (the [`crate::chunk`] wrapper
//! splits longer recordings), so it never needs whisper's own 30 s windowing or timestamps.

use crate::models::{self, ModelFile, Progress};
use agt_core::{
    config::{Device, SttConfig},
    stt::{Result, SAMPLE_RATE, SttEngine, TranscribeOpts, Transcript},
};
use std::time::Instant;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

macro_rules! ggml {
    ($name:literal, $size:literal, $sha:literal) => {
        ModelFile {
            name: $name,
            url: concat!(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/",
                $name
            ),
            size: $size,
            sha256: $sha,
        }
    };
}

/// `stt.model` -> ggml file, pinned to a revision of `ggerganov/whisper.cpp`.
#[rustfmt::skip]
const MODELS: &[(&str, ModelFile)] = &[
    ("whisper-large-v3-turbo", ggml!("ggml-large-v3-turbo.bin", 1624555275, "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69")),
    ("whisper-large-v3-turbo-q8_0", ggml!("ggml-large-v3-turbo-q8_0.bin", 874188075, "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1")),
    ("whisper-large-v3-turbo-q5_0", ggml!("ggml-large-v3-turbo-q5_0.bin", 574041195, "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2")),
    ("whisper-small.en", ggml!("ggml-small.en.bin", 487614201, "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d")),
    ("whisper-small.en-q8_0", ggml!("ggml-small.en-q8_0.bin", 264477561, "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067")),
];

/// The `stt.model` names this engine knows.
pub fn model_names() -> impl Iterator<Item = &'static str> {
    MODELS.iter().map(|(n, _)| *n)
}

fn model_file(name: &str) -> Result<&'static ModelFile> {
    MODELS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f)
        .ok_or_else(|| {
            let known: Vec<_> = MODELS.iter().map(|(n, _)| *n).collect();
            format!("unknown whisper model {name:?}; expected one of {known:?}").into()
        })
}

pub struct Whisper {
    name: String,
    state: WhisperState,
    threads: i32,
    english_only: bool,
}

impl Whisper {
    pub fn load(cfg: &SttConfig, progress: &mut dyn FnMut(Progress)) -> Result<Self> {
        let file = model_file(&cfg.model)?;
        let dir = cfg.models_path().join(&cfg.model);
        models::ensure(&dir, std::slice::from_ref(file), progress)?;

        let use_gpu = match cfg.device {
            Device::Cpu => false,
            Device::Auto => cfg!(any(feature = "whisper-vulkan", feature = "whisper-metal")),
            Device::Vulkan if cfg!(feature = "whisper-vulkan") => true,
            Device::Metal if cfg!(feature = "whisper-metal") => true,
            d => {
                let gpu = if cfg!(feature = "whisper-vulkan") {
                    " and \"vulkan\""
                } else if cfg!(feature = "whisper-metal") {
                    " and \"metal\""
                } else {
                    ""
                };
                return Err(format!(
                    "the whisper engine in this build can't use device {d:?}; it supports \"cpu\"{gpu}"
                )
                .into());
            }
        };
        // whisper.cpp/ggml log to stderr by default; route them to `tracing` (once).
        static HOOKS: std::sync::Once = std::sync::Once::new();
        HOOKS.call_once(whisper_rs::install_logging_hooks);
        let start = Instant::now();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(use_gpu);
        // Flash attention helps on GPUs; on the CPU backend it was slower in our tests.
        params.flash_attn(use_gpu);
        let path = dir.join(file.name);
        let ctx = WhisperContext::new_with_params(&path, params)
            .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        let state = ctx.create_state()?;
        let english_only = !ctx.is_multilingual();
        let threads = match cfg.threads {
            0 => std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(1)),
            n => n,
        } as i32;
        let device = match use_gpu {
            false => "cpu",
            true if cfg!(feature = "whisper-metal") => "metal",
            true => "vulkan",
        };
        let name = format!("{} ({device})", cfg.model);
        tracing::info!("loaded {name} in {:.2} s", start.elapsed().as_secs_f64());
        Ok(Self {
            name,
            state,
            threads,
            english_only,
        })
    }
}

impl SttEngine for Whisper {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn warmup(&mut self) -> Result<()> {
        // A short tone-free silence still runs the encoder and one decoder pass.
        self.transcribe(
            &vec![0.0; 2 * SAMPLE_RATE as usize],
            &TranscribeOpts::default(),
        )?;
        Ok(())
    }

    fn transcribe(&mut self, audio: &[f32], opts: &TranscribeOpts) -> Result<Transcript> {
        if audio.is_empty() {
            return Ok(Transcript::default());
        }
        let t0 = Instant::now();
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        let lang = if self.english_only {
            Some("en")
        } else {
            Some(opts.language.as_deref().unwrap_or("en"))
        };
        p.set_language(lang);
        p.set_translate(false);
        p.set_no_context(true);
        p.set_no_timestamps(true);
        p.set_single_segment(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        // No temperature fallback: one greedy pass, predictable latency.
        p.set_temperature_inc(0.0);
        // (Shrinking `audio_ctx` to the clip length would cut the encoder cost for short clips,
        // but large-v3-turbo then loops/hallucinates: WER went from 2% to >200% in our tests.)
        // whisper needs >= 1 s of audio; pad short clips with silence.
        let min = SAMPLE_RATE as usize + SAMPLE_RATE as usize / 10;
        let padded;
        let audio = if audio.len() < min {
            let mut v = audio.to_vec();
            v.resize(min, 0.0);
            padded = v;
            &padded[..]
        } else {
            audio
        };
        self.state.full(p, audio)?;
        let mut text = String::new();
        for seg in self.state.as_iter() {
            text.push_str(&seg.to_str_lossy()?);
        }
        tracing::debug!(
            "stt: whisper {:.1} ms for {:.2} s",
            t0.elapsed().as_secs_f64() * 1e3,
            audio.len() as f64 / SAMPLE_RATE as f64
        );
        Ok(Transcript {
            text: text.trim().to_owned(),
        })
    }
}
