//! Meeting mode (spike): records the microphone (Kole) and macOS system
//! audio (everyone else on the call) as two separate 16 kHz tracks, then
//! transcribes both with the active local model and writes an interleaved,
//! speaker-labelled Markdown transcript to ~/Documents/Lark Meetings/.
//!
//! Triggered from the tray menu. Independent of the dictation pipeline —
//! it owns its own AudioRecorder so the dictation hotkey keeps working.
//!
//! The mic side carries the same scar tissue as dictation: AirPods can
//! deliver pure digital silence after a failed Bluetooth handshake, and a
//! broken stream can also run off wall-clock (observed 1.7x drift). A
//! watchdog restarts a silent mic, and the track is normalised to
//! wall-clock length when it drifts, so timestamps stay aligned with the
//! system track.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::audio_toolkit::audio::SystemAudioTap;
use crate::audio_toolkit::audio::{
    list_input_devices, read_wav_samples, repair_wav_header, save_wav_file, AudioRecorder,
    ChunkSink, StreamingWavWriter, WavRepair,
};
use crate::helpers::clamshell;
use crate::managers::meeting_calendar::{self, CalendarContext};
use crate::managers::recording_guard::{self, Marker};
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;

const SAMPLE_RATE: usize = 16_000;
const FRAME: usize = 480; // 30 ms at 16 kHz

/// Raw chunk RMS above this means the mic is delivering real signal
/// (matches the dictation flow watchdog's threshold).
const FLOW_RMS_THRESHOLD: f32 = 1e-6;
/// Restart the mic if no signal for this long.
const MIC_SILENT_RESTART_MS: u64 = 3_000;
const MAX_MIC_RESTARTS: u32 = 3;
/// While recording, re-resolve the wanted mic this often — the call's mic
/// appearing or changing, or a late-attached pin — and switch the moment
/// the answer differs from the open device.
const MIC_RECHECK_MS: u64 = 5_000;
/// Ceiling on the rewind buffer whatever the setting says. Thirty minutes is
/// ~240 MB of RAM across the two tracks, which is as far as an 8 GB machine
/// should be asked to go for audio nobody has asked to keep yet.
const MAX_PRERECORD_MINUTES: u32 = 30;

#[derive(Clone, Copy, PartialEq)]
pub enum MeetingStatus {
    Idle,
    Recording,
    Processing,
}

/// Recording lifecycle only. Transcription of a *stopped* meeting is NOT a
/// state here — it runs on detached threads counted by
/// `MeetingManager::processing`, so a new recording can always start while
/// the previous meeting is still transcribing (2026-08-11: a back-to-back
/// call could not be recorded at all — prompt AND manual trigger were both
/// swallowed — because the 30-minute standup before it was still processing).
enum MeetingState {
    Idle,
    Recording {
        /// True while this is *standby* capture: both tracks are running and
        /// capped to the last few minutes, and everything older is thrown
        /// away. Nothing is written and `status()` still answers Idle — the
        /// user has not asked for a recording. Pressing Record promotes the
        /// buffer in place (`promote_standby`) rather than starting a fresh
        /// capture, which is how a late Record still catches the beginning
        /// of the call.
        standby: bool,
        mic: AudioRecorder,
        /// Samples salvaged across mic restarts, wall-clock normalised.
        mic_prefix: Vec<f32>,
        /// System audio captured before Record was pressed. Empty unless a
        /// standby buffer was promoted; the tap itself keeps running, so
        /// this is only the part that predates the recording.
        sys_prefix: Vec<f32>,
        /// When the streams were opened. Everything measured in relative
        /// terms — the flow watchdog, restart bookkeeping, the depth of the
        /// rewind buffer — counts from here.
        capture_started: Instant,
        /// Where the audio being kept begins: the same as `capture_started`
        /// for an ordinary recording, and backdated by the length of the
        /// pre-roll when a standby buffer was promoted. Every comparison of
        /// a track's length against wall-clock uses this one.
        audio_started: Instant,
        /// ms since capture_started of the last chunk with real signal.
        flow_last_ms: Arc<AtomicU64>,
        last_restart_ms: u64,
        restarts: u32,
        /// True while the open stream is a system-default fallback because
        /// a wanted device (the call's mic or the pin) wasn't attached —
        /// shows amber on the pill until a re-check lands on a real target.
        fallback: bool,
        tap: SystemAudioTap,
        started: DateTime<Local>,
        /// Filled in on a background thread — the first-ever lookup can sit on
        /// a permission dialog for as long as the user takes to answer it, and
        /// nothing about starting a recording may wait for that.
        /// Resolved when the *recording* starts, never during standby: the
        /// answer to "what is on now" belongs to the moment the user
        /// committed, and an unpromoted buffer has no meeting to name.
        calendar: Arc<Mutex<Option<CalendarContext>>>,
        /// Where `mic`/`tap` stream their chunks to disk, live — empty
        /// (`None` inside) for as long as `standby` is true, matching the
        /// existing "nothing written during standby" rule. `start()` fills
        /// these in immediately; `promote_standby()` fills them in at the
        /// moment of promotion. See `ChunkSink` (t17873258798635ef6).
        mic_sink: ChunkSink,
        sys_sink: ChunkSink,
        /// The provisional on-disk path each sink is streaming into, once
        /// installed — `None` until installed, or if streaming failed to
        /// open (disk full, permissions). `stop_and_process` finalises
        /// these and hands them to `process()`, which renames them to the
        /// calendar-titled final name; `None` falls back to the original
        /// write-everything-at-the-end behaviour for that one track.
        mic_wav_path: Option<PathBuf>,
        sys_wav_path: Option<PathBuf>,
        /// The `<stem>.recording.json` marker guarding this recording
        /// against Lark dying (`recording_guard`). `None` during standby,
        /// or when nothing could be streamed to disk — then there is
        /// nothing on disk to recover either.
        marker: Option<PathBuf>,
    },
}

/// A relaunched Lark carries on recording a call that is still live if the
/// hole left by its death is no longer than this. Longer, and the far side
/// of the call has moved on without us: transcribe what was captured.
const RESUME_GAP_MAX: Duration = Duration::from_secs(10 * 60);

pub struct MeetingManager {
    app_handle: AppHandle,
    state: Mutex<MeetingState>,
    /// In-flight transcriptions of stopped meetings. Only the tray label and
    /// the recovery CLI read it — it never gates a new recording.
    processing: std::sync::atomic::AtomicU32,
    /// Mic picked from the recording pill — the top precedence slot
    /// (manual → call → pin → default), so the 5s re-check never fights a
    /// choice the user just made. Cleared when the recording ends.
    manual_mic: Mutex<Option<String>>,
    /// Set by `set_manual_mic` so the watchdog re-resolves on its next
    /// 500ms tick instead of waiting out the 5s re-check interval.
    recheck_asap: std::sync::atomic::AtomicBool,
}

impl MeetingManager {
    pub fn new(app_handle: &AppHandle) -> Self {
        // Sweep expired meeting WAVs at startup too — meetings don't happen
        // every day, but the disk pressure does. Same pass recovers any
        // transcript orphaned by a run that died mid-batch.
        let handle = app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            if let Ok(dir) = meetings_dir_for(&handle) {
                cleanup_old_meeting_wavs(&dir);
            }
            recover_orphaned_meetings(&handle);
        });
        Self {
            app_handle: app_handle.clone(),
            state: Mutex::new(MeetingState::Idle),
            processing: std::sync::atomic::AtomicU32::new(0),
            manual_mic: Mutex::new(None),
            recheck_asap: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Sets (or clears, with `None`) the mic picked from the recording
    /// pill. Takes effect at the watchdog's next tick (≤500ms) and lasts
    /// until the recording ends. A name that isn't attached is skipped by
    /// resolution until it (re)appears, so an unplug falls back to
    /// call → pin → default instead of going silent.
    pub fn set_manual_mic(&self, device: Option<String>) {
        log::info!("Meeting mic picked from the recording pill: {device:?}");
        *self.manual_mic.lock().unwrap() = device;
        self.recheck_asap
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Standby deliberately reports `Idle`: nothing has been asked for, and
    /// every existing caller — the tray label, the detection card, the CLI,
    /// the recovery gate — should behave exactly as it does when no buffer
    /// is running. The buffer only ever changes what pressing Record
    /// *contains*, never whether it is offered.
    pub fn status(&self) -> MeetingStatus {
        match *self.state.lock().unwrap() {
            MeetingState::Recording { standby: false, .. } => MeetingStatus::Recording,
            MeetingState::Recording { .. } | MeetingState::Idle => {
                if self.processing.load(Ordering::Relaxed) > 0 {
                    MeetingStatus::Processing
                } else {
                    MeetingStatus::Idle
                }
            }
        }
    }

    /// How much audio a live recording already holds, in seconds — which for
    /// a recording promoted from a rewind buffer starts well above zero. The
    /// pill's clock is seeded from this so it reads as what the recording
    /// *contains*; starting it at 00:00 after a rewind would say, in the one
    /// place the user is looking, that the earlier part was not caught.
    pub fn recording_elapsed_secs(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        let MeetingState::Recording {
            standby: false,
            audio_started,
            ..
        } = &*state
        else {
            return None;
        };
        Some(audio_started.elapsed().as_secs())
    }

    /// How far back Record can currently reach, and the window it is filling
    /// towards, in seconds — or `None` when no standby capture is running.
    /// The first number stops climbing once the window is full: it answers
    /// "how much of this call would I get", not "how long has it been going".
    pub fn standby_rewind(&self) -> Option<(u64, u64)> {
        let state = self.state.lock().unwrap();
        let MeetingState::Recording {
            standby: true,
            capture_started,
            ..
        } = &*state
        else {
            return None;
        };
        let cap = prerecord_secs(&self.app_handle);
        Some((capture_started.elapsed().as_secs().min(cap), cap))
    }

    /// The calendar event title matched at `start()`, if a recording is
    /// active and the calendar resolved one — for the "stop" / "stop_ask"
    /// cards, so they name the same meeting the eventual transcript will.
    /// `None` while idle/processing, while the calendar lookup is still
    /// running, or when nothing matched.
    pub fn recording_calendar_title(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        let MeetingState::Recording { calendar, .. } = &*state else {
            return None;
        };
        let title = calendar.lock().unwrap().as_ref()?.title.clone();
        title
    }

    pub fn toggle(self: &Arc<Self>) {
        match self.status() {
            // Processing must not block a new recording: transcription runs
            // on its own thread, and a back-to-back call won't wait for it.
            MeetingStatus::Idle | MeetingStatus::Processing => {
                if let Err(e) = self.start() {
                    log::error!("Failed to start meeting recording: {e}");
                }
            }
            MeetingStatus::Recording => self.stop_and_process(),
        }
    }

    /// Opens both tracks — mic (resolved through manual → call → pin →
    /// default) and the system tap — and returns them started. Shared by an
    /// ordinary recording and by standby capture, which differ only in
    /// whether the buffers are capped and whether anything is kept.
    fn open_capture(&self) -> Result<OpenCapture> {
        // Empty — nothing is written until `start()`/`promote_standby()`
        // installs a `StreamingWavWriter` into one, matching the existing
        // "standby writes nothing" rule (t17873258798635ef6).
        let mic_sink: ChunkSink = Arc::new(Mutex::new(None));
        let sys_sink: ChunkSink = Arc::new(Mutex::new(None));

        let tap = SystemAudioTap::start(sys_sink.clone())?;

        let capture_started = Instant::now();
        let flow_last_ms = Arc::new(AtomicU64::new(0));
        let flow_clone = flow_last_ms.clone();
        let mut mic = AudioRecorder::new()
            .map_err(|e| anyhow!("{e}"))?
            .with_flow_callback(move |rms| {
                if rms > FLOW_RMS_THRESHOLD {
                    flow_clone.store(
                        capture_started.elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                }
            })
            .with_chunk_sink(mic_sink.clone());
        let resolution = self.resolve_mic_device();
        let fallback = resolution.fallback;
        let source = resolution.source;
        if let Err(e) = mic.open(resolution.device) {
            // Don't leave the tap running if the mic failed.
            let _ = tap.stop();
            return Err(anyhow!("failed to open microphone: {e}"));
        }
        mic.start().map_err(|e| anyhow!("{e}"))?;

        let mic_name = mic.device_name().unwrap_or_else(|| "default".into());
        Ok(OpenCapture {
            mic,
            tap,
            capture_started,
            flow_last_ms,
            fallback,
            mic_name,
            source: source.to_string(),
            mic_sink,
            sys_sink,
        })
    }

    /// Opens fresh WAV files under a provisional name (before the calendar
    /// title is known — see `process()`) and installs them into the given
    /// sinks so the consumer threads start streaming into them immediately.
    /// Best-effort: streaming to disk during the call is a safety net, not
    /// a requirement — a failure here is logged and returned as `None`
    /// rather than failing the caller, and the recording still works
    /// exactly as it did before this task, just without the safety net for
    /// this one call (t17873258798635ef6).
    ///
    /// `mic_prefix`/`sys_prefix` — audio that predates this writer (a
    /// promoted rewind buffer, or the recovered part of a resumed
    /// recording) — are written into each file *before* the writer is
    /// installed, so a live chunk can never land ahead of them.
    fn install_streaming_writers(
        &self,
        mic_sink: &ChunkSink,
        sys_sink: &ChunkSink,
        started: DateTime<Local>,
        mic_prefix: &[f32],
        sys_prefix: &[f32],
    ) -> (Option<PathBuf>, Option<PathBuf>) {
        let out_dir = match meetings_dir_for(&self.app_handle) {
            Ok(dir) => dir,
            Err(e) => {
                log::warn!("Streaming WAV write disabled — no meetings dir: {e}");
                return (None, None);
            }
        };
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            log::warn!("Streaming WAV write disabled — could not create meetings dir: {e}");
            return (None, None);
        }

        // A stable name before the calendar title is known — the lookup
        // resolves on a background thread and may still be running (see the
        // `calendar` field). `process()` renames to the calendar-titled
        // stem once it has one; this name is only ever seen on disk if the
        // app dies before that rename runs.
        let stem = provisional_stem(started);

        let mic_path = out_dir.join(format!("{stem} (mic).wav"));
        let mic_result = match StreamingWavWriter::create(&mic_path).and_then(|mut w| {
            w.write(mic_prefix)?;
            Ok(w)
        }) {
            Ok(writer) => {
                *mic_sink.lock().unwrap() = Some(writer);
                Some(mic_path)
            }
            Err(e) => {
                log::warn!("Streaming mic WAV write disabled: {e}");
                None
            }
        };

        let sys_path = out_dir.join(format!("{stem} (system).wav"));
        let sys_result = match StreamingWavWriter::create(&sys_path).and_then(|mut w| {
            w.write(sys_prefix)?;
            Ok(w)
        }) {
            Ok(writer) => {
                *sys_sink.lock().unwrap() = Some(writer);
                Some(sys_path)
            }
            Err(e) => {
                log::warn!("Streaming system WAV write disabled: {e}");
                None
            }
        };

        (mic_result, sys_result)
    }

    /// Writes the marker and starts the sentinel for a recording whose
    /// streamed WAVs are now open. See `recording_guard`.
    fn arm_guard(
        &self,
        started: DateTime<Local>,
        mic_wav_path: Option<&std::path::Path>,
        sys_wav_path: Option<&std::path::Path>,
        calendar_title: Option<String>,
    ) -> Option<PathBuf> {
        let dir = meetings_dir_for(&self.app_handle).ok()?;
        recording_guard::arm(
            &self.app_handle,
            &dir,
            &provisional_stem(started),
            started,
            mic_wav_path,
            sys_wav_path,
            calendar_title,
        )
    }

    fn start(self: &Arc<Self>) -> Result<()> {
        // A live rewind buffer becomes the head of this recording instead of
        // being thrown away. This is the whole feature: the click is late,
        // the recording is not.
        if self.promote_standby() {
            return Ok(());
        }

        let mut state = self.state.lock().unwrap();
        if !matches!(*state, MeetingState::Idle) {
            return Err(anyhow!("meeting recording already active"));
        }
        // A pill pick belongs to one recording only — a fresh one starts on
        // automatic resolution.
        *self.manual_mic.lock().unwrap() = None;

        let capture = self.open_capture()?;

        // The model is NOT pre-loaded here: holding 640MB through a whole
        // call would hurt on 8GB. process() loads it after the recording.

        let mic_name = capture.mic_name.clone();
        let fallback = capture.fallback;
        log::info!(
            "Meeting recording started (mic: {mic_name}, via {})",
            capture.source
        );

        let started = Local::now();
        // Start streaming to disk immediately — this is a real recording
        // from the first sample, unlike standby (t17873258798635ef6).
        let (mic_wav_path, sys_wav_path) =
            self.install_streaming_writers(&capture.mic_sink, &capture.sys_sink, started, &[], &[]);
        let marker = self.arm_guard(
            started,
            mic_wav_path.as_deref(),
            sys_wav_path.as_deref(),
            None,
        );

        *state = MeetingState::Recording {
            standby: false,
            mic: capture.mic,
            mic_prefix: Vec::new(),
            sys_prefix: Vec::new(),
            capture_started: capture.capture_started,
            audio_started: capture.capture_started,
            flow_last_ms: capture.flow_last_ms,
            last_restart_ms: 0,
            restarts: 0,
            fallback,
            tap: capture.tap,
            started,
            calendar: self.spawn_calendar_lookup(marker.clone()),
            mic_sink: capture.mic_sink,
            sys_sink: capture.sys_sink,
            mic_wav_path,
            sys_wav_path,
            marker,
        };
        drop(state);

        self.spawn_mic_watchdog();
        // Always-visible while recording: the small top-right indicator,
        // labelled with the mic actually being recorded so a wrong-device
        // fallback is visible while the meeting is happening.
        crate::overlay::show_meeting_recording_indicator(&self.app_handle);
        crate::overlay::emit_meeting_mic_status(&self.app_handle, Some(&mic_name), true, fallback);
        Ok(())
    }

    /// Resolves the calendar event off-thread: the first-ever lookup can sit
    /// on a permission dialog for as long as the user takes to answer it, and
    /// nothing about starting a recording may wait for that. `process()`
    /// reads the result an hour later, so the mutex is guard enough.
    ///
    /// The title is also written into the recording's marker, so a
    /// transcript rebuilt at the next launch keeps the meeting's name.
    fn spawn_calendar_lookup(
        &self,
        marker: Option<PathBuf>,
    ) -> Arc<Mutex<Option<CalendarContext>>> {
        let calendar = Arc::new(Mutex::new(None));
        let calendar_sink = calendar.clone();
        std::thread::spawn(move || {
            if let Some(ctx) = meeting_calendar::current_event() {
                log::info!("Meeting matched calendar event: {:?}", ctx.title);
                if let (Some(marker), Some(title)) = (&marker, &ctx.title) {
                    recording_guard::set_title(marker, title);
                }
                *calendar_sink.lock().unwrap() = Some(ctx);
            }
        });
        calendar
    }

    /// Starts capping-and-discarding capture of a detected call so that a
    /// later Record can reach back into it. Does nothing when the rewind is
    /// switched off, when anything is already capturing, or when the tap or
    /// mic won't open — a failed buffer must never cost the user the
    /// ordinary "start from now" recording that the card still offers.
    pub fn start_standby(self: &Arc<Self>, app: &str) {
        let cap_secs = prerecord_secs(&self.app_handle);
        if cap_secs == 0 {
            return;
        }
        {
            let state = self.state.lock().unwrap();
            if !matches!(*state, MeetingState::Idle) {
                return;
            }
        }

        let capture = match self.open_capture() {
            Ok(capture) => capture,
            Err(e) => {
                log::warn!("Rewind buffer for the {app} call could not start: {e}");
                return;
            }
        };
        let cap_samples = (cap_secs as usize) * SAMPLE_RATE;
        capture.mic.set_sample_cap(Some(cap_samples));
        capture.tap.set_sample_cap(Some(cap_samples));

        let mic_name = capture.mic_name.clone();
        let fallback = capture.fallback;
        log::info!(
            "Rewind buffer started for the {app} call: holding the last {} min (mic: {mic_name}, via {})",
            cap_secs / 60,
            capture.source
        );

        {
            let mut state = self.state.lock().unwrap();
            // Re-checked under the lock: a recording could have started
            // while the tap was being created.
            if !matches!(*state, MeetingState::Idle) {
                drop(state);
                let _ = capture.tap.stop();
                let mut mic = capture.mic;
                let _ = mic.stop();
                let _ = mic.close();
                return;
            }
            *state = MeetingState::Recording {
                standby: true,
                mic: capture.mic,
                mic_prefix: Vec::new(),
                sys_prefix: Vec::new(),
                capture_started: capture.capture_started,
                audio_started: capture.capture_started,
                flow_last_ms: capture.flow_last_ms,
                last_restart_ms: 0,
                restarts: 0,
                fallback,
                tap: capture.tap,
                started: Local::now(),
                calendar: Arc::new(Mutex::new(None)),
                // Nothing written yet — standby's sinks stay empty until
                // `promote_standby` fills them in (t17873258798635ef6).
                mic_sink: capture.mic_sink,
                sys_sink: capture.sys_sink,
                mic_wav_path: None,
                sys_wav_path: None,
                marker: None,
            };
        }

        // The same watchdog a recording gets: a silent mic is restarted and
        // the call's own mic is followed, so the buffer is worth promoting
        // when the moment comes.
        self.spawn_mic_watchdog();
        crate::overlay::show_meeting_ready_indicator(&self.app_handle, 0, cap_secs);
        crate::overlay::emit_meeting_mic_status(&self.app_handle, Some(&mic_name), true, fallback);
    }

    /// Turns a standby buffer into a real recording without touching either
    /// stream: the caps come off, what was buffered becomes the head of the
    /// take, and the clock is wound back so the transcript's timestamps
    /// start where the audio does. Returns false if there was no buffer.
    fn promote_standby(self: &Arc<Self>) -> bool {
        let mut state = self.state.lock().unwrap();
        let MeetingState::Recording {
            standby,
            mic,
            mic_prefix,
            sys_prefix,
            capture_started,
            audio_started,
            flow_last_ms,
            last_restart_ms,
            started,
            calendar,
            fallback,
            tap,
            mic_sink,
            sys_sink,
            mic_wav_path,
            sys_wav_path,
            marker,
            ..
        } = &mut *state
        else {
            return false;
        };
        if !*standby {
            return false;
        }

        // Wall-clock is the authority on how much to keep, not either
        // track's length: a mic restarted mid-buffer holds less audio than
        // the tap, and taking the shorter of the two would throw away the
        // far side of the call as well.
        let keep_secs =
            (capture_started.elapsed().as_secs_f64()).min(prerecord_secs(&self.app_handle) as f64);
        let keep = (keep_secs * SAMPLE_RATE as f64) as usize;

        mic.set_sample_cap(None);
        tap.set_sample_cap(None);
        let mic_buffered = mic.take_buffer();
        let sys_buffered = tap.take_buffer();
        let mic_short = keep.saturating_sub(mic_buffered.len()) as f32 / SAMPLE_RATE as f32;
        *mic_prefix = align_tail(mic_buffered, keep);
        *sys_prefix = align_tail(sys_buffered, keep);

        *audio_started = Instant::now() - Duration::from_secs_f64(keep_secs);
        *started = Local::now() - chrono::Duration::milliseconds((keep_secs * 1000.0) as i64);
        *standby = false;

        // Start streaming now that this is a real recording — using the
        // backdated `started` above so the provisional filename already
        // reflects the true start of the call. The promoted pre-roll goes
        // into each file before its writer is installed, so a live chunk
        // the consumer thread delivers the instant the sink fills in lands
        // after it, never ahead of it (t17873258798635ef6).
        let (paths_mic, paths_sys) =
            self.install_streaming_writers(mic_sink, sys_sink, *started, mic_prefix, sys_prefix);
        *mic_wav_path = paths_mic;
        *sys_wav_path = paths_sys;
        *marker = self.arm_guard(
            *started,
            mic_wav_path.as_deref(),
            sys_wav_path.as_deref(),
            None,
        );
        *calendar = self.spawn_calendar_lookup(marker.clone());
        let mic_name = mic.device_name();
        let fallback = *fallback;
        // Carry the mic's *actual* state across the swap of pills. The
        // watchdog only emits on a change, so telling the new pill "audio is
        // flowing" when the stream is already silent would leave it saying so
        // until the silence ends — the one moment the user is watching.
        let now_ms = capture_started.elapsed().as_millis() as u64;
        let last_signal = flow_last_ms.load(Ordering::Relaxed).max(*last_restart_ms);
        let flowing = now_ms.saturating_sub(last_signal) < MIC_SILENT_RESTART_MS;
        drop(state);

        log::info!(
            "Meeting recording started from the rewind buffer: {:.1}s of this call already captured",
            keep_secs
        );
        if mic_short > 1.0 {
            // Honest about a partial pre-roll rather than silently shipping a
            // shorter mic track: the far side is all there, the user's own
            // voice only from the point the mic settled on the right device.
            log::warn!(
                "Rewind mic track is {mic_short:.1}s short of the far side (a mid-call mic switch or a silent stream) — padded with silence so the timestamps still line up"
            );
        }
        crate::overlay::show_meeting_recording_indicator(&self.app_handle);
        crate::overlay::emit_meeting_mic_status(
            &self.app_handle,
            mic_name.as_deref(),
            flowing,
            fallback,
        );
        true
    }

    /// Drops an unpromoted rewind buffer — the call ended, or the user said
    /// no. Nothing was ever written, so this is the whole cleanup.
    pub fn stop_standby(self: &Arc<Self>, reason: &str) -> bool {
        let taken = {
            let mut state = self.state.lock().unwrap();
            if !matches!(*state, MeetingState::Recording { standby: true, .. }) {
                return false;
            }
            std::mem::replace(&mut *state, MeetingState::Idle)
        };
        let MeetingState::Recording {
            mut mic,
            tap,
            capture_started,
            ..
        } = taken
        else {
            return false;
        };
        let _ = mic.stop();
        let _ = mic.close();
        let _ = tap.stop();
        // A pill pick belongs to the call it was made on, buffered or not.
        *self.manual_mic.lock().unwrap() = None;
        log::info!(
            "Rewind buffer discarded after {:.0}s ({reason})",
            capture_started.elapsed().as_secs_f32()
        );
        crate::overlay::hide_meeting_prompt(&self.app_handle);
        true
    }

    /// Restarts the meeting mic when it delivers digital silence — the same
    /// AirPods handshake failure dictation recovers from. Salvaged samples
    /// are normalised to wall-clock so the gap becomes silence instead of a
    /// timestamp shift.
    ///
    /// Also the recovery path for a pinned mic that wasn't attached when the
    /// recording started (2026-08-10: the Jabra was plugged in seconds after
    /// the standup recording began, and the old watchdog burned its whole
    /// restart budget re-opening the silent built-in mic, then went inert
    /// for 36 minutes). While the stream is a default fallback, the pin is
    /// re-checked every few seconds and switched to the moment it appears;
    /// a switch to a different device resets the silence-restart budget.
    fn spawn_mic_watchdog(self: &Arc<Self>) {
        let manager = self.clone();
        std::thread::spawn(move || {
            let mut was_flowing = true;
            let mut last_target_check_ms = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let mut state = manager.state.lock().unwrap();
                let MeetingState::Recording {
                    standby,
                    mic,
                    mic_prefix,
                    capture_started,
                    audio_started,
                    flow_last_ms,
                    last_restart_ms,
                    restarts,
                    fallback,
                    mic_sink,
                    ..
                } = &mut *state
                else {
                    break;
                };
                let standby = *standby;

                let now_ms = capture_started.elapsed().as_millis() as u64;
                let last_signal = flow_last_ms.load(Ordering::Relaxed).max(*last_restart_ms);
                let silent_ms = now_ms.saturating_sub(last_signal);

                // Surface silence on the recording indicator the moment it
                // crosses the threshold, and clear it when audio returns —
                // a silent track must be visible during the meeting, not
                // discovered in the transcript afterwards.
                let flowing = silent_ms < MIC_SILENT_RESTART_MS;
                if flowing != was_flowing {
                    was_flowing = flowing;
                    if !flowing {
                        log::warn!("Meeting mic delivering no audio (silent {}ms)", silent_ms);
                    }
                    crate::overlay::emit_meeting_mic_status(
                        &manager.app_handle,
                        mic.device_name().as_deref(),
                        flowing,
                        *fallback,
                    );
                }

                // Re-resolve the wanted device on a timer — the call's mic
                // appearing or CHANGING (the meeting app switched input
                // mid-call), or a late-attached pin — and switch the moment
                // a named, attached target differs from the open device.
                // Runs regardless of how many silence restarts were spent:
                // the budget caps hopeless retries of one device, never
                // recovery onto a different one.
                let recheck_asap = manager
                    .recheck_asap
                    .swap(false, std::sync::atomic::Ordering::Relaxed);
                if recheck_asap || now_ms.saturating_sub(last_target_check_ms) >= MIC_RECHECK_MS {
                    last_target_check_ms = now_ms;
                    let resolution = manager.resolve_mic_device();
                    let current = mic.device_name();
                    let wants_switch = match (&resolution.target, resolution.fallback) {
                        (Some(target), false) => current.as_deref() != Some(target.as_str()),
                        // No named target (plain default), or the target
                        // isn't attached: keep whatever stream is open —
                        // never tear down a live track for "default".
                        _ => false,
                    };
                    if wants_switch {
                        log::info!(
                            "Meeting mic target is now {:?} (via {}) — switching mid-meeting",
                            resolution.target.as_deref().unwrap_or("?"),
                            resolution.source
                        );
                        let partial = mic.stop().unwrap_or_default();
                        let _ = mic.close();
                        salvage_partial(
                            standby,
                            partial,
                            mic_prefix,
                            mic_sink,
                            audio_started,
                            "a device switch",
                        );
                        match mic.open(resolution.device).and_then(|_| mic.start()) {
                            Ok(()) => {
                                *fallback = false;
                                // A different physical device gets a fresh
                                // silence budget.
                                *restarts = 0;
                                *last_restart_ms = capture_started.elapsed().as_millis() as u64;
                                log::info!(
                                    "Meeting mic switched to {}",
                                    mic.device_name().unwrap_or_else(|| "default".into())
                                );
                                crate::overlay::emit_meeting_mic_status(
                                    &manager.app_handle,
                                    mic.device_name().as_deref(),
                                    true,
                                    false,
                                );
                            }
                            Err(e) => log::error!("Switch to {} failed: {e}", resolution.source),
                        }
                        continue;
                    } else if *fallback
                        && !resolution.fallback
                        && resolution.target.is_some()
                        && current.as_deref() == resolution.target.as_deref()
                    {
                        // The open device became the wanted one without a
                        // switch (e.g. the pin names the default we already
                        // fell back to) — clear the amber without touching
                        // the stream.
                        *fallback = false;
                        crate::overlay::emit_meeting_mic_status(
                            &manager.app_handle,
                            current.as_deref(),
                            was_flowing,
                            false,
                        );
                    }
                }

                if silent_ms < MIC_SILENT_RESTART_MS {
                    continue;
                }
                if *restarts >= MAX_MIC_RESTARTS {
                    continue; // logged on the last attempt; record whatever comes
                }

                log::warn!(
                    "Meeting mic silent for {}ms — restarting stream (attempt {}/{})",
                    silent_ms,
                    *restarts + 1,
                    MAX_MIC_RESTARTS
                );
                let prev_device = mic.device_name();
                let partial = mic.stop().unwrap_or_default();
                let _ = mic.close();
                salvage_partial(
                    standby,
                    partial,
                    mic_prefix,
                    mic_sink,
                    audio_started,
                    "a silent stream",
                );

                let resolution = manager.resolve_mic_device();
                *fallback = resolution.fallback;
                match mic.open(resolution.device).and_then(|_| mic.start()) {
                    Ok(()) => {
                        let new_device = mic.device_name();
                        log::info!(
                            "Meeting mic stream restarted (mic: {})",
                            new_device.clone().unwrap_or_else(|| "default".into())
                        );
                        // The budget exists to stop hopeless retries of ONE
                        // device; landing on a different device (the pin
                        // appeared, or the fallback changed) starts fresh —
                        // and the pill gets the new name.
                        if new_device != prev_device {
                            *restarts = 0;
                            crate::overlay::emit_meeting_mic_status(
                                &manager.app_handle,
                                new_device.as_deref(),
                                true,
                                *fallback,
                            );
                            *last_restart_ms = capture_started.elapsed().as_millis() as u64;
                            continue;
                        }
                    }
                    Err(e) => log::error!("Meeting mic restart failed: {e}"),
                }
                *restarts += 1;
                *last_restart_ms = capture_started.elapsed().as_millis() as u64;
                if *restarts == MAX_MIC_RESTARTS {
                    log::error!(
                        "Meeting mic restart limit reached — mic track may be silent from here"
                    );
                }
            }
        });
    }

    fn stop_and_process(self: &Arc<Self>) {
        let (
            mut mic,
            mic_prefix,
            sys_prefix,
            audio_started,
            restarts,
            tap,
            started,
            calendar,
            mic_sink,
            sys_sink,
            mic_wav_path,
            sys_wav_path,
            marker,
        ) = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, MeetingState::Idle) {
                // Standby is not a recording and cannot be stopped into one:
                // it is discarded by `stop_standby`, never by this path.
                MeetingState::Recording {
                    standby: false,
                    mic,
                    mic_prefix,
                    sys_prefix,
                    audio_started,
                    restarts,
                    tap,
                    started,
                    calendar,
                    mic_sink,
                    sys_sink,
                    mic_wav_path,
                    sys_wav_path,
                    marker,
                    ..
                } => (
                    mic,
                    mic_prefix,
                    sys_prefix,
                    audio_started,
                    restarts,
                    tap,
                    started,
                    calendar,
                    mic_sink,
                    sys_sink,
                    mic_wav_path,
                    sys_wav_path,
                    marker,
                ),
                other => {
                    *state = other;
                    return;
                }
            }
        };

        // Recording is over — drop the indicator regardless of how the stop
        // was triggered (card, tray, or CLI), and retire the pill's mic pick
        // with it.
        *self.manual_mic.lock().unwrap() = None;
        crate::overlay::hide_meeting_prompt(&self.app_handle);

        // Finalise the streamed WAVs BEFORE calling mic.stop()/tap.stop() —
        // those are exactly the calls that hung forever on 2026-08-21 when
        // a device dropped off the bus mid-call (t17873258798635ef6). Taking
        // the writer out of its sink is safe to do concurrently with the
        // consumer thread that feeds it: the mutex means it either sees the
        // writer for one more chunk, or sees it already gone and simply
        // stops writing — never a torn write either way. So even if
        // everything below this line hangs forever, the file already on
        // disk is valid and holds the audio up to this exact moment.
        let mic_wav_path = finalize_streamed_wav(&mic_sink, mic_wav_path, "mic");
        let sys_wav_path = finalize_streamed_wav(&sys_sink, sys_wav_path, "system");
        // The recording ended on purpose: the sentinel stands down. The
        // marker itself stays until the transcript is written, so a death
        // during transcription is finished at the next launch.
        if let Some(marker) = &marker {
            recording_guard::mark_ended(marker);
        }

        let mut mic_samples = mic_prefix;
        match mic.stop() {
            Ok(samples) => mic_samples.extend(samples),
            Err(e) => log::error!("Failed to stop meeting mic: {e}"),
        }
        let _ = mic.close();
        let mut sys_samples = sys_prefix;
        match tap.stop() {
            Ok(samples) => sys_samples.extend(samples),
            Err(e) => log::error!("Failed to stop system tap: {e}"),
        }

        // A broken Bluetooth stream can run off wall-clock; normalise so
        // mic timestamps line up with the system track. Measured from the
        // start of the *audio*, which a promoted rewind buffer puts before
        // the moment Record was pressed.
        let wall_secs = audio_started.elapsed().as_secs_f64();
        let expected = (wall_secs * SAMPLE_RATE as f64) as usize;
        let drift = (mic_samples.len() as f64 - expected as f64).abs() / expected.max(1) as f64;
        if drift > 0.05 {
            log::warn!(
                "Meeting mic track drifted {:.0}% off wall-clock ({:.1}s vs {:.1}s) — normalising",
                drift * 100.0,
                mic_samples.len() as f32 / SAMPLE_RATE as f32,
                wall_secs
            );
            fit_to_length(&mut mic_samples, expected);
        }

        log::info!(
            "Meeting recording stopped: wall {:.1}s, mic {:.1}s ({restarts} restarts), system {:.1}s — transcribing",
            wall_secs,
            mic_samples.len() as f32 / SAMPLE_RATE as f32,
            sys_samples.len() as f32 / SAMPLE_RATE as f32,
        );

        let manager = self.clone();
        self.processing.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            // Decrements on every exit, panic included — a leaked count would
            // pin the tray label and block the recovery CLI forever.
            let _guard = ProcessingGuard(&manager.processing);
            let calendar = calendar.lock().unwrap().clone();
            // Cloned before `process()` takes ownership — the "saved" card
            // below wants the same title, not a re-derived one.
            let saved_title = calendar.as_ref().and_then(|c| c.title.clone());
            let result = manager.process(
                mic_samples,
                sys_samples,
                started,
                calendar,
                mic_wav_path,
                sys_wav_path,
            );
            drop(_guard);
            // A new recording may have started while this one transcribed —
            // only reset the tray when nothing is live.
            if manager.status() != MeetingStatus::Recording {
                crate::tray::update_tray_menu(
                    &manager.app_handle,
                    &crate::tray::TrayIconState::Idle,
                    None,
                );
            }
            match result {
                Ok(path) => {
                    log::info!("Meeting transcript written to {}", path.display());
                    if let Some(marker) = &marker {
                        recording_guard::disarm(marker);
                    }
                    let _ = manager
                        .app_handle
                        .opener()
                        .open_path(path.to_string_lossy().to_string(), None::<String>);
                    // Visible "it's done" beat. The card is an always-on-top
                    // panel, so this confirmation shows even while the main
                    // window is hidden during/after a call.
                    crate::overlay::show_meeting_prompt(
                        &manager.app_handle,
                        "saved",
                        "",
                        saved_title.as_deref(),
                        None,
                        None,
                    );
                }
                Err(e) => log::error!(
                    "Meeting transcription failed: {e} — the audio is on disk and the next launch will try again"
                ),
            }
        });
    }

    /// The process is ending — a confirmed quit, a signal, a logout. Leaves
    /// the streamed WAVs valid on disk and the marker saying the recording
    /// was ended on purpose, so the next launch builds the transcript and
    /// the sentinel stays quiet. Never blocks: the exit is happening
    /// whatever this function manages to do, so it waits at most two
    /// seconds for the state lock and leaves the audio devices to the OS
    /// rather than risk the teardown hang of 2026-08-21.
    pub fn finalize_for_exit(&self, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = loop {
            match self.state.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    log::error!(
                        "Lark exiting ({reason}): the recording state stayed locked — the streamed WAV headers were refreshed within the last second and are repaired at the next launch"
                    );
                    recording_guard::on_exit();
                    return;
                }
            }
        };
        match std::mem::replace(&mut *state, MeetingState::Idle) {
            MeetingState::Idle => {}
            MeetingState::Recording {
                standby: true,
                mic,
                tap,
                ..
            } => {
                std::mem::forget(mic);
                std::mem::forget(tap);
                log::info!("Lark exiting ({reason}) with a rewind buffer running — discarded");
            }
            MeetingState::Recording {
                mic,
                tap,
                mic_sink,
                sys_sink,
                mic_wav_path,
                sys_wav_path,
                marker,
                started,
                audio_started,
                ..
            } => {
                let mic_ok = finalize_streamed_wav(&mic_sink, mic_wav_path, "mic").is_some();
                let sys_ok = finalize_streamed_wav(&sys_sink, sys_wav_path, "system").is_some();
                std::mem::forget(mic);
                std::mem::forget(tap);
                if let Some(marker) = &marker {
                    recording_guard::mark_ended(marker);
                }
                log::warn!(
                    "Lark exiting ({reason}) while recording {:?} ({:.0}s in): audio finalised on disk (mic {}, system {}); the transcript will be built at the next launch",
                    provisional_stem(started),
                    audio_started.elapsed().as_secs_f32(),
                    if mic_ok { "ok" } else { "not streamed" },
                    if sys_ok { "ok" } else { "not streamed" },
                );
            }
        }
        drop(state);
        recording_guard::on_exit();
    }

    /// Finishes every recording a previous run left unfinished — the
    /// marker files in the meetings folder (`recording_guard`). Run once,
    /// shortly after launch. For each marker: the WAV headers are repaired
    /// if the run died without finalising them, then either the recording
    /// is **resumed** (the call is still live, the hole is short, and the
    /// recording was not ended on purpose) or its **transcript is built**
    /// from the audio. Nothing here needs a hand to touch a file.
    pub fn recover_dead_recordings(self: &Arc<Self>) {
        let Ok(dir) = meetings_dir_for(&self.app_handle) else {
            return;
        };
        let own_pid = std::process::id();
        for (marker_path, marker) in recording_guard::markers_in(&dir) {
            if marker.pid == own_pid {
                continue;
            }
            let prefix = recording_guard::stem_prefix(&marker.stem).to_string();
            if transcript_exists(&dir, &prefix) {
                log::info!(
                    "Recording {:?} already has its transcript — clearing the marker it left",
                    marker.stem
                );
                recording_guard::disarm(&marker_path);
                continue;
            }
            let Some(started) = marker.started() else {
                log::warn!(
                    "Recording marker {} has an unreadable start time — left alone",
                    marker_path.display()
                );
                continue;
            };

            let mic_path = locate_track(&dir, marker.mic_wav.as_deref(), &prefix, "mic");
            let sys_path = locate_track(&dir, marker.sys_wav.as_deref(), &prefix, "system");
            // The hole: from the last write (the header is refreshed every
            // second, so ≤1s before the death) to now. Measured BEFORE the
            // header repair below, which rewrites the file and would move its
            // mtime to now — the 2026-10-08 drill read a 0s hole for an 8s
            // death that way. Filled with silence if the recording resumes,
            // so the timestamps on both sides of it stay true.
            let newest_write = [&mic_path, &sys_path]
                .into_iter()
                .flatten()
                .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
                .max();
            let gap = newest_write
                .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                .unwrap_or(RESUME_GAP_MAX + Duration::from_secs(1));
            for (path, label) in [(&mic_path, "mic"), (&sys_path, "system")] {
                let Some(path) = path else { continue };
                match repair_wav_header(path) {
                    Ok(WavRepair::Repaired { data_bytes }) => log::warn!(
                        "Repaired the {label} WAV header of {:?}: {:.1}s of audio was on disk with a zeroed header",
                        marker.stem,
                        data_bytes as f32 / 2.0 / SAMPLE_RATE as f32
                    ),
                    Ok(WavRepair::Intact) => {}
                    Err(e) => log::warn!(
                        "Could not check the {label} WAV header of {:?}: {e}",
                        marker.stem
                    ),
                }
            }
            let read_track = |path: &Option<PathBuf>, label: &str| -> Vec<f32> {
                let Some(path) = path else {
                    return Vec::new();
                };
                read_wav_samples(path).unwrap_or_else(|e| {
                    log::warn!("No readable {label} track for {:?} ({e})", marker.stem);
                    Vec::new()
                })
            };
            let mic_samples = read_track(&mic_path, "mic");
            let sys_samples = read_track(&sys_path, "system");
            if mic_samples.is_empty() && sys_samples.is_empty() {
                log::error!(
                    "Recording {:?} died and left no readable audio — its marker is kept for inspection: {}",
                    marker.stem,
                    marker_path.display()
                );
                continue;
            }

            let call_live = crate::managers::meeting_detect::call_mic(own_pid as i32).is_some();
            let alerted_flag = recording_guard::alerted_path(&marker_path);
            let already_alerted = alerted_flag.exists();
            let _ = std::fs::remove_file(&alerted_flag);
            let title = marker.calendar_title.clone();

            if !marker.ended_by_lark && call_live && gap <= RESUME_GAP_MAX {
                match self.start_resumed(
                    &marker,
                    started,
                    mic_samples.clone(),
                    sys_samples.clone(),
                    gap,
                ) {
                    Ok(()) => {
                        if !already_alerted {
                            crate::audio_feedback::play_alert("recording resumed after a death");
                        }
                        continue;
                    }
                    Err(e) => log::error!(
                        "Could not resume the recording {:?} ({e}) — transcribing what was captured instead",
                        marker.stem
                    ),
                }
            }

            if marker.ended_by_lark {
                log::info!(
                    "Recovered a recording that died before its transcript was written: {:?} (mic {:.1}s, system {:.1}s)",
                    marker.stem,
                    mic_samples.len() as f32 / SAMPLE_RATE as f32,
                    sys_samples.len() as f32 / SAMPLE_RATE as f32,
                );
            } else {
                log::warn!(
                    "Recovered a recording that died while live: {:?} (mic {:.1}s, system {:.1}s; {} — not resuming)",
                    marker.stem,
                    mic_samples.len() as f32 / SAMPLE_RATE as f32,
                    sys_samples.len() as f32 / SAMPLE_RATE as f32,
                    if !call_live {
                        "no call is live now".to_string()
                    } else {
                        format!("the hole is {:.0}s long", gap.as_secs_f32())
                    }
                );
                if !already_alerted {
                    crate::audio_feedback::play_alert("recording died while live");
                }
            }
            if let Some(title) = &title {
                log::info!(
                    "Recovered recording {:?} keeps the calendar title {title:?} from the marker",
                    marker.stem
                );
            }
            self.transcribe_recovered(
                &dir,
                marker_path,
                title,
                started,
                mic_samples,
                sys_samples,
                mic_path,
                sys_path,
            );
        }

        self.recover_unmarked_recordings(&dir);
    }

    /// Recordings a build *without* markers left behind: a provisional
    /// `… Meeting (mic|system).wav` pair with no transcript, untouched for
    /// a minute, guarded by no marker. Transcribed with no title.
    fn recover_unmarked_recordings(self: &Arc<Self>, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut prefixes: Vec<String> = Vec::new();
        let cutoff = std::time::SystemTime::now() - Duration::from_secs(60);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name
                .strip_suffix(" (mic).wav")
                .or_else(|| name.strip_suffix(" (system).wav"))
            else {
                continue;
            };
            if !stem.ends_with(" Meeting") {
                continue;
            }
            let prefix = recording_guard::stem_prefix(stem).to_string();
            if prefixes.contains(&prefix)
                || transcript_exists(dir, &prefix)
                || recording_guard::guarded(dir, &prefix)
            {
                continue;
            }
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t > cutoff)
                .unwrap_or(true);
            if recent {
                continue;
            }
            prefixes.push(prefix);
        }
        for prefix in prefixes {
            let Some(started) = parse_stem_start(&prefix) else {
                continue;
            };
            let mic_path = locate_track(dir, None, &prefix, "mic");
            let sys_path = locate_track(dir, None, &prefix, "system");
            for path in [&mic_path, &sys_path].into_iter().flatten() {
                if let Ok(WavRepair::Repaired { .. }) = repair_wav_header(path) {
                    log::warn!("Repaired the WAV header of {}", path.display());
                }
            }
            let mic_samples = mic_path
                .as_ref()
                .and_then(|p| read_wav_samples(p).ok())
                .unwrap_or_default();
            let sys_samples = sys_path
                .as_ref()
                .and_then(|p| read_wav_samples(p).ok())
                .unwrap_or_default();
            if mic_samples.is_empty() && sys_samples.is_empty() {
                continue;
            }
            log::warn!(
                "Recovered a recording that died with no marker (an older build): {prefix} Meeting (mic {:.1}s, system {:.1}s)",
                mic_samples.len() as f32 / SAMPLE_RATE as f32,
                sys_samples.len() as f32 / SAMPLE_RATE as f32,
            );
            self.transcribe_recovered(
                dir,
                dir.join(format!(
                    "{prefix} Meeting{}",
                    recording_guard::MARKER_SUFFIX
                )),
                None,
                started,
                mic_samples,
                sys_samples,
                mic_path,
                sys_path,
            );
        }
    }

    /// Carries on a recording a previous run died in the middle of: the
    /// audio already on disk becomes the head of the take, the hole is
    /// filled with silence, and fresh capture continues into the same
    /// files under the same name — so the transcript reads as one meeting.
    #[allow(clippy::too_many_arguments)]
    fn start_resumed(
        self: &Arc<Self>,
        marker: &Marker,
        started: DateTime<Local>,
        mut mic_prefix: Vec<f32>,
        mut sys_prefix: Vec<f32>,
        gap: Duration,
    ) -> Result<()> {
        // The call detector may already have a rewind buffer running for
        // this same call: it is discarded — the recording predates it.
        self.stop_standby("resuming the recording that died");

        let mut state = self.state.lock().unwrap();
        if !matches!(*state, MeetingState::Idle) {
            return Err(anyhow!("a recording is already active"));
        }
        *self.manual_mic.lock().unwrap() = None;
        let capture = self.open_capture()?;

        let recovered_secs = mic_prefix.len().max(sys_prefix.len()) as f32 / SAMPLE_RATE as f32;
        let gap_samples = (gap.as_secs_f64() * SAMPLE_RATE as f64) as usize;
        let head = mic_prefix.len().max(sys_prefix.len()) + gap_samples;
        mic_prefix.resize(head, 0.0);
        sys_prefix.resize(head, 0.0);
        let audio_started =
            Instant::now() - Duration::from_secs_f64(head as f64 / SAMPLE_RATE as f64);

        let (mic_wav_path, sys_wav_path) = self.install_streaming_writers(
            &capture.mic_sink,
            &capture.sys_sink,
            started,
            &mic_prefix,
            &sys_prefix,
        );
        let title = marker.calendar_title.clone();
        let new_marker = self.arm_guard(
            started,
            mic_wav_path.as_deref(),
            sys_wav_path.as_deref(),
            title.clone(),
        );
        let calendar = Arc::new(Mutex::new(title.clone().map(|t| CalendarContext {
            title: Some(t),
            ..Default::default()
        })));
        let mic_name = capture.mic_name.clone();
        let fallback = capture.fallback;
        *state = MeetingState::Recording {
            standby: false,
            mic: capture.mic,
            mic_prefix,
            sys_prefix,
            capture_started: capture.capture_started,
            audio_started,
            flow_last_ms: capture.flow_last_ms,
            last_restart_ms: 0,
            restarts: 0,
            fallback,
            tap: capture.tap,
            started,
            calendar,
            mic_sink: capture.mic_sink,
            sys_sink: capture.sys_sink,
            mic_wav_path,
            sys_wav_path,
            marker: new_marker,
        };
        drop(state);

        self.spawn_mic_watchdog();
        log::warn!(
            "Lark restarted mid-call and resumed the recording {:?}: {recovered_secs:.1}s recovered from disk, a {:.0}s hole filled with silence (mic: {mic_name})",
            marker.stem,
            gap.as_secs_f32()
        );
        crate::overlay::show_meeting_recording_indicator(&self.app_handle);
        crate::overlay::emit_meeting_mic_status(&self.app_handle, Some(&mic_name), true, fallback);
        crate::overlay::show_meeting_prompt(
            &self.app_handle,
            "resumed",
            "",
            title.as_deref(),
            None,
            None,
        );
        crate::tray::update_tray_menu(&self.app_handle, &crate::tray::TrayIconState::Idle, None);
        Ok(())
    }

    /// Builds the transcript of a recovered recording on a detached thread,
    /// the same way a stopped meeting's is built. The marker is cleared
    /// only once the `.md` is on disk; on failure it stays, and the next
    /// launch tries again.
    #[allow(clippy::too_many_arguments)]
    fn transcribe_recovered(
        self: &Arc<Self>,
        dir: &std::path::Path,
        marker_path: PathBuf,
        title: Option<String>,
        started: DateTime<Local>,
        mic_samples: Vec<f32>,
        sys_samples: Vec<f32>,
        mic_path: Option<PathBuf>,
        sys_path: Option<PathBuf>,
    ) {
        recording_guard::remove_stale_sidecars(
            dir,
            recording_guard::stem_prefix(&provisional_stem(started)),
        );
        let calendar = title.clone().map(|t| CalendarContext {
            title: Some(t),
            ..Default::default()
        });
        let manager = self.clone();
        self.processing.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let _guard = ProcessingGuard(&manager.processing);
            let result = manager.process(
                mic_samples,
                sys_samples,
                started,
                calendar,
                mic_path,
                sys_path,
            );
            drop(_guard);
            if manager.status() != MeetingStatus::Recording {
                crate::tray::update_tray_menu(
                    &manager.app_handle,
                    &crate::tray::TrayIconState::Idle,
                    None,
                );
            }
            match result {
                Ok(path) => {
                    log::info!("Meeting transcript written to {}", path.display());
                    recording_guard::disarm(&marker_path);
                    crate::overlay::show_meeting_prompt(
                        &manager.app_handle,
                        "recovered",
                        "",
                        title.as_deref(),
                        None,
                        None,
                    );
                }
                Err(e) => log::error!(
                    "Recovered meeting transcription failed ({e}) — the marker stays so the next launch tries again: {}",
                    marker_path.display()
                ),
            }
        });
    }

    /// Re-transcribes a meeting from the `(mic).wav` / `(system).wav` tracks it
    /// left next to the transcript, overwriting the `.md` in place.
    ///
    /// The transcript is the only lossy step in meeting mode — the tracks are
    /// raw audio, so a transcript spoiled by a bad model, a bad setting or a
    /// bad custom-word list is recoverable for as long as the WAVs survive
    /// their 24h retention. Without this the only route back was re-recording
    /// a call that already happened.
    ///
    /// `path` may point at either track or at the transcript itself. The
    /// calendar is deliberately not consulted: it can only answer "what is on
    /// now", and re-reading it hours later would staple an unrelated meeting's
    /// title onto this one.
    pub fn retranscribe_from_wavs(self: &Arc<Self>, path: &std::path::Path) -> Result<()> {
        if !matches!(self.status(), MeetingStatus::Idle) {
            return Err(anyhow!("a meeting is already recording or processing"));
        }

        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("no parent directory for {}", path.display()))?
            .to_path_buf();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("unreadable path {}", path.display()))?;
        let stem = name
            .trim_end_matches(".md")
            .trim_end_matches(".wav")
            .trim_end_matches(" (mic)")
            .trim_end_matches(" (system)")
            .to_string();

        // Recover the original start time from the "YYYY-MM-DD HHMM …" stem so
        // the rebuilt transcript keeps the timestamps it was recorded with.
        let started = chrono::NaiveDateTime::parse_from_str(
            stem.get(..15).unwrap_or_default(),
            "%Y-%m-%d %H%M",
        )
        .map_err(|e| anyhow!("cannot read a start time from {stem:?}: {e}"))?
        .and_local_timezone(Local)
        .single()
        .ok_or_else(|| anyhow!("ambiguous local start time in {stem:?}"))?;

        let mic_path = dir.join(format!("{stem} (mic).wav"));
        let sys_path = dir.join(format!("{stem} (system).wav"));
        let mic_samples = read_wav_samples(&mic_path).unwrap_or_else(|e| {
            log::warn!("No mic track for {stem:?} ({e})");
            Vec::new()
        });
        let sys_samples = read_wav_samples(&sys_path).unwrap_or_else(|e| {
            log::warn!("No system track for {stem:?} ({e})");
            Vec::new()
        });
        if mic_samples.is_empty() && sys_samples.is_empty() {
            return Err(anyhow!("no audio tracks found for {stem:?}"));
        }

        log::info!(
            "Re-transcribing {stem:?}: mic {:.1}s, system {:.1}s",
            mic_samples.len() as f32 / SAMPLE_RATE as f32,
            sys_samples.len() as f32 / SAMPLE_RATE as f32,
        );

        self.processing.fetch_add(1, Ordering::SeqCst);
        let manager = self.clone();
        std::thread::spawn(move || {
            let _guard = ProcessingGuard(&manager.processing);
            let result = manager.process(mic_samples, sys_samples, started, None, None, None);
            drop(_guard);
            if manager.status() != MeetingStatus::Recording {
                crate::tray::update_tray_menu(
                    &manager.app_handle,
                    &crate::tray::TrayIconState::Idle,
                    None,
                );
            }
            match result {
                Ok(path) => log::info!("Meeting re-transcribed to {}", path.display()),
                Err(e) => log::error!("Meeting re-transcription failed: {e}"),
            }
        });
        Ok(())
    }

    fn process(
        &self,
        mic_samples: Vec<f32>,
        sys_samples: Vec<f32>,
        started: DateTime<Local>,
        calendar: Option<CalendarContext>,
        mic_wav_path: Option<PathBuf>,
        sys_wav_path: Option<PathBuf>,
    ) -> Result<PathBuf> {
        let out_dir = meetings_dir_for(&self.app_handle)?;
        std::fs::create_dir_all(&out_dir)?;

        // "2026-08-01 0930 Wilow standup" beats "2026-08-01 0930 Meeting" for a
        // human scanning the folder, and gives the downstream agents something
        // to put in a recap headline. Date-first so the folder sorts by time.
        let label = calendar
            .as_ref()
            .and_then(|c| c.title.as_deref())
            .map(sanitise_for_filename)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Meeting".to_string());
        let stem = format!("{} {}", started.format("%Y-%m-%d %H%M"), label);

        cleanup_old_meeting_wavs(&out_dir);

        // The tracks were streamed to disk live and finalised in
        // `stop_and_process` — get them into place by renaming to the now-
        // known calendar-titled stem, and only fall back to writing them
        // from the in-memory buffer (the original, at-risk behaviour) if
        // streaming produced nothing usable for that track
        // (t17873258798635ef6). Either way the file that ends up at this
        // path is what the transcript and the recovery CLI point to.
        if !mic_samples.is_empty() {
            finish_meeting_wav(
                &out_dir.join(format!("{stem} (mic).wav")),
                mic_wav_path.as_deref(),
                &mic_samples,
                "mic",
            );
        }
        if !sys_samples.is_empty() {
            finish_meeting_wav(
                &out_dir.join(format!("{stem} (system).wav")),
                sys_wav_path.as_deref(),
                &sys_samples,
                "system",
            );
        }

        // The model loads lazily for dictation because the hotkey press
        // initiates it; here WE are the initiator — without this every
        // segment fails with "Model is not loaded".
        let tm = self.app_handle.state::<Arc<TranscriptionManager>>();
        tm.initiate_model_load();
        let deadline = Instant::now() + Duration::from_secs(120);
        while !tm.is_model_loaded() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        if !tm.is_model_loaded() {
            return Err(anyhow!("transcription model failed to load within 120s"));
        }

        let mic_segments = segment_active_regions(&mic_samples);
        let sys_segments = segment_active_regions(&sys_samples);
        let mic_active_secs: usize = mic_segments
            .iter()
            .map(|(s, e)| (e - s) / SAMPLE_RATE)
            .sum();

        // Crash-safety: append every segment to a sidecar the moment it comes
        // back, before anything else can fail. Batch transcription of a long
        // meeting has been OOM-killed on this 8GB machine mid-run (a 31-min
        // customer demo, 2026-06-19), and because the `.md` was only written
        // at the very end, 88 good segments died with the process. The sidecar
        // carries the track label too, which the log-scrape recovery could not.
        let partial_path = out_dir.join(format!("{stem}.partial.jsonl"));
        let mut partial = std::fs::File::create(&partial_path).ok();

        let mut entries: Vec<(usize, &'static str, String)> = Vec::new();
        for (samples, segments, speaker) in [
            (&mic_samples, &mic_segments, "Kole"),
            (&sys_samples, &sys_segments, "Them"),
        ] {
            for &(start, end) in segments {
                match tm.transcribe(samples[start..end].to_vec()) {
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            append_partial(partial.as_mut(), start, speaker, &text);
                            entries.push((start, speaker, text));
                        }
                    }
                    Err(e) => log::error!(
                        "Meeting segment transcription failed ({speaker} @{start}): {e}"
                    ),
                }
            }
        }
        drop(partial);
        entries.sort_by_key(|(start, _, _)| *start);
        let entries = drop_mic_bleed(entries);

        let duration_secs = mic_samples.len().max(sys_samples.len()) / SAMPLE_RATE;
        let mut md = String::new();

        // YAML frontmatter, because the consumers are machines as much as they
        // are Kole: the Meeting Digest needs a title and an attendee list to
        // decide whether a recording qualifies for a recap, and to head it.
        // Emitted unconditionally so a parser never has to handle its absence.
        md.push_str("---\n");
        md.push_str(&format!("date: {}\n", started.format("%Y-%m-%d")));
        md.push_str(&format!("start: {}\n", started.format("%H:%M")));
        md.push_str(&format!("duration_minutes: {}\n", duration_secs / 60));
        match calendar.as_ref().and_then(|c| c.title.as_deref()) {
            Some(title) => md.push_str(&format!("title: {}\n", yaml_scalar(title))),
            None => md.push_str("title: null\n"),
        }
        let attendees = calendar
            .as_ref()
            .map(|c| c.attendees.clone())
            .unwrap_or_default();
        if attendees.is_empty() {
            md.push_str("attendees: []\n");
        } else {
            md.push_str("attendees:\n");
            for a in &attendees {
                md.push_str(&format!("  - {}\n", yaml_scalar(a)));
            }
        }
        // Lets a consumer tell "the calendar said nobody was there" apart from
        // "we never asked the calendar", which change the meaning of `[]`.
        md.push_str(&format!("calendar_matched: {}\n", calendar.is_some()));
        md.push_str("source: lark\n");
        md.push_str("---\n\n");

        md.push_str(&format!(
            "# {} — {}\n\n",
            calendar
                .as_ref()
                .and_then(|c| c.title.as_deref())
                .unwrap_or("Meeting"),
            started.format("%A %-d %B %Y, %H:%M")
        ));
        md.push_str(&format!(
            "- Duration: {} min {} sec\n- Transcribed locally by Lark\n",
            duration_secs / 60,
            duration_secs % 60
        ));
        if duration_secs > 60 && mic_active_secs < 3 {
            md.push_str(
                "- Note: the mic track was almost entirely silent — only the system side was captured. (AirPods handshake? Check the log.)\n",
            );
        }
        let transcript_body = render_transcript(&entries);

        // AI notes go ABOVE the transcript: the downstream agents (Meeting
        // Digest, The Surveyor) read summaries, not transcripts — that is how
        // they consumed Granola, whose free tier never exposed transcripts.
        match self.meeting_notes(&transcript_body) {
            Ok(Some(notes)) => {
                md.push_str("\n## Notes\n\n");
                md.push_str(notes.trim());
                md.push_str("\n");
            }
            Ok(None) => {}
            // A failed summary must never cost the transcript.
            Err(e) => {
                log::warn!("Meeting notes generation failed: {e}");
                md.push_str(&format!("\n## Notes\n\n_Not generated: {e}_\n"));
            }
        }

        md.push_str("\n## Transcript\n\n");
        md.push_str(&transcript_body);

        let md_path = out_dir.join(format!("{stem}.md"));
        std::fs::write(&md_path, md)?;
        // The transcript is safely on disk — the sidecar has done its job.
        let _ = std::fs::remove_file(&partial_path);
        Ok(md_path)
    }

    /// Summary + action items via the existing post-process LLM plumbing.
    /// Returns `Ok(None)` when the feature is off or unconfigured — that is a
    /// normal state, not an error, and must not surface as a failure note.
    fn meeting_notes(&self, transcript: &str) -> Result<Option<String>> {
        let settings = get_settings(&self.app_handle);
        if !settings.meeting_notes_enabled || transcript.trim().is_empty() {
            return Ok(None);
        }

        let provider = settings
            .post_process_providers
            .iter()
            .find(|p| p.id == settings.post_process_provider_id)
            .ok_or_else(|| anyhow!("provider {} not found", settings.post_process_provider_id))?
            .clone();
        let api_key = settings
            .post_process_api_keys
            .get(&provider.id)
            .cloned()
            .unwrap_or_default();
        let model = settings
            .post_process_models
            .get(&provider.id)
            .cloned()
            .unwrap_or_default();
        if api_key.is_empty() || model.is_empty() {
            log::info!(
                "Meeting notes enabled but {} has no key/model set — skipping",
                provider.id
            );
            return Ok(None);
        }

        // Spend guard. The DeepSeek key funds all 17 Hermes agents with no
        // fallback provider, so an unbounded transcript is a real risk to
        // something other than this app.
        let cap = settings.meeting_notes_max_chars;
        let (body, truncated) = if transcript.len() > cap {
            (&transcript[..cap], true)
        } else {
            (transcript, false)
        };
        if truncated {
            log::warn!(
                "Meeting transcript truncated from {} to {cap} chars for summarisation",
                transcript.len()
            );
        }

        let prompt = format!(
            "You are summarising a meeting transcript. \"Kole\" is the user; \
\"Them\" is everyone else on the call — the transcript cannot tell those people apart, so \
never invent names for them.\n\n\
Write, in this order and nothing else:\n\
1. A `### Summary` section: 3-5 plain sentences on what the meeting was about and what was decided.\n\
2. An `### Action items` section: a markdown checklist (`- [ ] `), each line naming who owns it. \
Use \"Kole\" when the transcript makes that clear, or a specific name only if the transcript \
genuinely names that person. Otherwise write \"The team\" — never \"Them\": that word is a \
transcript-only speaker label, not a name to write into what you produce. If an item plainly \
has no distinct owner, drop the owner and state the item on its own line.\n\
3. An `### Open questions` section only if something was explicitly left unresolved.\n\n\
Sections 2 and 3 are optional: if there is nothing to put in one, leave the heading out \
altogether. Never write a heading followed by \"none\" or \"nothing\" — an empty section is \
noise in a file other tools read.\n\n\
Rules: use only what is in the transcript — never infer or embellish. \
Local speech-to-text produces garbled words; read past obvious mistranscriptions rather than \
quoting them. Write plainly, no jargon, no preamble.{}\n\n\
Transcript:\n{}",
            if truncated {
                "\n\nNote: this transcript was truncated — say so in one line at the end."
            } else {
                ""
            },
            body
        );

        // `process()` runs on a plain worker thread, so drive the async call
        // to completion here rather than leaking async up the call chain.
        //
        // Two arguments here are load-bearing and were both missing until 2026-09-27.
        // `reasoning_effort: "none"` — the configured model is a reasoning model, and
        // left to itself it spends thousands of reasoning tokens before it writes a
        // word of summary. Measured on the 2026-09-21 standup transcript: 54.6s with
        // reasoning on, 9.3s with it off, and the shorter answer was the better one.
        // `send_chat_completion_batch` — nothing is waiting on this, and the prompt is
        // a whole transcript, so it gets the batch time budget rather than the
        // interactive one that the dictation path needs.
        let result = tauri::async_runtime::block_on(async move {
            crate::llm_client::send_chat_completion_batch(
                &provider,
                api_key,
                &model,
                prompt,
                Some("none".to_string()),
                None,
            )
            .await
        })
        .map_err(|e| anyhow!(e))?;

        Ok(result
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()))
    }

    /// Where the meeting mic track should come from, in precedence order:
    ///
    /// 1. **A mic picked on the recording pill** — the user pointing at a
    ///    device is not a guess, so it outranks everything for the rest of
    ///    the recording. Skipped (loudly) while unattached, picked back up
    ///    the moment it reappears.
    /// 2. **The call's own mic** — whatever input device the meeting app is
    ///    actually capturing from. If the call can hear Kole, so can Lark
    ///    (2026-08-10: the pin said Jabra, the call ran on another mic, and
    ///    36 minutes of his side were lost while the pill said "No audio").
    /// 3. **The pin** (same clamshell override as dictation).
    /// 4. **The system default.**
    ///
    /// A wanted device (the call's or the pin) that can't be opened is an
    /// ERROR in the log and `fallback` in the result — opening
    /// `device: None` means "system default", and a silently-missed target
    /// is indistinguishable from no target at all.
    fn resolve_mic_device(&self) -> MicResolution {
        let settings = get_settings(&self.app_handle);
        let use_clamshell =
            clamshell::is_clamshell().unwrap_or(false) && settings.clamshell_microphone.is_some();
        let pinned = if use_clamshell {
            settings.clamshell_microphone.clone()
        } else {
            settings.selected_microphone.clone()
        };
        let devices = list_input_devices().unwrap_or_else(|e| {
            log::error!("Failed to list input devices: {e}");
            Vec::new()
        });

        let manual = self.manual_mic.lock().unwrap().clone();
        if let Some(name) = manual {
            match devices.iter().position(|d| d.name == name) {
                Some(idx) => {
                    return MicResolution {
                        device: devices.into_iter().nth(idx).map(|d| d.device),
                        target: Some(name),
                        source: MicSource::Manual,
                        fallback: false,
                    };
                }
                // The picked device was unplugged: fall through to the rest
                // of the precedence list rather than record silence. The
                // pick stays set, so reattaching hands the stream back.
                None => log::error!(
                    "Mic picked on the pill {:?} is not attached (available inputs: {:?}) — falling back to call/pin/default",
                    name,
                    devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>()
                ),
            }
        }

        let own_pid = std::process::id() as i32;
        if let Some(call) = super::meeting_detect::call_mic(own_pid) {
            match devices.iter().position(|d| d.name == call.device_name) {
                Some(idx) => {
                    return MicResolution {
                        device: devices.into_iter().nth(idx).map(|d| d.device),
                        target: Some(call.device_name),
                        source: MicSource::Call(call.app),
                        fallback: false,
                    };
                }
                // Core Audio names it, cpal doesn't — can't open it, so fall
                // through to the pin, but loudly: the recorded track is not
                // what the call hears.
                None => log::error!(
                    "{} is capturing from {:?} but no cpal input matches that name (available: {:?}) — falling back to the pin",
                    call.app,
                    call.device_name,
                    devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>()
                ),
            }
        }

        let Some(device_name) = pinned.clone() else {
            return MicResolution {
                device: None,
                target: None,
                source: MicSource::Default,
                fallback: false,
            };
        };
        match devices.iter().position(|d| d.name == device_name) {
            Some(idx) => MicResolution {
                device: devices.into_iter().nth(idx).map(|d| d.device),
                target: pinned,
                source: MicSource::Pin,
                fallback: false,
            },
            None => {
                log::error!(
                    "Pinned microphone {:?} is not attached (available inputs: {:?}) — recording the system default instead",
                    device_name,
                    devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>()
                );
                MicResolution {
                    device: None,
                    target: pinned,
                    source: MicSource::Default,
                    fallback: true,
                }
            }
        }
    }
}

/// Why the meeting mic resolution chose its device — for the log, so a
/// transcript with a surprising track can be traced to a decision.
#[derive(Clone, Copy)]
enum MicSource {
    /// A device the user picked on the recording pill.
    Manual,
    /// The input device the call's meeting app is capturing from.
    Call(&'static str),
    /// The settings pin (or its clamshell override).
    Pin,
    /// No specific target — the system default.
    Default,
}

impl std::fmt::Display for MicSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MicSource::Manual => write!(f, "the mic picked on the pill"),
            MicSource::Call(app) => write!(f, "the {app} call's mic"),
            MicSource::Pin => write!(f, "the pinned mic"),
            MicSource::Default => write!(f, "the system default"),
        }
    }
}

/// Outcome of meeting-mic resolution: the device to open (`None` = system
/// default), the name that was asked for (`None` = nothing specific), why,
/// and whether a wanted device failed to match anything attached.
/// Both tracks, open and running, before either a recording or a standby
/// buffer decides what to do with them.
struct OpenCapture {
    mic: AudioRecorder,
    tap: SystemAudioTap,
    capture_started: Instant,
    flow_last_ms: Arc<AtomicU64>,
    fallback: bool,
    mic_name: String,
    source: String,
    /// Empty sinks (nothing installed) wired into `mic`/`tap` at
    /// construction — standby capture leaves them empty for as long as it
    /// stays standby; `start()` and `promote_standby()` install a writer
    /// into them the moment there is a real recording to write
    /// (t17873258798635ef6). Kept as their own fields rather than reached
    /// through `mic`/`tap` because `SystemAudioTap` doesn't expose its copy.
    mic_sink: ChunkSink,
    sys_sink: ChunkSink,
}

/// The rewind window in seconds, clamped to something an 8 GB machine can
/// hold. Read live rather than cached: changing it takes effect on the next
/// call, with no restart.
fn prerecord_secs(app: &AppHandle) -> u64 {
    get_settings(app)
        .meeting_prerecord_minutes
        .min(MAX_PRERECORD_MINUTES) as u64
        * 60
}

/// Trims a rolling buffer to its last `keep` samples, front-padding with
/// silence when it holds less than that.
///
/// Front, not back: a rolling buffer's contents are the *most recent* audio,
/// so a track that holds less than the window (a mic that was restarted, a
/// device that appeared late) is missing its beginning, not its end. Padding
/// the wrong end would slide every word in that track later than it was
/// spoken and break the interleave with the far side.
fn align_tail(mut samples: Vec<f32>, keep: usize) -> Vec<f32> {
    if samples.len() > keep {
        samples.drain(..samples.len() - keep);
        samples
    } else if samples.len() < keep {
        let mut padded = vec![0.0; keep - samples.len()];
        padded.extend(samples);
        padded
    } else {
        samples
    }
}

struct MicResolution {
    device: Option<cpal::Device>,
    target: Option<String>,
    source: MicSource,
    fallback: bool,
}

/// Decrements the in-flight transcription count on drop, so a panicking
/// processing thread can't pin `status()` at Processing forever.
struct ProcessingGuard<'a>(&'a std::sync::atomic::AtomicU32);

impl Drop for ProcessingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Truncate or zero-pad to the expected wall-clock sample count.
fn fit_to_length(samples: &mut Vec<f32>, expected: usize) {
    samples.resize(expected, 0.0);
}

/// What to do with the audio a mic stream had captured when the watchdog
/// tore it down.
///
/// A recording keeps it and normalises the track back to wall-clock, so the
/// gap becomes silence rather than a timestamp shift. **Standby throws it
/// away**: the rewind buffer is a rolling window with no fixed origin to
/// normalise against, and audio captured before a device switch came from a
/// device the user demonstrably was not talking into — the exact failure
/// (2026-08-10) that put 36 minutes of the wrong mic in a transcript.
/// Promotion front-pads whatever is missing, so dropping it costs silence in
/// the user's own track and nothing at all in the far side's.
fn salvage_partial(
    standby: bool,
    partial: Vec<f32>,
    mic_prefix: &mut Vec<f32>,
    mic_sink: &ChunkSink,
    audio_started: &Instant,
    reason: &str,
) {
    if standby {
        if !partial.is_empty() {
            log::info!(
                "Rewind buffer dropped {:.1}s of mic audio from the previous device after {reason}",
                partial.len() as f32 / SAMPLE_RATE as f32
            );
        }
        return;
    }
    // The dying stream's last samples were never seen by `handle_frame`'s
    // chunk sink (they're arriving here via `take_buffer`/`stop`, not the
    // consumer thread's normal per-frame path), so they have to be streamed
    // explicitly — otherwise every restart this watchdog performs (a common
    // path: it's the same recovery a silent AirPods handshake triggers)
    // opens a gap in the on-disk file (t17873258798635ef6).
    write_to_streamed_wav(mic_sink, &partial, "mic (restart salvage)");
    mic_prefix.extend(partial);
    let before = mic_prefix.len();
    let expected = (audio_started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
    fit_to_length(mic_prefix, expected);
    // `fit_to_length` only ever grows the track here — wall-clock can't run
    // behind a live capture at the moment of a restart — but guard the
    // subtraction anyway rather than assume it. The appended tail is
    // silence closing the restart gap; without also disk-writing it, the
    // streamed file drifts out of sync with the very thing this restart is
    // correcting, one restart at a time.
    if expected > before {
        let gap = vec![0.0_f32; expected - before];
        write_to_streamed_wav(mic_sink, &gap, "mic (restart gap padding)");
    }
}

/// Writes `samples` to the sink's writer if one is installed, or does
/// nothing (streaming is a best-effort safety net — see `ChunkSink`).
/// Shared by every write site outside the consumer threads themselves
/// (`recorder.rs`/`system_tap.rs` each have their own copy of this same
/// lock-and-write shape, since they can't depend on `meeting.rs`).
fn write_to_streamed_wav(sink: &ChunkSink, samples: &[f32], label: &str) {
    if samples.is_empty() {
        return;
    }
    let mut guard = sink.lock().unwrap();
    if let Some(writer) = guard.as_mut() {
        if let Err(e) = writer.write(samples) {
            log::warn!("Streaming WAV write failed ({label}): {e}");
        }
    }
}

/// The on-disk name a recording streams under before its calendar title is
/// known — and the stem its marker is named after.
fn provisional_stem(started: DateTime<Local>) -> String {
    format!("{} Meeting", started.format("%Y-%m-%d %H%M"))
}

/// The start time encoded in a stem's `YYYY-MM-DD HHMM` prefix.
fn parse_stem_start(prefix: &str) -> Option<DateTime<Local>> {
    chrono::NaiveDateTime::parse_from_str(prefix.get(..15)?, "%Y-%m-%d %H%M")
        .ok()?
        .and_local_timezone(Local)
        .single()
}

/// Whether a transcript for the recording that starts with `prefix` exists.
fn transcript_exists(dir: &std::path::Path, prefix: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.starts_with(prefix) && name.ends_with(".md")
    })
}

/// Finds a recording's `(mic|system).wav`: at the path the marker recorded
/// if it is still there, otherwise by the stem's date-time prefix —
/// `process()` renames the file to its calendar title before it finishes.
fn locate_track(
    dir: &std::path::Path,
    hinted: Option<&std::path::Path>,
    prefix: &str,
    label: &str,
) -> Option<PathBuf> {
    if let Some(hinted) = hinted {
        if hinted.exists() {
            return Some(hinted.to_path_buf());
        }
    }
    let suffix = format!(" ({label}).wav");
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.file_name()
            .map(|n| {
                let n = n.to_string_lossy();
                n.starts_with(prefix) && n.ends_with(&suffix)
            })
            .unwrap_or(false)
    })
}

/// Takes the writer out of `sink` (if any) and finalises it — writes the
/// real WAV header/size, without which the file is not a valid WAV a reader
/// can open. Returns `path` unchanged on success, so the caller can pass it
/// on to `process()` for the rename to the calendar-titled final name;
/// returns `None` on any failure (no writer was ever installed, or
/// `finalize()` itself errored), which is `process()`'s existing signal to
/// fall back to writing the WAV from the in-memory buffer the way this
/// task originally worked (t17873258798635ef6).
fn finalize_streamed_wav(sink: &ChunkSink, path: Option<PathBuf>, label: &str) -> Option<PathBuf> {
    let writer = sink.lock().unwrap().take()?;
    match writer.finalize() {
        Ok(()) => path,
        Err(e) => {
            log::warn!("Failed to finalise the streamed {label} WAV: {e}");
            None
        }
    }
}

/// Gets the final `(mic|system).wav` into place at `final_path`. The common
/// case is a rename: `streamed` already holds the whole track, written live
/// and finalised in `stop_and_process`, just under its provisional
/// pre-calendar name — renaming it is what this task exists to make
/// possible, since it means the file was safe on disk the whole time the
/// recording ran, not only after a `save_wav_file` call that could hang or
/// never run. Falls back to writing `samples` in one go — the original,
/// at-risk behaviour this task is closing — if there is no streamed file
/// (streaming never opened one, e.g. disk full) or the rename fails
/// (t17873258798635ef6).
fn finish_meeting_wav(
    final_path: &std::path::Path,
    streamed: Option<&std::path::Path>,
    samples: &[f32],
    label: &str,
) {
    if let Some(streamed) = streamed {
        if streamed == final_path {
            // Already at the final name — the calendar title never
            // resolved past the "Meeting" fallback both names share.
            return;
        }
        if streamed.exists() {
            match std::fs::rename(streamed, final_path) {
                Ok(()) => return,
                Err(e) => log::warn!(
                    "Could not rename the streamed {label} WAV, rewriting from memory instead: {e}"
                ),
            }
        }
    }
    let _ = save_wav_file(final_path, samples);
}

/// One JSON object per line, flushed immediately. Deliberately dependency-free
/// and append-only: the whole point is that a SIGKILL between two segments
/// leaves everything before it intact and parseable.
fn append_partial(file: Option<&mut std::fs::File>, start: usize, speaker: &str, text: &str) {
    use std::io::Write;
    let Some(file) = file else { return };
    let line = serde_json::json!({
        "start": start,
        "speaker": speaker,
        "text": text,
    });
    if writeln!(file, "{line}").is_err() {
        return;
    }
    // Flush per segment — buffered output would defeat the purpose.
    let _ = file.flush();
}

/// Calendar titles are free text and end up in a filename. Strip what the
/// filesystem or a shell would choke on, collapse whitespace, and keep it
/// short enough to stay readable in a folder listing.
fn sanitise_for_filename(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    // Leading dots hide the file; trailing ones confuse extension parsing.
    let trimmed = collapsed.trim_matches('.').trim();
    trimmed
        .chars()
        .take(60)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Quote a YAML scalar only when it needs it, so the common case stays
/// readable. Single quotes with doubling is the safest minimal form.
fn yaml_scalar(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value.starts_with([
            '-', '?', ':', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', '[', '{', '#',
        ])
        || value.contains(": ")
        || value.contains(" #")
        || value.ends_with(':')
        || value.trim() != value;
    if needs_quoting {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        value.to_string()
    }
}

fn render_transcript(entries: &[(usize, &'static str, String)]) -> String {
    if entries.is_empty() {
        return "_No speech detected on either track._\n".to_string();
    }
    let mut out = String::new();
    for (start, speaker, text) in entries {
        let secs = start / SAMPLE_RATE;
        out.push_str(&format!(
            "**[{:02}:{:02}] {speaker}:** {text}\n\n",
            secs / 60,
            secs % 60
        ));
    }
    out
}

/// Rebuild a `.md` from any sidecar left behind by a run that died mid-batch.
/// Called at startup: a meeting killed by the OOM reaper should cost the user
/// nothing but the summary. Sidecars whose `.md` already exists are just swept.
pub fn recover_orphaned_meetings(app_handle: &AppHandle) {
    let Ok(dir) = meetings_dir_for(app_handle) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".partial.jsonl") {
            continue;
        }
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.trim_end_matches(".partial.jsonl").to_string());
        let Some(stem) = stem else { continue };
        let md_path = dir.join(format!("{stem}.md"));
        if md_path.exists() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        // A marker means the audio itself is still there and
        // `recover_dead_recordings` is rebuilding the whole transcript from
        // it — better than the fragment this sidecar holds.
        if recording_guard::guarded(&dir, &stem) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut lines: Vec<(usize, String, String)> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| {
                Some((
                    v.get("start")?.as_u64()? as usize,
                    v.get("speaker")?.as_str()?.to_string(),
                    v.get("text")?.as_str()?.to_string(),
                ))
            })
            .collect();
        if lines.is_empty() {
            continue;
        }
        lines.sort_by_key(|(start, _, _)| *start);

        let mut md = format!("# Meeting — {stem}\n\n");
        md.push_str(
            "- **Recovered** from a run that ended before the transcript was written. \
No AI notes, and the mic-bleed dedup pass did not run.\n\n## Transcript\n\n",
        );
        for (start, speaker, text) in &lines {
            let secs = start / SAMPLE_RATE;
            md.push_str(&format!(
                "**[{:02}:{:02}] {speaker}:** {text}\n\n",
                secs / 60,
                secs % 60
            ));
        }
        match std::fs::write(&md_path, md) {
            Ok(()) => {
                log::info!(
                    "Recovered orphaned meeting transcript: {}",
                    md_path.display()
                );
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => log::warn!(
                "Failed to write recovered meeting {}: {e}",
                md_path.display()
            ),
        }
    }
}

/// Where transcripts land. Settings-driven since 2026-08-01 so the folder can
/// sit inside `~/Documents/Claude/` — the only tree Cowork agents can read,
/// and the reason the Meeting Digest / Surveyor can consume Lark at all.
/// Falls back to the historical path if the setting is somehow blank.
pub fn meetings_dir_for(app_handle: &AppHandle) -> Result<PathBuf> {
    let configured = get_settings(app_handle).meetings_folder;
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        // Expand a leading `~` — the setting is user-editable text.
        if let Some(rest) = trimmed.strip_prefix("~/") {
            let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
            return Ok(PathBuf::from(home).join(rest));
        }
        return Ok(PathBuf::from(trimmed));
    }
    legacy_meetings_dir()
}

fn legacy_meetings_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join("Documents").join("Lark Meetings"))
}

/// Raw meeting audio follows Kole's AudioDay1 dictation policy: keep WAVs
/// for 24h (debugging window), keep transcripts forever. ~275MB per hour
/// of meeting on a chronically full disk says delete.
fn cleanup_old_meeting_wavs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(24 * 3600);
    // Audio still guarded by a marker has no transcript yet — it is kept
    // whatever its age, until `recover_dead_recordings` has used it.
    let guarded: Vec<String> = recording_guard::markers_in(dir)
        .into_iter()
        .map(|(_, m)| recording_guard::stem_prefix(&m.stem).to_string())
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_wav = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("wav"))
            .unwrap_or(false);
        if !is_wav {
            continue;
        }
        let name = entry.file_name();
        let prefix = recording_guard::stem_prefix(&name.to_string_lossy()).to_string();
        if guarded.contains(&prefix) {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if expired {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("Deleted expired meeting audio: {}", path.display()),
                Err(e) => log::warn!("Failed to delete {}: {e}", path.display()),
            }
        }
    }
}

/// Without headphones the mic also hears the call audio from the speakers,
/// so the far side shows up on both tracks. The system tap is the
/// authoritative copy — drop mic entries that near-duplicate a system
/// entry close in time.
fn drop_mic_bleed(
    entries: Vec<(usize, &'static str, String)>,
) -> Vec<(usize, &'static str, String)> {
    const BLEED_WINDOW: i64 = (8 * SAMPLE_RATE) as i64;
    entries
        .iter()
        .filter(|(start, speaker, text)| {
            *speaker != "Kole"
                || !entries.iter().any(|(s2, sp2, t2)| {
                    *sp2 == "Them"
                        && (*start as i64 - *s2 as i64).abs() < BLEED_WINDOW
                        && strsim::normalized_levenshtein(text, t2) > 0.75
                })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(secs: usize, speaker: &'static str, text: &str) -> (usize, &'static str, String) {
        (secs * SAMPLE_RATE, speaker, text.to_string())
    }

    #[test]
    fn drops_mic_copy_of_system_line_nearby() {
        let out = drop_mic_bleed(vec![
            entry(20, "Kole", "I think we should move the launch date to July"),
            entry(
                20,
                "Them",
                "I think we should move the launch date to July.",
            ),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "Them");
    }

    #[test]
    fn drops_mic_copy_with_minor_transcription_differences() {
        let out = drop_mic_bleed(vec![
            entry(
                28,
                "Kole",
                "The website copy needs a final review before we ship",
            ),
            entry(
                30,
                "Them",
                "The Win Side copy needs a final review before we shift.",
            ),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "Them");
    }

    #[test]
    fn keeps_genuine_kole_speech() {
        let out = drop_mic_bleed(vec![
            entry(10, "Kole", "Let me check the budget spreadsheet first"),
            entry(12, "Them", "Sure, take your time, no rush at all"),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn keeps_similar_text_far_apart_in_time() {
        let out = drop_mic_bleed(vec![
            entry(5, "Kole", "Let's confirm the budget by Friday"),
            entry(60, "Them", "Let's confirm the budget by Friday."),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn never_drops_system_entries() {
        let out = drop_mic_bleed(vec![
            entry(20, "Them", "The same sentence twice somehow"),
            entry(21, "Them", "The same sentence twice somehow"),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn segments_split_on_silence_and_respect_minimums() {
        let mut samples = vec![0.0f32; 16 * SAMPLE_RATE];
        // 2s of "speech" at t=3s and t=10s, separated by >1s silence
        for region in [(3, 5), (10, 12)] {
            for s in &mut samples[region.0 * SAMPLE_RATE..region.1 * SAMPLE_RATE] {
                *s = 0.3;
            }
        }
        let regions = segment_active_regions(&samples);
        assert_eq!(regions.len(), 2);
        // padded starts land just before the speech
        assert!(regions[0].0 < 3 * SAMPLE_RATE);
        assert!(regions[1].0 < 10 * SAMPLE_RATE && regions[1].0 > 8 * SAMPLE_RATE);
    }

    /// An over-full rewind buffer keeps its most recent audio. Dropping the
    /// wrong end would hand the recording the oldest few minutes of the call
    /// and lose everything said since.
    #[test]
    fn align_tail_keeps_the_most_recent_audio() {
        let buffer: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let kept = align_tail(buffer, 10);
        assert_eq!(kept.len(), 10);
        assert_eq!(kept[0], 90.0);
        assert_eq!(kept[9], 99.0);
    }

    /// A track holding less than the window is missing its *beginning* — a
    /// mic restarted mid-buffer, or a device that appeared late. The silence
    /// goes in front so both tracks still end at the same instant, which is
    /// what keeps the two speakers interleaved at the right timestamps.
    #[test]
    fn align_tail_pads_a_short_track_at_the_front() {
        let buffer: Vec<f32> = vec![7.0, 8.0, 9.0];
        let kept = align_tail(buffer, 6);
        assert_eq!(kept, vec![0.0, 0.0, 0.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn align_tail_leaves_an_exact_fit_alone() {
        let buffer: Vec<f32> = vec![1.0, 2.0, 3.0];
        assert_eq!(align_tail(buffer, 3), vec![1.0, 2.0, 3.0]);
    }

    /// The two tracks are trimmed independently against wall-clock, so the
    /// mic being short must not shorten the far side — the pair has to come
    /// out the same length or every timestamp after the gap is wrong.
    #[test]
    fn align_tail_gives_both_tracks_the_same_length() {
        let keep = 8;
        let mic = align_tail(vec![1.0; 3], keep);
        let system = align_tail((0..40).map(|i| i as f32).collect(), keep);
        assert_eq!(mic.len(), system.len());
        assert_eq!(system[keep - 1], 39.0);
    }
}

/// Energy-based speech segmentation: returns sample ranges containing
/// activity, padded and merged so each range transcribes as one utterance.
/// The threshold adapts to each track's noise floor so quiet system audio
/// and a hot mic both segment sensibly.
fn segment_active_regions(samples: &[f32]) -> Vec<(usize, usize)> {
    const MERGE_GAP_FRAMES: usize = 33; // ~1 s of silence ends an utterance
    const PAD_FRAMES: usize = 10; // ~300 ms context either side
    const MIN_REGION_FRAMES: usize = 13; // drop blips under ~400 ms
    const MAX_REGION_SAMPLES: usize = 30 * SAMPLE_RATE; // hard split at 30 s

    if samples.len() < FRAME {
        return Vec::new();
    }

    let mut rms: Vec<f32> = samples
        .chunks(FRAME)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();

    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_floor = sorted[sorted.len() / 5]; // 20th percentile
    let threshold = (noise_floor * 3.0).max(0.004);

    // Mark active frames, then group into regions separated by long silence.
    let active: Vec<bool> = rms.drain(..).map(|v| v > threshold).collect();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    let mut silence_run = 0usize;

    for (i, &is_active) in active.iter().enumerate() {
        if is_active {
            silence_run = 0;
            current = match current {
                None => Some((i, i + 1)),
                Some((s, _)) => Some((s, i + 1)),
            };
        } else if let Some((s, e)) = current {
            silence_run += 1;
            if silence_run >= MERGE_GAP_FRAMES {
                regions.push((s, e));
                current = None;
                silence_run = 0;
            }
        }
    }
    if let Some(r) = current {
        regions.push(r);
    }

    let mut out = Vec::new();
    for (s, e) in regions {
        if e - s < MIN_REGION_FRAMES {
            continue;
        }
        let start = s.saturating_sub(PAD_FRAMES) * FRAME;
        let end = ((e + PAD_FRAMES) * FRAME).min(samples.len());
        // Hard-split very long regions so the model never sees > 30 s at once.
        let mut chunk_start = start;
        while chunk_start < end {
            let chunk_end = (chunk_start + MAX_REGION_SAMPLES).min(end);
            out.push((chunk_start, chunk_end));
            chunk_start = chunk_end;
        }
    }
    out
}
