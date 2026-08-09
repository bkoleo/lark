//! Guards the auto-paste against a moving target.
//!
//! Transcription takes a second or two, and in that gap it is easy to click
//! into another app. The paste then lands wherever the cursor happens to be —
//! at best in the wrong window, at worst nowhere at all, and the dictation
//! feels lost.
//!
//! So Lark remembers which app was frontmost when the recording stopped (that
//! is the app the user was talking into) and re-checks it at paste time. Same
//! app: paste, exactly as before. Different app: hold the text here and let
//! the overlay offer a Copy button instead.
//!
//! Fails open by design — an unknown "before" or "after" app always pastes.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

/// pid of the app that was frontmost when the last dictation stopped.
/// 0 means unknown, which is treated as "don't interfere".
static EXPECTED_TARGET_PID: AtomicI32 = AtomicI32::new(0);

/// Transcription that was withheld from the paste and is waiting for the user
/// to click Copy.
static PENDING_TEXT: Mutex<Option<String>> = Mutex::new(None);

/// pid of the frontmost application, via NSWorkspace.
///
/// Must be called on the main thread — NSWorkspace is not thread-safe. No new
/// dependency: the class is looked up at runtime, and AppKit is already linked
/// into the app.
#[cfg(target_os = "macos")]
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

#[cfg(not(target_os = "macos"))]
pub fn frontmost_app_pid() -> Option<i32> {
    None
}

/// Records the app the user is dictating into. Call on the main thread when a
/// recording stops, before transcription starts.
pub fn remember_target() {
    let pid = frontmost_app_pid().unwrap_or(0);
    EXPECTED_TARGET_PID.store(pid, Ordering::SeqCst);
    log::debug!("Paste target remembered: pid {pid}");
}

/// True when we know the user has moved to a different app since the recording
/// stopped. Call on the main thread, immediately before pasting.
pub fn target_changed() -> bool {
    let expected = EXPECTED_TARGET_PID.load(Ordering::SeqCst);
    if expected == 0 {
        return false; // never captured — don't interfere
    }
    match frontmost_app_pid() {
        Some(current) => current != expected,
        None => false, // can't tell — don't interfere
    }
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
