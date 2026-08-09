use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings;
use crate::tray_i18n::get_tray_translations;
use log::{error, info, warn};
use std::sync::Arc;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Manager, Theme};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Debug, PartialEq)]
pub enum TrayIconState {
    Idle,
    Recording,
    Transcribing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppTheme {
    Dark,
    Light,
    Colored, // Pink/colored theme for Linux
}

/// Gets the current app theme, with Linux defaulting to Colored theme
pub fn get_current_theme(app: &AppHandle) -> AppTheme {
    if cfg!(target_os = "linux") {
        // On Linux, always use the colored theme
        AppTheme::Colored
    } else {
        // On other platforms, map system theme to our app theme
        if let Some(main_window) = app.get_webview_window("main") {
            match main_window.theme().unwrap_or(Theme::Dark) {
                Theme::Light => AppTheme::Light,
                Theme::Dark => AppTheme::Dark,
                _ => AppTheme::Dark, // Default fallback
            }
        } else {
            AppTheme::Dark
        }
    }
}

/// Gets the appropriate icon path for the given theme and state
pub fn get_icon_path(theme: AppTheme, state: TrayIconState) -> &'static str {
    match (theme, state) {
        // Dark theme uses light icons
        (AppTheme::Dark, TrayIconState::Idle) => "resources/tray_idle.png",
        (AppTheme::Dark, TrayIconState::Recording) => "resources/tray_recording.png",
        (AppTheme::Dark, TrayIconState::Transcribing) => "resources/tray_transcribing.png",
        // Light theme uses dark icons
        (AppTheme::Light, TrayIconState::Idle) => "resources/tray_idle_dark.png",
        (AppTheme::Light, TrayIconState::Recording) => "resources/tray_recording_dark.png",
        (AppTheme::Light, TrayIconState::Transcribing) => "resources/tray_transcribing_dark.png",
        // Colored theme uses pink icons (for Linux)
        (AppTheme::Colored, TrayIconState::Idle) => "resources/handy.png",
        (AppTheme::Colored, TrayIconState::Recording) => "resources/recording.png",
        (AppTheme::Colored, TrayIconState::Transcribing) => "resources/transcribing.png",
    }
}

pub fn change_tray_icon(app: &AppHandle, icon: TrayIconState) {
    let tray = app.state::<TrayIcon>();
    let theme = get_current_theme(app);

    let icon_path = get_icon_path(theme, icon.clone());

    let _ = tray.set_icon(Some(
        Image::from_path(
            app.path()
                .resolve(icon_path, tauri::path::BaseDirectory::Resource)
                .expect("failed to resolve"),
        )
        .expect("failed to set icon"),
    ));

    // Update menu based on state
    update_tray_menu(app, &icon, None);
}

pub fn tray_tooltip() -> String {
    version_label()
}

/// How long a title can get before it starts pushing the clock off the screen.
/// The menu bar has no scrollbar and no overflow — it silently eats whatever
/// sits to the left of the offender, so this is a hard budget, not a hint.
#[cfg(target_os = "macos")]
const MAX_TITLE_CHARS: usize = 24;

/// The next meeting, rendered for the menu bar.
///
/// Under an hour it counts down, because that is when the number changes what
/// you do. Beyond that a countdown is just a clock with extra steps, so it
/// becomes the start time.
///
/// A clear day says so out loud rather than going blank. Blank is ambiguous
/// between "nothing left" and "this feature is broken", and the whole value of
/// a menu bar item is being able to trust it at a glance — so the only states
/// that render nothing are the setting being off and calendar access being
/// unavailable, neither of which is a claim about the day.
#[cfg(target_os = "macos")]
pub fn next_meeting_label(app: &AppHandle) -> String {
    use crate::managers::meeting_calendar::NextMeeting;

    let settings = settings::get_settings(app);
    if !settings.show_next_meeting {
        return String::new();
    }

    match crate::managers::meeting_calendar::next_meeting() {
        NextMeeting::Upcoming(event) => format_meeting_label(&event),
        NextMeeting::Clear => get_tray_translations(Some(settings.app_language)).meetings_done,
        NextMeeting::Unavailable => String::new(),
    }
}

/// Split from the EventKit lookup so the wording is testable without a calendar.
#[cfg(target_os = "macos")]
fn format_meeting_label(event: &crate::managers::meeting_calendar::UpcomingEvent) -> String {
    let title = truncate_title(&event.title);
    if event.starts_in_secs <= 0 {
        format!("now · {}", title)
    } else if event.starts_in_secs < 60 * 60 {
        // Round up: with 90 seconds left, "in 2m" is honest and "in 1m" is not.
        let minutes = (event.starts_in_secs + 59) / 60;
        format!("in {}m · {}", minutes, title)
    } else {
        format!("{} {}", event.start_hm, title)
    }
}

/// Truncate on character boundaries, never bytes — an accented name or an emoji
/// in a meeting title would panic a byte slice.
#[cfg(target_os = "macos")]
fn truncate_title(title: &str) -> String {
    let title = title.trim();
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    let kept: String = title.chars().take(MAX_TITLE_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

/// Push the current label to the menu bar, skipping the call when nothing has
/// changed. The label only moves once a minute at most, so the overwhelming
/// majority of ticks are no-ops.
#[cfg(target_os = "macos")]
pub fn refresh_next_meeting(app: &AppHandle) {
    use std::sync::Mutex;
    static LAST: Mutex<Option<String>> = Mutex::new(None);

    let label = next_meeting_label(app);

    let mut last = match LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if last.as_deref() == Some(label.as_str()) {
        return;
    }
    *last = Some(label.clone());
    drop(last);

    // Only fires on a change, so this is a handful of lines a day. Worth it:
    // a blank menu bar is ambiguous between "clear diary", "permission denied"
    // and "broken", and this is the only thing that tells them apart.
    if label.is_empty() {
        info!("Next meeting: hidden (setting off, or no calendar access)");
    } else {
        info!("Next meeting: {}", label);
    }

    let Some(tray) = app.try_state::<TrayIcon>() else {
        return;
    };
    let result = if label.is_empty() {
        tray.set_title(None::<&str>)
    } else {
        tray.set_title(Some(&label))
    };
    if let Err(e) = result {
        warn!("Failed to set menu bar meeting label: {}", e);
    }
}

/// Keep the menu bar label current.
///
/// Thirty seconds against a minute-granularity countdown means the number is
/// never more than half a minute stale, and an EventKit read of one day's
/// events is cheap enough that the tighter loop costs nothing worth saving.
#[cfg(target_os = "macos")]
pub fn start_next_meeting_updates(app: AppHandle) {
    std::thread::spawn(move || loop {
        refresh_next_meeting(&app);
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
}

fn version_label() -> String {
    if cfg!(debug_assertions) {
        format!("Lark v{} (Dev)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("Lark v{}", env!("CARGO_PKG_VERSION"))
    }
}

pub fn update_tray_menu(app: &AppHandle, state: &TrayIconState, locale: Option<&str>) {
    let settings = settings::get_settings(app);

    let locale = locale.unwrap_or(&settings.app_language);
    let strings = get_tray_translations(Some(locale.to_string()));

    // Platform-specific accelerators
    #[cfg(target_os = "macos")]
    let (settings_accelerator, quit_accelerator) = (Some("Cmd+,"), Some("Cmd+Q"));
    #[cfg(not(target_os = "macos"))]
    let (settings_accelerator, quit_accelerator) = (Some("Ctrl+,"), Some("Ctrl+Q"));

    // Create common menu items
    let version_label = version_label();
    let version_i = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)
        .expect("failed to create version item");
    let settings_i = MenuItem::with_id(
        app,
        "settings",
        &strings.settings,
        true,
        settings_accelerator,
    )
    .expect("failed to create settings item");
    let check_updates_i = MenuItem::with_id(
        app,
        "check_updates",
        &strings.check_updates,
        settings.update_checks_enabled,
        None::<&str>,
    )
    .expect("failed to create check updates item");
    let copy_last_transcript_i = MenuItem::with_id(
        app,
        "copy_last_transcript",
        &strings.copy_last_transcript,
        true,
        None::<&str>,
    )
    .expect("failed to create copy last transcript item");
    let model_loaded = app.state::<Arc<TranscriptionManager>>().is_model_loaded();
    let quit_i = MenuItem::with_id(app, "quit", &strings.quit, true, quit_accelerator)
        .expect("failed to create quit item");
    let separator = || PredefinedMenuItem::separator(app).expect("failed to create separator");

    #[cfg(target_os = "macos")]
    let meeting_i = {
        use crate::managers::meeting::{MeetingManager, MeetingStatus};
        let status = app
            .try_state::<Arc<MeetingManager>>()
            .map(|m| m.status())
            .unwrap_or(MeetingStatus::Idle);
        let (label, enabled) = match status {
            MeetingStatus::Idle => (&strings.meeting_start, true),
            MeetingStatus::Recording => (&strings.meeting_stop, true),
            MeetingStatus::Processing => (&strings.meeting_processing, false),
        };
        MenuItem::with_id(app, "meeting_toggle", label, enabled, None::<&str>)
            .expect("failed to create meeting item")
    };

    // Build model submenu — label is the active model name
    let model_manager = app.state::<Arc<ModelManager>>();
    let models = model_manager.get_available_models();
    let current_model_id = &settings.selected_model;

    let mut downloaded: Vec<_> = models.into_iter().filter(|m| m.is_downloaded).collect();
    downloaded.sort_by(|a, b| a.name.cmp(&b.name));

    let submenu_label = downloaded
        .iter()
        .find(|m| m.id == *current_model_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| strings.model.clone());

    let model_submenu = {
        let submenu = Submenu::with_id(app, "model_submenu", &submenu_label, true)
            .expect("failed to create model submenu");

        for model in &downloaded {
            let is_active = model.id == *current_model_id;
            let item_id = format!("model_select:{}", model.id);
            let item =
                CheckMenuItem::with_id(app, &item_id, &model.name, true, is_active, None::<&str>)
                    .expect("failed to create model item");
            let _ = submenu.append(&item);
        }

        submenu
    };

    let unload_model_i = MenuItem::with_id(
        app,
        "unload_model",
        &strings.unload_model,
        model_loaded,
        None::<&str>,
    )
    .expect("failed to create unload model item");

    let menu = match state {
        TrayIconState::Recording | TrayIconState::Transcribing => {
            let cancel_i = MenuItem::with_id(app, "cancel", &strings.cancel, true, None::<&str>)
                .expect("failed to create cancel item");
            Menu::with_items(
                app,
                &[
                    &version_i,
                    &separator(),
                    &cancel_i,
                    &separator(),
                    &copy_last_transcript_i,
                    #[cfg(target_os = "macos")]
                    &meeting_i,
                    &separator(),
                    &settings_i,
                    &check_updates_i,
                    &separator(),
                    &quit_i,
                ],
            )
            .expect("failed to create menu")
        }
        TrayIconState::Idle => Menu::with_items(
            app,
            &[
                &version_i,
                &separator(),
                &copy_last_transcript_i,
                #[cfg(target_os = "macos")]
                &meeting_i,
                &separator(),
                &model_submenu,
                &unload_model_i,
                &separator(),
                &settings_i,
                &check_updates_i,
                &separator(),
                &quit_i,
            ],
        )
        .expect("failed to create menu"),
    };

    let tray = app.state::<TrayIcon>();
    let _ = tray.set_menu(Some(menu));
    let _ = tray.set_icon_as_template(true);
    let _ = tray.set_tooltip(Some(version_label));
}

fn last_transcript_text(entry: &HistoryEntry) -> &str {
    entry
        .post_processed_text
        .as_deref()
        .unwrap_or(&entry.transcription_text)
}

pub fn set_tray_visibility(app: &AppHandle, visible: bool) {
    let tray = app.state::<TrayIcon>();
    if let Err(e) = tray.set_visible(visible) {
        error!("Failed to set tray visibility: {}", e);
    } else {
        info!("Tray visibility set to: {}", visible);
    }
}

pub fn copy_last_transcript(app: &AppHandle) {
    let history_manager = app.state::<Arc<HistoryManager>>();
    let entry = match history_manager.get_latest_completed_entry() {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            warn!("No completed transcription history entries available for tray copy.");
            return;
        }
        Err(err) => {
            error!(
                "Failed to fetch last completed transcription entry: {}",
                err
            );
            return;
        }
    };

    let text = last_transcript_text(&entry);
    if text.trim().is_empty() {
        warn!("Last completed transcription is empty; skipping tray copy.");
        return;
    }

    if let Err(err) = app.clipboard().write_text(text) {
        error!("Failed to copy last transcript to clipboard: {}", err);
        return;
    }

    info!("Copied last transcript to clipboard via tray.");
}

#[cfg(test)]
mod tests {
    use super::last_transcript_text;
    use crate::managers::history::HistoryEntry;

    fn build_entry(transcription: &str, post_processed: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            file_name: "handy-1.wav".to_string(),
            timestamp: 0,
            saved: false,
            title: "Recording".to_string(),
            transcription_text: transcription.to_string(),
            post_processed_text: post_processed.map(|text| text.to_string()),
            post_process_prompt: None,
            post_process_requested: false,
        }
    }

    #[test]
    fn uses_post_processed_text_when_available() {
        let entry = build_entry("raw", Some("processed"));
        assert_eq!(last_transcript_text(&entry), "processed");
    }

    #[test]
    fn falls_back_to_raw_transcription() {
        let entry = build_entry("raw", None);
        assert_eq!(last_transcript_text(&entry), "raw");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod next_meeting_tests {
    use super::*;
    use crate::managers::meeting_calendar::UpcomingEvent;

    fn event(starts_in_secs: i64, title: &str) -> UpcomingEvent {
        UpcomingEvent {
            title: title.to_string(),
            starts_in_secs,
            start_hm: "14:30".to_string(),
        }
    }

    #[test]
    fn counts_down_within_the_hour() {
        assert_eq!(
            format_meeting_label(&event(8 * 60, "Elizabeth 1:1")),
            "in 8m · Elizabeth 1:1"
        );
    }

    #[test]
    fn rounds_the_countdown_up() {
        // 90s left is nearer two minutes than one, and never overstates the time
        // remaining.
        assert_eq!(
            format_meeting_label(&event(90, "Standup")),
            "in 2m · Standup"
        );
        assert_eq!(
            format_meeting_label(&event(1, "Standup")),
            "in 1m · Standup"
        );
    }

    #[test]
    fn shows_the_clock_time_beyond_an_hour() {
        assert_eq!(
            format_meeting_label(&event(60 * 60, "Elizabeth 1:1")),
            "14:30 Elizabeth 1:1"
        );
    }

    #[test]
    fn marks_a_meeting_already_under_way() {
        assert_eq!(format_meeting_label(&event(0, "Standup")), "now · Standup");
        assert_eq!(
            format_meeting_label(&event(-120, "Standup")),
            "now · Standup"
        );
    }

    #[test]
    fn truncates_a_long_title_without_splitting_a_character() {
        let label = format_meeting_label(&event(300, "Quarterly planning and roadmap review"));
        assert_eq!(label, "in 5m · Quarterly planning and…");
        // The budget is on the title, not the whole label.
        assert_eq!(label.chars().filter(|c| *c == '…').count(), 1);
    }

    #[test]
    fn multibyte_titles_do_not_panic() {
        // A byte slice at the same offset would split the é and panic.
        let title = "Réunion générale avec l'équipe entière";
        let label = format_meeting_label(&event(300, title));
        assert!(label.starts_with("in 5m · Réunion"));
        assert!(label.ends_with('…'));
    }

    #[test]
    fn short_titles_are_left_alone() {
        assert_eq!(
            format_meeting_label(&event(300, "  Standup  ")),
            "in 5m · Standup"
        );
    }

    #[test]
    fn a_clear_day_says_so_and_is_translated() {
        // The reassurance only works if it is actually present in the strings
        // the tray renders from — an empty one would show as a blank menu bar,
        // which is the state it exists to rule out.
        let strings = get_tray_translations(Some("en-US".to_string()));
        assert_eq!(strings.meetings_done, "Meetings done");
    }

    #[test]
    fn locales_without_the_key_fall_back_to_english() {
        // Spanish has no tray translations for the meeting entries. It must
        // render the English word, never an invisible empty string.
        let es = get_tray_translations(Some("es".to_string()));
        assert_eq!(es.meetings_done, "Meetings done");
        assert!(!es.meeting_start.is_empty());
        assert!(!es.meeting_stop.is_empty());
        assert!(!es.meeting_processing.is_empty());
        // Keys Spanish *does* translate must stay Spanish.
        assert_ne!(es.quit, get_tray_translations(Some("en".to_string())).quit);
    }
}
