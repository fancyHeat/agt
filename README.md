# AGT — Actually Good Transcription

Hold a key, speak, let go: your words are typed into whatever app you're using.
AGT is a small, fast dictation app for Windows and Apple Silicon Macs. Speech is turned into
text **on your own computer**; your voice never leaves it.

![The AGT overlay while recording](docs/img/overlay-recording.png)

## Download

- **Windows (x64):** [AGT-win-x64-Setup.exe](https://github.com/fancyHeat/agt/releases/latest/download/AGT-win-x64-Setup.exe)
- **macOS (Apple Silicon):** [AGT-osx-arm64-Portable.zip](https://github.com/fancyHeat/agt/releases/latest/download/AGT-osx-arm64-Portable.zip)

These links always point to the newest version. All releases:
[github.com/fancyHeat/agt/releases/latest](https://github.com/fancyHeat/agt/releases/latest).

On macOS, only Apple Silicon (M1 and later) Macs running macOS 13.4 or newer are supported.
**Intel Macs are not supported.**

> **macOS is a work in progress.** The Mac app installs, runs in the menu bar and updates
> itself, but the dictation hotkey doesn't work there yet. Windows is fully supported.

## Quick start

1. **Install.** Windows: run `AGT-win-x64-Setup.exe`. macOS: unzip and move `AGT.app` to
   Applications.
2. **First launch.** AGT lives in the system tray (macOS: the menu bar). It downloads its
   speech model (about 670 MB) once; the tray menu shows the progress and says "Ready" when
   it's done.
3. **Dictate.** Click into any text field, then hold the hotkey and speak:
   - Windows: **Left Ctrl + Left Win**
   - macOS: **Fn** (the Globe key)

   A small bar at the bottom of the screen shows that AGT is listening.
4. **Let go.** A moment later the text is pasted where your cursor is.

Good to know:

- Press **Esc** while holding the hotkey to cancel.
- If a paste didn't land, tray menu → **Copy last transcript** puts the text on your clipboard.
- On a Mac, AGT will ask for **Microphone**, **Input Monitoring** and **Accessibility** access
  (System Settings → Privacy & Security) once dictation ships there.

## Updates

AGT updates itself from this page's releases in the background; the new version is used the
next time it starts. Tray menu → **Update now** checks right away.

## Settings

Settings live in a plain text file, `config.toml`, with a comment on every option:

- Windows: `%APPDATA%\AGT\config.toml`
- macOS: `~/Library/Application Support/AGT/config.toml`

Tray menu → **Open config folder** takes you there. Restart AGT after editing. You can change
the hotkey, turn the overlay off, or enable optional AI clean-up of your text (removes "um"s,
fixes punctuation; off by default, and it runs locally unless you point it at a cloud service).

## Privacy

- Transcription runs entirely on your computer. Audio is never saved or uploaded.
- AGT goes online only to download its models and to check for updates.
- The one exception is opt-in: if you configure the optional clean-up to use a cloud service
  (such as Claude), the text of each dictation is sent to that service.

## Building from source

Install [Rust](https://rustup.rs) (on Windows also the MSVC build tools), then run `cargo run`.

## License

MIT
