//! Startup maintenance: low-disk warning + recovery of orphaned recordings.
//!
//! Lark runs on a low-RAM Mac with a chronically near-full disk. When free
//! space gets tight, macOS swap is starved and a transcription can hang. Two
//! safeguards live here:
//!
//! 1. **Low-disk warning** — on launch, if the volume holding the recordings is
//!    below [`LOW_DISK_BYTES`], surface a toast so the user can free space
//!    before a dictation wedges.
//! 2. **Orphaned-recording recovery** — a dictation's WAV is written *before*
//!    its transcription is saved to history. If transcription is interrupted
//!    (hang/timeout/crash), the WAV survives with no transcript. On launch we
//!    re-transcribe any such WAV and write the text to history (never auto-paste
//!    — the user reads it from the Transcription history).

use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

/// Warn below 2 GB free — the threshold under which swap pressure starts wedging
/// transcription on the 8 GB Mac.
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Cap recoveries per launch so a backlog can't cause a long startup storm.
const MAX_RECOVERIES_PER_LAUNCH: usize = 10;

/// Only recover recent interruptions. Older WAVs are stale (and re-transcribing
/// a large old one under memory pressure is the exact hang we're avoiding).
const RECOVERY_MAX_AGE_SECS: u64 = 48 * 3600;

#[derive(Clone, serde::Serialize)]
struct LowDiskPayload {
    free_mb: u64,
}

/// Free bytes on the volume that `path` lives on, via `df -k` (no extra deps).
fn free_bytes_at(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // BSD `df -k`: header line, then
    // Filesystem 1024-blocks Used Available Capacity ... → Available is col 3.
    let line = text.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let avail_kb: u64 = cols.get(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

/// Spawn the startup maintenance worker. Runs once, on its own thread, a short
/// delay after launch so it never competes with app startup.
pub fn spawn_startup_maintenance(app: AppHandle) {
    std::thread::spawn(move || {
        // Let the app settle (window, managers, model preload) first.
        std::thread::sleep(std::time::Duration::from_secs(30));

        let recordings_dir = {
            let hm = app.state::<Arc<HistoryManager>>();
            hm.recordings_dir().to_path_buf()
        };

        // 1. Low-disk warning.
        if let Some(free) = free_bytes_at(&recordings_dir) {
            let free_mb = free / (1024 * 1024);
            if free < LOW_DISK_BYTES {
                warn!(
                    "Low disk: only {} MB free where recordings are stored — transcription may stall",
                    free_mb
                );
                let _ = app.emit("low-disk-warning", LowDiskPayload { free_mb });
            } else {
                debug!("Disk check OK: {} MB free", free_mb);
            }
        }

        // 2. Recover orphaned recordings.
        recover_orphaned_recordings(&app, &recordings_dir);
    });
}

fn recover_orphaned_recordings(app: &AppHandle, recordings_dir: &Path) {
    let hm = app.state::<Arc<HistoryManager>>();
    let tm = app.state::<Arc<TranscriptionManager>>();

    // Map file_name -> (history id, already has non-empty text).
    let entries = match hm.get_all_entries() {
        Ok(e) => e,
        Err(e) => {
            warn!("Recovery: could not read history: {}", e);
            return;
        }
    };
    let mut by_file: HashMap<String, (i64, bool)> = HashMap::new();
    for e in &entries {
        if e.file_name.is_empty() {
            continue;
        }
        let has_text = !e.transcription_text.trim().is_empty();
        by_file
            .entry(e.file_name.clone())
            .and_modify(|v| {
                if has_text {
                    v.1 = true;
                }
            })
            .or_insert((e.id, has_text));
    }

    // Collect recent WAV files (path + modified time), newest first. Recover the
    // most recent interruption before older ones, and ignore stale recordings.
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(RECOVERY_MAX_AGE_SECS));
    let mut wavs: Vec<(std::path::PathBuf, std::time::SystemTime)> =
        match std::fs::read_dir(recordings_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let path = e.path();
                    if path.extension().map(|x| x == "wav").unwrap_or(false) {
                        let modified = e.metadata().ok()?.modified().ok()?;
                        Some((path, modified))
                    } else {
                        None
                    }
                })
                .filter(|(_, modified)| cutoff.map(|c| *modified >= c).unwrap_or(true))
                .collect(),
            Err(_) => return,
        };
    wavs.sort_by(|a, b| b.1.cmp(&a.1));

    // Keep only WAVs that still need recovery (no non-empty-text history row).
    wavs.retain(
        |(path, _)| match path.file_name().and_then(|s| s.to_str()) {
            Some(fname) => !matches!(by_file.get(fname), Some((_, true))),
            None => false,
        },
    );
    if wavs.is_empty() {
        return;
    }

    // Ensure the model is loaded. At startup nothing has loaded it yet, and
    // transcribe() errors rather than lazy-loading (same reason the meeting path
    // initiates the load itself). Only pay this 640MB cost when there's real
    // work to recover.
    tm.initiate_model_load();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !tm.is_model_loaded() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !tm.is_model_loaded() {
        warn!("Recovery: transcription model failed to load within 120s; skipping this launch");
        return;
    }

    let mut recovered = 0usize;
    for (path, _) in wavs {
        if recovered >= MAX_RECOVERIES_PER_LAUNCH {
            break;
        }
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Skip WAVs that already have a real transcript.
        if matches!(by_file.get(&fname), Some((_, true))) {
            continue;
        }

        // Never re-trigger the very thrash we're guarding against.
        if let Some(free) = free_bytes_at(recordings_dir) {
            if free < LOW_DISK_BYTES {
                warn!(
                    "Recovery paused: low disk; leaving {} for next launch",
                    fname
                );
                break;
            }
        }

        let samples = match crate::audio_toolkit::read_wav_samples(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!("Recovery: cannot read {}: {}", fname, e);
                continue;
            }
        };
        if samples.is_empty() {
            continue;
        }

        info!(
            "Recovering orphaned recording {} ({} samples)",
            fname,
            samples.len()
        );
        match tm.transcribe(samples) {
            Ok(text) if !text.trim().is_empty() => {
                let result = match by_file.get(&fname) {
                    // Empty-text row from an aborted run → fill it in.
                    Some((id, _)) => hm.update_transcription(*id, text, None, None).map(|_| ()),
                    // No row at all → create one (not auto-pasted).
                    None => hm
                        .save_entry(fname.clone(), text, false, None, None)
                        .map(|_| ()),
                };
                match result {
                    Ok(()) => {
                        recovered += 1;
                        info!("Recovered transcription for {}", fname);
                    }
                    Err(e) => warn!("Recovery: failed to save {}: {}", fname, e),
                }
            }
            Ok(_) => debug!("Recovery of {} produced empty text; skipping", fname),
            Err(e) => warn!("Recovery of {} failed: {}", fname, e),
        }
    }

    if recovered > 0 {
        info!(
            "Startup recovery complete: {} recording(s) recovered to history",
            recovered
        );
    }
}
