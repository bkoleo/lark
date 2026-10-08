//! Stops a quit from silently ending a meeting recording.
//!
//! On 2026-10-08 Lark was quit from the Dock fourteen minutes into a
//! job-interview recording — the third of three apps closed in five
//! seconds, so almost certainly a tidy-up that did not register Lark as
//! one of them. macOS delivered a Quit Apple Event, AppKit asked tao's
//! delegate `applicationShouldTerminate:` (tao does not implement it, so
//! AppKit took the default: yes), ran `applicationWillTerminate:`, and
//! called `exit(0)`. Nothing in Lark ran: no stop, no WAV finalise, no
//! transcript, no log line. The audio was recovered by hand.
//!
//! Two defences, one shared decision:
//!
//! 1. `applicationShouldTerminate:` is added to tao's delegate class at
//!    runtime (`class_addMethod`), so Dock quit, Cmd-Q and any Quit Apple
//!    Event reach [`allow_quit`] *before* anything is torn down.
//! 2. Tauri's own `RunEvent::ExitRequested` (tray Quit, `app.exit`) asks
//!    the same function.
//!
//! The decision: idle → quit. Recording → the first request is vetoed with
//! an alert sound and a card, and a second request within 60 s (or the
//! card's own Quit button) goes through. A logout, restart or shutdown is
//! never vetoed — blocking the session for a recording is the wrong
//! trade — but the exit path still finalises the WAVs on the way out
//! (`MeetingManager::finalize_for_exit`, wired in `lib.rs`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::managers::meeting::{MeetingManager, MeetingStatus};

/// How long a vetoed quit stays "armed": a second request inside this
/// window is taken as deliberate.
const ARM_WINDOW: Duration = Duration::from_secs(60);

struct Guard {
    app: AppHandle,
    armed_until: Mutex<Option<Instant>>,
}

static GUARD: OnceLock<Guard> = OnceLock::new();

/// Call once from `setup`, on the main thread, after the `MeetingManager`
/// is managed and after tao has set its application delegate (it has, by
/// the time `setup` runs).
pub fn install(app: &AppHandle) {
    let _ = GUARD.set(Guard {
        app: app.clone(),
        armed_until: Mutex::new(None),
    });
    // SAFETY: main thread, NSApp exists, and the delegate class is tao's
    // own — adding a method it does not define is exactly the extension
    // point the Objective-C runtime provides.
    unsafe { install_should_terminate() };
}

/// The card's Quit button: the user has said so explicitly, so the next
/// quit request goes straight through.
pub fn confirm() {
    if let Some(guard) = GUARD.get() {
        *guard.armed_until.lock().unwrap() = Some(Instant::now() + ARM_WINDOW);
    }
}

/// Whether a quit may proceed right now. `source` is only for the log —
/// the line that was missing on 2026-10-08.
pub fn allow_quit(source: &str) -> bool {
    let Some(guard) = GUARD.get() else {
        return true;
    };
    let manager = guard.app.try_state::<Arc<MeetingManager>>();
    let recording = manager
        .as_ref()
        .map(|m| m.status() == MeetingStatus::Recording)
        .unwrap_or(false);
    if !recording {
        log::info!("Quit requested ({source}) with no recording in progress — allowing");
        return true;
    }

    let mut armed = guard.armed_until.lock().unwrap();
    if armed.map(|until| Instant::now() < until).unwrap_or(false) {
        *armed = None;
        log::warn!(
            "Quit confirmed ({source}) while recording — allowing; the audio is finalised on exit and transcribed at the next launch"
        );
        return true;
    }
    *armed = Some(Instant::now() + ARM_WINDOW);
    drop(armed);

    log::warn!(
        "Quit vetoed ({source}): a meeting is recording. Quit again within {}s, or use the card's Quit, to really quit",
        ARM_WINDOW.as_secs()
    );
    crate::audio_feedback::play_alert("quit requested while recording");
    let title = manager.as_ref().and_then(|m| m.recording_calendar_title());
    crate::overlay::show_meeting_prompt(&guard.app, "quit", "", title.as_deref(), None, None);
    false
}

// ---- AppKit side --------------------------------------------------------

const NS_TERMINATE_CANCEL: usize = 0;
const NS_TERMINATE_NOW: usize = 1;

/// `kAEQuitReason` ('why?') and the values that mean the session itself is
/// ending, from AERegistry.h / AppleEvents.h.
const K_AE_QUIT_REASON: u32 = u32::from_be_bytes(*b"why?");
const SESSION_ENDING_REASONS: [(u32, &str); 6] = [
    (u32::from_be_bytes(*b"logo"), "logout"),
    (u32::from_be_bytes(*b"rlgo"), "logout"),
    (u32::from_be_bytes(*b"shut"), "shutdown"),
    (u32::from_be_bytes(*b"rest"), "restart"),
    (u32::from_be_bytes(*b"rsdn"), "shutdown"),
    (u32::from_be_bytes(*b"rrst"), "restart"),
];

unsafe fn install_should_terminate() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::{class, msg_send, sel};

    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    let delegate: *mut AnyObject = msg_send![app, delegate];
    if delegate.is_null() {
        log::warn!("Quit guard not installed: NSApp has no delegate yet");
        return;
    }
    let selector: Sel = sel!(applicationShouldTerminate:);
    let responds: bool = msg_send![delegate, respondsToSelector: selector];
    if responds {
        log::info!(
            "Quit guard not installed: the app delegate already implements applicationShouldTerminate:"
        );
        return;
    }
    let class: &AnyClass = (*delegate).class();
    let imp: Imp = std::mem::transmute(
        should_terminate
            as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
    );
    // Return NSUInteger (NSApplicationTerminateReply), args self, _cmd, sender.
    let added: Bool = objc2::ffi::class_addMethod(
        class as *const AnyClass as *mut AnyClass,
        selector,
        imp,
        c"Q@:@".as_ptr(),
    );
    if !added.as_bool() {
        log::warn!("Quit guard not installed: class_addMethod refused");
        return;
    }
    // AppKit notes which optional delegate methods exist when the delegate
    // is set; re-set it so the new one is seen.
    let _: () = msg_send![app, setDelegate: std::ptr::null_mut::<AnyObject>()];
    let _: () = msg_send![app, setDelegate: delegate];
    log::info!("Quit guard installed on {:?}", class.name());
}

unsafe extern "C-unwind" fn should_terminate(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) -> usize {
    let (source, session_ending) = quit_source();
    if session_ending {
        log::warn!("Quit requested by a {source} — allowing; any recording is finalised on exit");
        return NS_TERMINATE_NOW;
    }
    if allow_quit(&source) {
        NS_TERMINATE_NOW
    } else {
        NS_TERMINATE_CANCEL
    }
}

/// Names where the quit came from, and whether it is the session ending.
/// Inside a Quit Apple Event handler the event is current and may carry a
/// `kAEQuitReason`; a menu Quit / Cmd-Q has no current event at all.
unsafe fn quit_source() -> (String, bool) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let manager: *mut AnyObject = msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
    let event: *mut AnyObject = msg_send![manager, currentAppleEvent];
    if event.is_null() {
        return ("Cmd-Q or the application menu".to_string(), false);
    }
    let reason: *mut AnyObject = msg_send![event, attributeDescriptorForKeyword: K_AE_QUIT_REASON];
    if reason.is_null() {
        return (
            "a Quit Apple Event — the Dock, Activity Monitor or a script".to_string(),
            false,
        );
    }
    let code: u32 = msg_send![reason, enumCodeValue];
    match SESSION_ENDING_REASONS.iter().find(|(c, _)| *c == code) {
        Some((_, name)) => (name.to_string(), true),
        None => (
            format!(
                "a Quit Apple Event with reason '{}'",
                String::from_utf8_lossy(&code.to_be_bytes())
            ),
            false,
        ),
    }
}
