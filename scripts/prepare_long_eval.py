# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["huggingface_hub==0.36.0", "pyarrow==21.0.0", "soundfile==0.13.1", "numpy==2.3.3", "soxr==1.0.0"]
# ///
"""Build AGT's long-dictation eval set: minutes of speech with thinking pauses (issue #6).

    uv run scripts/prepare_long_eval.py [out_dir]   # default: eval-data/long (gitignored)

Writes `<out_dir>/clips/<source>-dict-<n>.wav` (16 kHz mono 16-bit) with the reference in a
`.txt` sidecar (what `agt-bench` reads), where the inserted pauses are in a `.pauses` sidecar
(start and end in seconds per line) and `<out_dir>/manifest.tsv`, checked against the
committed `scripts/long_eval_manifest.tsv` like `prepare_eval.py` does. Same sources, pinned
revisions and lockfile contents as `prepare_eval.py` (whose helpers it imports).

Each dictation is consecutive utterances of one recording (one speaker) with a pause after
every utterance, so it sounds like someone who stops to think: 50% 0.3-1 s (within a
thought), 15% 1-2 s (hesitation), 35% 2-30 s log-uniform (thinking). Pauses are filled with
the recording's own background noise (its 10 ms blocks near its noise floor, tiled with short
crossfades), not digital zero. Two dictations per source, one of 1-2.5 min and one of 3-6 min
in total; the reference is the utterances' references joined. Everything random comes from a
generator seeded by the dictation's name, so the output is the same on every run.
"""

import hashlib
import re
import sys
from pathlib import Path

import numpy as np
import soundfile as sf
from huggingface_hub import HfFileSystem

from prepare_eval import SOURCES, SR, hkey, list_segments, read_audio

TARGETS = [(60.0, 150.0), (180.0, 360.0)]  # total seconds per dictation, drawn uniformly
MAX_TOTAL = 360.0
BLOCK = SR // 100  # 10 ms
FADE = SR // 200  # 5 ms crossfade between noise pieces


def usable(seg):
    words = seg["text"].split()
    return len(words) >= 3 and seg["dur"] >= 1.0 and not re.search(r"[<>\[\]]", seg["text"])


def follows(source, a, b):
    """b may follow a in a dictation: the next utterance of the same recording. TED and AMI
    segments are ordered by start time (the gaps between them are replaced by our pauses
    anyway); the others are numbered consecutively."""
    if source in ("ted", "ami"):
        return True
    return b["pos"] == a["pos"] + 1


def pause_secs(rng):
    u = rng.random()
    if u < 0.5:
        return rng.uniform(0.3, 1.0)
    if u < 0.65:
        return rng.uniform(1.0, 2.0)
    return float(np.exp(rng.uniform(np.log(2.0), np.log(30.0))))


def plan(source, segs, name, used):
    """(segments, pauses after each but the last) for one dictation: from the first recording
    (in hash order) long enough for the drawn target, else the longest one (VoxPopuli's
    speeches are short)."""
    rng = np.random.default_rng(int(hkey(name)[:16], 16))
    lo, hi = TARGETS[int(name[-2:]) - 1]
    target = rng.uniform(lo, hi)
    by_rec = {}
    for s in segs:
        if usable(s):
            by_rec.setdefault(s["rec"], []).append(s)
    best = None
    for rec in sorted(by_rec, key=hkey):
        if rec in used:
            continue
        run = sorted(by_rec[rec], key=lambda s: s["pos"])
        chosen, pauses, total = [run[0]], [], run[0]["dur"]
        for s in run[1:]:
            if not follows(source, chosen[-1], s):
                break
            p = pause_secs(rng)
            if total + p + s["dur"] > MAX_TOTAL:
                break
            pauses.append(p)
            chosen.append(s)
            total += p + s["dur"]
            if total >= target:
                break
        if total >= max(lo, 0.8 * target):
            used.add(rec)
            return chosen, pauses
        if best is None or total > best[0]:
            best = (total, rec, chosen, pauses)
    if best is None or best[0] < TARGETS[0][0]:
        raise SystemExit(f"{source}: no recording long enough for {name}")
    used.add(best[1])
    return best[2], best[3]


def noise_pieces(clips):
    """Runs (>= 30 ms) of the recording's background noise: 10 ms blocks within 6 dB of its
    floor (the 5th percentile of all its blocks), so soft speech and breaths stay out."""
    levels = []
    for a in clips:
        n = len(a) // BLOCK
        levels.append(10 * np.log10(np.mean(a[: n * BLOCK].reshape(n, BLOCK) ** 2, axis=1)
                                    + 1e-12))
    floor = np.percentile(np.concatenate(levels), 5)
    pieces = []
    for a, db in zip(clips, levels):
        n = len(db)
        quiet = db <= floor + 6
        i = 0
        while i < n:
            if not quiet[i]:
                i += 1
                continue
            j = i
            while j < n and quiet[j]:
                j += 1
            if j - i >= 3:
                pieces.append(a[i * BLOCK : j * BLOCK])
            i = j
    return pieces


def noise(pieces, secs, rng):
    """`secs` of background noise: random pieces overlap-added with raised-cosine fades."""
    n = int(round(secs * SR))
    out = np.zeros(n + 2 * SR, dtype=np.float32)
    ramp = (0.5 - 0.5 * np.cos(np.linspace(0, np.pi, FADE))).astype(np.float32)
    at = 0
    while at < n:
        p = pieces[rng.integers(len(pieces))].copy()
        if len(p) > 2 * FADE:
            p[:FADE] *= ramp
            p[-FADE:] *= ramp[::-1]
        out[at : at + len(p)] += p
        at += max(len(p) - FADE, 1)
    return out[:n]


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "eval-data/long")
    clips = out / "clips"
    clips.mkdir(parents=True, exist_ok=True)
    fs = HfFileSystem()
    rows, written = [], set()
    for source, (category, license_, files) in SOURCES.items():
        print(f"{source}: listing segments", flush=True)
        segs = list_segments(fs, source, files)
        used = set()
        plans = [(f"{source}-dict-{i + 1:02}", *plan(source, segs, f"{source}-dict-{i + 1:02}", used))
                 for i in range(len(TARGETS))]
        audio = read_audio(fs, [s for _, group, _ in plans for s in group])
        for name, group, pauses in plans:
            rng = np.random.default_rng(int(hkey(name + "/noise")[:16], 16))
            pieces = noise_pieces([audio[s["id"]] for s in group])
            parts, at, spans = [], 0, []
            for i, s in enumerate(group):
                parts.append(audio[s["id"]])
                at += len(parts[-1])
                if i < len(pauses):
                    parts.append(noise(pieces, pauses[i], rng))
                    spans.append(f"{at / SR:.3f}\t{(at + len(parts[-1])) / SR:.3f}\n")
                    at += len(parts[-1])
            pcm = np.concatenate(parts)
            # Where the inserted pauses are (start, end in s), for `agt-bench live --cuts-only`.
            (clips / f"{name}.pauses").write_text("".join(spans), encoding="utf-8", newline="\n")
            wav = clips / f"{name}.wav"
            sf.write(wav, pcm, SR, subtype="PCM_16")
            txt = clips / f"{name}.txt"
            txt.write_text(" ".join(s["text"] for s in group) + "\n", encoding="utf-8",
                           newline="\n")
            digest = hashlib.sha256(wav.read_bytes()).hexdigest()
            ref_digest = hashlib.sha256(txt.read_bytes()).hexdigest()
            speech = sum(s["dur"] for s in group)
            thinking = sum(1 for p in pauses if p >= 2.0)
            rows.append(f"{name}\t{category}\t{license_}\t{len(pcm) / SR:.2f}\t{speech:.2f}\t"
                        f"{len(group)}\t{thinking}\t{digest}\t{ref_digest}\t"
                        + "+".join(s["id"] for s in group))
            written.update((wav.name, txt.name, f"{name}.pauses"))
            print(f"{name}: {len(pcm) / SR:.0f} s ({speech:.0f} s speech, {len(group)} "
                  f"utterances, {thinking} pauses >= 2 s)", flush=True)
    for f in clips.iterdir():
        if f.name not in written:
            print(f"removing {f} (not in this eval set)")
            f.unlink()
    manifest = ("name\tcategory\tlicense\tseconds\tspeech_seconds\tutterances\tthinking_pauses\t"
                "sha256\tref_sha256\tsource_ids\n" + "\n".join(rows) + "\n")
    (out / "manifest.tsv").write_text(manifest, encoding="utf-8", newline="\n")
    total = sum(float(r.split("\t")[3]) for r in rows)
    print(f"wrote {len(rows)} dictations ({total / 60:.1f} min) to {clips}")
    expected = Path(__file__).with_name("long_eval_manifest.tsv")
    if not expected.exists():
        print(f"no {expected.name} to check against; copy {out / 'manifest.tsv'} there to pin it")
        return
    want, got = expected.read_text(encoding="utf-8").splitlines(), manifest.splitlines()
    if want == got:
        print(f"checksums match {expected.name}")
        return
    # As in prepare_eval.py: only the WAV hashes may differ (decoder/resampler builds).
    def no_hash(lines):
        return [l.split("\t")[:7] + l.split("\t")[8:] for l in lines]
    for w, g in zip(want, got):
        if w != g:
            print(f"  expected: {w}\n  got:      {g}")
    if no_hash(want) != no_hash(got):
        raise SystemExit(f"eval set differs from {expected}: not reproducible")
    print(f"WARNING: same dictations as {expected.name}, but some audio hashes differ")


if __name__ == "__main__":
    main()
