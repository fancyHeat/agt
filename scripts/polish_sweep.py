"""Polish speed sweep: run llama-server with any flags and S1-mini's prompt over a file of
transcripts (one per line), and report latency, decode speed, draft acceptance and memory.

`agt-bench polish` measures what AGT ships; this script is for trying llama-server flags
(speculative decoding, flash attention, KV cache type, backends) without changing AGT. The
fixed flags are the ones AGT's managed server always uses (crates/polish/src/managed/process.rs).
Standard library only:

    python3 scripts/polish_sweep.py --exe <llama-server> --model <s1-mini-q4_k_m.gguf> \
        --texts long.txt --label spec --args "--n-gpu-layers all --cache-ram 0 --spec-type ngram-simple" \
        [--runs 1] [--pause-ms 3000 | --suspend-ms 10000] [--warm warm.txt] [--compare base]

Results go to <out>/<label>.json (every output, llama-server's own timings); --compare <label>
counts the outputs that differ from an earlier run's, and by how many words. Latency is the
first run of each line (later runs of the same line hit caches); p50/p90/max over lines.
"""

import argparse
import difflib
import json
import os
import platform
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.request

# S1-mini's required system prompt and AGT's default control line (crates/polish/src/prompt.rs).
SYSTEM = (
    "You are a text normalizer for speech-to-text transcripts. The input begins with a control "
    "line specifying the styling, structure, and context settings; clean the transcript to match "
    "those settings and output only the cleaned text."
)
CONTROL = "[Styling: semi-formal] [Structure: prose] [Context: general]"
FIXED = ["--host", "127.0.0.1", "--jinja", "--chat-template-kwargs", '{"enable_thinking":false}',
         "--temp", "0", "--ctx-size", "4096", "--parallel", "1", "--no-webui", "--slots", "--offline"]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def suspend(pid, on):
    """What AGT does to an idle server (crates/polish/src/managed/gate.rs)."""
    if os.name == "nt":
        import ctypes
        h = ctypes.windll.kernel32.OpenProcess(0x0800, False, pid)  # PROCESS_SUSPEND_RESUME
        (ctypes.windll.ntdll.NtSuspendProcess if on else ctypes.windll.ntdll.NtResumeProcess)(h)
        ctypes.windll.kernel32.CloseHandle(h)
    else:
        os.kill(pid, signal.SIGSTOP if on else signal.SIGCONT)


def memory(pid):
    """Windows: working set and private MB, dedicated GPU MB; macOS: physical footprint."""
    if os.name == "nt":
        ps = (f"$p = Get-Process -Id {pid}; $g = (Get-Counter '\\GPU Process Memory(pid_{pid}_*)\\Dedicated Usage'"
              f" -ErrorAction SilentlyContinue).CounterSamples | Measure-Object CookedValue -Sum;"
              f" \"$($p.WorkingSet64) $($p.PrivateMemorySize64) $($g.Sum)\"")
        out = subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True, text=True).stdout
        ws, private, gpu = (round(float(x or 0) / 2**20) for x in (out.split() + ["0"] * 3)[:3])
        return f"working set {ws} MB, private {private} MB, GPU dedicated {gpu} MB"
    if platform.system() == "Darwin":
        out = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True).stdout
        return next((l.strip() for l in out.splitlines() if "Footprint:" in l), out.strip())
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return f"RSS {int(out.strip() or 0) // 1024} MB"


def word_edits(a, b):
    sm = difflib.SequenceMatcher(None, a.split(), b.split(), autojunk=False)
    return sum(max(i2 - i1, j2 - j1) for t, i1, i2, j1, j2 in sm.get_opcodes() if t != "equal")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--exe", required=True, help="llama-server")
    ap.add_argument("--model", required=True, help="the GGUF")
    ap.add_argument("--texts", required=True, help="transcripts, one per line")
    ap.add_argument("--label", required=True)
    ap.add_argument("--args", default="", help="more llama-server flags")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--pause-ms", type=int, default=0, help="idle this long before each request")
    ap.add_argument("--suspend-ms", type=int, default=0, help="suspend the server this long before each request")
    ap.add_argument("--ping-ms", type=int, help="after the pause, send a 1-token request (as at key-down) "
                    "and wait this long (the dictation) before the measured one")
    ap.add_argument("--warm", help="polish every line of this file once first (not measured)")
    ap.add_argument("--compare", help="label of an earlier run to compare the outputs with")
    ap.add_argument("--out", default="polish-sweep")
    a = ap.parse_args()
    read = lambda p: [l.strip() for l in open(p, encoding="utf-8-sig") if l.strip() and not l.startswith("#")]
    texts = read(a.texts)
    os.makedirs(a.out, exist_ok=True)

    port = free_port()
    cmd = [a.exe, "-m", a.model, "--port", str(port)] + FIXED + shlex.split(a.args)
    if os.name == "nt" and "--device none" not in a.args:
        cmd += ["--load-mode", "none"]  # as AGT runs it on Windows (weights read, not mapped)
    log = open(os.path.join(a.out, a.label + ".log"), "w")
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, cwd=os.path.dirname(a.exe) or None)
    root = f"http://127.0.0.1:{port}"
    while True:
        try:
            with urllib.request.urlopen(root + "/health", timeout=2) as r:
                if r.status == 200:
                    break
        except OSError:
            pass
        if proc.poll() is not None:
            sys.exit(f"llama-server exited, see {log.name}")
        time.sleep(0.1)

    def polish(text, max_tokens=None):
        body = {"model": "s1-mini", "temperature": 0, "stream": False,
                "max_tokens": max_tokens or len(text.encode()) // 2 + 64,  # as AGT sends it
                "messages": [{"role": "system", "content": SYSTEM},
                             {"role": "user", "content": f"{CONTROL}\n{text}"}]}
        req = urllib.request.Request(root + "/v1/chat/completions", data=json.dumps(body).encode(),
                                     headers={"content-type": "application/json"})
        t = time.perf_counter()
        with urllib.request.urlopen(req, timeout=300) as r:
            v = json.loads(r.read())
        return (time.perf_counter() - t) * 1e3, v["choices"][0]["message"]["content"], v.get("timings", {})

    try:
        polish("um so this is uh a test")  # AGT's warmup
        for text in read(a.warm) if a.warm else []:
            polish(text)
        rows = []
        for i, text in enumerate(texts):
            for run in range(a.runs):
                if a.suspend_ms:
                    suspend(proc.pid, True)
                    time.sleep(a.suspend_ms / 1e3)
                    suspend(proc.pid, False)
                else:
                    time.sleep(a.pause_ms / 1e3)
                if a.ping_ms is not None:
                    polish("um so this is uh a test", max_tokens=1)
                    time.sleep(a.ping_ms / 1e3)
                ms, out, t = polish(text)
                rows.append({"line": i, "run": run, "words": len(text.split()), "ms": ms, "out": out,
                             "prompt_n": t.get("prompt_n"), "prompt_ms": t.get("prompt_ms"),
                             "predicted_n": t.get("predicted_n"), "predicted_ms": t.get("predicted_ms"),
                             "draft_n": t.get("draft_n", 0), "draft_accepted": t.get("draft_n_accepted", 0)})
                print(f"{i:3d} {len(text.split()):4d} words {ms:6.0f} ms | prompt {t.get('prompt_n')} tokens"
                      f" {t.get('prompt_ms', 0):.0f} ms | output {t.get('predicted_n')} tokens"
                      f" {t.get('predicted_ms', 0):.0f} ms | drafts {t.get('draft_n_accepted', 0)}/{t.get('draft_n', 0)}",
                      flush=True)
        mem = memory(proc.pid)
    finally:
        proc.terminate()
        proc.wait()
    json.dump({"label": a.label, "args": a.args, "texts": a.texts, "memory": mem, "rows": rows},
              open(os.path.join(a.out, a.label + ".json"), "w"), indent=1)

    first = [r for r in rows if r["run"] == 0]
    ms = sorted(r["ms"] for r in first)
    pct = lambda p: ms[round((len(ms) - 1) * p)]
    tok_s = sum(r["predicted_n"] for r in first) / sum(r["predicted_ms"] for r in first) * 1e3
    drafted, accepted = sum(r["draft_n"] for r in first), sum(r["draft_accepted"] for r in first)
    print(f"\n{a.label}: p50 {pct(.5):.0f} ms | p90 {pct(.9):.0f} ms | max {ms[-1]:.0f} ms | {tok_s:.0f} tokens/s"
          f" | drafts accepted {accepted}/{drafted} | {mem}")
    if a.compare:
        base = json.load(open(os.path.join(a.out, a.compare + ".json")))["rows"]
        base = {r["line"]: r["out"] for r in base if r["run"] == 0}
        same = sum(base[r["line"]] == r["out"] for r in first)
        edits = sum(word_edits(base[r["line"]], r["out"]) for r in first)
        words = sum(len(base[r["line"]].split()) for r in first)
        print(f"vs {a.compare}: {same}/{len(first)} outputs identical, {edits} word edits in {words} words")
    # A repeat of the same line reuses the cached prompt, which can change a near-tie.
    flaky = sum(len({r["out"] for r in rows if r["line"] == i}) > 1 for i in range(len(texts)))
    if flaky:
        print(f"{flaky} lines gave a different output when repeated (prompt cache hit)")


if __name__ == "__main__":
    main()
