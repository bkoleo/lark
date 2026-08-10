use crate::input;
use crate::settings;
use crate::settings::OverlayPosition;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

#[cfg(not(target_os = "macos"))]
use log::debug;

#[cfg(not(target_os = "macos"))]
use tauri::WebviewWindowBuilder;

#[cfg(target_os = "macos")]
use tauri::WebviewUrl;

#[cfg(target_os = "macos")]
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel, StyleMask};

#[cfg(target_os = "linux")]
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

#[cfg(target_os = "linux")]
use std::env;

#[cfg(target_os = "macos")]
tauri_panel! {
    panel!(RecordingOverlayPanel {
        config: {
            can_become_key_window: false,
            is_floating_panel: true
        }
    })
}

// The meeting card reuses RecordingOverlayPanel's class: identical config
// (non-activating floating panel), and a second tauri_panel! invocation in
// one module collides on the macro's generated imports.

const OVERLAY_WIDTH: f64 = 200.0;
const OVERLAY_HEIGHT: f64 = 52.0;

#[cfg(target_os = "macos")]
const OVERLAY_TOP_OFFSET: f64 = 46.0;
#[cfg(any(target_os = "windows", target_os = "linux"))]
const OVERLAY_TOP_OFFSET: f64 = 4.0;

#[cfg(target_os = "macos")]
const OVERLAY_BOTTOM_OFFSET: f64 = 15.0;

#[cfg(any(target_os = "windows", target_os = "linux"))]
const OVERLAY_BOTTOM_OFFSET: f64 = 40.0;

const OVERLAY_LEFT_OFFSET: f64 = 16.0;

#[cfg(target_os = "linux")]
fn update_gtk_layer_shell_anchors(overlay_window: &tauri::webview::WebviewWindow) {
    let window_clone = overlay_window.clone();
    let _ = overlay_window.run_on_main_thread(move || {
        // Try to get the GTK window from the Tauri webview
        if let Ok(gtk_window) = window_clone.gtk_window() {
            let settings = settings::get_settings(window_clone.app_handle());
            match settings.overlay_position {
                OverlayPosition::Top => {
                    gtk_window.set_anchor(Edge::Top, true);
                    gtk_window.set_anchor(Edge::Bottom, false);
                }
                OverlayPosition::Bottom | OverlayPosition::None => {
                    gtk_window.set_anchor(Edge::Bottom, true);
                    gtk_window.set_anchor(Edge::Top, false);
                }
            }
        }
    });
}

/// Returns true when the environment variable is set to a truthy value
/// (e.g. "1", "true", "yes", "on").
/// "0", "false", "no", "off" and empty string are treated as falsy (case-insensitive).
/// Returns false when the variable is not set.
#[cfg(target_os = "linux")]
fn env_flag_enabled(name: &str) -> bool {
    match env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// Initializes GTK layer shell for Linux overlay window
/// Returns true if layer shell was successfully initialized, false otherwise
#[cfg(target_os = "linux")]
fn init_gtk_layer_shell(overlay_window: &tauri::webview::WebviewWindow) -> bool {
    if env_flag_enabled("HANDY_NO_GTK_LAYER_SHELL") {
        debug!("Skipping GTK layer shell init (HANDY_NO_GTK_LAYER_SHELL is enabled)");
        return false;
    }

    if !gtk_layer_shell::is_supported() {
        return false;
    }

    // Try to get the GTK window from the Tauri webview
    if let Ok(gtk_window) = overlay_window.gtk_window() {
        // Initialize layer shell
        gtk_window.init_layer_shell();
        gtk_window.set_layer(Layer::Overlay);
        gtk_window.set_keyboard_mode(KeyboardMode::None);
        gtk_window.set_exclusive_zone(0);

        update_gtk_layer_shell_anchors(overlay_window);

        return true;
    }
    false
}

/// Forces a window to be topmost using Win32 API (Windows only)
/// This is more reliable than Tauri's set_always_on_top which can be overridden
#[cfg(target_os = "windows")]
fn force_overlay_topmost(overlay_window: &tauri::webview::WebviewWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    // Clone because run_on_main_thread takes 'static
    let overlay_clone = overlay_window.clone();

    // Make sure the Win32 call happens on the UI thread
    let _ = overlay_clone.clone().run_on_main_thread(move || {
        if let Ok(hwnd) = overlay_clone.hwnd() {
            unsafe {
                // Force Z-order: make this window topmost without changing size/pos or stealing focus
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    });
}

fn get_monitor_with_cursor(app_handle: &AppHandle) -> Option<tauri::Monitor> {
    if let Some(mouse_location) = input::get_cursor_position(app_handle) {
        if let Ok(monitors) = app_handle.available_monitors() {
            for monitor in monitors {
                // Tauri's monitor position/size are physical pixels, but enigo
                // may return logical coordinates (confirmed on macOS via
                // NSEvent::mouseLocation; on Windows, GetCursorPos behavior
                // depends on the process DPI-awareness context). Dividing by
                // scale_factor normalizes to logical, which is safe regardless:
                // if enigo returns logical it matches directly, and if it returns
                // physical on a scale=1 monitor the division is a no-op.
                let scale = monitor.scale_factor();
                let pos = PhysicalPosition::new(
                    (monitor.position().x as f64 / scale) as i32,
                    (monitor.position().y as f64 / scale) as i32,
                );
                let size = PhysicalSize::new(
                    (monitor.size().width as f64 / scale) as u32,
                    (monitor.size().height as f64 / scale) as u32,
                );
                if is_mouse_within_monitor(mouse_location, &pos, &size) {
                    return Some(monitor);
                }
            }
        }
    }

    app_handle.primary_monitor().ok().flatten()
}

fn is_mouse_within_monitor(
    mouse_pos: (i32, i32),
    monitor_pos: &PhysicalPosition<i32>,
    monitor_size: &PhysicalSize<u32>,
) -> bool {
    let (mouse_x, mouse_y) = mouse_pos;
    let PhysicalPosition {
        x: monitor_x,
        y: monitor_y,
    } = *monitor_pos;
    let PhysicalSize {
        width: monitor_width,
        height: monitor_height,
    } = *monitor_size;

    mouse_x >= monitor_x
        && mouse_x < (monitor_x + monitor_width as i32)
        && mouse_y >= monitor_y
        && mouse_y < (monitor_y + monitor_height as i32)
}

/// Returns overlay position in logical coordinates (points on macOS).
///
/// Uses monitor position/size directly rather than work_area(), which can
/// return incorrect coordinates on macOS for monitors with negative positions.
/// The per-platform OVERLAY_TOP_OFFSET / OVERLAY_BOTTOM_OFFSET constants
/// already account for system chrome (menu bar, taskbar).
///
/// We must use LogicalPosition (not PhysicalPosition) because Tauri/tao
/// converts PhysicalPosition using the scale factor of the monitor the window
/// is *currently* on, which is wrong when moving cross-monitor.
fn calculate_overlay_position(app_handle: &AppHandle) -> Option<(f64, f64)> {
    let monitor = get_monitor_with_cursor(app_handle)?;
    let scale = monitor.scale_factor();
    let monitor_x = monitor.position().x as f64 / scale;
    let monitor_y = monitor.position().y as f64 / scale;
    let monitor_width = monitor.size().width as f64 / scale;
    let monitor_height = monitor.size().height as f64 / scale;

    let settings = settings::get_settings(app_handle);

    // A user-dragged position wins over the preset. The offset is relative to
    // the monitor's top-left corner so it survives monitor changes; clamp it
    // so the overlay can never end up off-screen.
    if let Some((ox, oy)) = settings.overlay_custom_offset {
        let x = (monitor_x + ox).clamp(monitor_x, monitor_x + monitor_width - OVERLAY_WIDTH);
        let y = (monitor_y + oy).clamp(monitor_y, monitor_y + monitor_height - OVERLAY_HEIGHT);
        return Some((x, y));
    }

    // Lark default: bottom-left, out of the way of centered app content.
    let x = monitor_x + OVERLAY_LEFT_OFFSET;
    let y = match settings.overlay_position {
        OverlayPosition::Top => monitor_y + OVERLAY_TOP_OFFSET,
        OverlayPosition::Bottom | OverlayPosition::None => {
            monitor_y + monitor_height - OVERLAY_HEIGHT - OVERLAY_BOTTOM_OFFSET
        }
    };

    Some((x, y))
}

/// Set whenever Lark itself repositions the overlay, so the Moved listener can
/// tell programmatic moves apart from the user dragging the pill.
static LAST_PROGRAMMATIC_MOVE: Mutex<Option<Instant>> = Mutex::new(None);
static DRAG_SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Watches the overlay window for user drags and persists the dropped
/// position (debounced) as a monitor-relative logical offset.
fn attach_overlay_drag_tracking(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window("recording_overlay") else {
        return;
    };
    let app = app_handle.clone();
    let win = window.clone();
    window.on_window_event(move |event| {
        let tauri::WindowEvent::Moved(pos) = event else {
            return;
        };
        // Ignore moves Lark made itself (repositioning before show).
        if let Some(t) = *LAST_PROGRAMMATIC_MOVE.lock().unwrap() {
            if t.elapsed() < std::time::Duration::from_millis(600) {
                return;
            }
        }
        let pos = *pos;
        let gen = DRAG_SAVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app.clone();
        let win = win.clone();
        // Debounce: drags fire a stream of Moved events; save only after the
        // window has been still for a moment.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            if DRAG_SAVE_GENERATION.load(Ordering::SeqCst) != gen {
                return;
            }
            let Ok(Some(monitor)) = win.current_monitor() else {
                return;
            };
            let scale = monitor.scale_factor();
            let ox = pos.x as f64 / scale - monitor.position().x as f64 / scale;
            let oy = pos.y as f64 / scale - monitor.position().y as f64 / scale;
            let mut settings = settings::get_settings(&app);
            settings.overlay_custom_offset = Some((ox, oy));
            settings::write_settings(&app, settings);
            log::debug!("Saved overlay custom offset: ({ox:.0}, {oy:.0})");
        });
    });
}

/// Creates the recording overlay window and keeps it hidden by default
#[cfg(not(target_os = "macos"))]
pub fn create_recording_overlay(app_handle: &AppHandle) {
    // On Linux (Wayland), monitor detection often fails, but we don't need exact coordinates
    // for Layer Shell as we use anchors. On other platforms, we require a monitor.
    #[cfg(not(target_os = "linux"))]
    {
        let position = calculate_overlay_position(app_handle);
        if position.is_none() {
            debug!("Failed to determine overlay position, not creating overlay window");
            return;
        }
    }

    // Position starts unset — update_overlay_position() sets the correct
    // LogicalPosition before the overlay is shown.
    let mut builder = WebviewWindowBuilder::new(
        app_handle,
        "recording_overlay",
        tauri::WebviewUrl::App("src/overlay/index.html".into()),
    )
    .title("Recording")
    .resizable(false)
    .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
    .shadow(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .accept_first_mouse(true)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .transparent(true)
    .focused(false)
    .visible(false);

    if let Some(data_dir) = crate::portable::data_dir() {
        builder = builder.data_directory(data_dir.join("webview"));
    }

    #[allow(unused_variables)]
    match builder.build() {
        Ok(window) => {
            #[cfg(target_os = "linux")]
            {
                // Try to initialize GTK layer shell, ignore errors if compositor doesn't support it
                if init_gtk_layer_shell(&window) {
                    debug!("GTK layer shell initialized for overlay window");
                } else {
                    debug!("GTK layer shell not available, falling back to regular window");
                }
            }

            attach_overlay_drag_tracking(app_handle);
            debug!("Recording overlay window created successfully (hidden)");
        }
        Err(e) => {
            debug!("Failed to create recording overlay window: {}", e);
        }
    }
}

/// Creates the recording overlay panel and keeps it hidden by default (macOS)
#[cfg(target_os = "macos")]
pub fn create_recording_overlay(app_handle: &AppHandle) {
    if let Some((x, y)) = calculate_overlay_position(app_handle) {
        // PanelBuilder creates a Tauri window then converts it to NSPanel.
        // The window remains registered, so get_webview_window() still works.
        match PanelBuilder::<_, RecordingOverlayPanel>::new(app_handle, "recording_overlay")
            .url(WebviewUrl::App("src/overlay/index.html".into()))
            .title("Recording")
            .position(tauri::Position::Logical(tauri::LogicalPosition { x, y }))
            .level(PanelLevel::Status)
            .size(tauri::Size::Logical(tauri::LogicalSize {
                width: OVERLAY_WIDTH,
                height: OVERLAY_HEIGHT,
            }))
            .has_shadow(false)
            .transparent(true)
            .no_activate(true)
            // no_activate only suppresses activation while the panel is being
            // created; NonactivatingPanel is what keeps a *click* on the pill
            // (e.g. the Copy button) from activating the app — activating
            // would drag the main settings window to the front with it.
            // borderless() must come first: it resets the mask, not ORs it.
            .style_mask(StyleMask::empty().borderless().nonactivating_panel())
            .corner_radius(0.0)
            .with_window(|w| {
                // Pin to dark so the HUD glass stays dark even when the
                // system is in light mode (HudWindow material follows the
                // window's appearance, not the CSS).
                // accept_first_mouse: the app is never active when the pill is
                // clicked, so the first click must land, not just focus.
                w.decorations(false)
                    .transparent(true)
                    .accept_first_mouse(true)
                    .theme(Some(tauri::Theme::Dark))
            })
            .collection_behavior(
                CollectionBehavior::new()
                    .can_join_all_spaces()
                    .full_screen_auxiliary(),
            )
            .build()
        {
            Ok(panel) => {
                let _ = panel.hide();
                // Native macOS glass: real background blur behind the pill,
                // matching the system HUD material. CSS backdrop-filter can't
                // do this — it only blurs the webview's own content.
                if let Some(win) = app_handle.get_webview_window("recording_overlay") {
                    use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                    if let Err(e) =
                        apply_vibrancy(&win, NSVisualEffectMaterial::HudWindow, None, Some(26.0))
                    {
                        log::warn!("Failed to apply overlay vibrancy: {e}");
                    }
                }
                attach_overlay_drag_tracking(app_handle);
            }
            Err(e) => {
                log::error!("Failed to create recording overlay panel: {}", e);
            }
        }
    }
}

fn show_overlay_state(app_handle: &AppHandle, state: &str) {
    // Check if overlay should be shown based on position setting
    let settings = settings::get_settings(app_handle);
    if settings.overlay_position == OverlayPosition::None {
        return;
    }

    update_overlay_position(app_handle);

    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        let _ = overlay_window.show();

        // On Windows, aggressively re-assert "topmost" in the native Z-order after showing
        #[cfg(target_os = "windows")]
        force_overlay_topmost(&overlay_window);

        let _ = overlay_window.emit("show-overlay", state);
    }
}

/// Shows the recording overlay window with fade-in animation
pub fn show_recording_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "recording");
}

#[cfg(target_os = "macos")]
const MEETING_CARD_WIDTH: f64 = 360.0;
#[cfg(target_os = "macos")]
const MEETING_CARD_HEIGHT: f64 = 84.0;
/// Collapsed while-recording indicator: small and unobtrusive.
#[cfg(target_os = "macos")]
const MEETING_MINI_WIDTH: f64 = 130.0;
#[cfg(target_os = "macos")]
const MEETING_MINI_HEIGHT: f64 = 48.0;

/// Top-right corner of the monitor the cursor is on (Granola puts its
/// meeting card there; Kole asked for the same).
#[cfg(target_os = "macos")]
fn meeting_window_position(app_handle: &AppHandle, width: f64) -> Option<(f64, f64)> {
    let monitor = get_monitor_with_cursor(app_handle)?;
    let scale = monitor.scale_factor();
    let x =
        monitor.position().x as f64 / scale + monitor.size().width as f64 / scale - width - 12.0;
    let y = monitor.position().y as f64 / scale + 42.0;
    Some((x, y))
}

#[cfg(target_os = "macos")]
fn place_meeting_window(app_handle: &AppHandle, width: f64, height: f64) {
    if let Some(window) = app_handle.get_webview_window("meeting_prompt") {
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
        if let Some((x, y)) = meeting_window_position(app_handle, width) {
            let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        }
    }
}

/// Creates the meeting-prompt card window (hidden until a meeting is
/// detected). A separate window from the recording pill so the two never
/// fight: the pill is the user-positioned dictation HUD, the card is a
/// transient top-right ask.
#[cfg(target_os = "macos")]
pub fn create_meeting_prompt_window(app_handle: &AppHandle) {
    let Some((x, y)) = meeting_window_position(app_handle, MEETING_CARD_WIDTH) else {
        return;
    };
    match PanelBuilder::<_, RecordingOverlayPanel>::new(app_handle, "meeting_prompt")
        .url(WebviewUrl::App("src/meeting-prompt/index.html".into()))
        .title("Meeting")
        .position(tauri::Position::Logical(tauri::LogicalPosition { x, y }))
        .level(PanelLevel::Status)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: MEETING_CARD_WIDTH,
            height: MEETING_CARD_HEIGHT,
        }))
        .has_shadow(false)
        .transparent(true)
        .no_activate(true)
        // Same as the recording pill: clicking the card's Record button must
        // not activate the app (see create_recording_overlay).
        .style_mask(StyleMask::empty().borderless().nonactivating_panel())
        .corner_radius(0.0)
        .with_window(|w| {
            w.decorations(false)
                .transparent(true)
                .accept_first_mouse(true)
        })
        .collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary(),
        )
        .build()
    {
        Ok(panel) => {
            let _ = panel.hide();
        }
        Err(e) => log::error!("Failed to create meeting prompt panel: {e}"),
    }
}

/// Shows the Granola-pop meeting card: a real calendar event title + time
/// range when known (`title`/`time_range`), falling back to the app name
/// otherwise. `kind` is "start" (call began, offer to record), "stop"
/// (call ended while recording, offer to stop & transcribe), "stop_ask"
/// (user expanded the mini pill to stop manually), or "saved" (transcript
/// written). `title`/`time_range` are `None` whenever the calendar didn't
/// match — the card's own fallback text is what carries the app name then.
#[cfg(target_os = "macos")]
pub fn show_meeting_prompt(
    app_handle: &AppHandle,
    kind: &str,
    app_name: &str,
    title: Option<&str>,
    time_range: Option<&str>,
) {
    place_meeting_window(app_handle, MEETING_CARD_WIDTH, MEETING_CARD_HEIGHT);
    if let Some(window) = app_handle.get_webview_window("meeting_prompt") {
        let _ = window.show();
        let _ = window.emit(
            "meeting-prompt",
            serde_json::json!({
                "kind": kind,
                "app": app_name,
                "title": title,
                "time_range": time_range,
            }),
        );
    }
}

/// While a meeting recording runs, a small pulsing-dot pill stays top-right
/// so it's always clear Lark is capturing. Clicking it expands back into
/// the card with a Stop button.
#[cfg(target_os = "macos")]
pub fn show_meeting_recording_indicator(app_handle: &AppHandle) {
    place_meeting_window(app_handle, MEETING_MINI_WIDTH, MEETING_MINI_HEIGHT);
    if let Some(window) = app_handle.get_webview_window("meeting_prompt") {
        let _ = window.show();
        let _ = window.emit("meeting-recording", ());
    }
}

/// Hides the meeting card (after a button click or auto-dismiss).
#[cfg(target_os = "macos")]
pub fn hide_meeting_prompt(app_handle: &AppHandle) {
    if let Some(window) = app_handle.get_webview_window("meeting_prompt") {
        let _ = window.hide();
    }
}

/// Tells the overlay how much audio is being transcribed and how long that
/// is expected to take (from the learned machine speed), so it can show a
/// progress ring and time-remaining instead of a bare spinner.
pub fn emit_transcribing_info(app_handle: &AppHandle, audio_secs: u64, estimated_secs: u64) {
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        let _ = overlay_window.emit(
            "transcribing-info",
            serde_json::json!({ "audioSecs": audio_secs, "estimatedSecs": estimated_secs }),
        );
    }
}

/// Shows the transcribing overlay window
pub fn show_transcribing_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "transcribing");
}

/// Turns the pill into a Copy button after a paste was withheld because the
/// user switched apps mid-transcription (see paste_guard).
///
/// Returns false when there is no overlay to show it on — the caller must then
/// paste as normal rather than silently swallowing the text.
pub fn show_copy_ready_overlay(app_handle: &AppHandle) -> bool {
    let settings = settings::get_settings(app_handle);
    if settings.overlay_position == OverlayPosition::None {
        return false;
    }

    update_overlay_position(app_handle);

    let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") else {
        return false;
    };
    if overlay_window.show().is_err() {
        return false;
    }

    #[cfg(target_os = "windows")]
    force_overlay_topmost(&overlay_window);

    overlay_window.emit("copy-ready", ()).is_ok()
}

/// Shows the processing overlay window
pub fn show_processing_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "processing");
}

/// Updates the overlay window position based on current settings
pub fn update_overlay_position(app_handle: &AppHandle) {
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        #[cfg(target_os = "linux")]
        {
            update_gtk_layer_shell_anchors(&overlay_window);
        }

        if let Some((x, y)) = calculate_overlay_position(app_handle) {
            *LAST_PROGRAMMATIC_MOVE.lock().unwrap() = Some(Instant::now());
            let _ = overlay_window
                .set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        }
    }
}

/// Hides the recording overlay window with fade-out animation
pub fn hide_recording_overlay(app_handle: &AppHandle) {
    // Always hide the overlay regardless of settings - if setting was changed while recording,
    // we still want to hide it properly
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        // Emit event to trigger fade-out animation
        let _ = overlay_window.emit("hide-overlay", ());
        // Hide the window after a short delay to allow animation to complete
        let window_clone = overlay_window.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = window_clone.hide();
        });
    }
}

pub fn emit_levels(app_handle: &AppHandle, levels: &Vec<f32>) {
    // emit levels to main app
    let _ = app_handle.emit("mic-level", levels);

    // also emit to the recording overlay if it's open
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        let _ = overlay_window.emit("mic-level", levels);
    }
}
