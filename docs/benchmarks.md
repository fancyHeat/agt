# STT benchmarks (Phase 2 model spike; macOS: issue #11)

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

**Hardware** (Windows; the Mac is described in its own section): Intel i9-9900K (8 cores / 16 threads), 32 GB, NVIDIA RTX 2080 Ti 11 GB (driver
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

### With pauses between dictations (Windows)

The table above is back to back. As on the Mac (see below), a GPU that idles between
dictations is slower on the next one. Parakeet v2 int8, full eval set, one run per clip with
a 3 s pause before each (`--gap-ms 3000 --runs 1`) vs back to back (`--runs 3`), same session:

| Engine | short | medium | long | WER all |
|---|---|---|---|---|
| DirectML, back to back | 142/150 | 162/173 | 361/586 | 3.99 |
| DirectML, 3 s pauses | 210/227 | **230/252** | **444/622** | 3.99 |
| CPU, back to back | 169/229 | 429/558 | 1472/2182 | 4.09 |
| CPU, 3 s pauses | **171/216** | 436/559 | 1718/2072 | 4.09 |

8 short clips (2.2-3.1 s; LibriSpeech, TED, VoxPopuli, Earnings-22, AMI, as in the Mac
sweep), 3 runs each, per-call p50 in ms, `AGT_LOG=debug` for the encoder:

| Pause before each call | 0 (back to back) | 1 s | 1.5 s | 2 s | 2.5 s | 3 s | 10 s | 30 s¹ |
|---|---|---|---|---|---|---|---|---|
| DirectML: whole call | 148 | 151 | 150 | 214 | 230 | 219 | 202 | 211 |
| DirectML: encoder only | 138 | 141 | 141 | 204 | 219 | 208 | 199 | 203 |
| CPU int8: whole call¹ | 129 | 135 | 139 | - | - | 146 | 128 | 144 |
| CPU int8: encoder only¹ | 130 | 125 | 137 | - | - | 135 | 126 | 137 |

¹ From an earlier session (DirectML there: 141 back to back, 209 at 3 s). With a browser
using ~30% of the GPU, DirectML was 187 back to back and 260-298 ms at 2-5 s.

- DirectML pays **55-80 ms after 2 s or more of idle** and nothing up to 1.5 s (the 1.5 s p90
  already shows it); 3, 10 and 30 s cost the same. The same shape as Core ML (+90 ms from 3 s
  on), so most likely the GPU clocking down too. The CPU stays flat within noise on short and
  medium clips; its long ones are 17% slower after a pause (1718 vs 1472 ms).
- On short dictations after a pause the CPU wins (171 vs 210 ms, 3.4 s avg); DirectML wins
  medium ones by ~200 ms and long ones by more than a second. Break-even is around 4-5 s.
  Unlike the Mac, where the CPU int8 encoder is fast enough for medium dictations too, the
  i9's cost grows steeply with length, so DirectML stays the default.
- The old start-up probe compared 3 s back to back (DirectML's best case, ~1.0x the CPU);
  after a pause the short-clip ratio here is 1.23 (full eval) to 1.5 (sweep), at or past its
  1.25 limit, for a GPU that is far ahead on everything longer. The probe now times DirectML
  after a pause and weighs a medium clip too (see the Decision).

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

## Results (macOS, Apple Silicon)

Issue #11. Same eval set, normalizer and `agt-bench` as above, on a real Mac (Phase 4's Core
ML path: fixed 10 s encoder window, fp32, GPU; CPU sessions with `PreferCPU`).

**Hardware**: MacBook Air (Mac16,13, fanless), **Apple M4** (4 performance + 6 efficiency
cores, 10-core GPU), **16 GB**, **macOS 27.0.1** (26A434), on AC power, Low Power Mode off.
ONNX Runtime 1.31 (pyke prebuilt, Core ML EP), whisper.cpp via whisper-rs 0.16 with Metal.
The prepared clips matched `eval_manifest.tsv` (same clips and references; some WAV hashes
differ, i.e. a different decoder/resampler build).

**Method differences from the Windows runs**: every configuration ran in its own process,
3 processes each, in alternating order (round 2 reversed); numbers are the median over the
processes of each process's p50/p90. Another session was running interactive latency tests
of the AGT app on the same Mac at the same time, so the GPU was busy now and then; the
per-process spread is small (short-clip p50 within ±8 ms for Parakeet, except where noted).
**Memory**: `/usr/bin/time -l`: "footprint" is the peak *physical footprint* (dirty, private
memory: what counts against memory pressure), "RSS" the peak resident set, which also counts
clean, file-backed pages of memory-mapped model files. **Load**: engine construction with
the model files in the page cache; "first" is the first load ever (Core ML compiles the model
for this Mac and caches it in `<models>/<model>/coreml-<frames>/`).

### Warm, back to back (what the Windows table measures)

Each clip transcribed 3 times right after one another; ms p50/p90.

| # | Engine, backend | short | medium | long | WER read | WER real | WER all | load | footprint | RSS | download |
|---|---|---|---|---|---|---|---|---|---|---|---|
| M1 | Parakeet v2 fp32, **Core ML** (10 s window; Phase 4 default) | 102/109 | 187/222 | **438/680** | 1.96 | 5.93 | 4.45 | 5.1 s (3.6-8.9); first 43 s | 0.43 GB | 4.5 GB | 2.5 GB + 2.4 GB compiled |
| M2 | **Parakeet v2 int8, CPU** | **82/112** | 220/291 | 711/1115 | 2.23 | **4.20** | **3.47** | **1.0 s** | 1.37 GB | 1.4 GB | 0.65 GB |
| M3 | Parakeet v2 fp32, CPU | 95/105 | 208/261 | 626/1004 | 2.23 | 4.61 | 3.73 | 4.9 s¹ | 2.43 GB | 2.7 GB | 2.5 GB |
| M4 | Parakeet v3 fp32, Core ML (10 s window) | 92/99 | 185/202 | 427/691 | 2.51 | 5.60 | 4.45 | 6.6 s; first 30 s | 0.72 GB | 6.3 GB | 2.5 GB + 2.4 GB compiled |
| M5 | Parakeet v3 int8, CPU | 86/117 | 228/292 | 744/1130 | 2.79 | 5.93 | 4.76 | 0.9 s | 1.38 GB | 1.4 GB | 0.67 GB |
| M6 | Whisper large-v3-turbo q5_0, Metal | 1255/1468 | 1314/1479 | 2738/4196 | **1.82** | 7.49 | 5.39 | 0.35 s | 0.78 GB | 0.75 GB | 0.57 GB |
| M7 | Whisper small.en q8_0, Metal | 250/270 | 331/379 | 818/1263 | 3.35 | 7.65 | 6.06 | 0.2 s | 0.48 GB | 0.46 GB | 0.26 GB |

¹ ~5 s when another model ran just before it (the 2.5 GB file had left the page cache), 1.1 s
right after itself. Core ML's cached load varied 3.6-8.9 s between processes (it reads and
maps the compiled 2.4 GB copy; the other session's GPU use may add to it). Core ML short p50
was 88-103 ms over five back-to-back processes; M1 shows the median of the matrix's three.
Whisper turbo's second process was ~7% slower than the other two (1326 vs 1238/1255 ms short;
probably the other session), the median is unaffected.

WER per source (%; ~250-360 words each, so ±1 point is noise):

| # | ls-clean | ls-other | ted | vox | earn | ami |
|---|---|---|---|---|---|---|
| M1 Parakeet v2 Core ML (10 s window) | 3.02 | 0.85 | 4.49 | 3.50 | 5.46 | 11.51 |
| M1' Parakeet v2 Core ML, 15 s window | 3.30 | 1.14 | 4.21 | 3.18 | 6.48 | 4.76 |
| M2 Parakeet v2 int8 CPU | 3.30 | 1.14 | 3.93 | 2.87 | 4.78 | 5.56 |
| M3 Parakeet v2 fp32 CPU | 3.30 | 1.14 | 4.21 | 3.18 | 4.78 | 6.75 |
| M4 Parakeet v3 Core ML | 3.02 | 1.99 | 3.37 | 4.14 | 8.53 | 7.14 |
| M5 Parakeet v3 int8 CPU | 1.92 | 3.69 | 3.37 | 2.55 | 8.53 | 10.71 |
| M6 Whisper turbo q5_0 Metal | 1.65 | 1.99 | 4.21 | 3.50 | 4.10 | 21.03 |
| M7 Whisper small.en q8_0 Metal | 3.02 | 3.69 | 5.34 | 3.50 | 8.19 | 15.48 |

(The v2 int8 CPU AMI number is 5.6% here vs 10.3% on the i9: ONNX Runtime's int8 kernels
differ between ARM and x86, and AMI's long clip swings with a few words.)

### With pauses between dictations (what the app sees)

The app's log showed `stt ≈ 200 ms` for 2-2.5 s Core ML dictations while its start-up probe
measured ~85-90 ms for 3 s of audio. The difference is idle time: the probe's runs are back to
back, real dictations are seconds apart. `agt-bench --gap-ms N` sleeps N ms before every
transcription. 8 short clips (2.2-3.1 s; LibriSpeech, TED, VoxPopuli, Earnings-22, AMI),
per-call p50 in ms, `AGT_LOG=debug` per-stage timings:

| Pause before each call | 0 (back to back) | 1 s | 3 s | 10 s | 30 s | 5 min² |
|---|---|---|---|---|---|---|
| Core ML: whole call | 90 | 131 | 186 | 193 | 186 | 192 |
| Core ML: encoder only | 80 | 108 | 167 | 175 | 171 | 179 |
| CPU int8: whole call | 63 | 97 | 98 | 98 | 98 | 108 |
| CPU int8: encoder only | 57 | 86 | 85 | 88 | 86 | 93 |

² One clip, two calls each after 5 minutes without any AGT work (first calls: 212 and 110 ms).
The Mac wasn't idle as a whole (the other session).

- Core ML pays **~90 ms extra after a pause of 3 s or more**, already +30 ms after 1 s, and
  that's where it stays: 10 s, 30 s and 5 minutes cost the same. Nothing worse after a long
  idle (no "GPU wake-up" beyond that). The CPU pays ~35 ms once (the log-mel preprocessor goes
  from 1.5-3.5 to ~10 ms in both; it's also clock/cache warm-up) and stays flat as well.
- The pattern matches the GPU clocking down between bursts of work (an 80 ms encoder run is
  too short to ramp it back up) rather than anything in ORT; confirming it would need
  `powermetrics` (root). ORT puts 1443 of the encoder's 1458 nodes on Core ML in 5 partitions
  (the rest are shape ops on the CPU), the same in every run, so it isn't a partitioning
  change either.
- The app's start-up probe makes it worse: under `auto` it runs the Core ML encoder, then the
  CPU one (creating a 2.4 GB fp32 CPU session), then `warmup()` on Core ML again. That first
  Core ML call after the CPU detour took **660-1420 ms** in the encoder in the app's log.

The full eval set with a 3 s pause before every call (one run per clip, median of two
processes; Core ML 15 s window: one process):

| Engine | short | medium | long | WER all |
|---|---|---|---|---|
| Parakeet v2 Core ML, 10 s window | 215/238 | 290/322 | **506/750** | 4.45 |
| Parakeet v2 Core ML, 15 s window | 245/261 | 255/270 | 507/758 | 3.73 |
| **Parakeet v2 int8, CPU** | **113/145** | **239/286** | 668/1002 | **3.47** |
| Parakeet v2 fp32, CPU | 135/146 | 236/281 | 648/990 | 3.73 |

### Core ML window sweep (does the window change WER or only latency?)

Throwaway builds with `COREML_WINDOW_SECS` = 5 / 10 / 15 (the chunker cuts audio to the
window); back to back, ms p50/p90:

| Window | short | medium | long | WER read | WER real | WER all | first load |
|---|---|---|---|---|---|---|---|
| 5 s | 58/64 | 176/236 | 536/824 | 3.21 | 5.93 | 4.92 | 17 s |
| 10 s (current) | 88/98 | 177/186 | 392/668 | 1.96 | 5.93 | 4.45 | 43 s |
| 15 s | 122/134 | 134/141 | 426/670 | 2.23 | 4.61 | 3.73 | 19 s |

- **The padding doesn't hurt accuracy; the chunk cuts do.** With a 15 s window Core ML's WER
  equals the fp32 CPU's (2.23 / 4.61 / 3.73; per source Earnings-22 is 1.7 points worse and AMI
  2 points better, i.e. a few words each way). With 10 s, 21 of the
  36 medium clips (10-15 s) are cut in two and real-speech WER is 1.3 points worse; most of it
  is the AMI long clip (4 chunks instead of 2: Core ML transcribed cross-talk the headset
  reference doesn't contain, 49% vs 11% on that clip). With 5 s it's worse still on read speech.
  fp32 on Core ML's GPU itself is fine.
- Latency per window follows the encoder cost (~51 / 76 / 104 ms per call for 5 / 10 / 15 s,
  Phase 4) times the number of chunks.
- One of three first-ever loads of the 15 s model aborted the process inside Core ML
  (`MPSGraphTensorData.mm:266: failed assertion 'shape.count = 0 != strides.count = 3'`) during
  the first encoder run; two re-compiles from scratch and all later loads were fine. An abort
  can't be caught and turned into a CPU fallback.

### Memory and 8 GB Macs

- **Core ML's private memory is small** (0.39 GB physical footprint while transcribing, peaks
  0.4-0.8 GB), but it runs from a **2.5 GB memory-mapped compiled model** (clean, file-backed
  pages, so RSS is 4.5-6.3 GB at its peak while loading, when the ONNX file is mapped too).
  Under memory pressure macOS evicts those pages instead of swapping, and the next dictation
  reads them back from disk. Plus 2.4 GB of download and 2.4 GB of compiled cache on disk.
- CPU int8: 1.2-1.4 GB private memory (ONNX Runtime copies the weights), 0.65 GB on disk.
  fp32 on the CPU: 2.4 GB.
- On an 8 GB Mac (with a browser and an editor open), CPU int8's 1.4 GB is the comfortable
  choice; Core ML's working set is about twice that and degrades silently (page-ins) rather
  than failing. Not measured on an 8 GB machine.

### What the numbers say (macOS)

- **Core ML only wins back to back.** Warm and back to back it is ~15% faster than the CPU for
  6-20 s and ~40% for > 20 s, and no faster for short clips. With the pauses real dictation
  has, the CPU int8 encoder is **~2x faster for short clips** (113 vs 215 ms p50), faster for
  medium ones too (239 vs 290) and only slower for > 20 s (668 vs 506). Most dictations are short.
- **CPU int8 is also the most accurate** on real speech here (4.20%, Core ML 5.93%), loads in
  1 s instead of 4-9 s (plus ~45 s once to compile), and downloads 0.65 GB instead of 2.5 GB
  plus 2.4 GB of compiled cache.
- **Whisper on Metal is out**: large-v3-turbo needs 1.25 s for a 3 s clip on the M4's GPU (it
  always encodes a 30 s window; the RTX 2080 Ti did it in 126 ms), small.en 250 ms with worse
  real-speech WER than any Parakeet row.
- v3 behaves like on Windows: ~0.5-1.5 points worse English WER than v2 for the same latency.

## Decision

**Windows default: Parakeet TDT 0.6B v2, int8, `stt.device = "auto"`** (= DirectML with the
static-shape encoder, falling back to the CPU when there is no usable DirectX 12 GPU).

- Latency: 142 ms / 162 ms / 362 ms p50 for 3 / 11 / 36 s, vs 152 / 407 / 1335 ms on the CPU;
  p90 is tighter too (147 vs 206 ms short). Only CUDA is faster. With 3 s pauses between
  dictations: 210 / 230 / 444 vs 171 / 436 / 1718 ms (the CPU wins below ~4-5 s).
- Accuracy: same model and WER as the Phase 1 default (best on real speech of everything tried).
- Size: nothing new to ship (DirectML is already in the release), same 650 MB model.
- Cost: 1.1 GB of VRAM while AGT runs (and ~1 GB RAM, as on the CPU). Existing configs keep
  the `device = "cpu"` written on their first run.
- Guards, since `auto` is now what every new Windows install runs:
  - DirectML asks for the **high-performance adapter** (the discrete GPU on a hybrid laptop;
    the default preference usually gets the integrated one).
  - At start-up, `auto` compares DirectML and the CPU as a dictation sees them. The CPU
    transcribes 3 s and 10 s of audio, best of two back to back (short and medium clips
    barely slow after a pause). DirectML, after one untimed run that compiles it, runs the
    10 s probe once after 3 s of GPU idle; its padded window costs about the same for any
    length up to 25 s (242 vs 241 ms for 3 and 10 s), so that one run counts for both
    lengths. DirectML is kept if twice its time is at most the CPU's total. Logged; here:
    `stt.device "auto": DirectML 226 ms per call after 3 s idle (fixed window), CPU 168 +
    399 ms, for 3 + 10 s of audio; using DirectML` (452 vs 567 ms; 0.64-0.87 over 11
    loads). The sum weighs one short and one medium dictation: the CPU wins the short one
    after a pause, DirectML the medium one by more; a GPU that pays several times the CPU's
    time for a 25 s window (an integrated GPU, a software adapter) loses both. 3 s, not 2:
    this GPU's penalty starts between 1.5 and 2 s and 3 s is the measured plateau. Not
    measured on an integrated GPU (this machine's is disabled), which is why it's a check in
    the code rather than a docs caveat. Cost: the CPU's session and five runs (~3-4 s here)
    fill most or all of the pause, so loading takes 7.1-9.6 s vs 6.4-8.1 s with the old
    back-to-back probe and ~2.7 s without one (same session; three outliers at 11-22 s with
    the desktop busy), in the background at start-up, before the tray says "Ready".
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

**macOS default: Parakeet TDT 0.6B v2, int8, on the CPU** (`stt.device = "auto"` now means
the CPU on every Mac; no start-up probe). Phase 4 had made `auto` = Core ML on Macs with
>= 12 GB, kept only if a 3 s probe found it no slower than the CPU. The numbers above say
otherwise:

- Latency as the app sees it (seconds between dictations): CPU int8 113 / 239 / 668 ms p50 for
  short / medium / long vs Core ML 215 / 290 / 506. Core ML only wins on dictations over ~20 s.
  Its back-to-back advantage (what Phase 4's `say` clips and the start-up probe measured)
  disappears after ~1 s of GPU idle.
- Accuracy: CPU int8 has the best real-speech WER (4.20% vs 5.93% for Core ML's 10 s window).
- Cost: 0.65 GB download and ~1 s load, vs 2.5 GB + 2.4 GB of compiled cache, a ~45 s first
  compile and 4-9 s loads.
- The probe itself was a liability: it measures the GPU warm and back to back (Core ML's best
  case), costs ~6 s and a temporary 2.4 GB fp32 CPU session at every launch, left the first
  Core ML dictation at 0.7-1.4 s, and flipped to the CPU (fp32) while this benchmark was using
  the GPU (app log: "Core ML 149 ms, CPU 105 ms for 3 s of audio; using the CPU"). Windows
  keeps DirectML and a probe: the RTX 2080 Ti pays a similar 55-80 ms after a pause, but the
  i9's CPU is so much slower on longer dictations that DirectML still wins there (Windows
  pause section), and the probe now measures after a pause.
- **8 GB Macs**: the same default. CPU int8 needs ~1.4 GB; don't switch to Core ML or fp32
  there (Core ML's working set is ~2.9 GB including the mapped compiled model, and it degrades
  by paging rather than failing).
- `device = "coreml"` stays available for people who dictate long passages (> 20 s) on a
  16 GB+ Mac. Worth doing if it's kept: a 15 s window (same WER as the CPU, medium clips in one
  chunk; 122 vs 88 ms short back to back). Not changed here; the window sweep was a throwaway
  build.
- Existing installs: configs written by earlier builds say `device = "auto"`, so they move to
  the CPU with this change; the int8 encoder (0.65 GB) is downloaded on the next start if it's
  missing. The Core ML files stay on disk (`models/<model>/coreml-1001/`, `encoder-model.onnx*`)
  until deleted by hand.

`.github/workflows/bench.yml` (manual only) runs the CPU and whisper.cpp Metal rows on
GitHub's macos-14 runner, a VM without GPU or Neural Engine access; its numbers are superseded
by the section above and not used for decisions.

## Long dictations: transcribing while recording (issue #6)

Until now nothing is transcribed before the hotkey is released, so release -> text grows with
the length of the dictation (the 25 s chunks of `agt_stt::chunk` all run after release). This
spike transcribes finished pieces while the user is still talking, and asks where to cut, what
to do with long pauses, and which device wins once only a tail is left at release.

**Eval set** (`uv run scripts/prepare_long_eval.py`, checked against
`scripts/long_eval_manifest.tsv`): 12 synthetic long dictations, 2 per source of the set above
(78 s to 6 min, 35.2 min in all, 3,855 reference words). Each is consecutive utterances of one
recording (one speaker) with a pause after every utterance: 50% 0.3-1 s, 15% 1-2 s, 35% 2-30 s
(log-uniform), filled with the recording's own background noise (its 10 ms blocks near its
noise floor, tiled with crossfades), not digital silence. About 30% of the audio is pause; 78
pauses are over 1.1 s. VoxPopuli's "noise" is parliament murmur, which matters below. The
reference is the utterances' references joined; WER as above. AMI's references leave out the
cross-talk its headset mics pick up, which swings the AMI dictations by tens of points from
one cut to another, so totals are also given without AMI.

**What was built** (`agt-stt`): `chunk::Cutter`, fed with audio as it arrives (`push`), hands
out finished segments; `finish` on release hands out the rest. Pure (no threads, no I/O),
holds only the audio not handed out yet, and cancelling is dropping it. It cuts:

- at a **pause**: a silence of `len` ends the segment as soon as it has lasted that long, cut
  `pad` after the last speech, if the segment has at least `min_segment` of audio (shorter
  ones carry on past the pause);
- at the **limit** (25 s, or the engine's window if shorter) like `split`, at the quietest
  400 ms of the last 5 s. Without pause cuts it makes exactly `split`'s cuts (tested), so the
  post-release path (`Chunked`) is unchanged;
- with **trimming**, each pause's silence beyond `pad` either side is dropped instead of
  transcribed, and a tail that is only silence is not transcribed at all. Only once speech has
  been heard: until then nothing is dropped (see below).

Silence is a 10 ms level (median of the last 9 blocks, so clicks and 10-20 dB noise flutter
don't count) less than 15 dB above the noise floor (the lowest level of the last 20 s, kept
within -70..-40 dBFS). That makes it independent of the mic gain but not of the SNR: speech
has to stand out from the noise (tested down to ~25 dB peak to noise), and at low levels the
-70 dB clamp can take the end of the last syllable (~0.1 s) for the start of a pause. The floor
is learned from whatever comes first, so a recording that starts mid-sentence at a low level
can learn the speech itself as the floor; that's why nothing is trimmed before speech has been
heard (the usual worst case is transcribing some noise rather than dropping speech; unit-tested.
Still possible: a loud burst at the very start running gaplessly into quiet speech can still be
learned as the floor; real speech's word gaps normally set the floor first). On this
set it finds all 78 inserted pauses over 1.1 s and 33 natural 1 s pauses inside the
utterances (`agt-bench live --cuts-only`). `CutConfig::live` is the setting recommended below.

**Bench** (`agt-bench live`): replays each dictation as if recorded. Audio goes into the
cutter in 20 ms pieces; one worker transcribes the segments in order, each when it is ready
and the worker is free, then the tail at release. The transcriptions are real, and so is the
idle time between them: it is slept, up to 4 s per gap (an idle GPU pays the same after 3 s as
after 30 s, above), so the engine sees the gaps it would in the app. Release -> text is when
the last segment is done minus the end of the audio (no paste, no polish). "Today" is the
whole recording transcribed through `Chunked` after 4 s of idle. `--realtime` instead feeds the
audio at its real pace on a thread with a worker thread and a third thread doing small slices
of CPU work (a stand-in for the foreground app); on two dictations its release -> text matched
the timeline within 30 ms (CPU 451 vs 423 and 220 vs 199 ms, DirectML 237 vs 235 and 216 vs
203 ms). Windows: the machine above. Mac: a Mac mini M4 (Mac16,10), 16 GB, which another
session was also using now and then.

### Results

Release -> text in ms, p50/p90/max over the 12 dictations; WER in %, without AMI first (the
number to read; see above); compute is the total time spent in `transcribe`. "Cut at pauses"
is `CutConfig::live`: 1 s pause, 8 s minimum segment, 250 ms padding, trimmed. (Its WER is
from a re-run after a review fix to the cutter, which only changes how leading silence is
handled: one AMI word up or down, nothing outside AMI; the timings are from before it.)

| Platform, engine | Path | release -> text | WER w/o AMI | WER all | compute | segments (pause/limit/tail) | tail p50/p90 |
|---|---|---|---|---|---|---|---|
| Windows, CPU int8 | today, after release | 5710/11722/13732 | 7.33 | 8.17 | 86.2 s | - | - |
| | while recording, limit cuts only | 586/974/991 | 7.33 | 8.17 | 83.4 s | 0/86/12 | 13.8/24.7 s |
| | **while recording, cut at pauses** | **378/747/900** | **7.04** | 7.21 | 61.7 s | 80/12/12 | 9.5/15.4 s |
| Windows, DirectML int8 | today, after release | 1371/2729/3330 | 10.31 | 10.71 | 20.3 s | - | - |
| | while recording, limit cuts only | 239/262/267 | 10.31 | 10.71 | 25.9 s | 0/86/12 | 13.8/24.7 s |
| | **while recording, cut at pauses** | **228/268/271** | **7.01** | 7.19 | 26.3 s | 80/12/12 | 9.5/15.4 s |
| macOS, CPU int8 | today, after release | 2512/5441/6400 | - | 9.60 | 39.8 s | - | - |
| | while recording, limit cuts only | 295/528/531 | - | 9.60 | 44.4 s | 0/86/12 | 13.8/24.7 s |
| | **while recording, cut at pauses** | **229/360/456** | **7.10** | 7.29 | 32.8 s | 80/12/12 | 9.5/15.4 s |
| macOS, Core ML fp32 (10 s window) | today, after release | 1432/3188/4023 | - | 9.70 | 24.0 s | - | - |
| | while recording, limit cuts only | 125/144/148 | - | 9.70 | 39.4 s | 0/277/12 | 5.6/8.3 s |
| | while recording, cut at pauses | 124/143/145 | 7.16 | 7.55 | 26.0 s | 23/155/12 | 5.7/9.3 s |

On the Windows CPU the WER difference (7.33 -> 7.04% without AMI, ~9 words of ~3,100 in 12
synthetic dictations) is noise: read it as "no regression". The real accuracy change is on
DirectML and the Mac CPU, where today's path drops whole sentences next to long silence and
trimming fixes it (10.31 -> 7.01% on DirectML; the Mac's 9.60% "all" is the same effect, see
below).

Memory: Windows CPU peak RSS 1.37 GB for the whole run (0.9 GB in the real-time runs, which
only transcribe while recording); DirectML 1.16 GB + 1.1 GB VRAM; Mac CPU 1.33 GB footprint;
Core ML 4.4 GB RSS (the memory-mapped compiled model), 2.8 GB footprint. The cutter itself
holds at most 25 s of audio (1.6 MB); all segments are at most 25 s, so nothing grows with the
length of the dictation except the text.

**Where to cut** (Windows CPU int8, back to back, so only WER, cut counts and relative compute
mean anything; after the review fix):

| Pause cut | min segment | trim | segments (pause/limit/tail) | tail p50/p90 | trimmed | compute | WER w/o AMI | all | vox |
|---|---|---|---|---|---|---|---|---|---|
| none (limit cuts only) | - | - | 0/86/12 | 13.8/24.7 s | 0% | 96.7 s | 7.33 | 8.17 | 5.26 |
| 0.5 s | 0 | yes | 285/1/11 | 3.5/13.3 s | 33% | 72.5 s | 9.06 | 9.36 | 14.04 |
| 0.7 s | 0 | yes | 231/1/11 | 8.8/14.3 s | 32% | 69.3 s | 8.53 | 8.90 | 13.60 |
| 1 s | 0 | yes | 159/7/12 | 9.5/14.5 s | 31% | 70.3 s | 7.39 | 7.91 | 7.02 |
| 1.5 s | 0 | yes | 107/15/12 | 8.3/14.5 s | 30% | 67.3 s | 7.30 | 8.02 | 4.82 |
| 2 s | 0 | yes | 70/29/12 | 9.5/18.6 s | 28% | 69.2 s | 7.25 | 7.73 | 4.39 |
| 0.5 s | 8 s | yes | 110/1/12 | 8.8/13.3 s | 33% | 64.7 s | 7.07 | 7.34 | 3.51 |
| 0.7 s | 8 s | yes | 99/3/12 | 9.5/14.5 s | 32% | 65.0 s | 7.07 | 7.19 | 4.39 |
| 1 s | 3 s | yes | 128/7/12 | 9.5/14.5 s | 31% | 67.9 s | 7.27 | 7.55 | 6.58 |
| 1 s | 5 s | yes | 111/7/12 | 9.5/14.5 s | 31% | 66.0 s | 7.07 | 7.42 | 3.07 |
| **1 s** | **8 s** | **yes** | 80/12/12 | 9.5/15.4 s | 31% | 66.1 s | **7.04** | 7.21 | 3.07 |
| 1 s | 12 s | yes | 57/15/12 | 13.3/18.2 s | 31% | 64.5 s | 6.98 | 7.37 | 3.95 |
| 1 s | 16 s | yes | 39/23/12 | 14.3/18.2 s | 31% | 65.4 s | 7.04 | 7.44 | 3.07 |
| 1.5 s | 8 s | yes | 64/18/12 | 10.5/17.1 s | 30% | 66.5 s | 7.16 | 7.65 | 3.07 |
| 2 s | 8 s | yes | 51/29/12 | 10.1/19.9 s | 28% | 67.7 s | 7.04 | 7.68 | 3.07 |
| 1 s | 0 | no | 159/20/12 | 13.2/18.0 s | 0% | 100.9 s | 7.39 | 8.12 | 7.46 |
| 1 s | 8 s | no | 99/27/12 | 13.2/21.7 s | 0% | 94.4 s | 7.01 | 7.89 | 3.07 |

The untrimmed rows also transcribe pure-noise segments: after a pause cut the rest of a long
pause stays in the buffer and becomes `Limit` segments of room noise (e.g. 10 s of speech,
45 s of silence, 5 s of speech gives a 20.5 s segment of only noise). That is the setting with
the most hallucination risk; it exists to measure trimming, not to ship.

**Running alongside the user** (`--realtime`, cut at pauses; `ted-dict-01`, 141 s, and
`earn-dict-01`, 112 s): "cores" is process CPU time over wall time while a segment is
transcribed; "feeds late" is how late the 20 ms audio pieces were handed to the cutter (a
stand-in for capture), p99/max; the foreground slice is ~0.26 ms of single-threaded work timed
in a loop, p50 (p90), with the worker idle vs transcribing.

| Machine, engine | release -> text | cores | feeds late p99/max | foreground slice idle vs busy |
|---|---|---|---|---|
| Windows, CPU int8 (ORT default threads) | 451, 220 ms | 5.7 | 0.6/2.4 ms | 0.26 (0.26) vs 0.26 (0.27) ms |
| Windows, CPU int8, `threads = 4` | 510, 246 ms | 4.0 | 0.7/2.2 ms | 0.26 (0.26) vs 0.26 (0.26) ms |
| Windows, DirectML int8 | 237, 216 ms | 1.3 | 0.6/1.3 ms | 0.26 (0.26) vs 0.26 (0.26) ms |
| macOS, CPU int8 | 223, 106 ms | 4.2-4.7 | 9.9/16.1 ms¹ | 0.27 (0.31) vs 0.30 (0.43) ms |

¹ macOS oversleeps a plain thread's 20 ms sleep by ~6.7 ms at p50 whether or not anything is
being transcribed (timer coalescing); real capture runs on Core Audio's real-time thread.

**Polish on long text** (`agt-bench polish --provider s1-mini --runs 1`, Windows, S1-mini's
default device, the 12 transcripts of 95-570 words): p50 1168 ms, p90 2145 ms, max 4544 ms
(363 words; the others 413-2145 ms, roughly 2-4 ms per word).

### What the numbers say (long dictations)

- **Transcribing while recording works.** Release -> text no longer grows with length: for 1-6
  minute dictations it goes from 5.7 s p50 / 13.7 s max (CPU) and 1.4 / 3.3 s (DirectML) to
  0.38 / 0.9 s and 0.23 / 0.27 s, on the Mac CPU from 2.5 / 6.4 s to 0.23 / 0.46 s. What is
  left at release is the audio since the last pause cut, 9.5 s p50 and 15 s p90 here (the last
  burst of speech before release). The worker was never busy at release; it had been idle
  ~12 s (p50), so a GPU pays its idle penalty on the tail, and that is included above.
- **Cuts at pauses between utterances don't hurt; short segments do.** With an 8 s minimum
  segment every pause length from 0.5 to 2 s is within noise of today's path (7.0-7.2% without
  AMI vs 7.3%). Without a minimum, 0.5-0.7 s cuts are worse (8.5-9.1%), almost all of it
  VoxPopuli: a 2-3 s segment of background murmur on its own comes back as "Okay. Yeah. So
  okay." while the same murmur next to real speech is ignored. That's Phase 1's boundary
  hallucination; the minimum segment is what fixes it, not a longer pause. With 8 s, 0.5-2 s
  all score within ~0.15 points, so the pause length is chosen on other grounds: 1 s makes
  fewer calls than 0.5-0.7 s (80 vs 99-110 pause cuts in 35 min) and is less likely to fire on
  a hesitation inside a sentence, while 1.5-2 s make the tail longer (p90 17-20 s vs 15 s).
- **What this doesn't show.** Every inserted pause sits between two of the corpus's
  utterances, usually at a sentence end; real thinking pauses often come mid-sentence ("I think
  we should ... move it to Friday"). The only mid-utterance cuts measured are the 13 "pause
  cuts elsewhere" of the recommended setting. And the WER normalizer drops case and
  punctuation, so the typical boundary artifact, the model closing the clause and capitalizing
  the next segment ("we should. Move it"), is invisible here. The wiring phase should look at
  the joined cased text of real long dictations (and whether polish repairs such joins).
- **Trimming pauses removes a failure mode, and on the CPU a third of the compute.**
  Untrimmed, on the CPU the WER is about the same (7.01 vs 7.04% without AMI), but long silence
  inside a chunk makes the model drop whole sentences on DirectML and the Mac CPU: `ls-clean-dict-02`
  (4 min, 3 pauses of 3-25 s) is 19.8% (DirectML) and 14.5% (Mac CPU) today and with limit cuts,
  1.4% and 2.0% trimmed ("The late astounding events, however, had rendered Procope manifestly
  uneasy..." is missing entirely from the untrimmed transcript). That is today's post-release
  path too: trimming would help it regardless of incremental transcription. The compute saving
  is CPU-only: on DirectML (20.3 -> 26.3 s) and Core ML (24.0 -> 26.0 s; 39.4 s with limit cuts
  only) the total goes *up* compared with one pass after release, because more, smaller calls
  each pay the GPU's idle wake-up. Fine for latency, but it is background GPU work (and
  battery) that grows with the number of segments.
- **The device question doesn't flip on Windows.** The tail is a medium-length clip (p50 9.5
  s), and after a pause DirectML wins medium clips (Windows pause section). While recording,
  DirectML is 228/268 ms vs the CPU's 378/747 ms; the CPU default would cost ~150 ms at p50 and
  ~500 ms at p90 on long dictations, on top of the ~200 ms it already loses on 5-20 s ones. In
  exchange it would save the 1.1 GB of VRAM, the ~4-7 s start-up probe and DirectML's
  failure modes. Background work is also lighter on DirectML (0.3-1.3 cores vs 4-6).
- **Medium dictations without a pause (5-20 s)** don't change at all: the whole clip is the
  tail. Their tradeoff is the existing one: on Windows DirectML 230 vs CPU 436 ms after a pause
  (CPU wins only below ~4-5 s); on the Mac the CPU wins (239 vs 290 ms) and the Core ML window
  (10 s) caps every tail at 10 s, which is why Core ML's long-dictation tails are short (124 vs
  229 ms) - at more WER (7.55 vs 7.29%), 2.8 GB of footprint and slow loads.
- **A short tail after a pause cut is transcribed on its own.** The 8 s minimum guards the
  segments cut at pauses, not the tail: 10 s of speech, a pause cut, then a cough or "um" just
  before release gives a ~1 s `End` segment, the kind of short segment that hallucinates. Not
  measured (every tail here is a whole utterance); the proposed tail-length telemetry will show
  how often tails are that short. (The app now joins such tails to the segment before them:
  see "In the app" below.)
- **No contention worth worrying about.** On the i9 the CPU worker uses 4-6 of 16 hardware
  threads while transcribing a segment, and neither the 20 ms feeds (p99 0.6 ms late) nor a
  foreground thread slowed measurably; `threads = 4` only made the tail 30-60 ms slower. On the
  M4 a foreground thread was ~10% slower at p50 (~40% at p90) while a segment was being
  transcribed, i.e. for ~0.1-0.5 s every 10-25 s of dictation.
- **Polish is now the long pole** for long dictations: 1.2 s p50, 2.1 s p90 for a few hundred
  words, 5-10x the STT tail. It still needs the whole text (sentence boundaries and style
  depend on context), so it stays a single call at release; polishing finished segments in the
  background is the next lever if long dictations feel slow, but it was not tried here (it is
  in "Polish speed" below).

### Recommendation for the app wiring phase

- **Cut** with `CutConfig::live(max_secs)`: 1 s pause, 8 s minimum segment, 250 ms padding,
  pauses trimmed, the 25 s limit cut as the fallback for monologues (`max_secs` =
  `min(CHUNK_SECS, engine.max_input_secs())`, i.e. 10 s on Core ML). Feed the cutter from the
  thread that drains the recorder, in callback-sized pieces (a 10 ms block costs microseconds),
  not from the audio callback itself.
- **Transcribe** each segment on the existing STT worker as soon as the cutter hands it out (a
  channel; the engine stays single-threaded), in order; on release push the `finish()` tail,
  wait for the queue to drain and join the texts with spaces, then polish and paste as today.
  Short dictations take the same path: a 3 s recording is one `End` segment, its trailing
  silence trimmed to 250 ms and its leading silence kept (nothing is trimmed before speech is
  heard). On the regular eval set (78 clips of 2-60 s) that gives the same WER as today without
  AMI (3.16%; 4.19 vs 4.09% with it, two AMI words; measured before the review fix, which only
  keeps more leading silence).
- **Before shipping**, check the joined text for boundary artifacts on real dictations with
  mid-sentence pauses (see above), and consider a guard for short tails (e.g. if the tail is
  under ~2 s and the previous segment is still queued, transcribe them together).
- **Cancel / Esc / short tap / focus change**: drop the cutter and the queued segments and bump
  a generation number that the worker checks before it delivers or starts anything; the call
  already running is not interrupted (at most ~1 s on the i9 CPU for a 25 s segment, ~0.3 s
  on DirectML) and its text is discarded. A new dictation just queues behind it.
- **Recording cap**: `audio::MAX_SECONDS` (5 min) was a memory and context guard for
  post-release chunking; with segments transcribed on the way, 30 minutes of audio is 115 MB
  and the release cost doesn't grow, so it can be raised (the history WAV is the only full copy
  needed, and only when the user turned it on).
- **Devices**: unchanged. Windows keeps `auto` (DirectML, start-up probe); the CPU would be
  simpler but is ~150/500 ms slower at p50/p90 on long dictations and ~200 ms on medium ones on
  this i9. Worth re-measuring the probe's verdict on a recent 8-core CPU, where the gap may
  close. macOS stays on the CPU (int8). ORT threads stay at the default.
- **Telemetry** (metrics only, always on, never audio or text): per dictation the recording
  length, segments by cut (pause / limit / end), seconds trimmed, tail length, release -> text
  STT time (tail plus any wait), total background STT time, whether the worker was still busy
  at release and how long it had been idle, the longest queue, and for cancels the discarded
  in-flight time. That shows the tail distribution and the idle-penalty share for real users,
  which this synthetic set can only approximate.

### In the app (status)

Wired in as recommended above (`pipeline.rs`, `agt_stt::chunk::Live`): every 250 ms the
audio thread takes the new audio from the recorder and pushes it into the cutter
(`CutConfig::live`, segments as long as the engine's chunks: 25 s, 10 s on Core ML); each
finished segment goes to the STT worker at once; at release the tail goes, the texts are
joined and polished once. A recording with no cut by release (anything under ~25 s without a
1 s pause after 8 s) is transcribed whole after release, exactly as before. Cancelling drops
the cutter; segments still queued are skipped and the text of one being transcribed is
discarded (a flag per recording), while an earlier dictation still queued pastes in order.
The recording cap went from 5 to 30 minutes (115 MB of samples at most; the recorder's
callback buffer is emptied every 250 ms so a long recording is never reallocated inside the
audio callback). Telemetry: `release_to_text_ms` and the `live_*` columns of `dictations` (for a cancelled one too, as
far as it got). Polish is skipped (`polish_outcome = too_long`) for a transcript over 1,400
words with S1-mini: its 4096-token context holds about that much plus the answer, and a
longer one would only fail after generating until the timeout (25-70 s for 10-30 minutes,
blocking the next dictation).
`agt-bench live` runs the same `Live` (`--no-guards` for the bare cutter, `--joins` for the
check below).

`Live` adds two guards to the cutter (review follow-ups of the spike):

- **Nothing worth transcribing.** A segment in which the cutter heard no speech (no 10 ms
  level 15 dB over the floor) isn't transcribed, but only once speech has been heard in the
  recording: until then the floor may be the speech itself (see above), so such segments are
  held back and, if no speech is ever heard, transcribed after all. On the 12 dictations it
  never fires (the WER is unchanged, 7.04/7.21% CPU, nothing skipped). On 21 clips made of a
  long inserted pause (13-29 s) followed by 10 s of the next utterance it skipped 7 noise
  segments, which removed "I'll hit the top." (LibriSpeech room noise, also in today's
  output), and changed no word of the speech. It can't help a recording that is only noise
  (nothing is heard, so everything is kept: the same as today), nor murmur, which counts as
  speech: VoxPopuli's parliament murmur alone still comes back as "Send send send then."
  where today's single pass returns nothing.
- **Short tails.** A tail of 2 s or less (padding included: about 1.5 s of sound) after a
  cut is transcribed together with the segment before it, whose text it replaces. On 46
  dictations cut 0.5 or 1 s into the utterance after a long pause, 21 tails were joined; on
  their own, six of those tails (half a second or so of a word) came back as made-up words,
  "Mm-hmm." (4 times), "Yeah." or "That's why.", which the joined call didn't produce; the
  rest differed by a few words at most (with the context, "presumption" became the right
  "presentation" once).
  The cost is the previous segment transcribed again at release: release -> text p50 106 ->
  462 ms, worst ~1.3 s on the i9 CPU (DirectML ~0.2-0.3 s). The tails of the 12 dictations
  are whole utterances: none was joined. `live_tail_secs` / `live_tail_joined` will show how
  often real ones are this short.

**Sentence breaks at joins** (`--joins`, CPU, the 12 dictations): 92 joins. At the 67 cuts
inside an inserted pause the joined text has a sentence break (period or question mark, then
a capital) 53 times, as expected between utterances. At the 13 pause cuts inside an
utterance there are 7 breaks, all where the utterance itself ends a sentence ("...and our
YouTube channel. | Let's get started."). At the 12 limit cuts there are 7, two of them stray:
"according to the user. | Choice. So if he wants" and "He wanted to. | Capture the emotion of
gaming", where the single pass has neither. So about 2 in 92, both at limit cuts (a
monologue with no 1 s pause in 25 s), which today's post-release chunking makes the same way
at its own 25 s cuts. Not fixed: lowercasing after a join would break the real sentence
starts, which are far more common; polish sees the whole text and can repair it.

## Polish speed

With long dictations transcribed while recording, release -> text is 0.2-0.4 s and the
S1-mini polish (on by default; 1.2 s p50, 2.1 s p90, 4.5 s max above) is the long pole. S1-mini is
Qwen3-0.6B (Q4_K_M, 485 MB) run by AGT's own `llama-server` (llama.cpp b11669); its output is
the transcript again with small edits, so a request is almost all decoding: 100-680 prompt
tokens are read in 20-50 ms, then it writes about as many tokens one by one (~3.2 ms each on
the RTX 2080 Ti, ~6.5 ms on the M4).

**Inputs.** The 12 long dictations above as Parakeet (DirectML) transcribed them while
recording (83-569 words, 3,807 polished words; `agt-bench live --texts --segments`), and the
78 clips of the regular set (2-60 s, 1,883 words). Latency is the first request for each
transcript (the same text again hits llama-server's caches), 3 s idle before each, after a
warm-up pass over 30 other clips; p50/p90/max over transcripts. Windows: the i9-9900K + RTX
2080 Ti above, Vulkan (what AGT ships). Mac: the M4 mini, Metal. Flags were tried with
`scripts/polish_sweep.py` (llama-server with AGT's fixed flags plus the ones under test, S1-mini's
prompt and AGT's request); what ships was then measured with `agt-bench polish`.

### Results

**Speculative decoding from the prompt** (`--spec-type ngram-simple`, in b11669): find the
last *n* generated tokens earlier in the context (here: in the transcript) and propose the *m*
that followed them; one batched forward pass checks all of them and keeps the ones the model
would have chosen anyway. No draft model, no extra memory. Long transcripts, Windows (warm-up
pass, back to back):

| Drafting | p50/p90/max ms | tokens/s | drafts accepted |
|---|---|---|---|
| none (today) | 1244/2283/2330 | 315 | - |
| `ngram-simple` defaults (n 12, m 48) | 674/1214/1824 | 528 | 48% |
| n 2, m 16 | 633/947/1187 | 680 | 50% |
| n 3, m 8 | 743/1117/1318 | 618 | 77% |
| n 3, m 16 | 608/874/1179 | 725 | 62% |
| n 4, m 8 | 704/1105/1323 | 619 | 81% |
| n 4, m 16 | 628/1065/1517 | 623 | 67% |
| n 4, m 32 | 568/1033/1422 | 675 | 51% |
| n 6, m 16 | 585/1045/1312 | 659 | 72% |
| n 8, m 16 | 671/1179/1517 | 575 | 76% |
| `ngram-mod` defaults | 772/1595/2014 | 434 | 43% |
| `ngram-map-k` defaults (no warm-up pass) | 1058/2059/2085 | 341 | 47% |

With 3 s idle before each request, which separates the close ones (two runs each where shown):

| Drafting | Windows p50/p90/max ms | Mac p50/p90/max ms |
|---|---|---|
| none (today) | 1200/2235/2371 | 2459/4753/4787 (154 tokens/s) |
| n 3, m 8 | - | 1227/1922/2242 |
| n 3, m 16 | 623/1045/1209, 593/919/1206 | 1323/2088/2623, 1316/2059/2648 |
| n 4, m 16 | 622/995/1245 | 1257/2146/2568 |
| **n 3, m 24** | **520/937/1151, 516/903/1183** | **978/1688/2198, 954/1672/2141** (432-438 tokens/s) |
| n 4, m 24 | - | 927/1781/2197 |
| n 3, m 32 | - | 1221/2061/2720 |
| n 3, m 48 | - | 1153/2062/2574 |
| n 3, m 16 + `ngram-mod` | 1005/2413/2943 | - |

**Fast drafting as shipped** (opt-in: `polish.s1-mini.fast_drafting = true` adds
`--spec-type ngram-simple --spec-ngram-simple-size-n 3 --spec-ngram-simple-size-m 24
--spec-draft-n-max 24`; `--cache-ram 0` is always on). Same tool, same server build and device,
off and on back to back (`agt-bench polish --runs 1 --fast-drafting off|on`), p50/p90/max ms:

| | off | on |
|---|---|---|
| Windows Vulkan, long, 3 s idle (two rounds) | 1177/2222/2233, 1131/2125/2174 | 519/873/1178, 512/897/1171 |
| Windows Vulkan, clips 2-60 s, 3 s idle (two rounds) | 123/201/662, 122/207/587 | 61/129/331, 61/125/348 |
| Windows CPU device, long, back to back | 9740/22263/22907 | 5552/9670/12227 |
| Windows CPU device, clips, back to back | 578/1012/4165 | 314/590/2124 |
| Mac Metal, long, back to back | 2480/4629/4699 | 973/1662/2079 |
| Mac Metal, clips, back to back | 174/309/1152 | 84/162/489 |
| Mac CPU device, long, back to back | 4120/9155/9164 | 4434/8027/9310 |
| Mac CPU device, clips, back to back | 232/423/1730 | 209/354/1359 |

The CPU rows ran with `--timeout-ms 120000`: at the default deadline (2 s + 15 ms a word)
drafting-off timed out on several long dictations on the Windows CPU and they were pasted raw;
with drafting on, none did.

**Speed depends on how much the model copies.** Drafts are accepted only where the output
repeats the input. On Parakeet's punctuated transcripts 50-80% are accepted (above); in the
review of this change, with already-polished text as input, 86% (1116/2308 -> 330/556 ms,
3.4x); with lowercase unpunctuated input only 13-22%: 1081/2048 -> 1053/1772 ms and 1118/2228
-> 1235-1674/1824-1977 ms on Windows (no faster, sometimes slower), no faster on the CPU (4320
-> 4333 ms p50). The Mac's CPU device gains little even on Parakeet's text (p50 slower, p90
12% faster). Parakeet and Whisper both punctuate, so AGT's input is near the good end, but the
speed-up is "up to", not a constant.

**Accuracy: drafting on vs off.** Same build, device and inputs; the polished text scored
against the eval set's references with the WER normalizer above (case and punctuation
ignored), substitutions, deletions (dropped words) and insertions (added words) apart; then the
two outputs scored against each other (`agt-bench polish --refs --save`, then `--compare`). The
long set is the 12 long dictations (3,855 reference words), the clips the 78 clips of the
regular set (1,931 words; 2 are under `min_words` and not polished):

| Device, set | raw STT WER | polished, off: WER (sub/del/ins) | polished, on: WER (sub/del/ins) | outputs that differ | WER between off and on (sub/del/ins) |
|---|---|---|---|---|---|
| Windows Vulkan, long | 7.19% | 7.91% (163/77/65) | 8.04% (165/80/65) | 1 of 12 | 0.13% (2/3/0) |
| Windows Vulkan, clips | 4.04% | 5.02% (44/40/13) | 5.02% (44/40/13) | 1 of 78 | 0.00% (punctuation only) |
| Windows CPU, long | 7.19% | 7.96% (167/75/65) | 8.02% (168/76/65) | 3 of 12 | 0.10% (1/2/1) |
| Windows CPU, clips | 4.04% | 4.97% (42/40/14) | 5.02% (44/40/13) | 2 of 78 | 0.10% (1/1/0) |
| Mac Metal, long | 7.19% | 8.04% (165/80/65) | 7.91% (163/77/65) | 2 of 12 | 0.13% (2/0/3) |
| Mac Metal, clips | 4.04% | 5.02% (44/40/13) | 5.02% (44/40/13) | 1 of 78 | 0.00% (punctuation only) |
| Mac CPU, long | 7.19% | 7.91% (163/77/65) | 8.09% (166/81/65) | 6 of 12 | 0.39% (9/5/1) |
| Mac CPU, clips | 4.04% | 4.97% (44/40/12) | 4.97% (45/39/12) | 2 of 78 | 0.05% (0/0/1) |

Every differing passage (off -> on; `{}` = nothing):

- Windows Vulkan: vox-dict-02 `saying no, no, no,` -> `saying, "No, no, no,"`; `opposition to
  the project. The EU is a 50-year-old attempted answer` -> `opposition to the EU as a
  50-year-old attempt`; clips earn-medium-04 `prices,` -> `prices`.
- Windows CPU: ami-dict-02 `doll,` -> `doll`; ls-clean-dict-01 `Tonight,` -> `To night,`;
  ted-dict-01 `So I there better be a nine` -> `So I better be a 9`, `And why is it true?
  Smallpox. Smallpox killed` -> `And why is it true? Smallpox killed`, `trying stuff out,` ->
  `trying stuff out`; clips ami-long-01 `go to bed at night, no,` -> `go to bed at night,`;
  ami-short-03 `used to buying batteries,` -> `used to buying battery`.
- Mac Metal: the vox-dict-02 passage the other way round (on the Mac drafting-off already
  wrote `the EU as a 50-year-old attempt`); earn-dict-02 `measures,` -> `measures`; clips
  ls-other-medium-03 `after dinner, only` -> `after dinner. Only`.
- Mac CPU: ami-dict-02 `at a far off distance, at remote distance.` -> `at a far off
  distance.`, `one of the feature` -> `one of the features`, `the TV, television.` -> `the
  TV.`; earn-dict-02 `3rd quarter` -> `Third Quarter`; ls-clean-dict-01 `Mr. Harry Cresswell,
  and his sister, John Taylor,` -> `Mr. Harry Cresswell, Mr. John Taylor, Mr. John Taylor,`
  (the input: "mister Harry Cresswell and his sister, John Taylor and his sister"); ls-clean-dict-02
  `Five` -> `5`; ls-other-dict-02 `Miss` -> `Mrs.` (4 times; the input says "misses"), `said`
  -> `Said`; vox-dict-01 `continually reinterpreted, misinterpreted,` -> `continually
  misinterpreted`; clips earn-medium-04 `prices` -> `prices,`, earn-medium-06 `bookings up
  126%,` -> `bookings up 126% sequential,`.

On the GPUs drafting costs no accuracy measurable here: the WER moves by 0.13 points one way on
Windows and the same 0.13 points the other way on the Mac (it is one passage that flips), and
clips are identical in words. On the CPU devices it changes more: 3 and 6 of 12 long outputs,
+0.06 and +0.18 points of WER, and on the Mac CPU two real content errors (a name list rewritten
into `Mr. John Taylor, Mr. John Taylor`, `reinterpreted` dropped) next to ones that are as good
or better (`Mrs.` for "misses", `features`). Where the model rewrites a lot (the review's runs
with lowercase unpunctuated input, Windows Vulkan) drafting changed 2-4 of 12 long outputs
(9-12 words), e.g. `would like to remind, allow me to remind you` -> `would like to remind
you`, `find out what features a user` -> `find out what are the features which a user`; on the
CPU 4 of 5. Why outputs differ at all: checking 24 drafted tokens in one batch runs different
kernels than one token at a time, and where two tokens are nearly tied the rounding difference
can pick the other one. The model's own run-to-run behavior is not the
noise floor here (each setup is deterministic); a different backend is: CUDA vs Vulkan without
drafting already differ on 5 of 12 long outputs. Within that, drafting is a small but real
source of different words, more so on the CPU, so it is a setting, off by default.

**Other flags** (Windows, long, 3 s idle, with the drafting above; 516/903/1183 ms without
them): flash attention off 581/1028/1765, forced on 689/1095/1384; q8_0 KV cache 823/1500/3175
(and 7 of 12 outputs changed); `-t 8` 718/1094/1347. None helps: at a 4096 context the
attention and KV cache are a small part of a 0.6B model's step, and prefill is already ~3% of a
request. Batch and ubatch only affect prefill. The system prompt is already reused from the
slot's cache (`cache_n`); it's ~50 tokens anyway. The CUDA 12.4 build (b11669, +650 MB with
its runtime) decodes no faster than Vulkan (298 vs 315 tokens/s in the server, 370 in
`llama-bench tg128`): at 0.6B the step is launch- and sync-bound, not bandwidth-bound. Vulkan
stays.

**Memory.** llama-server's `--cache-ram` (default 8192 MiB) keeps the KV state of earlier
prompts in RAM, least recently used dropped first, to reuse a prompt's prefix later. With one
slot and a new transcript every time, the only reusable prefix is S1-mini's ~50-token system
prompt, which the slot keeps anyway (`cache_n` ~69 on the next request with `--cache-ram 0`),
so AGT never hits this cache; it just fills up towards its 8 GiB cap: after ~45 long requests
the server's working set was 1,569 MB vs 346 MB with `--cache-ram 0` (private bytes 2,856 vs
1,629 MB); on the Mac the physical footprint was 1,818 vs 579 MB. No latency difference (1218 vs 1215 ms p50). VRAM is the same with and without
drafts (Windows 860 MB after short requests, ~1,260 MB once long ones ran); working set after
the runs ~250-310 MB, Mac footprint ~596 MB.

**Idle and suspension** (Windows, 12 clips of 17-85 words, drafting on): back to back 67 ms p50;
after 2 s idle 211 ms, after 2 s suspended 216 ms, after 10 s suspended 261 ms (today's flags:
437 ms). Half an hour later the same 10 s suspended run gave 89 ms (and 87-89 ms with a
1-token request 2, 5 or 10 s before, as a key-down "wake-up" would send). Suspension itself
costs nothing measurable: what varies is the GPU's clocks after idle (P8 at 300 MHz), which
depends on what else the desktop is doing. Mac: back to back 100 ms, 10 s idle 129 ms, 10 s
suspended 139 ms. Resuming the server at key-down would save microseconds; a wake-up request at
key-down didn't help in the state measured, and can't keep the GPU up through a long dictation.

**Polishing ahead of release** (`agt-bench polish --segments --pause-ms 3000 --fast-drafting
on`; without drafting every request here takes about twice as long):
the pipeline could polish while the user is still talking, leaving only the end for release.
Two ways, both against polishing the whole text at release (the 12 long dictations, 104 STT
segments, 92 segment boundaries):

- *each segment*: polish every STT segment on its own as it's transcribed; at release, the
  tail.
- *ahead* (`agt_polish::ahead`): cut pieces only at a sentence end *inside* a segment, never at
  a segment boundary; what follows the last such sentence end waits for the next segment; at
  release, polish what's left plus the tail. When in doubt it doesn't cut: words with a period
  inside (`U.S.`, `a.m.`, `e.g.`, `Ph.D.`), abbreviations (`Mr.`, `approx.`, `Fig.`, `Inc.`)
  and initials never end a sentence, and the next word must start with a capital letter (not
  a digit). Held text over 800 words (a run-on without a usable sentence end) is cut at its
  last segment boundary, and the release text is split into pieces of at most 1,000 words, so
  no piece reaches S1-mini's 1,400-word limit. `Ahead::new` refuses S1-mini's email context
  and list structure, which need the whole text.

Release latency for both is the best case: the bench polishes every background piece, then
idles 3 s, so at release nothing else is running. In the app the last background piece may
still be in flight at release (it starts when its segment's STT is done) and STT shares the
GPU, so release can also wait for up to one background request (p90 ~0.2 s Windows, ~0.3 s
Mac, below).

| | Windows release p50/p90/max ms | Mac release p50/p90/max ms | background requests p50/p90 ms (Win, Mac) | words at release p50/p90 | words differing from whole (Win, Mac) | sentence breaks at piece joins (extra/lost vs whole) | WER (Win, Mac) |
|---|---|---|---|---|---|---|---|
| whole text (today's path, drafting on) | 508/895/1157 | 878/1690/2135 | - | 278/566 | - | - | 8.04%, 7.91% |
| each segment | 67/146/168 | 122/185/254 | 90/146, 152/232 (92) | 27/51 | 3.08%, 2.86% | 10/2 of 92 | 8.07%, 8.04% |
| **ahead** | **174/293/320** | **277/443/461** | 102/191, 160/294 (62) | 62/108 | 2.05%, 1.89% | **0/0 of 62** | **7.99%, 7.99%** |

WER is against the eval set's references with the normalizer above, which ignores case and
punctuation; the raw STT text scores 7.19%, so polish's rewrites cost 0.7-0.9 points under this
normalizer however it is split. "Words differing" counts exact word edits (case and
punctuation included) between the joined pieces and the whole-text output.

A segment polished on its own comes back as finished sentences, and the speech recognizer
often closes a segment with a period already, also mid-sentence (these DirectML transcripts
have more such joins than the CPU ones counted under "Sentence breaks at joins"). Polishing the
whole text repairs those joins; polishing a segment alone can't: *each segment* left 10 of 92
joins broken (`My great-grandparents. | Died all of them...`,
`The face of someone. | Who, against all odds...`, `...the TV. | And as this is...`). *Ahead*
never cuts there, so every segment join is polished with both sides and the piece joins are
real sentence ends (0 of 62 differ from the whole text). Its other differences are the
near-tie choices described above, spread through the text (`Mrs.`/`Miss Farinder`, `the
project. The EU is`/`the EU as`, `5`/`five`), not concentrated at the joins, and WER is the
same.

### What the numbers say (polish speed)

- **Prompt-lookup drafting makes polish up to ~3x faster when the model mostly keeps the
  transcript, which is the normal case with a punctuating recognizer.** On Parakeet's
  transcripts n-gram drafting accepts 50-80% of what it proposes: long dictations go from
  1.1-1.2 s to 0.5 s p50 and 2.1-2.2 s to 0.9 s p90 on Windows, 2.5 s to 1.0 s p50 and 4.6 s
  to 1.7 s p90 on the Mac; 2-60 s clips about twice as fast; the Windows CPU device 1.8x, the
  Mac's CPU device hardly at all. Where the model rewrites a lot (lowercase unpunctuated
  input) only 13-22% is accepted and it is no faster (1.0-1.15x, sometimes slower). No model,
  download, VRAM or RAM is added.
  Short lookups (n = 3) recover fastest after an edit; 24-token drafts were best on both
  machines (m = 16 is as good on Windows, 35% slower on the Mac; 32-48 waste more work on
  rejected drafts).
- **On the GPUs drafting costs no measurable accuracy; on the CPU devices it changes more,
  and sometimes for the worse.** With Parakeet's transcripts, Vulkan and Metal outputs are
  byte-identical on 88 and 87 of 90, the WER against the references moves by ±0.13 points
  (the same passage, opposite ways on the two machines) and the clips are word-identical.
  The CPU devices change 3 and 6 of 12 long outputs (+0.06 and +0.18 WER points), including a
  duplicated name and a dropped word on the Mac CPU, and heavy rewrites change 2-4 of 12 on
  the GPU too. Each setup is deterministic, so these are drafting's, not noise; they are the
  kind of near-tie change a backend switch already makes (CUDA vs Vulkan: 5 of 12), on a step
  whose output isn't ground truth. Small, but real: hence off by default.
- **`--cache-ram` holds memory AGT never uses.** With b11669's default the server keeps
  earlier prompts' KV state in RAM up to 8 GiB (+1.2 GB after ~45 long dictations), and with
  one slot and a new transcript each time AGT never hits it. Off, with no latency cost.
- **Nothing else in the server pays.** Flash attention, KV cache type, threads, batch sizes
  and CUDA are all within noise or worse; prefill is ~3% of a request and the fixed prompt is
  already cached. Decoding 0.6B parameters is launch-bound on both GPUs (~3 and ~6.5 ms a
  token), so fewer forward passes (drafting) is the lever, not faster ones.
- **Suspension is free; GPU clocks are not.** Resuming the suspended server costs nothing
  measurable, so resuming it at key-down buys nothing. After idle the RTX 2080 Ti sometimes
  needs ~150 ms more for a short request (P8 -> P0), sometimes ~20 ms, depending on the desktop;
  a key-down wake-up request didn't change it when measured, and couldn't survive a long
  dictation anyway. Not worth wiring.
- **max_tokens** (bytes / 2 + 64, ~2x the expected output) only stops a runaway answer;
  generation ends at the end-of-text token long before it, so tightening it changes no latency.
  **Skipping polish** when it can't change anything isn't possible to know in advance: STT
  output with correct punctuation still gets numbers, repeats and false starts rewritten.
- **Polishing ahead turns release -> polished text into a tail-sized request.** With drafting,
  *ahead* takes long dictations from 0.5/0.9 s to 0.17/0.29 s (p50/p90) on Windows and from
  0.9/1.7 s to 0.28/0.44 s on the Mac, with the same WER and no broken joins; the release piece
  is the last sentence of the previous segment plus the tail (62 words p50). Polishing each
  segment on its own is faster still (0.07/0.15 s) but leaves ~1 in 9 segment joins as a
  sentence break mid-sentence, which is exactly the boundary artifact whole-text polish was
  repairing. The cost is background work: about one 0.1-0.2 s request per segment that has a
  sentence end (62 requests in 35 min of dictation, ~6.5 s of GPU time on Windows), each with
  a GPU wake-up.

### Recommendation (polish speed)

- **Ship now** (this change): `--cache-ram 0` always, and fast drafting as an opt-in setting,
  **off by default**: `polish.s1-mini.fast_drafting` (tray: Settings > Polish > "Fast
  drafting"; like every setting, a click restarts AGT and with it the managed server, now with
  `--spec-type ngram-simple --spec-ngram-simple-size-n 3 --spec-ngram-simple-size-m 24
  --spec-draft-n-max 24`). Off by default because it changes a few words now and then (most on
  the CPU devices, see "Accuracy" above) and the default should be the model's own output; on
  for someone who dictates long texts on a GPU and wants them about twice as fast. Same pinned
  build, no new download. Telemetry records the setting per session (`s1_fast_drafting`).
- **Wire *ahead* after the incremental STT wiring lands**, on the STT worker's output: each
  segment's text goes to `Ahead::push`, a returned piece is polished right away on a polish
  worker (one thread, in order, same `polish_or_raw` and guard per piece), and at release
  the pieces of `Ahead::finish(tail)` are polished and all pieces are joined with spaces.
  Short dictations need no special case: a single segment never yields a piece before
  `finish`, so they take exactly today's path. `Ahead::new(&cfg)` returns `None` for S1-mini's
  email context and list structure (whole text only); the caller decides about providers (only
  the managed or a local server: a cloud provider would bill a request per piece). Cancel drops
  the queue like STT's does. Count guard fallbacks per piece in telemetry (a dictation can
  then be part raw, part polished), and check on real dictations whether self-corrections that
  span a cut ("Tuesday. No, actually Wednesday.") suffer. Worth it mostly on the Mac and for
  multi-minute dictations; on Windows it saves ~0.3 s p50 / ~0.6 s p90 for 1-6 minute ones in
  the best case (see the caveat above). It would also lift the 1,400-word cap on polishing
  (`polish_outcome = too_long`): no piece is over 1,000 words, however long the dictation.
  *Wired (#46)* as described, with one difference: the STT worker feeds `Ahead` one segment
  behind, because at release a short tail may be transcribed again together with the last
  segment and replace its text (`Piece::redo`). So the release piece is the rest of the
  segment before the last, plus the last segment and the tail: about one segment more than
  measured here. Telemetry: `polish_ahead_pieces`, `polish_ahead_ms`, `polish_release_words`,
  `polish_fallbacks` and `polish_outcome = partial`; `polish_ms` now runs from the raw text to
  the polished one, waits included.
- **Telemetry** (metrics only): polish time at release, the release piece's word count, pieces
  polished ahead and their total time, guard fallbacks per piece, and draft acceptance
  (llama-server's `timings.draft_n` / `draft_n_accepted`) to see whether 24-token drafts suit
  real dictation as well as this set.
- **Not worth doing**: resuming at key-down or a wake-up request, the CUDA build, flash
  attention / KV cache / thread / batch settings, `ngram-mod` (it learns across requests, so it
  is fast on a repeated text and slower on a new one: 336 vs 772 ms), smaller `max_tokens`.

## Reproducing

```sh
uv run scripts/prepare_eval.py                      # eval-data/clips, ~5 min
cargo build --release -p agt-bench                  # Parakeet: auto, cpu, directml (Windows)
target/release/agt-bench --device directml eval-data/clips
target/release/agt-bench --device directml --gap-ms 3000 --runs 1 eval-data/clips   # pauses between calls
AGT_LOG=info target/release/agt-bench --device auto eval-data/clips   # logs the start-up probe
cargo build --release -p agt-bench --features cuda  # + CUDA 13 / cuDNN 9 DLLs on PATH
cargo build --release -p agt-bench --features whisper-vulkan   # needs CMake, libclang, Vulkan headers (see below)
target/release/agt-bench --engine whisper --model whisper-large-v3-turbo-q5_0 --device vulkan eval-data/clips

# Long dictations (issue #6)
uv run scripts/prepare_long_eval.py                 # eval-data/long/clips, ~5 min
target/release/agt-bench live --cuts-only --pause-ms 0,500,1000,2000 eval-data/long/clips   # cutter only
target/release/agt-bench live --device directml --baseline --pause-ms 0,1000 eval-data/long/clips
target/release/agt-bench live --device cpu --idle-ms 0 --pause-ms 500,1000 --min-secs 0,8 eval-data/long/clips   # WER sweep
target/release/agt-bench live --device cpu --realtime --only ted-dict-01 eval-data/long/clips
target/release/agt-bench live --device cpu --idle-ms 0 --joins eval-data/long/clips   # the app's path, joins
target/release/agt-bench live --device cpu --idle-ms 0 --no-guards eval-data/long/clips   # bare cutter
target/release/agt-bench live --device coreml --precision fp32 --max-secs 10 eval-data/long/clips   # macOS

# Polish speed: transcripts first (DirectML), one per line for the plain runs
target/release/agt-bench live --device directml --idle-ms 0 --texts long.tsv --segments segs.tsv eval-data/long/clips
cut -f3,4 long.tsv > long.txt                                               # name<TAB>text
target/release/agt-bench polish --runs 1 --pause-ms 3000 --fast-drafting off long.txt   # latency
target/release/agt-bench polish --runs 1 --pause-ms 3000 --fast-drafting on long.txt
# accuracy (CPU device: add --s1-device cpu --timeout-ms 120000)
target/release/agt-bench polish --runs 1 --fast-drafting off --refs eval-data/long/clips --save off.tsv long.txt
target/release/agt-bench polish --runs 1 --fast-drafting on --refs eval-data/long/clips --save on.tsv --compare off.tsv long.txt
target/release/agt-bench polish --segments --pause-ms 3000 --fast-drafting on --refs eval-data/long/clips segs.tsv
# llama-server flags (the managed server's files are in AGT's models folder, s1-mini/ and agt-llama-server/b11669/)
python3 scripts/polish_sweep.py --exe <llama-server> --model <s1-mini-q4_k_m.gguf> --texts long.txt \
  --warm warm.txt --pause-ms 3000 --label base --args "--n-gpu-layers all"
python3 scripts/polish_sweep.py ... --label spec --compare base \
  --args "--n-gpu-layers all --cache-ram 0 --spec-type ngram-simple --spec-ngram-simple-size-n 3 --spec-ngram-simple-size-m 24 --spec-draft-n-max 24"

# macOS (Apple Silicon): `brew install cmake` for whisper-metal
cargo build --release --locked -p agt-bench
target/release/agt-bench --device coreml eval-data/clips          # or --device cpu --precision int8
target/release/agt-bench --device cpu --gap-ms 3000 --runs 1 eval-data/clips   # pauses between calls
AGT_LOG=debug target/release/agt-bench ...                          # per-stage timings
cargo build --release --locked -p agt-bench --features whisper-metal --target-dir target/whisper
target/whisper/release/agt-bench --engine whisper --model whisper-large-v3-turbo-q5_0 --device metal eval-data/clips
/usr/bin/time -l target/release/agt-bench ...                       # peak RSS and physical footprint
```

The CUDA DLLs used here came from NVIDIA's PyPI wheels (`nvidia-cuda-runtime`,
`nvidia-cublas`, `nvidia-cufft` 13.x, `nvidia-cudnn-cu13`); on Windows the whisper build used
CMake 4.1, Ninja, libclang 18 from the `libclang` wheel, and Vulkan headers 1.4.328 + `glslc`
and `vulkan-1.lib` from conda-forge (no Vulkan SDK installer needed).
