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
  the GPU (app log: "Core ML 149 ms, CPU 105 ms for 3 s of audio; using the CPU"). Windows keeps its DirectML probe for now; whether a discrete GPU loses as much after
  a pause hasn't been measured (`--gap-ms` makes that a one-line run).
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

## Reproducing

```sh
uv run scripts/prepare_eval.py                      # eval-data/clips, ~5 min
cargo build --release -p agt-bench                  # Parakeet: auto, cpu, directml (Windows)
target/release/agt-bench --device directml eval-data/clips
cargo build --release -p agt-bench --features cuda  # + CUDA 13 / cuDNN 9 DLLs on PATH
cargo build --release -p agt-bench --features whisper-vulkan   # needs CMake, libclang, Vulkan headers (see below)
target/release/agt-bench --engine whisper --model whisper-large-v3-turbo-q5_0 --device vulkan eval-data/clips

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
