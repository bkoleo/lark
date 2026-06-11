//! Meeting detection (macOS): watches which processes hold the microphone
//! via Core Audio process objects — the same signal that lights the orange
//! menu-bar dot. When a known meeting app grabs the mic, Lark sends a
//! notification suggesting a recording; when the app releases it during a
//! recording, it suggests stopping.
//!
//! Deliberately notification-only in the spike: Lark never starts or stops
//! a meeting recording on its own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cidre::core_audio as ca;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::{NotificationExt, PermissionState};

use crate::managers::meeting::{MeetingManager, MeetingStatus};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Don't nag: at most one "record this?" prompt per this window.
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(300);

/// Bundle-id prefixes of apps whose mic use means "probably a meeting".
/// Prefix matching catches helper processes (e.g. com.google.Chrome.helper).
const MEETING_APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "Zoom"),
    ("com.microsoft.teams", "Teams"),
    ("com.apple.FaceTime", "FaceTime"),
    ("com.apple.avconference", "FaceTime"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.hnc.Discord", "Discord"),
    ("com.cisco", "Webex"),
    ("Cisco-Systems", "Webex"),
    ("com.google.Chrome", "Chrome"),
    ("com.brave.Browser", "Brave"),
    ("com.apple.Safari", "Safari"),
    ("company.thebrowser.Browser", "Arc"),
    ("com.microsoft.edgemac", "Edge"),
    ("org.mozilla.firefox", "Firefox"),
];

pub fn spawn_meeting_detector(app_handle: &AppHandle) {
    let app = app_handle.clone();
    std::thread::spawn(move || {
        // Ask for notification permission once, up front.
        if let Ok(state) = app.notification().permission_state() {
            if state != PermissionState::Granted {
                let _ = app.notification().request_permission();
            }
        }

        let own_pid = std::process::id() as i32;
        let mut previous: Vec<&'static str> = Vec::new();
        let mut last_prompt: Option<Instant> = None;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            let current = mic_using_meeting_apps(own_pid);
            let status = app
                .try_state::<Arc<MeetingManager>>()
                .map(|m| m.status())
                .unwrap_or(MeetingStatus::Idle);

            let call_started = !current.is_empty() && previous.is_empty();
            let call_ended = current.is_empty() && !previous.is_empty();

            if call_started && status == MeetingStatus::Idle {
                let cooled_down = last_prompt
                    .map(|t| t.elapsed() >= NOTIFY_COOLDOWN)
                    .unwrap_or(true);
                if cooled_down {
                    log::info!("Meeting detected: {} is using the mic", current[0]);
                    notify(
                        &app,
                        &format!(
                            "{} is using the mic. Record the meeting? Click the Lark menu bar icon.",
                            current[0]
                        ),
                    );
                    last_prompt = Some(Instant::now());
                }
            }

            if call_ended && status == MeetingStatus::Recording {
                log::info!("Meeting app released the mic while recording");
                notify(
                    &app,
                    "The call seems to have ended. Stop & transcribe from the Lark menu bar icon.",
                );
            }

            previous = current;
        }
    });
}

fn notify(app: &AppHandle, body: &str) {
    if let Err(e) = app.notification().builder().title("Lark").body(body).show() {
        log::warn!("Failed to show meeting notification: {e}");
    }
}

/// Friendly names of known meeting apps currently holding the microphone.
fn mic_using_meeting_apps(own_pid: i32) -> Vec<&'static str> {
    let Ok(processes) = ca::System::processes() else {
        return Vec::new();
    };

    let mut found: Vec<&'static str> = Vec::new();
    for process in processes {
        let Ok(pid) = process.pid() else { continue };
        if pid == own_pid {
            continue;
        }
        if !process.is_running_input().unwrap_or(false) {
            continue;
        }
        let Ok(bundle_id) = process.bundle_id() else {
            continue;
        };
        let bundle_id = bundle_id.to_string();

        match MEETING_APPS
            .iter()
            .find(|(prefix, _)| bundle_id.starts_with(prefix))
        {
            Some((_, name)) => {
                if !found.contains(name) {
                    found.push(name);
                }
            }
            // Log unknowns so the allowlist can grow from real usage.
            None => log::debug!("Mic in use by unrecognised app: {bundle_id}"),
        }
    }
    found
}
