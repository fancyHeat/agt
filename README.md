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

## Config, logs and data

| | Windows | macOS |
|---|---|---|
| Config | `%APPDATA%\AGT\config.toml` | `~/Library/Application Support/AGT/config.toml` |
| Logs | `%LOCALAPPDATA%\AGT\logs\agt.YYYY-MM-DD.log` | `~/Library/Application Support/AGT/logs/` |

Config is in the roaming profile; logs (and, later, models) are machine-local. On Windows
`%LOCALAPPDATA%\AGT` is also the install directory, so uninstalling removes logs and models too.
Set `AGT_HOME` to put everything in one other directory (handy for testing).

A commented default config is written on first run; the tray menu has "Open config folder".
AGT never rewrites an existing config. If the file has an error (including an unknown key), AGT
logs it, shows "Config error" in the tray menu, and runs with defaults **with updates off**, so a
typo can't silently re-enable updates you turned off.

Only one AGT runs at a time (lock file `agt.lock` in the data dir); a second launch exits.

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
- **preflight**: fails unless the tag equals the Cargo version and the commit is on `main`, then runs
  fmt, clippy and tests.
- **build**: builds Windows x64 and macOS arm64 and packages them with [Velopack](https://velopack.io)
  (`vpk pack`, including a delta from the previous release). macOS uses `packaging/macos/Info.plist`
  (version stamped in CI; `LSUIElement`, microphone usage string) and `packaging/macos/agt.entitlements`.
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
