# STT benchmarks (Phase 2 model spike)

Goal: pick the fastest accurate default speech-to-text engine/backend per platform, from real
numbers. Latency from hotkey release to pasted text matters most, then accuracy, then what it
costs to ship (AGT updates itself with Velopack, so every MB is downloaded by every user).

## Method

**Eval set** (`uv run scripts/prepare_eval.py`, checked against `scripts/eval_manifest.tsv`):
78 clips, 12.1 min, all real human speech from the ESB test sets as packaged for the Hugging
Face Open ASR Leaderboard (pinned revisions; only the needed parquet row groups are fetched).
Per source: 6 short clips (2-5 s), 6 medium (8-15 s) and 1 long (30-60 s, consecutive segments
of one recording). Chosen deterministically (hash order, at most one clip per speaker/recording).

| Source | What | Category | License |
|---|---|---|---|
| `ls-clean` | LibriSpeech test-clean: audiobooks | read | CC-BY-4.0 |
| `ls-other` | LibriSpeech test-other: harder audiobooks | read | CC-BY-4.0 |
| `ted` | TED-LIUM 3 test: TED talks | real | CC-BY-NC-ND-3.0 |
| `vox` | VoxPopuli: European Parliament speeches, many accents | real | CC0 |
| `earn` | Earnings-22: earnings calls, phone audio, spontaneous | real | CC-BY-SA-4.0 |
| `ami` | AMI: meetings, headset mics, very spontaneous | real | CC-BY-4.0 |

"Read" is the classic benchmark condition; "real" is closer to how people dictate. Caveats: the
AMI references only cover the headset wearer, so cross-talk a model (correctly) hears counts as
insertions; TED-LIUM and LibriSpeech references are normalized (lowercase, no punctuation) while
the others are cased and punctuated, which the normalizer evens out.

**WER normalizer** (`crates/bench/src/wer.rs`, applied to reference and hypothesis): lowercase;
`&` -> "and", `%` -> "percent", `$N` -> "N dollars"; numbers in digits to words (years like
1984 -> "nineteen eighty four", ordinals, decimals); hyphens split words; punctuation dropped
(inner apostrophes kept); hesitations dropped (uh, um, hmm, mm, mhm, er, ah, ...); mr/mister,
mrs/missus, dr/doctor, ok/okay and British "-isation" unified. Simpler than Whisper's English
normalizer; what it misses costs every engine about equally.

**Latency** is the wall time of one `SttEngine::transcribe` call on a warm engine (after
`warmup()`), i.e. release -> text minus paste: features, encoder, decoding and, for audio over
25 s, chunking. Each clip is transcribed 3 times (once for rows 2, 16 and 17) and its median
taken; p50/p90 are over the clips of a bucket: short (< 6 s, avg 3.4 s), medium (6-20 s, avg
10.7 s), long (> 20 s, avg 36.5 s). Runs were only started with no compiler running; the
desktop was otherwise in normal (light) use. **Load** is engine construction with the model
files in the OS cache (a cold first start after boot reads 0.65-2.5 GB more from disk).
**RSS** is the peak working set; **VRAM** the process's dedicated GPU memory at the end of the
run (Windows "GPU Process Memory" counters, which cover D3D12, CUDA and Vulkan alike).

**CPU means CPU**: `ort` defaults every session to ONNX Runtime's automatic device selection
with a "max efficiency" (prefer NPU) policy; CPU sessions now ask for `PreferCPU` explicitly
(same change as the Phase 4 macOS branch, where an M4's Neural Engine silently took nodes).
This machine has no NPU: the GPU memory counters show no GPU use in any CPU run, and re-running
the CPU rows with `PreferCPU` gave the same latency within noise (e.g. 141-154 ms short, 410-441
ms medium, both ways).

**Hardware**: Intel i9-9900K (8 cores / 16 threads), 32 GB, NVIDIA RTX 2080 Ti 11 GB (driver
617.14), Windows 11 Pro 26200. ONNX Runtime 1.31 (pyke prebuilt; DirectML build, CUDA 13
build for `cuda`), whisper.cpp 1.8.3 (whisper-rs 0.16), CUDA 13.4 / cuDNN 9.27 runtime DLLs.

## Results (Windows)

Warm latency in ms as p50/p90 per bucket; WER in %; load in s; memory in MB. "Download" is
the model, fetched on first use; "ships" is what the installer/update would have to carry on
top of today's release (which already contains ONNX Runtime and `DirectML.dll`).

| # | Engine, backend | short | medium | long | WER read | WER real | WER all | load | RSS | VRAM | download | ships |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | Parakeet v2 int8, CPU (Phase 1 default) | 152/206 | 407/517 | 1335/2021 | 2.23 | 5.19 | 4.09 | 2.7 | 1102 | - | 0.65 GB | - |
| 2 | Parakeet v2 fp32, CPU | 183/239 | 388/494 | 1263/1991 | 2.23 | 4.61 | 3.73 | 4.0 | 2602 | - | 2.5 GB | - |
| 3 | Parakeet v3 int8, CPU | 161/219 | 423/561 | 1351/2167 | 2.79 | 5.68 | 4.61 | 3.0 | 1112 | - | 0.67 GB | - |
| 4 | Parakeet v2 int8, DirectML, dynamic shapes (Phase 1) | 439/492 | 467/488 | 996/1489 | 1.96 | 5.10 | 3.94 | 4.0 | 807 | 1257 | 0.65 GB | - |
| 5 | Parakeet v2 fp32, DirectML, dynamic shapes (Phase 1) | 255/293 | 288/314 | 659/1002 | 2.23 | 4.61 | 3.73 | 3.8 | 479 | 2805 | 2.5 GB | - |
| **6** | **Parakeet v2 int8, DirectML, static shape (new default)** | **142/147** | **162/170** | **362/566** | **2.09** | **5.10** | **3.99** | 2.7 | 1033 | 1135 | 0.65 GB | - |
| 7 | Parakeet v2 fp32, DirectML, static shape | 101/108 | 120/127 | 284/426 | 2.23 | 4.69 | 3.78 | 3.3 | 400 | 2654 | 2.5 GB | - |
| 8 | Parakeet v3 int8, DirectML, static shape | 151/171 | 183/197 | 439/667 | 2.93 | 5.68 | 4.66 | 2.6 | 1034 | 1135 | 0.67 GB | - |
| 9 | Parakeet v3 fp32, DirectML, static shape | 111/120 | 145/196 | 368/533 | 2.09 | 6.83 | 5.08 | 2.8¹ | 415 | 2654 | 2.5 GB | - |
| 10 | Parakeet v2 int8, CUDA | 188/241 | 434/519 | 1272/1945 | 2.23 | 4.86 | 3.88 | 8.4 | 1436 | 671 | 0.65 GB | ~1.4 GB |
| 11 | Parakeet v2 fp32, CUDA | **30/35** | **59/72** | **220/335** | 2.23 | 4.61 | 3.73 | 3.1 | 895 | 3231 | 2.5 GB | ~1.4 GB² |
| 12 | Parakeet v3 fp32, CUDA | 40/46 | 83/104 | 292/453 | 2.09 | 6.91 | 5.13 | 2.8 | 904 | 3231 | 2.5 GB | ~1.4 GB² |
| 13 | Whisper large-v3-turbo f16, Vulkan | 112/122 | 161/187 | 462/687 | **1.82** | 7.82 | 5.59 | 2.1 | 340 | 1899 | 1.6 GB | +54 MB³ |
| 14 | Whisper large-v3-turbo q5_0, Vulkan | 126/135 | 172/199 | 444/682 | 1.82 | 7.49 | 5.39 | 0.9 | 222 | 899 | 0.57 GB | +54 MB³ |
| 15 | Whisper small.en q8_0, Vulkan | 66/83 | 138/174 | 433/688 | 3.35 | 7.41 | 5.90 | 0.6 | 233 | 590 | 0.26 GB | +54 MB³ |
| 16 | Whisper small.en q8_0, CPU | 2587/2756 | 2847/3106 | 6168/9376 | 4.89 | 7.98 | 6.84 | 0.3 | 598 | - | 0.26 GB | +1 MB |
| 17 | Whisper large-v3-turbo q5_0, CPU | 15852/16655 | 16315/17425 | 32313/49681 | 1.82 | 7.57 | 5.44 | 0.6 | 983 | - | 0.57 GB | +1 MB |

¹ 33.8 s the first time (2.5 GB read from disk). ² CUDA 13 runtime + cuBLAS/cuBLASLt + cuFFT +
cuDNN 9 DLLs (~1.35 GB uncompressed) plus ONNX Runtime's CUDA provider DLL (89 MB). ³ ggml's
Vulkan shaders are compiled into the exe (24.5 -> 78.5 MB); `vulkan-1.dll` comes with every GPU
driver. Whisper on Vulkan also spends 8-13 s compiling pipelines the very first time (warmup;
the driver caches them, later starts take ~0.1 s).

WER per source (%; ~250-360 words each, so ±1 point is noise):

| # | ls-clean | ls-other | ted | vox | earn | ami |
|---|---|---|---|---|---|---|
| 1 Parakeet v2 int8 CPU | 3.30 | 1.14 | 3.93 | 2.87 | 4.78 | 10.32 |
| 2 Parakeet v2 fp32 CPU | 3.30 | 1.14 | 4.21 | 3.18 | 4.78 | 6.75 |
| 3 Parakeet v3 int8 CPU | 1.92 | 3.69 | 3.37 | 2.55 | 8.19 | 9.92 |
| 6 Parakeet v2 int8 DirectML | 3.02 | 1.14 | 3.93 | 2.87 | 4.78 | 9.92 |
| 7 Parakeet v2 fp32 DirectML | 3.30 | 1.14 | 4.21 | 3.18 | 5.46 | 6.35 |
| 9 Parakeet v3 fp32 DirectML | 2.47 | 1.70 | 3.37 | 3.50 | 8.53 | 13.89 |
| 13 Whisper turbo f16 Vulkan | 1.65 | 1.99 | 4.21 | 3.50 | 4.10 | 22.62 |
| 15 Whisper small.en q8 Vulkan | 3.02 | 3.69 | 5.34 | 3.50 | 8.19 | 14.29 |

### What the numbers say

- **Read vs real speech.** Every engine is 2-4x worse on real speech than on audiobooks; the
  gap is mostly AMI (spontaneous meeting speech, cross-talk) and, for v3 and small.en,
  Earnings-22 (phone audio, numbers, names). On TED, VoxPopuli and Earnings-22 the good engines
  are within noise of each other.
- **Parakeet v2 is the most accurate on real speech** (4.6-5.2%); Whisper large-v3-turbo wins on
  read speech (1.8%) but loses on spontaneous speech: on the 31 s AMI clip it dropped whole
  phrases (86% WER) and it smooths over repetitions and false starts that the verbatim
  references keep. v3 (multilingual) costs ~0.5-1.5 points of English WER over v2.
- **int8 vs fp32** is the same within noise except on AMI (int8 ~3-4 points worse there).
- **The DirectML floor was dynamic shapes.** With a fixed input shape the encoder's operators
  are compiled once instead of on every call; short clips went from 439 to 142 ms (int8) and
  255 to 101 ms (fp32), and padding 3 s of audio to 25 s costs almost nothing (800 vs 2600
  padded frames: 102 vs 110 ms; the remaining ~100 ms is per-operator dispatch overhead).
- **CUDA is the fastest by far** (fp32: 30 ms for 3 s, 59 ms for 11 s), but needs ~1.4 GB of
  NVIDIA DLLs and only helps NVIDIA users. CUDA int8 is no faster than the CPU: ONNX Runtime
  runs the quantized MatMuls on the CPU.
- **Whisper on the CPU is out**: 2.6 s (small.en) and 16 s (large-v3-turbo) for a 3 s clip (it always encodes a
  30 s window; shrinking `audio_ctx` made large-v3-turbo loop and hallucinate). On Vulkan it is
  competitive with DirectML Parakeet on latency (small.en is the fastest short-clip non-CUDA
  option at 66 ms) but less accurate on real speech and adds 54 MB to the exe.
- Per-dictation time outside the encoder (CPU log-mel features + greedy TDT decoding) is
  10-40 ms for 3-11 s of audio; it's most of what remains on CUDA.

### DirectML: why fixed-length padding gave wrong transcripts

Phase 1 found that padding the features to a fixed length made DirectML fast but returned
wrong (here: empty) transcripts while the CPU was correct with the same padding. Findings:

- The padding itself is fine: CPU with fixed 27 s shapes gives identical transcripts, and the
  `length` input masks the padded frames (zeros) as intended.
- What breaks is DirectML's **graph fusion** (it compiles the whole static-shape graph into one
  DirectML graph). With `ep.dml.disable_graph_fusion = 1` the output is correct (WER equal to the
  CPU within noise, rows 6/7). Turning ONNX Runtime's graph optimizations off entirely is also
  correct (but slower); with basic optimizations (Level 1) and fusion on, it is wrong. So it's
  the fused graph after ORT's constant folding; most likely the length/mask computation (int64
  Range/Less/Where chain, which DirectML emulates with 32-bit) is folded or handled wrongly. We
  did not dig further: the unfused static graph is already 2.3-3x faster than dynamic shapes.

## Decision

**Windows default: Parakeet TDT 0.6B v2, int8, `stt.device = "auto"`** (= DirectML with the
static-shape encoder, falling back to the CPU when there is no usable DirectX 12 GPU).

- Latency: 142 ms / 162 ms / 362 ms p50 for 3 / 11 / 36 s, vs 152 / 407 / 1335 ms on the CPU;
  p90 is tighter too (147 vs 206 ms short). Only CUDA is faster.
- Accuracy: same model and WER as the Phase 1 default (best on real speech of everything tried).
- Size: nothing new to ship (DirectML is already in the release), same 650 MB model.
- Cost: 1.1 GB of VRAM while AGT runs (and ~1 GB RAM, as on the CPU). Existing configs keep
  the `device = "cpu"` written on their first run.
- Guards, since `auto` is now what every new Windows install runs:
  - DirectML asks for the **high-performance adapter** (the discrete GPU on a hybrid laptop;
    the default preference usually gets the integrated one).
  - At start-up, `auto` transcribes 3 s of audio on DirectML and on the CPU (best of two,
    after one untimed run) and keeps DirectML only if it takes at most 1.25x the CPU's time.
    Logged; here: `stt.device "auto": DirectML 142 ms, CPU 150 ms for 3 s of audio; using
    DirectML`. 3 s is the CPU's best case: the padded DirectML encoder costs the same for
    any length up to 25 s, the CPU's grows with the length, so a GPU that is roughly even at
    3 s is far ahead at 10 s (row 6 vs row 1), and one that pays several times the CPU's time
    for a 25 s window (an integrated GPU, a software adapter) loses. Not measured on an
    integrated GPU (this machine's is disabled), which is why it's a check in the code rather
    than a docs caveat. Cost: loading takes ~6 s instead of ~2.7 s (a second encoder session
    and six probe runs; in the background at start-up, before the tray says "Ready").
  - A GPU that fails to run (not only to load: driver reset, device removed, out of VRAM),
    at start-up or on any later dictation, is replaced by the CPU once and for good; logged,
    and noted in the tray only if `device` named the GPU explicitly.
- Options documented: `precision = "fp32"` for ~40 ms less on a GPU with VRAM to spare (2.5 GB
  download, 2.6 GB VRAM); the `cuda` build for NVIDIA power users (not shipped: +1.4 GB).
  A future option is a downloadable "CUDA pack" (the DLLs fetched like a model on first use of
  `device = "cuda"`), which would make row 11 available without growing the installer.
- Whisper stays an optional cargo feature (`whisper`, `whisper-vulkan`, `whisper-metal`) for
  languages Parakeet doesn't cover; it is not in release builds.

### macOS

Not decided here: this spike had no Apple Silicon hardware. For now `auto` is the CPU on
macOS (Parakeet v2 int8). The macOS default belongs to the Phase 4 work (issue #9), and the
full real-speech benchmark on a Mac to issue #11.

What there is: Phase 4 measured Parakeet v2 on a real **Apple M4** (Mac mini, 16 GB), warm
median of 5 runs, with **synthetic clips made with `say`** (0% WER everywhere, so they say
nothing about accuracy on real speech):

| Clip | Core ML (fp32, fixed 10 s window, GPU) | CPU int8 |
|---|---|---|
| 2.75 s | 74 ms | 75 ms |
| 9.1 s | 84 ms | 190 ms |
| 28.3 s | 320 ms | 600 ms |

Core ML there uses the same fixed-window trick as DirectML here (with dynamic shapes it was
no faster than the CPU) and needs the fp32 encoder (2.4 GB download, ~3 GB peak memory, vs
0.65 GB / 1.2 GB for CPU int8). The fixed window is shared code: an engine reports its window
as `SttEngine::max_input_secs` (DirectML 25 s; Core ML will report 10 s) and the chunker cuts
audio to fit it.

`.github/workflows/bench.yml` (manual only) runs the CPU and whisper.cpp Metal rows on
GitHub's macos-14 runner, a VM without GPU or Neural Engine access; its numbers are not used
for decisions.

## Reproducing

```sh
uv run scripts/prepare_eval.py                      # eval-data/clips, ~5 min
cargo build --release -p agt-bench                  # Parakeet: auto, cpu, directml (Windows)
target/release/agt-bench --device directml eval-data/clips
cargo build --release -p agt-bench --features cuda  # + CUDA 13 / cuDNN 9 DLLs on PATH
cargo build --release -p agt-bench --features whisper-vulkan   # see README "Build"
target/release/agt-bench --engine whisper --model whisper-large-v3-turbo-q5_0 --device vulkan eval-data/clips
```

The CUDA DLLs used here came from NVIDIA's PyPI wheels (`nvidia-cuda-runtime`,
`nvidia-cublas`, `nvidia-cufft` 13.x, `nvidia-cudnn-cu13`); on Windows the whisper build used
CMake 4.1, Ninja, libclang 18 from the `libclang` wheel, and Vulkan headers 1.4.328 + `glslc`
and `vulkan-1.lib` from conda-forge (no Vulkan SDK installer needed).
