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
use crate::audio_toolkit::audio::{list_input_devices, save_wav_file, AudioRecorder};
use crate::helpers::clamshell;
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

#[derive(Clone, Copy, PartialEq)]
pub enum MeetingStatus {
    Idle,
    Recording,
    Processing,
}

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
        tap: SystemAudioTap,
        started: DateTime<Local>,
    },
    Processing,
}

pub struct MeetingManager {
    app_handle: AppHandle,
    state: Mutex<MeetingState>,
}

impl MeetingManager {
    pub fn new(app_handle: &AppHandle) -> Self {
        // Sweep expired meeting WAVs at startup too — meetings don't happen
        // every day, but the disk pressure does.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(30));
            cleanup_old_meeting_wavs();
        });
        Self {
            app_handle: app_handle.clone(),
            state: Mutex::new(MeetingState::Idle),
        }
    }

    pub fn status(&self) -> MeetingStatus {
        match *self.state.lock().unwrap() {
            MeetingState::Idle => MeetingStatus::Idle,
            MeetingState::Recording { .. } => MeetingStatus::Recording,
            MeetingState::Processing => MeetingStatus::Processing,
        }
    }

    pub fn toggle(self: &Arc<Self>) {
        match self.status() {
            MeetingStatus::Idle => {
                if let Err(e) = self.start() {
                    log::error!("Failed to start meeting recording: {e}");
                }
            }
            MeetingStatus::Recording => self.stop_and_process(),
            MeetingStatus::Processing => {
                log::warn!("Meeting transcription still in progress; ignoring toggle");
            }
        }
    }

    fn start(self: &Arc<Self>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, MeetingState::Idle) {
            return Err(anyhow!("meeting recording already active"));
        }

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
        let device = self.selected_mic_device();
        if let Err(e) = mic.open(device) {
            // Don't leave the tap running if the mic failed.
            let _ = tap.stop();
            return Err(anyhow!("failed to open microphone: {e}"));
        }
        mic.start().map_err(|e| anyhow!("{e}"))?;

        // The model is NOT pre-loaded here: holding 640MB through a whole
        // call would hurt on 8GB. process() loads it after the recording.

        log::info!(
            "Meeting recording started (mic: {})",
            mic.device_name().unwrap_or_else(|| "default".into())
        );
        *state = MeetingState::Recording {
            mic,
            mic_prefix: Vec::new(),
            mic_started,
            flow_last_ms,
            last_restart_ms: 0,
            restarts: 0,
            tap,
            started: Local::now(),
        };
        drop(state);

        self.spawn_mic_watchdog();
        // Always-visible while recording: the small top-right indicator.
        crate::overlay::show_meeting_recording_indicator(&self.app_handle);
        Ok(())
    }

    /// Restarts the meeting mic when it delivers digital silence — the same
    /// AirPods handshake failure dictation recovers from. Salvaged samples
    /// are normalised to wall-clock so the gap becomes silence instead of a
    /// timestamp shift.
    fn spawn_mic_watchdog(self: &Arc<Self>) {
        let manager = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let mut state = manager.state.lock().unwrap();
            let MeetingState::Recording {
                mic,
                mic_prefix,
                mic_started,
                flow_last_ms,
                last_restart_ms,
                restarts,
                ..
            } = &mut *state
            else {
                break;
            };

            let now_ms = mic_started.elapsed().as_millis() as u64;
            let last_signal = flow_last_ms.load(Ordering::Relaxed).max(*last_restart_ms);
            if now_ms.saturating_sub(last_signal) < MIC_SILENT_RESTART_MS {
                continue;
            }
            if *restarts >= MAX_MIC_RESTARTS {
                continue; // logged on the last attempt; record whatever comes
            }

            log::warn!(
                "Meeting mic silent for {}ms — restarting stream (attempt {}/{})",
                now_ms - last_signal,
                *restarts + 1,
                MAX_MIC_RESTARTS
            );
            let partial = mic.stop().unwrap_or_default();
            let _ = mic.close();
            mic_prefix.extend(partial);
            let expected = (mic_started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
            fit_to_length(mic_prefix, expected);

            let device = manager.selected_mic_device();
            match mic.open(device).and_then(|_| mic.start()) {
                Ok(()) => log::info!("Meeting mic stream restarted"),
                Err(e) => log::error!("Meeting mic restart failed: {e}"),
            }
            *restarts += 1;
            *last_restart_ms = mic_started.elapsed().as_millis() as u64;
            if *restarts == MAX_MIC_RESTARTS {
                log::error!(
                    "Meeting mic restart limit reached — mic track may be silent from here"
                );
            }
        });
    }

    fn stop_and_process(self: &Arc<Self>) {
        let (mut mic, mic_prefix, mic_started, restarts, tap, started) = {
            let mut state = self.state.lock().unwrap();
            match std::mem::replace(&mut *state, MeetingState::Processing) {
                MeetingState::Recording {
                    mic,
                    mic_prefix,
                    mic_started,
                    restarts,
                    tap,
                    started,
                    ..
                } => (mic, mic_prefix, mic_started, restarts, tap, started),
                other => {
                    *state = other;
                    return;
                }
            }
        };

        // Recording is over — drop the indicator regardless of how the stop
        // was triggered (card, tray, or CLI).
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
        let drift =
            (mic_samples.len() as f64 - expected as f64).abs() / expected.max(1) as f64;
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
        std::thread::spawn(move || {
            let result = manager.process(mic_samples, sys_samples, started);
            *manager.state.lock().unwrap() = MeetingState::Idle;
            crate::tray::update_tray_menu(
                &manager.app_handle,
                &crate::tray::TrayIconState::Idle,
                None,
            );
            match result {
                Ok(path) => {
                    log::info!("Meeting transcript written to {}", path.display());
                    let _ = manager
                        .app_handle
                        .opener()
                        .open_path(path.to_string_lossy().to_string(), None::<String>);
                }
                Err(e) => log::error!("Meeting transcription failed: {e}"),
            }
        });
    }

    fn process(
        &self,
        mic_samples: Vec<f32>,
        sys_samples: Vec<f32>,
        started: DateTime<Local>,
    ) -> Result<PathBuf> {
        let out_dir = meetings_dir()?;
        std::fs::create_dir_all(&out_dir)?;
        let stem = format!("{} Meeting", started.format("%Y-%m-%d %H%M"));

        cleanup_old_meeting_wavs();

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
                            entries.push((start, speaker, text));
                        }
                    }
                    Err(e) => log::error!(
                        "Meeting segment transcription failed ({speaker} @{start}): {e}"
                    ),
                }
            }
        }
        entries.sort_by_key(|(start, _, _)| *start);
        let entries = drop_mic_bleed(entries);

        let duration_secs = mic_samples.len().max(sys_samples.len()) / SAMPLE_RATE;
        let mut md = String::new();
        md.push_str(&format!(
            "# Meeting — {}\n\n",
            started.format("%A %-d %B %Y, %H:%M")
        ));
        md.push_str(&format!(
            "- Duration: {} min {} sec\n- Transcribed locally by Lark (meeting mode spike)\n",
            duration_secs / 60,
            duration_secs % 60
        ));
        if duration_secs > 60 && mic_active_secs < 3 {
            md.push_str(
                "- Note: the mic track was almost entirely silent — only the system side was captured. (AirPods handshake? Check the log.)\n",
            );
        }
        md.push_str("\n## Transcript\n\n");
        if entries.is_empty() {
            md.push_str("_No speech detected on either track._\n");
        }
        for (start, speaker, text) in &entries {
            let secs = start / SAMPLE_RATE;
            md.push_str(&format!(
                "**[{:02}:{:02}] {speaker}:** {text}\n\n",
                secs / 60,
                secs % 60
            ));
        }

        let md_path = out_dir.join(format!("{stem}.md"));
        std::fs::write(&md_path, md)?;
        Ok(md_path)
    }

    /// Same device resolution the dictation pipeline uses, including the
    /// clamshell (lid closed) override.
    fn selected_mic_device(&self) -> Option<cpal::Device> {
        let settings = get_settings(&self.app_handle);
        let use_clamshell = clamshell::is_clamshell().unwrap_or(false)
            && settings.clamshell_microphone.is_some();
        let device_name = if use_clamshell {
            settings.clamshell_microphone.clone()
        } else {
            settings.selected_microphone.clone()
        }?;
        list_input_devices()
            .ok()?
            .into_iter()
            .find(|d| d.name == device_name)
            .map(|d| d.device)
    }
}

/// Truncate or zero-pad to the expected wall-clock sample count.
fn fit_to_length(samples: &mut Vec<f32>, expected: usize) {
    samples.resize(expected, 0.0);
}

fn meetings_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join("Documents").join("Lark Meetings"))
}

/// Raw meeting audio follows Kole's AudioDay1 dictation policy: keep WAVs
/// for 24h (debugging window), keep transcripts forever. ~275MB per hour
/// of meeting on a chronically full disk says delete.
fn cleanup_old_meeting_wavs() {
    let Ok(dir) = meetings_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
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
            entry(20, "Them", "I think we should move the launch date to July."),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "Them");
    }

    #[test]
    fn drops_mic_copy_with_minor_transcription_differences() {
        let out = drop_mic_bleed(vec![
            entry(28, "Kole", "The website copy needs a final review before we ship"),
            entry(30, "Them", "The Win Side copy needs a final review before we shift."),
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
