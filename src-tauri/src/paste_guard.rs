//! Guards the auto-paste against a moving target.
//!
//! Transcription takes a second or two, and in that gap it is easy to get
//! impatient and click somewhere else. The paste then lands wherever the
//! caret now is — at best the wrong window, at worst nowhere, and the
//! dictation feels lost.
//!
//! So Lark watches for the user moving between the moment the recording stops
//! and the moment the text is ready. Two signals, either of which counts:
//!
//! - **A mouse click.** macOS keeps a per-session counter of hardware mouse
//!   events; comparing it across the gap catches a click anywhere, including
//!   one inside the same app that a frontmost-app check cannot see. This is
//!   the signal that matters in practice.
//! - **A different frontmost app**, which catches a keyboard-only switch
//!   (Cmd+Tab) that produces no click.
//!
//! Either one and the paste is withheld: the overlay becomes a Copy button
//! instead. Fails open by design — anything we cannot measure pastes as before.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;

/// pid of the app that was frontmost when the current dictation started.
/// 0 means unknown, which is treated as "don't interfere".
static EXPECTED_TARGET_PID: AtomicI32 = AtomicI32::new(0);

/// Hardware mouse-click count sampled when the recording stopped.
/// u32::MAX means unsampled.
static CLICKS_AT_STOP: AtomicU32 = AtomicU32::new(u32::MAX);

/// Transcription that was withheld from the paste and is waiting for the user
/// to click Copy.
static PENDING_TEXT: Mutex<Option<String>> = Mutex::new(None);

#[cfg(target_os = "macos")]
mod sys {
    // CGEventSourceStateID
    pub const HID_SYSTEM_STATE: i32 = 1;
    // CGEventType
    const LEFT_MOUSE_DOWN: u32 = 1;
    const RIGHT_MOUSE_DOWN: u32 = 3;
    const OTHER_MOUSE_DOWN: u32 = 25;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceCounterForEventType(state_id: i32, event_type: u32) -> u32;
    }

    /// Running count of physical mouse-button presses this login session.
    /// HID state rather than the combined session state, so synthetic clicks
    /// posted by other software don't register as the user moving.
    pub fn mouse_click_count() -> u32 {
        unsafe {
            CGEventSourceCounterForEventType(HID_SYSTEM_STATE, LEFT_MOUSE_DOWN)
                .wrapping_add(CGEventSourceCounterForEventType(
                    HID_SYSTEM_STATE,
                    RIGHT_MOUSE_DOWN,
                ))
                .wrapping_add(CGEventSourceCounterForEventType(
                    HID_SYSTEM_STATE,
                    OTHER_MOUSE_DOWN,
                ))
        }
    }

    /// pid of the frontmost application, via NSWorkspace.
    ///
    /// Must be called on the main thread — NSWorkspace is not thread-safe. No
    /// new dependency: the class is looked up at runtime, and AppKit is
    /// already linked into the app.
    pub fn frontmost_app_pid() -> Option<i32> {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};

        unsafe {
            let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            if workspace.is_null() {
                return None;
            }
            let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
            if app.is_null() {
                return None;
            }
            let pid: i32 = msg_send![app, processIdentifier];
            if pid <= 0 {
                None
            } else {
                Some(pid)
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod sys {
    pub fn mouse_click_count() -> u32 {
        u32::MAX
    }
    pub fn frontmost_app_pid() -> Option<i32> {
        None
    }
}

/// Records the app the user is dictating into. Call on the main thread when a
/// recording *starts* — the hop has whole seconds of speech to complete in,
/// where a hop queued at stop can land after the user has already moved and
/// record the wrong app.
pub fn remember_target() {
    let pid = sys::frontmost_app_pid().unwrap_or(0);
    EXPECTED_TARGET_PID.store(pid, Ordering::SeqCst);
    log::debug!("Paste target remembered: pid {pid}");
}

/// Opens the watch window. Call when the recording stops; safe from any
/// thread.
pub fn arm() {
    CLICKS_AT_STOP.store(sys::mouse_click_count(), Ordering::SeqCst);
}

/// True when the user moved while the transcription was running. Call on the
/// main thread, immediately before pasting.
pub fn user_moved() -> bool {
    let clicks_before = CLICKS_AT_STOP.load(Ordering::SeqCst);
    let clicks_now = sys::mouse_click_count();
    let clicked =
        clicks_before != u32::MAX && clicks_now != u32::MAX && clicks_now != clicks_before;

    let expected_pid = EXPECTED_TARGET_PID.load(Ordering::SeqCst);
    let current_pid = sys::frontmost_app_pid();
    let switched_app = expected_pid != 0 && current_pid.is_some_and(|pid| pid != expected_pid);

    log::debug!(
        "Paste check: app {expected_pid} -> {current_pid:?} (switched: {switched_app}), \
         clicks {clicks_before} -> {clicks_now} (clicked: {clicked})"
    );

    clicked || switched_app
}

/// Holds text that was not pasted, until the user copies it or a new dictation
/// replaces it.
pub fn set_pending(text: String) {
    *PENDING_TEXT.lock().unwrap() = Some(text);
}

pub fn take_pending() -> Option<String> {
    PENDING_TEXT.lock().unwrap().take()
}

pub fn clear_pending() {
    *PENDING_TEXT.lock().unwrap() = None;
}
