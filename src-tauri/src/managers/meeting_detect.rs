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
use crate::managers::meeting_calendar::{self, CalendarContext};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Don't nag: at most one "record this?" prompt per this window.
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(300);
/// After a call ends, auto-stop the recording once this grace passes —
/// unless the meeting app re-grabs the mic (device switch, reconnect).
const AUTO_STOP_GRACE: Duration = Duration::from_secs(20);

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
        if let Some((_, name)) = MEETING_APPS
            .iter()
            .find(|(prefix, _)| bid.starts_with(prefix))
        {
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
        let mut call_end_grace: Option<Instant> = None;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            let current = mic_using_meeting_apps(own_pid);
            let status = app
                .try_state::<Arc<MeetingManager>>()
                .map(|m| m.status())
                .unwrap_or(MeetingStatus::Idle);

            let call_started = !current.is_empty() && previous.is_empty();
            let call_ended = current.is_empty() && !previous.is_empty();

            // Not gated on Idle: a back-to-back call often starts while the
            // previous meeting is still transcribing, and that was exactly
            // when the prompt went missing (2026-08-11).
            if call_started && status != MeetingStatus::Recording {
                log::info!("Meeting detected: {} is using the mic", current[0]);
                // Buffer from here whether or not the card is shown: the
                // rewind follows the call, not the prompt. Suppressed by the
                // cooldown it would be a buffer nobody could reach, and
                // gated on the user noticing the card it would be no use to
                // the user who didn't.
                if let Some(manager) = app.try_state::<Arc<MeetingManager>>() {
                    manager.inner().clone().start_standby(current[0]);
                }
                let cooled_down = last_prompt
                    .map(|t| t.elapsed() >= NOTIFY_COOLDOWN)
                    .unwrap_or(true);
                if cooled_down {
                    // Read-only EventKit lookup, same call `MeetingManager::start()`
                    // makes when the user actually clicks Record — local (no
                    // network), so no perceptible delay before the card pops.
                    // Recording hasn't started yet at this point, so there is
                    // no `MeetingManager`-held calendar to read instead.
                    let calendar = meeting_calendar::current_event();
                    let (title, time_range) = calendar_card_fields(calendar.as_ref());
                    crate::overlay::show_meeting_prompt(
                        &app,
                        "start",
                        current[0],
                        title.as_deref(),
                        time_range.as_deref(),
                        None,
                    );
                    last_prompt = Some(Instant::now());
                }
            }

            // The call is over and nobody pressed Record: the buffer was
            // never anything but a possibility, so it goes. Held any longer
            // it would be memory kept for a meeting that has finished, and
            // the next call's buffer starts clean.
            if call_ended && status != MeetingStatus::Recording {
                if let Some(manager) = app.try_state::<Arc<MeetingManager>>() {
                    manager.inner().clone().stop_standby("the call ended");
                }
            }

            if call_ended && status == MeetingStatus::Recording {
                log::info!(
                    "Meeting app released the mic while recording — auto-stop in {}s",
                    AUTO_STOP_GRACE.as_secs()
                );
                // Recording is already under way, so `MeetingManager` holds the
                // calendar match `start()` resolved — the same one the eventual
                // transcript will carry. Reading it here (rather than a second
                // EventKit query) means the "stop" card can never name a
                // different meeting than the one being recorded.
                let title = app
                    .try_state::<Arc<MeetingManager>>()
                    .and_then(|m| m.recording_calendar_title());
                crate::overlay::show_meeting_prompt(
                    &app,
                    "stop",
                    previous[0],
                    title.as_deref(),
                    None,
                    None,
                );
                call_end_grace = Some(Instant::now());
            }

            // Mic came back during the grace window: the call continues
            // (device switch / reconnect), cancel the pending auto-stop.
            if !current.is_empty() && call_end_grace.take().is_some() {
                if status == MeetingStatus::Recording {
                    log::info!("Call resumed — auto-stop cancelled");
                    crate::overlay::show_meeting_recording_indicator(&app);
                }
            }

            if let Some(ended_at) = call_end_grace {
                if status != MeetingStatus::Recording {
                    // User already stopped (card click, tray, CLI).
                    call_end_grace = None;
                } else if ended_at.elapsed() >= AUTO_STOP_GRACE {
                    call_end_grace = None;
                    log::info!("Auto-stopping meeting recording (call ended, no action taken)");
                    if let Some(manager) = app.try_state::<Arc<MeetingManager>>() {
                        let manager = manager.inner().clone();
                        manager.toggle();
                        crate::tray::update_tray_menu(
                            &app,
                            &crate::tray::TrayIconState::Idle,
                            None,
                        );
                    }
                }
            }

            previous = current;
        }
    });
}

/// The card's title + time-range text for a resolved calendar match, or
/// `(None, None)` when nothing matched — the card itself falls back to the
/// app name in that case, per the Granola-pop design
/// (`Lark/design/2026-08-08-meeting-card-granola-pop-mockup.html`).
fn calendar_card_fields(ctx: Option<&CalendarContext>) -> (Option<String>, Option<String>) {
    let Some(ctx) = ctx else {
        return (None, None);
    };
    let time_range = match (ctx.start_epoch, ctx.end_epoch) {
        (Some(s), Some(e)) => meeting_calendar::format_time_range(s, e),
        _ => None,
    };
    (ctx.title.clone(), time_range)
}

/// The microphone a meeting app on the current call is actually capturing
/// from: the input device the call will hear, whatever the pin says.
pub struct CallMic {
    pub device_name: String,
    pub app: &'static str,
}

/// Which input device is the in-progress call's meeting app using? Walks the
/// same Core Audio process objects as detection, then asks the mic-holding
/// process for its input-scope device list (kAudioProcessPropertyDevices) —
/// for browsers that is the HELPER process, which the path fallback in
/// `match_meeting_app` already covers. Virtual devices (Teams/Zoom loopbacks,
/// Descript/Loom recorders, Lark's own tap) are skipped: recording one would
/// capture the call's output, not Kole. Returns the first real device found.
pub fn call_mic(own_pid: i32) -> Option<CallMic> {
    let processes = ca::System::processes().ok()?;
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
        let Some(app) = match_meeting_app(bundle_id.as_deref(), path.as_deref()) else {
            continue;
        };
        let devices: Vec<ca::Device> = process
            .prop_vec(&ca::PropSelector::PROCESS_DEVICES.input_addr())
            .unwrap_or_default();
        for device in devices {
            let Ok(name) = device.name() else { continue };
            let name = name.to_string();
            let transport = device
                .transport_type()
                .unwrap_or(ca::DeviceTransportType::UNKNOWN);
            if transport == ca::DeviceTransportType::VIRTUAL {
                log::debug!("Call mic for {app}: skipping virtual input {name:?}");
                continue;
            }
            return Some(CallMic {
                device_name: name,
                app,
            });
        }
    }
    None
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
