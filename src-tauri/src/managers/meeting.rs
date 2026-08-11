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
    list_input_devices, read_wav_samples, save_wav_file, AudioRecorder,
};
use crate::helpers::clamshell;
use crate::managers::meeting_calendar::{self, CalendarContext};
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
        mic: AudioRecorder,
        /// Samples salvaged across mic restarts, wall-clock normalised.
        mic_prefix: Vec<f32>,
        mic_started: Instant,
        /// ms since mic_started of the last chunk with real signal.
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
        calendar: Arc<Mutex<Option<CalendarContext>>>,
    },
}

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

    pub fn status(&self) -> MeetingStatus {
        match *self.state.lock().unwrap() {
            MeetingState::Recording { .. } => MeetingStatus::Recording,
            MeetingState::Idle => {
                if self.processing.load(Ordering::Relaxed) > 0 {
                    MeetingStatus::Processing
                } else {
                    MeetingStatus::Idle
                }
            }
        }
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

    fn start(self: &Arc<Self>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, MeetingState::Idle) {
            return Err(anyhow!("meeting recording already active"));
        }
        // A pill pick belongs to one recording only — a fresh one starts on
        // automatic resolution.
        *self.manual_mic.lock().unwrap() = None;

        let tap = SystemAudioTap::start()?;

        let mic_started = Instant::now();
        let flow_last_ms = Arc::new(AtomicU64::new(0));
        let flow_clone = flow_last_ms.clone();
        let mut mic = AudioRecorder::new()
            .map_err(|e| anyhow!("{e}"))?
            .with_flow_callback(move |rms| {
                if rms > FLOW_RMS_THRESHOLD {
                    flow_clone.store(mic_started.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
            });
        let resolution = self.resolve_mic_device();
        let fallback = resolution.fallback;
        let source = resolution.source;
        if let Err(e) = mic.open(resolution.device) {
            // Don't leave the tap running if the mic failed.
            let _ = tap.stop();
            return Err(anyhow!("failed to open microphone: {e}"));
        }
        mic.start().map_err(|e| anyhow!("{e}"))?;

        // The model is NOT pre-loaded here: holding 640MB through a whole
        // call would hurt on 8GB. process() loads it after the recording.

        let mic_name = mic.device_name().unwrap_or_else(|| "default".into());
        log::info!("Meeting recording started (mic: {mic_name}, via {source})");
        // Resolve the calendar event off-thread; process() reads it ~an hour
        // later, so there is no race worth guarding beyond the mutex.
        let calendar = Arc::new(Mutex::new(None));
        let calendar_sink = calendar.clone();
        std::thread::spawn(move || {
            if let Some(ctx) = meeting_calendar::current_event() {
                log::info!("Meeting matched calendar event: {:?}", ctx.title);
                *calendar_sink.lock().unwrap() = Some(ctx);
            }
        });

        *state = MeetingState::Recording {
            mic,
            mic_prefix: Vec::new(),
            mic_started,
            flow_last_ms,
            last_restart_ms: 0,
            restarts: 0,
            fallback,
            tap,
            started: Local::now(),
            calendar,
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
                    mic,
                    mic_prefix,
                    mic_started,
                    flow_last_ms,
                    last_restart_ms,
                    restarts,
                    fallback,
                    ..
                } = &mut *state
                else {
                    break;
                };

                let now_ms = mic_started.elapsed().as_millis() as u64;
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
                        mic_prefix.extend(partial);
                        let expected =
                            (mic_started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
                        fit_to_length(mic_prefix, expected);
                        match mic.open(resolution.device).and_then(|_| mic.start()) {
                            Ok(()) => {
                                *fallback = false;
                                // A different physical device gets a fresh
                                // silence budget.
                                *restarts = 0;
                                *last_restart_ms = mic_started.elapsed().as_millis() as u64;
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
                mic_prefix.extend(partial);
                let expected = (mic_started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
                fit_to_length(mic_prefix, expected);

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
                            *last_restart_ms = mic_started.elapsed().as_millis() as u64;
                            continue;
                        }
                    }
                    Err(e) => log::error!("Meeting mic restart failed: {e}"),
                }
                *restarts += 1;
                *last_restart_ms = mic_started.elapsed().as_millis() as u64;
                if *restarts == MAX_MIC_RESTARTS {
                    log::error!(
                        "Meeting mic restart limit reached — mic track may be silent from here"
                    );
                }
            }
        });
    }

    fn stop_and_process(self: &Arc<Self>) {
        let (mut mic, mic_prefix, mic_started, restarts, tap, started, calendar) = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, MeetingState::Idle) {
                MeetingState::Recording {
                    mic,
                    mic_prefix,
                    mic_started,
                    restarts,
                    tap,
                    started,
                    calendar,
                    ..
                } => (
                    mic,
                    mic_prefix,
                    mic_started,
                    restarts,
                    tap,
                    started,
                    calendar,
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

        let mut mic_samples = mic_prefix;
        match mic.stop() {
            Ok(samples) => mic_samples.extend(samples),
            Err(e) => log::error!("Failed to stop meeting mic: {e}"),
        }
        let _ = mic.close();
        let sys_samples = tap.stop().unwrap_or_else(|e| {
            log::error!("Failed to stop system tap: {e}");
            Vec::new()
        });

        // A broken Bluetooth stream can run off wall-clock; normalise so
        // mic timestamps line up with the system track.
        let wall_secs = mic_started.elapsed().as_secs_f64();
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
            let result = manager.process(mic_samples, sys_samples, started, calendar);
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
                    );
                }
                Err(e) => log::error!("Meeting transcription failed: {e}"),
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
            let result = manager.process(mic_samples, sys_samples, started, None);
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

        // Keep the raw tracks next to the transcript while meeting mode is a
        // spike — lets us debug a bad transcript by listening back.
        if !mic_samples.is_empty() {
            let _ = save_wav_file(out_dir.join(format!("{stem} (mic).wav")), &mic_samples);
        }
        if !sys_samples.is_empty() {
            let _ = save_wav_file(out_dir.join(format!("{stem} (system).wav")), &sys_samples);
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
2. An `### Action items` section: a markdown checklist (`- [ ] `), each line naming who owns it \
(Kole, or Them if it is the other side).\n\
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
        let result = tauri::async_runtime::block_on(async move {
            crate::llm_client::send_chat_completion(&provider, api_key, &model, prompt, None, None)
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
    for entry in entries.flatten() {
        let path = entry.path();
        let is_wav = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("wav"))
            .unwrap_or(false);
        if !is_wav {
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
