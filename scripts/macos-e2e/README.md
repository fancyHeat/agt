# macOS end-to-end test driver

A tiny Swift app for testing AGT on macOS without a human speaker or keyboard. It opens a window
with a text view, makes itself frontmost, synthesizes the AGT hotkey with `CGEvent` (as hardware
would: `flagsChanged` with the device-dependent modifier bits), and measures from the key release
until the pasted text shows up in its text view. Not shipped; development only.

## Build

```sh
AGT_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" scripts/macos-e2e/build.sh
```

Sign with a real identity: macOS keys the Accessibility permission to the signature, so with ad hoc
signing you'd have to grant it again after every build.

## Permissions

- The driver needs **Accessibility** (posting keyboard events). `--prompt-only` asks for it and
  exits; turn it on in System Settings > Privacy & Security > Accessibility.
- AGT needs Microphone, Input Monitoring and Accessibility as usual (`packaging/macos/dev-app.sh`
  builds a signed AGT.app).

Launch apps with `open` so they're their own "responsible process" for TCC.

## Run

Start AGT with a test recording (every dictation transcribes this file instead of the mic), then
the driver:

```sh
open --env AGT_TEST_WAV=/path/to/clip10s.wav /path/to/AGT.app
open "target/macos-e2e/AGT Test Driver.app" --args --runs 5 --hold-ms 600 --out /tmp/e2e.txt
cat /tmp/e2e.txt          # per run: release -> text latency and the pasted text; then the median
```

Options (see the top of `main.swift`):

| Option | What it checks |
|---|---|
| `--key 61\|54\|63\|105` | Right Option (default), Right Command, Fn, F13 |
| `--overlap` | presses the key again right after releasing, so it's held during the paste: the paste must still arrive (not turn into Cmd+Option+V) |
| `--cancel` | presses Esc while holding: nothing may be pasted |
| `--missed-release` | sends the release with AGT's own injected-event mark, so AGT's tap ignores it: AGT must notice through its key-state poll (within ~250 ms) |

Make test WAVs with `say -o x.aiff "text" && afconvert -f WAVE -d LEI16@16000 -c 1 x.aiff x.wav`.
Don't type on the Mac while it runs (real key events mix with the synthesized ones).
