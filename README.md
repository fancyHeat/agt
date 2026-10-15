# AGT — Actually Good Transcription

Local, fast, hold-to-talk dictation for Windows and Apple Silicon macOS (13.4+), written in Rust.
Hold a hotkey, speak, release: the text is transcribed on-device and pasted into the focused app.

> Status: Phase 1. Dictation works on Windows (hold **Right Ctrl**). On macOS the app runs
> (tray, updates) but dictation is not implemented yet.

## Using it

Hold the hotkey (default Right Ctrl), speak, release. AGT transcribes the recording with NVIDIA
Parakeet TDT 0.6B on your machine and pastes the text into whatever has focus (via the
clipboard, which it restores afterwards). The tray menu shows the state: loading/downloading
the model, ready, recording, transcribing, or an error.

- Holds shorter than `hotkey.min_hold_ms` (150 ms) are ignored, so a stray tap never pastes.
- Hotkeys (`hotkey.key`): `RightCtrl` (default), `LeftCtrl`, `RightShift`, `LeftShift` keep
  working normally in apps; `F13`-`F24` (handy for a remapped mouse or keyboard button) are
  taken over by AGT, so apps never see them. Alt, Win, CapsLock, F1-F12 etc. are not offered:
  they would either open menus on release or have to be taken away from apps.
- Pressing any other non-modifier key while holding (Esc, or a shortcut like Right Ctrl+C)
  cancels the dictation.
- If the release is missed (it happened on the UAC / Ctrl+Alt+Del screen), AGT notices within
  250 ms that a Ctrl/Shift hotkey is up and finishes the dictation normally; for F13-F24 the
  stale recording is discarded on the next press.
- Long dictations are fine: audio over 25 s is transcribed in chunks split at pauses, so time
  and memory grow linearly (a 5-minute recording takes ~12 s on an i9-9900K).
- Dictating again while the previous one is still being transcribed is fine: they're
  transcribed and pasted in order.
- The first run downloads the model (~670 MB) with progress in the tray; it's SHA-256 verified.
  If you're offline the tray shows an error; press the hotkey to retry.
- Pasting into apps running as administrator doesn't work unless AGT runs elevated too; AGT
  then puts your clipboard back, shows an error in the tray and writes the transcript to the log.

## Layout

| Crate | Purpose |
|---|---|
| `crates/core` (`agt-core`) | Config, paths, and the `SttEngine` trait |
| `crates/audio` (`agt-audio`) | Microphone capture (cpal) to 16 kHz mono, resampler, WAV reading, level meter |
| `crates/platform` (`agt-platform`) | Global hold-to-talk hotkey and clipboard paste (Windows; macOS stubs) |
| `crates/stt` (`agt-stt`) | STT engines (Parakeet on ONNX Runtime) and model downloads |
| `crates/app` (`agt`) | The binary: tray, pipeline threads, auto-update, logging |
| `crates/bench` (`agt-bench`) | Runs an engine over a folder of WAVs: latency, RTF, WER |

Planned: `crates/polish` (LLM clean-up), more engines, the macOS platform layer, an overlay.

## Build

```sh
cargo run              # debug build, logs also go to stderr
cargo build --release  # target/release/agt(.exe); no console window on Windows
```

Requires stable Rust. Windows needs the MSVC build tools. ONNX Runtime is downloaded prebuilt
(and statically linked) by the `ort` crate at build time; on Windows the build also copies
`DirectML.dll` next to the exe, which must ship with it.

## Config, logs and data

| | Windows | macOS |
|---|---|---|
| Config | `%APPDATA%\AGT\config.toml` | `~/Library/Application Support/AGT/config.toml` |
| Logs | `%LOCALAPPDATA%\AGT\logs\agt.YYYY-MM-DD.log` | `~/Library/Application Support/AGT/logs/` |
| Models | `%LOCALAPPDATA%\AGT-models\<model>\` | `~/Library/Application Support/AGT/models/<model>/` |

Config is in the roaming profile; logs and models are machine-local. On Windows
`%LOCALAPPDATA%\AGT` is also the install directory (re-running Setup.exe or uninstalling wipes
it), which is why models live in a separate `AGT-models` folder. `stt.models_dir` overrides the
models location. Set `AGT_HOME` to put everything (config, logs, lock, models) in one other
directory (handy for testing).

**Uninstalling does not remove downloaded models** (0.7-3.1 GB): delete `%LOCALAPPDATA%\AGT-models`
(macOS: `~/Library/Application Support/AGT/models`) by hand if you no longer need them.

A commented default config is written on first run; the tray menu has "Open config folder".
AGT never rewrites an existing config. If the file has an error (including an unknown key), AGT
logs it, shows "Config error" in the tray menu, and runs with defaults **with updates off**, so a
typo can't silently re-enable updates you turned off.

Only one AGT runs at a time (lock file `agt.lock` in the data dir); a second launch exits.

### Speech-to-text

`stt.engine = "parakeet"` uses the ONNX export of Parakeet TDT 0.6B
([`istupakov/parakeet-tdt-0.6b-v2-onnx`](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx),
or `-v3` for 25 European languages), pinned to a repo revision and hash-checked. Preprocessing
(NeMo's log-mel, shipped as an ONNX graph), the encoder and greedy TDT decoding run in-process
on ONNX Runtime.

| Setting | Default | |
|---|---|---|
| `stt.model` | `parakeet-tdt-0.6b-v2` | or `parakeet-tdt-0.6b-v3` |
| `stt.precision` | `int8` | encoder weights; `fp32` is 2.5 GB (decoder is always int8) |
| `stt.device` | `cpu` | `directml` runs the encoder on any DX12 GPU (Windows); falls back to the CPU (with a note in the tray) if no GPU is usable |
| `stt.threads` | `0` (auto) | CPU threads for inference |

Every dictation logs its timings at info level, e.g.
`dictation: audio 2.97 s | release→stt 0 ms | stt 157 ms | paste 4 ms | release→paste 161 ms`.

### Microphone

The input stream is built once at start-up (~40 ms) and only *started* while the hotkey is held
(~4 ms to start, first audio ~5 ms later), so the OS "microphone in use" indicator is off between
dictations. `audio.keep_mic_open = true` keeps it running instead (no start-up gap, plus the
last 300 ms before the key press is included), at the cost of the indicator staying on and ~1%
of one core.

### Benchmark

```sh
cargo run --release -p agt-bench -- [--model m] [--precision int8|fp32] [--device cpu|directml] [--runs n] [--no-warmup] <dir>
```

Transcribes every `*.wav` in `<dir>` (any rate/channels) and prints per-file latency, real-time
factor and transcript, plus WER against `<name>.txt` when present. `AGT_LOG=debug` adds
per-stage timings (preprocess / encoder / decode).

### Debug: `AGT_TEST_WAV`

If `AGT_TEST_WAV=<file.wav>` is set, every dictation transcribes that file instead of the
microphone recording (the hotkey, mic, paste and clipboard paths all run as usual). AGT logs a
warning at start-up when it's set. Used for end-to-end tests without a human speaker.

### Updates

Installed builds update themselves from this repo's public GitHub Releases: the app checks on
startup and every `update.check_interval_minutes` (default 240, minimum 30), downloads in the
background, and applies the update when you quit from the tray (or on next launch). Set
`update.enabled = false` to turn this off.

The feed is read from `releases/latest/download/` over plain HTTPS (no GitHub API, so no token
and no rate limit), with a hard timeout on every request.

## Releasing

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo check` to update
   `Cargo.lock`, and merge to `main`.
2. Tag that commit on `main` and push: `git tag v0.1.0 origin/main && git push origin v0.1.0`.

The `Release` workflow:
- **preflight**: fails unless the tag equals the Cargo version and the commit is on `main` (and rejects
  prerelease tags such as `v1.2.0-beta`, since the update feed has no prerelease channel), then runs
  fmt, clippy and tests.
- **build**: builds Windows x64 and macOS arm64 and packages them with [Velopack](https://velopack.io)
  (`vpk pack`, including a delta from the previous release). macOS uses `packaging/macos/Info.plist`
  (version stamped in CI; `LSUIElement`, microphone usage string) and `packaging/macos/agt.entitlements`.
  Windows ships `DirectML.dll` next to `agt.exe`, and Setup installs the VC++ 2015-2022 runtime
  if missing (`--framework vcredist143-x64`; the statically linked ONNX Runtime needs `MSVCP140.dll`).
- **publish**: uploads both channels (`win-x64`, `osx-arm64`) to a draft release, then publishes it once.

Installers: `AGT-win-x64-Setup.exe`, `AGT-osx-arm64-Setup.pkg` (plus portable zips).

PRs that touch the workflow or `packaging/` run the same build + package as a dry run (no signing,
no publishing); from `main` it can also be started manually (`workflow_dispatch`).

When bumping the `velopack` crate, bump `VPK_VERSION` in `.github/workflows/release.yml` to match.

### macOS signing and notarization (optional, recommended)

Without these secrets, macOS builds are unsigned and Gatekeeper will block them. Add them under
**Settings → Secrets and variables → Actions → New repository secret**:

| Secret | Value |
|---|---|
| `APPLE_CERT_P12_BASE64` | Base64 of a `.p12` containing your Developer ID certificate(s) and private key(s) |
| `APPLE_CERT_PASSWORD` | Password of that `.p12` |
| `APPLE_SIGN_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_INSTALLER_IDENTITY` | e.g. `Developer ID Installer: Your Name (TEAMID)` — optional; without it no `.pkg` is built, only the portable `.zip` |
| `APPLE_ID` | Your Apple ID email (for notarization) |
| `APPLE_TEAM_ID` | Your 10-character Team ID |
| `APPLE_APP_PASSWORD` | An app-specific password for that Apple ID |

Signing runs when `APPLE_CERT_P12_BASE64` is set (never on PR dry runs); notarization additionally
needs `APPLE_ID`. The app is signed with the hardened runtime and only the entitlements in
`packaging/macos/agt.entitlements` (microphone); keep that file minimal.

**Export the certificate as base64** (on a Mac):

1. In the [Apple Developer portal](https://developer.apple.com/account/resources/certificates/list),
   create a **Developer ID Application** certificate (and optionally **Developer ID Installer**),
   download it and double-click to add it to Keychain Access.
2. In Keychain Access → *My Certificates*, select the certificate(s) (expand to check the private
   key is there), right-click → *Export N items…* → save as `agt.p12` with a strong password
   (that's `APPLE_CERT_PASSWORD`).
3. `base64 -i agt.p12 | pbcopy` and paste into `APPLE_CERT_P12_BASE64`.
4. The identity names are shown by `security find-identity -v -p codesigning` (Application) and
   in Keychain Access (Installer). Team ID is at <https://developer.apple.com/account> → Membership.

**Create an app-specific password:** sign in at <https://account.apple.com> → *Sign-In and
Security* → *App-Specific Passwords* → *+*, name it e.g. "AGT notarization", and save the
generated password as `APPLE_APP_PASSWORD`.

### Windows signing (not set up)

Windows builds are unsigned (SmartScreen will warn). `release.yml` only has a **hook**: if a
`WINDOWS_SIGN_PARAMS` secret is set, it's passed to `vpk pack --signParams` (signtool parameters).
That alone signs nothing: a step that imports the code-signing certificate onto the runner (or a
switch to `--azureTrustedSignFile` with Azure Trusted Signing) is still to be written.
