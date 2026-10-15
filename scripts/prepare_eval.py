# /// script
# requires-python = ">=3.10"
# dependencies = ["huggingface_hub==0.36.0", "pyarrow==21.0.0", "soundfile==0.13.1", "numpy==2.3.3", "soxr==1.0.0"]
# ///
"""Download and prepare AGT's speech eval set (real speech, ~80 clips, ~13 min).

    uv run scripts/prepare_eval.py [out_dir]        # default: eval-data/ (gitignored)

Writes `<out_dir>/clips/<source>-<bucket>-<n>.wav` (16 kHz mono 16-bit) with the reference
transcript in a `.txt` sidecar, which is what `agt-bench` reads, plus `<out_dir>/manifest.tsv`
(source, license, original id, duration, SHA-256 of the WAV). The manifest is checked against
the committed `scripts/eval_manifest.tsv`, the checksum for the whole set: a different clip
selection fails; different WAV hashes for the same clips (a different decoder/resampler build)
only warn. Nothing is committed to the repo but this script and the manifest. Requires `uv`.

Sources: the ESB test sets as packaged for the Hugging Face Open ASR Leaderboard, pinned to a
dataset revision. Only the needed parquet row groups are fetched (HTTP range requests), so the
download is a few hundred MB, not the full corpora.

| source    | what                                         | license          | category |
|-----------|----------------------------------------------|------------------|----------|
| ls-clean  | LibriSpeech test-clean (audiobooks)          | CC-BY-4.0        | read     |
| ls-other  | LibriSpeech test-other (harder audiobooks)   | CC-BY-4.0        | read     |
| ted       | TED-LIUM 3 test (TED talks)                  | CC-BY-NC-ND-3.0  | real     |
| vox       | VoxPopuli en test (European Parliament)      | CC0              | real     |
| earn      | Earnings-22 test (earnings calls, phone)     | CC-BY-SA-4.0     | real     |
| ami       | AMI test, headset mics (meetings)            | CC-BY-4.0        | real     |

Per source: 6 short clips (2-5 s), 6 medium (8-15 s) and 1 long one (30-60 s, consecutive
segments of one recording joined with 0.3 s of silence where they weren't contiguous). Clips
are chosen deterministically (sorted by a hash of the id, at most one per speaker/recording).
"""

import hashlib
import io
import re
import sys
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq
import soundfile as sf
import soxr
from huggingface_hub import HfFileSystem

OAL = "datasets/hf-audio/open-asr-leaderboard@de79e57adf9983993bcc72586d7313460825f5eb"
# TED-LIUM isn't in the leaderboard repo; take it from the ESB test-only repo's parquet export.
ESB = "datasets/hf-audio/esb-datasets-test-only@d442b8a83742230ad9071437f685bf691252a27f"

SOURCES = {
    "ls-clean": ("read", "CC-BY-4.0", [f"{OAL}/librispeech/test.clean-00000-of-00001.parquet"]),
    "ls-other": ("read", "CC-BY-4.0", [f"{OAL}/librispeech/test.other-00000-of-00001.parquet"]),
    "ted": ("real", "CC-BY-NC-ND-3.0", [f"{ESB}/tedlium/test/0000.parquet"]),
    "vox": ("real", "CC0", [f"{OAL}/voxpopuli/test-0000{i}-of-00004.parquet" for i in range(4)]),
    "earn": ("real", "CC-BY-SA-4.0", [f"{OAL}/earnings22/test-0000{i}-of-00005.parquet" for i in range(5)]),
    "ami": ("real", "CC-BY-4.0", [f"{OAL}/ami/test-{i:05}-of-00015.parquet" for i in range(15)]),
}
SR = 16000
N_PER_BUCKET = 6
BUCKETS = {"short": (2.0, 5.0), "medium": (8.0, 15.0)}
LONG = (30.0, 60.0)
GAP = 0.3  # seconds of silence between non-contiguous segments in a long clip


def recording_key(source, seg_id):
    """(recording, position) for grouping and ordering segments; one speaker per recording."""
    if source.startswith("ls-"):  # speaker-chapter-utt
        spk, chap, utt = seg_id.split("-")
        return f"{spk}-{chap}", float(utt)
    if source == "ted":  # Talk-start-end-<...>
        talk, start, _end = seg_id.split("-")[:3]
        return talk, float(start)
    if source == "vox":  # session_speechstart_index
        head, idx = seg_id.rsplit("_", 1)
        return head, float(idx)
    if source == "earn":  # call/chunk.wav
        call, chunk = seg_id.split("/")
        return call, float(chunk.removesuffix(".wav"))
    if source == "ami":  # AMI_meeting_headset_speaker_start_end
        parts = seg_id.split("_")
        return "_".join(parts[1:4]), float(parts[4])
    raise ValueError(source)


def ted_bounds(seg_id):
    _talk, start, end = seg_id.split("-")[:3]
    return float(start), float(end)


def hkey(s):
    return hashlib.sha256(s.encode()).hexdigest()


def list_segments(fs, source, files):
    """All segments of a source: dicts with id, text, dur, and where to find the audio."""
    segs = []
    for f in files:
        pf = pq.ParquetFile(fs.open(f, block_size=1 << 20))
        cols = [c for c in ("id", "text", "audio_length_s") if c in pf.schema_arrow.names]
        for rg in range(pf.metadata.num_row_groups):
            t = pf.read_row_group(rg, columns=cols).to_pylist()
            for row, r in enumerate(t):
                if "audio_length_s" in r:
                    dur = r["audio_length_s"]
                else:
                    s, e = ted_bounds(r["id"])
                    dur = e - s
                rec, pos = recording_key(source, r["id"])
                segs.append(dict(id=r["id"], text=r["text"].strip(), dur=dur, rec=rec, pos=pos,
                                 file=f, rg=rg, row=row))
    return segs


def usable(seg):
    words = seg["text"].split()
    # Skip near-empty references and ones with markup (none expected, but cheap to guard).
    return len(words) >= 3 and not re.search(r"[<>\[\]]", seg["text"])


def select(source, segs):
    picked = []  # (bucket, n, [segments])
    for bucket, (lo, hi) in BUCKETS.items():
        cands = sorted((s for s in segs if lo <= s["dur"] <= hi and usable(s)),
                       key=lambda s: hkey(s["id"]))
        seen = set()
        n = 0
        for s in cands:
            if s["rec"] in seen:
                continue
            seen.add(s["rec"])
            n += 1
            picked.append((bucket, n, [s]))
            if n == N_PER_BUCKET:
                break
        if n < N_PER_BUCKET:
            raise SystemExit(f"{source}: only {n} {bucket} candidates")
    # Long: the first run (in hash order of recordings) of consecutive usable segments that
    # reaches LONG[0] seconds without exceeding LONG[1].
    by_rec = {}
    for s in segs:
        by_rec.setdefault(s["rec"], []).append(s)
    for rec in sorted(by_rec, key=hkey):
        run = sorted(by_rec[rec], key=lambda s: s["pos"])
        for i in range(len(run)):
            total, chosen = 0.0, []
            for s in run[i:]:
                if chosen and not consecutive(source, chosen[-1], s):
                    break
                if not s["text"]:
                    break
                total += s["dur"] + (GAP if chosen else 0.0)
                chosen.append(s)
                if total >= LONG[0]:
                    break
            if LONG[0] <= total <= LONG[1] and len(chosen) >= 2:
                picked.append(("long", 1, chosen))
                return picked
    raise SystemExit(f"{source}: no long clip found")


def consecutive(source, a, b):
    if source == "ted":
        return abs(ted_bounds(b["id"])[0] - ted_bounds(a["id"])[1]) < 2.0
    if source == "ami":  # start/end in 10 ms units; allow pauses up to 2 s
        a_end = int(a["id"].split("_")[5]) / 100
        b_start = int(b["id"].split("_")[4]) / 100
        return 0 <= b_start - a_end < 2.0
    return b["pos"] == a["pos"] + 1


def contiguous(source, a, b):
    """True if b starts exactly where a ends in the original recording (no silence needed)."""
    if source == "ted":
        return abs(ted_bounds(b["id"])[0] - ted_bounds(a["id"])[1]) < 0.05
    return False


def read_audio(fs, segs):
    cache = {}
    out = {}
    for s in segs:
        k = (s["file"], s["rg"])
        if k not in cache:
            pf = pq.ParquetFile(fs.open(s["file"], block_size=1 << 20))
            cache[k] = pf.read_row_group(s["rg"], columns=["id", "audio"]).to_pylist()
        r = cache[k][s["row"]]
        assert r["id"] == s["id"]
        data, sr = sf.read(io.BytesIO(r["audio"]["bytes"]), dtype="float32", always_2d=True)
        mono = data.mean(axis=1)
        if sr != SR:  # some Earnings-22 segments are stored at 24 kHz
            mono = soxr.resample(mono, sr, SR, quality="HQ").astype(np.float32)
        out[s["id"]] = mono
    return out


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "eval-data")
    clips = out / "clips"
    clips.mkdir(parents=True, exist_ok=True)
    fs = HfFileSystem()
    rows = []
    for source, (category, license_, files) in SOURCES.items():
        print(f"{source}: listing segments", flush=True)
        segs = list_segments(fs, source, files)
        picked = select(source, segs)
        audio = read_audio(fs, [s for _, _, group in picked for s in group])
        for bucket, n, group in picked:
            parts, texts = [], []
            for i, s in enumerate(group):
                if i and not contiguous(source, group[i - 1], s):
                    parts.append(np.zeros(int(GAP * SR), dtype=np.float32))
                parts.append(audio[s["id"]])
                texts.append(s["text"])
            pcm = np.concatenate(parts)
            name = f"{source}-{bucket}-{n:02}"
            wav = clips / f"{name}.wav"
            sf.write(wav, pcm, SR, subtype="PCM_16")
            text = " ".join(texts)
            (clips / f"{name}.txt").write_text(text + "\n", encoding="utf-8")
            digest = hashlib.sha256(wav.read_bytes()).hexdigest()
            ids = "+".join(s["id"] for s in group)
            rows.append(f"{name}\t{category}\t{license_}\t{len(pcm) / SR:.2f}\t{digest}\t{ids}")
        print(f"{source}: {len(picked)} clips", flush=True)
    manifest = "name\tcategory\tlicense\tseconds\tsha256\tsource_ids\n" + "\n".join(rows) + "\n"
    (out / "manifest.tsv").write_text(manifest, encoding="utf-8", newline="\n")
    total = sum(float(r.split("\t")[3]) for r in rows)
    print(f"wrote {len(rows)} clips ({total / 60:.1f} min) to {clips}")
    expected = Path(__file__).with_name("eval_manifest.tsv")
    if expected.exists():
        want = expected.read_text(encoding="utf-8").splitlines()
        got = manifest.splitlines()
        if want == got:
            print(f"checksums match {expected.name}")
        else:
            # Same clips and references but different WAV bytes can only come from decoding /
            # resampling differences (e.g. another soxr build): warn. Anything else is an error.
            def no_hash(lines):
                return [l.split("\t")[:4] + l.split("\t")[5:] for l in lines]
            for w, g in zip(want, got):
                if w != g:
                    print(f"  expected: {w}\n  got:      {g}")
            if no_hash(want) != no_hash(got):
                raise SystemExit(f"eval set differs from {expected}: not reproducible")
            print(f"WARNING: same clips as {expected.name}, but some audio hashes differ")
    else:
        print(f"no {expected.name} to check against; copy {out / 'manifest.tsv'} there to pin it")


if __name__ == "__main__":
    main()
