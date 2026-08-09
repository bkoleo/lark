<p align="center">
  <img src="src-tauri/icons/128x128.png" width="96" alt="Lark">
</p>

# Lark

**A macOS dictation app that also records and transcribes your meetings, entirely on your own machine.**

Press a key, talk, and your words appear in whatever text field you were in. Start a meeting recording and you get a speaker-separated Markdown transcript when the call ends. No audio leaves the computer. No account, no subscription, no cloud.

Lark is a fork of [Handy](https://github.com/cjpais/Handy) by [CJ Pais](https://cjpais.com), MIT licensed. Handy says it is trying to be the most forkable speech-to-text app rather than the best one. This repository is that claim being tested. Everything good about the transcription pipeline is Handy's. The meeting recorder, the reliability work and the interface changes are the fork.

This is the build I use every day. It is published because it works, not because it is a product.

## What it does

**Dictation.** Tap a hotkey to start, tap again to stop. Transcription runs locally on Parakeet V3 and the text is pasted straight into the focused app.

**Meetings.** Start a recording and Lark captures two separate tracks: your microphone, and the other side of the call through a macOS system audio tap. It never joins the call. When you stop, each track is segmented and transcribed, then interleaved into one timestamped Markdown file in `~/Documents/Lark Meetings/`.

**Meeting detection.** Lark watches which processes hold the microphone. When a known meeting app takes it, a card appears offering to record. When the app releases it, the card offers to stop, and stops on its own after twenty seconds if you have walked away. It never starts or stops without being asked first.

## What is different from Handy

| Area | Change |
|---|---|
| Meeting mode | The whole thing. System audio capture through Core Audio process taps, two-track recording, energy-based segmentation, interleaved transcripts, and cross-track dedup so your microphone hearing the speakers does not double every line |
| Meeting detection | Polls Core Audio for microphone holders every two seconds, matched by bundle id and by executable path. Path matching is what catches browser meetings, because browser helper processes report no bundle id at all |
| Honest microphone feedback | The start chime only plays once real audio energy arrives. Bluetooth headsets can deliver digital silence for the first few seconds, and the original behaviour was to chime anyway and lose whatever you said into it |
| Automatic recovery | A silent stream is restarted at 2.5 seconds rather than left dead. Only if the retry is also silent do you get an alert |
| Clock drift correction | A broken Bluetooth stream can produce 523 seconds of samples inside a 310 second recording. The track is normalised to wall clock length so the two meeting tracks stay aligned |
| Transcription watchdog | A wedged transcription times out cleanly instead of hanging forever, and an orphaned recording is re-transcribed into history on the next launch |
| Overlay | Translucent capsule, draggable, position remembered per monitor, live microphone name, and a progress estimate for long dictations |
| Models | All in on Parakeet V3. The model picker is gone |
| Retention | New option that deletes the audio after 24 hours and keeps the transcripts |
| Updater | Removed, so it cannot update itself back into Handy |

## Requirements

- macOS 14.2 or later on Apple silicon. The meeting features use Core Audio process taps, which do not exist before 14.2 and are macOS only. Dictation still builds elsewhere, inherited from Handy
- About 640 MB of disk for the Parakeet V3 model, downloaded on first run
- Roughly 5 GB free for a build, and keep some free afterwards. Low disk starves swap, and a paged out model was the root cause of every hang worth reporting

## Build it

```bash
git clone https://github.com/bkoleo/lark.git
cd lark
bun install
CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri build
```

You need Rust, [Bun](https://bun.sh), `cmake` and current Xcode Command Line Tools. The build lands at `src-tauri/target/release/bundle/macos/Lark.app`. A clean build takes about twenty minutes, an incremental one about five.

**Change the signing identity before you build.** `src-tauri/tauri.conf.json` sets `signingIdentity` to `Lark Dev`, which is a self-signed certificate in my keychain. Yours will not have it and the build will fail. Set it to `"-"` for ad hoc signing, or to the name of your own certificate.

Ad hoc signing has a cost worth knowing about. macOS ties Accessibility and Microphone permissions to the signature, so every rebuild invalidates them and you re-grant by hand. A self-signed certificate with a long validity fixes that permanently and takes two minutes in Keychain Access. Create one, then put its name in `signingIdentity`.

If a build fails on a stale `clang/16` path, delete `src-tauri/target/release/build/ort-sys-*` and build again.

## Things to know before relying on it

- **The meeting transcript labels you as "Kole".** It is hardcoded in `src-tauri/src/managers/meeting.rs`. It should be a setting and it is not one yet
- **There is no notarized download.** Building it yourself is the only distribution
- **Meeting mode was built as a spike** and has been in daily use since, which is a different thing from being hardened. The two track pipeline is solid. The edges around switching audio devices mid-call are less so
- **The meeting app allowlist is a list.** Zoom, Teams, FaceTime, Slack, Discord, Webex, and the major browsers for Meet. Anything else is logged at debug level and ignored, which is where you grow the list from
- **First run asks for three permissions:** Accessibility, Microphone, and on the first meeting recording, system audio capture

## Debugging

Logs are at `~/Library/Logs/com.kole.lark/handy.log`.

```bash
# dictation
grep -E 'Using device|sample count|Transcription result|silent' ~/Library/Logs/com.kole.lark/handy.log | tail

# meetings
grep -E 'Meeting|System tap|mic silent' ~/Library/Logs/com.kole.lark/handy.log | tail
```

A dictation that never pastes usually shows `Saved WAV file` with no `Transcription completed` after it. That is the transcribe thread wedged, and it is nearly always disk pressure.

## Licence

MIT, inherited from Handy. Copyright for the original work remains with CJ Pais and the `LICENSE` file is unchanged. Modifications in this fork are released under the same terms.

The reference implementation for system audio capture and meeting detection was [anarlog](https://github.com/fastrepl/hyprnote) by fastrepl, also MIT. Both ideas, resolving microphone holders by executable path and tapping process audio through Core Audio, came from reading their code.

Upstream Handy's own README, covering the cross platform build and the Whisper model options this fork dropped, is worth reading if you want the original: [cjpais/Handy](https://github.com/cjpais/Handy).

## Related

Built by [Kole Ogundipe](https://www.tripledouble.marketing). More working systems given away at [tripledouble.marketing/build](https://www.tripledouble.marketing/build).
