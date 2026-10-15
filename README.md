# AGT — Actually Good Transcription

Hold a key, speak, let go: your words are typed into whatever app you're using.
AGT is a small, fast dictation app for Windows and Apple Silicon Macs. Speech is turned into
text **on your own computer**; your voice never leaves it.

<p align="center">
  <a href="https://github.com/fancyHeat/agt/releases/latest/download/AGT-win-x64-Setup.exe"><img alt="Download for Windows" src="https://img.shields.io/badge/Download%20for%20Windows-0078D4?style=for-the-badge&logo=data:image/svg%2Bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI%2BPHBhdGggZmlsbD0id2hpdGUiIGQ9Ik0wIDBoMTEuNHYxMS40SDB6TTEyLjYgMEgyNHYxMS40SDEyLjZ6TTAgMTIuNmgxMS40VjI0SDB6TTEyLjYgMTIuNkgyNFYyNEgxMi42eiIvPjwvc3ZnPg%3D%3D"></a>
  <a href="https://github.com/fancyHeat/agt/releases/latest/download/AGT-osx-arm64-Portable.zip"><img alt="Download for macOS · Apple Silicon" src="https://img.shields.io/badge/Download%20for%20macOS%20%C2%B7%20Apple%20Silicon-000000?style=for-the-badge&logo=apple&logoColor=white"></a>
</p>

<p align="center">
  <sub>Always the newest version · <a href="https://github.com/fancyHeat/agt/releases/latest">All releases</a></sub>
</p>

> **macOS:** Apple Silicon (M1 and later) Macs running macOS 13.4 or newer. **Intel Macs are
> not supported.** See [macOS](#macos) for the permissions AGT asks for.

![The AGT overlay while recording](docs/img/overlay-recording.png)

## Quick start

1. **Install.** Windows: run `AGT-win-x64-Setup.exe`. macOS: unzip and move `AGT.app` to
   Applications.
2. **First launch.** AGT lives in the system tray (macOS: the menu bar). It downloads its
   speech model once (about 670 MB; 2.4 GB on Macs with more than 8 GB of memory, which run it
   on the GPU); the tray menu shows the progress and says "Ready" when it's done. On a Mac,
   allow the permissions it asks for first (see [macOS](#macos)).
3. **Dictate.** Click into any text field, then hold the hotkey and speak:
   - Windows: **Left Ctrl + Windows key**
   - macOS: **Fn** (the Globe key)

   A small bar at the bottom of the screen shows that AGT is listening.
4. **Let go.** A moment later the text is pasted where your cursor is.

Good to know:

- Press **Esc** while holding the hotkey to cancel.
- If a paste didn't land, tray menu → **Copy last transcript** puts the text on your clipboard.

## macOS

AGT asks for three permissions on first launch, one after another:

- **Microphone**: to hear you. Click Allow.
- **Input Monitoring** ("receive keystrokes from any application"): to notice the hotkey. Click
  Open System Settings and switch AGT on; macOS then offers to quit and reopen AGT, which it
  needs before the hotkey works.
- **Accessibility** (called **Device Control** from macOS 27 on): to paste with Cmd+V. Switch AGT
  on in the same way; this one takes effect right away.

Until everything is granted, the menu-bar item says which permission is missing, and its
"Privacy & Security settings..." item opens the right page.

The default hotkey is **Fn** (the 🌐 Globe key). Set System Settings > Keyboard > "Press 🌐 key
to" to **Do Nothing**, or releasing it also opens the emoji picker or starts Apple's dictation.
Other choices in `config.toml`: `RightOption`, `RightCommand`, `F13`-`F20`, or a chord such as
`"Fn+RightOption"`.

On Macs with more than 8 GB of memory AGT transcribes on the GPU (Core ML), which is about
twice as fast as the CPU for longer dictations. That needs the larger model (2.4 GB download,
about 3 GB of memory while AGT runs) and a one-time ~20 s preparation on first start. To use
the smaller CPU model instead, set `device = "cpu"` under `[stt]` in `config.toml`.

Known gap: Cmd+V is sent as the key in the "V" position of a QWERTY keyboard, so with a Dvorak
or Colemak layout (other than "Dvorak - QWERTY ⌘") the paste may not work.

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
- AGT goes online only to download its models and to check for updates. If you turn on the
  local AI clean-up, it also downloads llama.cpp (12–32 MB) once to run it.
- The one exception is opt-in: if you configure the optional clean-up to use a cloud service
  (such as Claude), the text of each dictation is sent to that service.

## Building from source

Install [Rust](https://rustup.rs) (on Windows also the MSVC build tools), then run `cargo run -p agt`.

## License

[MIT](LICENSE)
