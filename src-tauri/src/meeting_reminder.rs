//! A heads-up card shortly before the next calendar event.
//!
//! The menu-bar countdown answers "when is my next meeting?" only when the
//! user thinks to look at it. This is the other half: one minute before an
//! event starts, the meeting card (the same top-right panel the call
//! detector uses) pops up with the event's title, so a meeting can't sneak
//! up on a full-screen window.
//!
//! Deliberate choices, all inherited from the features around it:
//! - **Not a macOS notification.** `tauri-plugin-notification` never
//!   registers a self-signed app with Notification Center — the request is a
//!   silent no-op — so the app's own always-on-top panel is the only surface
//!   that actually appears.
//! - **Reads the calendar, never asks.** Same rule as the menu-bar label:
//!   this runs on a timer, not at a moment the user chose, so it must never
//!   be the thing that pops the permission dialog. No access simply means no
//!   reminders.
//! - **Rides the tray tick's thread.** EventKit answers the first handful of
//!   stores a process creates and then returns empty lists forever (the
//!   2026-08-10 menu-bar bug), so lookups must come from a thread that keeps
//!   its store. Spawning a thread per reminder would walk straight back into
//!   that; the tray loop already owns a store and ticks often enough.

#[cfg(target_os = "macos")]
pub use imp::tick;

#[cfg(target_os = "macos")]
mod imp {
    use crate::managers::meeting_calendar::{next_meeting, NextMeeting};
    use crate::settings;
    use log::info;
    use std::sync::Mutex;
    use std::time::Duration;
    use tauri::AppHandle;

    /// The reminder never waits longer than one tray tick, so the label
    /// refresh sharing this thread is never stalled further than that.
    pub(super) const MAX_HOLD_SECS: i64 = crate::tray::MEETING_TICK_SECS as i64;

    /// Wildly large lead values are a settings-file typo, not a plan.
    const MAX_LEAD_MINUTES: u32 = 120;

    /// One reminder per event occurrence. Keyed on date + start minute +
    /// title so a rescheduled meeting earns a fresh reminder, a tick that
    /// re-sees the same event does not, and — the date's whole job — a daily
    /// recurring event is a new occurrence each day, not yesterday's key.
    static LAST_FIRED: Mutex<Option<String>> = Mutex::new(None);

    /// Called once per tray tick, on the tray tick's own thread.
    ///
    /// May sleep up to one tick (see [`fire_delay`]) so the card lands at the
    /// configured lead exactly, rather than wherever the tick boundary fell.
    pub fn tick(app: &AppHandle) {
        let minutes = settings::get_settings(app)
            .meeting_reminder_minutes
            .min(MAX_LEAD_MINUTES);
        if minutes == 0 {
            return;
        }

        let NextMeeting::Upcoming(event) = next_meeting() else {
            return;
        };

        let Some(hold) = fire_delay(event.starts_in_secs, i64::from(minutes) * 60) else {
            return;
        };

        // Key before the hold: the hold is part of delivering this reminder,
        // not a window in which to decide again.
        let key = format!(
            "{}@{} {}",
            event.title,
            chrono::Local::now().format("%Y-%m-%d"),
            event.start_hm
        );
        {
            let mut last = match LAST_FIRED.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if last.as_deref() == Some(key.as_str()) {
                return;
            }
            *last = Some(key);
        }

        if hold > 0 {
            std::thread::sleep(Duration::from_secs(hold as u64));
        }

        info!(
            "Meeting reminder: \"{}\" starts at {} — showing the card",
            event.title, event.start_hm
        );
        crate::overlay::show_meeting_upcoming(app, &event.title, minutes);
    }

    /// Seconds to hold before showing the card, or `None` for "not now".
    ///
    /// The tick lands wherever it lands, so a meeting first seen at 73
    /// seconds out is held for 13 and announced at 60 — the reminder means
    /// "one minute", not "somewhere inside the tick that straddled it". A
    /// meeting already inside the lead (Lark launched late, or an event
    /// created last-minute) is announced immediately; one already started is
    /// not — the menu bar's `now ·` has that covered, and a "starts soon"
    /// card after the start would be the card lying.
    pub(super) fn fire_delay(starts_in_secs: i64, lead_secs: i64) -> Option<i64> {
        if starts_in_secs <= 0 {
            return None;
        }
        let over = starts_in_secs - lead_secs;
        if over <= 0 {
            Some(0)
        } else if over <= MAX_HOLD_SECS {
            Some(over)
        } else {
            None
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::imp::{fire_delay, MAX_HOLD_SECS};

    #[test]
    fn holds_to_the_exact_lead_when_the_tick_straddles_it() {
        assert_eq!(fire_delay(60 + MAX_HOLD_SECS, 60), Some(MAX_HOLD_SECS));
        assert_eq!(fire_delay(73, 60), Some(13));
    }

    #[test]
    fn fires_immediately_once_inside_the_lead() {
        // Lark launched 40 seconds before the meeting: announce now.
        assert_eq!(fire_delay(40, 60), Some(0));
        assert_eq!(fire_delay(60, 60), Some(0));
    }

    #[test]
    fn stays_quiet_while_the_meeting_is_still_far_off() {
        assert_eq!(fire_delay(60 + MAX_HOLD_SECS + 1, 60), None);
        assert_eq!(fire_delay(3600, 60), None);
    }

    #[test]
    fn never_announces_a_meeting_that_already_started() {
        assert_eq!(fire_delay(0, 60), None);
        assert_eq!(fire_delay(-300, 60), None);
    }

    #[test]
    fn a_longer_lead_moves_the_window_with_it() {
        assert_eq!(fire_delay(5 * 60 + 4, 5 * 60), Some(4));
        assert_eq!(fire_delay(2 * 60, 5 * 60), Some(0));
    }
}
