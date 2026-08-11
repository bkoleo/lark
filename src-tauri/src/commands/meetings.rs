//! Meeting notes browser: lists transcripts from the configured meetings
//! folder for the in-app Meetings tab. Content is included so the frontend
//! can search inside transcripts, not just titles.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::managers::meeting::meetings_dir_for;

#[derive(serde::Serialize, specta::Type)]
pub struct MeetingNote {
    pub title: String,
    pub path: String,
    pub modified_ms: f64,
    pub content: String,
}

/// `AppHandle` is injected by Tauri and never appears in the generated TS
/// signature, so adding it here does not change `bindings.ts` — which matters,
/// because bindings only regenerate in debug builds.
fn meetings_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    meetings_dir_for(app).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn list_meeting_notes(app: AppHandle) -> Result<Vec<MeetingNote>, String> {
    let dir = meetings_dir(&app)?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new()); // no meetings yet
    };

    let mut notes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "md").unwrap_or(true) {
            continue;
        }
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let modified_ms = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        content.truncate(200_000); // hard cap, transcripts are ~50KB
        notes.push(MeetingNote {
            title,
            path: path.to_string_lossy().to_string(),
            modified_ms,
            content,
        });
    }
    notes.sort_by(|a, b| b.modified_ms.total_cmp(&a.modified_ms));
    Ok(notes)
}

#[tauri::command]
#[specta::specta]
pub fn open_meeting_note(app: AppHandle, path: String) -> Result<(), String> {
    // Only open files that actually live in the meetings folder.
    let dir = meetings_dir(&app)?;
    let requested = std::path::PathBuf::from(&path);
    if !requested.starts_with(&dir) {
        return Err("not a meeting note".to_string());
    }
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn open_meetings_folder(app: AppHandle) -> Result<(), String> {
    let dir = meetings_dir(&app)?;
    let _ = std::fs::create_dir_all(&dir);
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| e.to_string())
}

/// Applies a mic choice made on the recording pill's picker. The picked
/// device takes the top precedence slot (manual → call → pin → default)
/// for the rest of the recording; an empty name clears it back to
/// automatic. The switch itself happens on the watchdog's next tick.
#[tauri::command]
#[specta::specta]
pub fn meeting_set_mic(app: AppHandle, device: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let manager = app
            .state::<std::sync::Arc<crate::managers::meeting::MeetingManager>>()
            .inner()
            .clone();
        manager.set_manual_mic(if device.is_empty() { None } else { Some(device) });
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, device);
        Err("meeting mode is macOS-only".to_string())
    }
}

/// Grows the recording-indicator window so the pill's mic picker fits
/// (`rows` menu rows), or collapses it back with `rows: 0`.
#[tauri::command]
#[specta::specta]
pub fn meeting_picker_resize(app: AppHandle, rows: u32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        crate::overlay::resize_meeting_indicator(&app, rows);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, rows);
        Err("meeting mode is macOS-only".to_string())
    }
}
