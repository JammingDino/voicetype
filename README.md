# voicetype

Dictate into any window on Windows, with speech recognition running on your own
machine. Hold a hotkey, talk, let go, and the text is pasted (or typed) wherever
your cursor is. Nothing is sent to the cloud.

## Install

Download `voicetype-setup.exe` from [Releases](../../releases) and run it. It's
unsigned, so SmartScreen asks first: **More info → Run anyway**. Click **Yes** to
install for your account into `%LOCALAPPDATA%\Programs\voicetype` (no admin
needed), with a Start menu shortcut and an entry in **Installed apps**, where it
also uninstalls. **No** runs it once without installing. After that it updates
itself.

On first run, open **Models** and download one (Base (English), 148 MB, is a good
start). With an NVIDIA GPU, also download the GPU engine (670 MB): Large v3 Turbo
then transcribes an 11 s clip in about 0.1 s instead of about 16 s.

## Use

- **Shortcut:** `Ctrl+Alt+Space` by default. Hold to talk, or switch to press to
  start and press again to stop. Click the shortcut box and press new keys to change it.
- **Hands-free (all optional):** stop on silence, real-time typing (each phrase
  is typed as soon as you pause), a live preview in the floating indicator with
  its own update rate, and a wake word ("hey jarvis") that starts recording without
  the keyboard, with a choice of how strictly it matches, its own silence timeout
  and a sound when it triggers.
- **Output:** paste (default; fast, any character, and your clipboard text is put back
  afterwards) or synthetic keystrokes for places that block paste. Choose what goes
  after each phrase (nothing, a space, a new line, your own text), a key to press when
  a recording ends (Enter, Shift + Enter, Ctrl + Enter, Tab), and whether to drop
  the ending period.
- **Test bench:** record or load a WAV and run it through every installed model
  to compare speed and accuracy.
- **General:** run at startup (starts in the tray), close to tray, and the
  floating indicator.
- **Updates:** each launch checks GitHub for a newer release. With automatic
  updates on (the default) it downloads it, checks its SHA-256, and restarts into
  it once you're not dictating. Otherwise General shows an **Update now** button.

Settings, models and the engine live in `%APPDATA%\voicetype`.

## How it works

voicetype doesn't embed a speech engine. It downloads whisper.cpp's prebuilt
`whisper-server` (pinned to v1.9.2, SHA-256 verified) and the
[ggml whisper models](https://huggingface.co/ggerganov/whisper.cpp) (pinned to a
commit, SHA-256 verified), runs the server locally, and sends it 16 kHz WAV clips.
The server stops after 10 idle minutes and when the app quits.

| File | What |
|---|---|
| `src/engine.rs` | model table, verified downloads, server lifecycle, transcription |
| `src/audio.rs` | mic capture (cpal), resampling, WAV, voice activity detection |
| `src/dictation.rs` | hotkey sessions, auto-stop, split on pause, wake word |
| `src/output.rs` | paste / keystroke injection |
| `src/ui.rs` | window, overlay, tray |
| `src/install.rs` | the exe as its own installer and uninstaller |
| `src/update.rs` | checking GitHub releases and swapping in a new exe |

## Build

```sh
cargo build --release        # no C toolchain needed; rc.exe from the Windows SDK embeds the icon
cargo test
cargo test -- --ignored      # dictation end to end (needs samples/jfk.wav, tiny.en and the engine)
voicetype --test base.en jfk.wav    # transcribe a file from the command line
```

For testing without a mic or typing into real apps, set
`VOICETYPE_FAKE_MIC=clip.wav` (plays the file as the microphone) and
`VOICETYPE_FAKE_OUTPUT=out.txt` (appends text there instead of typing it).

## Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0 && git push origin main v0.2.0
```

The `release` workflow tests, builds and publishes `voicetype-setup.exe` and its
`.sha256`, with `packaging/release-notes.md` as the release text. Installed copies pick it up on their next launch; the updater
only offers releases that have the `.sha256` file. To test updating, run a
release build with `VOICETYPE_PRETEND_VERSION=0.0.1`. `--silent` installs (or,
with `--uninstall`, uninstalls) without prompts.
