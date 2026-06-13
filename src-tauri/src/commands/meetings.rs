//! Meeting notes browser: lists transcripts from ~/Documents/Lark Meetings
//! for the in-app Meetings tab. Content is included so the frontend can
//! search inside transcripts, not just titles.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[derive(serde::Serialize, specta::Type)]
pub struct MeetingNote {
    pub title: String,
    pub path: String,
    pub modified_ms: f64,
    pub content: String,
}

fn meetings_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(std::path::PathBuf::from(home)
        .join("Documents")
        .join("Lark Meetings"))
}

#[tauri::command]
#[specta::specta]
pub fn list_meeting_notes() -> Result<Vec<MeetingNote>, String> {
    let dir = meetings_dir()?;
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
    let dir = meetings_dir()?;
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
    let dir = meetings_dir()?;
    let _ = std::fs::create_dir_all(&dir);
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| e.to_string())
}
