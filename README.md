# AGT — Actually Good Transcription

Hold a key, speak, let go: your words are typed into whatever app you're using.
AGT is a small, fast dictation app for Windows and Apple Silicon Macs. Speech is turned into
text **on your own computer**; your voice never leaves it.

<p align="center">
  <a href="https://github.com/fancyHeat/agt/releases/latest/download/AGT-win-x64-Setup.exe"><img alt="Download for Windows" src="https://img.shields.io/badge/Download%20for%20Windows-0078D4?style=for-the-badge&logo=data:image/svg%2Bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI%2BPHBhdGggZmlsbD0id2hpdGUiIGQ9Ik0wIDBoMTEuNHYxMS40SDB6TTEyLjYgMEgyNHYxMS40SDEyLjZ6TTAgMTIuNmgxMS40VjI0SDB6TTEyLjYgMTIuNkgyNFYyNEgxMi42eiIvPjwvc3ZnPg%3D%3D"></a>
  <a href="https://github.com/fancyHeat/agt/releases/latest/download/AGT-osx-arm64-Setup.pkg"><img alt="Download for macOS · Apple Silicon" src="https://img.shields.io/badge/Download%20for%20macOS%20%C2%B7%20Apple%20Silicon-000000?style=for-the-badge&logo=apple&logoColor=white"></a>
</p>

<p align="center">
  <sub>Always the newest version · <a href="https://github.com/fancyHeat/agt/releases/latest">All releases</a></sub>
</p>

> **macOS:** Apple Silicon (M1 and later) Macs running macOS 13.4 or newer. **Intel Macs are
> not supported.** See [macOS](#macos) for the permissions AGT asks for.

![The AGT overlay while recording](docs/img/overlay-recording.png)

## Quick start

1. **Install.** Windows: run `AGT-win-x64-Setup.exe`. macOS: open `AGT-osx-arm64-Setup.pkg`
   and follow the installer.
2. **First launch.** AGT lives in the system tray (macOS: the menu bar). It downloads its
   speech model (about 670 MB) once; the tray menu shows the progress and says "Ready" when
   it's done. On a Mac,
   allow the permissions it asks for first (see [macOS](#macos)).
3. **Dictate.** Click into any text field, then hold the hotkey and speak:
   - Windows: **Left Ctrl + Windows key**
   - macOS: **Fn** (the Globe key)

   A small bar at the bottom of the screen shows that AGT is listening.
4. **Let go.** A moment later the text is pasted where your cursor is.

Good to know:

- Press **Esc** while holding the hotkey to cancel.
- Long dictations are fine (up to 30 minutes): AGT transcribes what you said before each pause
  while you keep talking, so when you let go only the last few seconds are left to transcribe.
  With S1-mini polish, a dictation of more than about 1,400 words (some 10 minutes) is pasted
  unpolished: it doesn't fit the model's context. Settings... → Polish → **Fast drafting**
  (`fast_drafting` under `[polish.s1-mini]`) polishes long text about twice as fast on a GPU,
  at the cost of an occasional different word or comma; it's off by default.
- **Hands-free:** tap the hotkey twice quickly and AGT keeps recording after you let go; press
  it once more to stop and paste. Esc cancels, and one left running for 30 minutes is
  discarded. `double_tap_ms` under `[hotkey]` in `config.toml` sets how quick (0 turns it
  off).
- While you hold the hotkey, other audio (music, videos) is turned down to 10% of its volume,
  and comes back when you let go. Settings... → **Lower other audio while dictating** turns
  this off, and **Lower other audio to** picks how quiet (`duck` and `duck_level` under `[audio]`
  in `config.toml`).
- If a paste didn't land, tray menu → **Copy last transcript** puts the text on your clipboard.
- macOS: if focus clearly wasn't in a text box when the text was pasted (you'd clicked a web
  page, a link or the desktop), the text is still pasted, and a note where the bar was says so,
  with a **Copy** button that puts the text on your clipboard. It needs the overlay on.

## macOS

AGT asks for two permissions on first launch, one after the other:

- **Microphone**: to hear you. Click Allow.
- **Accessibility** (called **Device Control** from macOS 27 on): to notice the hotkey and paste
  with Cmd+V. Click Open System Settings and switch AGT on; it takes effect right away.

Until everything is granted, the menu-bar item says which permission is missing, and its
"Privacy & Security settings..." item opens the right page.

The default hotkey is **Fn** (the 🌐 Globe key). With Accessibility granted, AGT keeps Fn's
own presses from apps, so tapping it (a hands-free double tap is two taps) doesn't also do
what System Settings > Keyboard > "Press 🌐 key to" says, such as opening the emoji picker.
Fn+key shortcuts still work, but while AGT runs, tapping 🌐 alone does nothing else. If the
picker still opens, set "Press 🌐 key to" to **Do Nothing**.
Other choices in `config.toml`: `RightOption`, `RightCommand`, `F13`-`F20`, or a chord such as
`"Fn+RightOption"`.

AGT transcribes on the CPU by default: with the usual pauses between dictations it's faster
than the GPU for anything under ~20 s, and more accurate (see
[benchmarks](docs/benchmarks.md#results-macos-apple-silicon)). If you mostly dictate for longer than
that, `device = "coreml"` under `[stt]` in `config.toml` uses the GPU instead, at the cost of a
2.5 GB download, a 2.4 GB compiled copy and a one-time ~45 s preparation on first start.

Known gap: Cmd+V is sent as the key in the "V" position of a QWERTY keyboard, so with a Dvorak
or Colemak layout (other than "Dvorak - QWERTY ⌘") the paste may not work.

## Updates

AGT updates itself from this page's releases in the background; the new version is used the
next time it starts. Tray menu → **Update now** checks right away and, if there is an update,
downloads and installs it and restarts AGT (as soon as dictation is idle) — no further clicks.

## Settings

Tray menu → **Settings...** opens the settings window: every option, in pages (General,
Speech, Polish, History, Advanced). The tray's **Quick settings** submenu has the same
choices without opening a window.

Either way, settings live in a plain text file, `config.toml`, with a comment on every option:

- Windows: `%APPDATA%\AGT\config.toml`
- macOS: `~/Library/Application Support/AGT/config.toml`

Tray menu → **Open config folder** takes you there. AGT restarts on its own to apply a change
(once dictation is idle): right away for one made in **Quick settings**, a few seconds after
the last change for one made in the settings window or the file. You can change the hotkey, turn the
overlay off, or enable optional AI clean-up of your text (removes "um"s, fixes punctuation;
off by default, and it runs locally unless you point it at a cloud service).

## Dictation history

Off by default. **Settings... → History** lets AGT keep a copy of each dictation:

- **Save transcripts**: the transcript as recognized, the cleaned-up text (if AI clean-up is
  on) and the text that was pasted.
- **Save audio**: the recording, as a WAV file (about 1.9 MB per minute of speech).
- **Keep for**: 7, 30 (the default) or 90 days, or forever. Older days are deleted
  automatically (a whole day at a time, so up to one day more may be kept).

**Open history folder** takes you there: `dictations` next to `agt.db` (Windows:
`%LOCALAPPDATA%\AGT-data\dictations`, macOS: `~/Library/Application Support/AGT/dictations`),
one folder per day with a `dictations.jsonl` file (one dictation per line) and the WAV files.
They stay on your computer as plain files and are never uploaded. Turning saving off keeps
what's already there until it's due for deletion. Uninstalling AGT on Windows removes it; on
macOS, delete the folder yourself. In `config.toml` it's the `[history]` section.

Each line looks like this (fields for what you chose not to save are left out):

```json
{"started_at":1792153385123,"time":"2026-10-16T14:23:05.123+02:00","audio":"142305-123.wav","audio_secs":3.52,"raw":"um so lets meet on tuesday","polished":"So, let's meet on Tuesday.","polish_outcome":"polished","pasted":"So, let's meet on Tuesday. "}
```

`polished` is `null` when clean-up was off, skipped or failed; `pasted` is `null` when nothing
was pasted (no speech, or the paste failed). `started_at` matches the `dictations` table in
`agt.db`, which has that dictation's timings and the settings it ran with (model, device,
clean-up style, ...), so the two can be joined, e.g. to label data with a bigger model or to
build a custom dictionary. `agt.db` keeps those rows for 180 days: history kept longer than
that no longer joins.

## Privacy

- Transcription runs entirely on your computer. Audio is never uploaded, and only saved if you
  turn on the [dictation history](#dictation-history).
- AGT goes online only to download its models and to check for updates. If you turn on the
  local AI clean-up, it also downloads llama.cpp (12–32 MB) once to run it.
- The one exception is opt-in: if you configure the optional clean-up to use a cloud service
  (such as Claude), the text of each dictation is sent to that service.
- Usage data stays local: timings, outcomes and errors go to `agt.db` (Windows:
  `%LOCALAPPDATA%\AGT-data`, macOS: next to the config), never audio or text (those only go
  to the opt-in dictation history). `agt stats` sums it up;
  raw rows are kept 180 days (events 30, sessions 365), daily summaries for good.

## Building from source

Install [Rust](https://rustup.rs) (on Windows also the MSVC build tools), then run `cargo run -p agt`.

## License

[MIT](LICENSE)
