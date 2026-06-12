//! Meeting detection (macOS): watches which processes hold the microphone
//! via Core Audio process objects — the same signal that lights the orange
//! menu-bar dot. When a known meeting app grabs the mic, Lark shows a
//! Granola-style prompt in its own overlay pill ("Zoom call? Record");
//! when the app releases the mic during a recording, it offers to stop.
//!
//! The prompt lives in Lark's overlay rather than macOS notifications:
//! self-signed apps don't reliably register with Notification Center, and
//! the overlay needs no permission at all. Deliberately prompt-only:
//! Lark never starts or stops a meeting recording on its own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cidre::core_audio as ca;
use tauri::{AppHandle, Manager};

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

/// Executable-path fragments as a fallback: browser/app HELPER processes
/// are what actually hold the mic, and they often report no bundle id to
/// Core Audio at all (this is why anarlog resolves by path too — and why
/// Brave's test call was missed by bundle-id matching alone).
const MEETING_APP_PATHS: &[(&str, &str)] = &[
    ("zoom.us.app", "Zoom"),
    ("Microsoft Teams.app", "Teams"),
    ("FaceTime.app", "FaceTime"),
    ("avconferenced", "FaceTime"),
    ("Slack.app", "Slack"),
    ("Discord.app", "Discord"),
    ("Webex.app", "Webex"),
    ("Google Chrome", "Chrome"),
    ("Brave Browser", "Brave"),
    ("Safari", "Safari"),
    ("Arc.app", "Arc"),
    ("Microsoft Edge", "Edge"),
    ("Firefox", "Firefox"),
];

/// Executable path for a pid via libproc (part of libSystem, no extra
/// linking needed).
fn pid_executable_path(pid: i32) -> Option<String> {
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
    }
    let mut buf = vec![0u8; 4096];
    let len = unsafe { proc_pidpath(pid, buf.as_mut_ptr(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}

fn match_meeting_app(bundle_id: Option<&str>, path: Option<&str>) -> Option<&'static str> {
    if let Some(bid) = bundle_id {
        if let Some((_, name)) = MEETING_APPS.iter().find(|(prefix, _)| bid.starts_with(prefix)) {
            return Some(name);
        }
    }
    if let Some(path) = path {
        if let Some((_, name)) = MEETING_APP_PATHS
            .iter()
            .find(|(fragment, _)| path.contains(fragment))
        {
            return Some(name);
        }
    }
    None
}

pub fn spawn_meeting_detector(app_handle: &AppHandle) {
    let app = app_handle.clone();
    std::thread::spawn(move || {
        log::info!("Meeting detector started");
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
                    crate::overlay::show_meeting_prompt(&app, "start", current[0]);
                    last_prompt = Some(Instant::now());
                }
            }

            if call_ended && status == MeetingStatus::Recording {
                log::info!("Meeting app released the mic while recording");
                crate::overlay::show_meeting_prompt(&app, "stop", previous[0]);
            }

            previous = current;
        }
    });
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
        let bundle_id = process.bundle_id().ok().map(|s| s.to_string());
        let path = pid_executable_path(pid);

        match match_meeting_app(bundle_id.as_deref(), path.as_deref()) {
            Some(name) => {
                if !found.contains(&name) {
                    found.push(name);
                }
            }
            // Log unknowns (with path!) so the allowlist can grow from
            // real usage.
            None => log::debug!(
                "Mic in use by unmatched process: pid={pid} bundle={bundle_id:?} path={path:?}"
            ),
        }
    }
    found
}
