# AGT — Actually Good Transcription

Local, fast, hold-to-talk dictation for Windows and Apple Silicon macOS, written in Rust.
Hold a hotkey, speak, release: the text is transcribed on-device and pasted into the focused app.

> Status: Phase 0. The app is a tray icon with auto-update; dictation comes in later phases.

## Layout

| Crate | Purpose |
|---|---|
| `crates/core` (`agt-core`) | Config, paths; later the pipeline and the `SttEngine` / `Polisher` traits |
| `crates/app` (`agt`) | The binary: tray icon, auto-update, logging; later the overlay |

Planned: `crates/audio`, `crates/stt-*`, `crates/polish`, `crates/platform`.

## Build

```sh
cargo run              # debug build, logs also go to stderr
cargo build --release  # target/release/agt(.exe); no console window on Windows
```

Requires stable Rust. Windows needs the MSVC build tools.

## Config and logs

| | Windows | macOS |
|---|---|---|
| Config | `%APPDATA%\AGT\config.toml` | `~/Library/Application Support/AGT/config.toml` |
| Logs | `%APPDATA%\AGT\logs\agt.YYYY-MM-DD.log` | `~/Library/Application Support/AGT/logs/` |

A commented default config is written on first run; the tray menu has "Open config folder".
Set `AGT_HOME` to use a different directory (handy for testing).

### Updates

Installed builds update themselves from this repo's public GitHub Releases: the app checks on
startup and every `update.check_interval_minutes` (default 240), downloads in the background, and
applies the update when you quit from the tray (or on next launch). Set `update.enabled = false`
to turn this off.

No token is needed. Optionally, `update.github_token` (or the `AGT_GITHUB_TOKEN` environment
variable, which takes precedence) authenticates the GitHub API calls to lift the anonymous
rate limit (60 requests/hour per IP). Never commit a token; the config file lives outside the repo.

## Releasing

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo check` to update
   `Cargo.lock`, and commit.
2. Tag and push: `git tag v0.1.0 && git push origin v0.1.0`.

The `Release` workflow checks the tag matches the Cargo version, builds Windows x64 and macOS arm64,
packages them with [Velopack](https://velopack.io) (`vpk pack`, including a delta from the previous
release), and publishes one GitHub Release with both channels (`win-x64`, `osx-arm64`).
Installers: `AGT-win-x64-Setup.exe`, `AGT-osx-arm64-Setup.pkg` (plus portable zips).

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

Signing runs when `APPLE_CERT_P12_BASE64` is set; notarization additionally needs `APPLE_ID`.

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

Windows builds are unsigned (SmartScreen will warn). To sign, add a `WINDOWS_SIGN_PARAMS` secret with
`signtool.exe` parameters (passed to `vpk pack --signParams`), or switch the hook in
`release.yml` to `--azureTrustedSignFile`.
