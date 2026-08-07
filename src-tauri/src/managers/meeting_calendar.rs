//! Calendar lookup for meeting mode.
//!
//! A Lark transcript on its own is called `2026-08-01 0930 Meeting` and knows
//! nothing about who was on the call. That is enough for a human browsing the
//! Meetings tab and nowhere near enough for the downstream agents: the Meeting
//! Digest decides which meetings *qualify* from the participant list and titles
//! each recap, which is exactly the metadata Granola supplied and Lark did not.
//!
//! So at the moment a recording starts we ask EventKit for the calendar event
//! covering right now, and carry its title and attendees into the transcript's
//! frontmatter.
//!
//! Deliberate choices:
//! - **Read-only, and never blocking.** No calendar event means no metadata and
//!   a transcript that looks exactly like today's. A meeting recording must
//!   never fail because the calendar was unavailable or permission was denied.
//! - **Pure-Rust bindings** (`objc2-event-kit`). A Swift bridge would need the
//!   full Xcode that already forced Apple Intelligence to be stubbed out.
//! - **Permission is requested, not assumed.** First run shows the macOS
//!   "Lark would like to access your calendar" prompt; a denial is cached by
//!   the OS and simply yields `None` from then on.
//!
//! Two things have to be in the bundle for that prompt to appear at all, and
//! only one of them is obvious: the `NSCalendars*UsageDescription` strings in
//! Info.plist, **and** `com.apple.security.personal-information.calendars` in
//! Entitlements.plist. The app runs under the Hardened Runtime, which refuses
//! the personal-information resources in-process when the entitlement is
//! absent — instantly, with no dialog and nothing in tccd's log to explain it.

#[cfg(target_os = "macos")]
mod imp {
    use objc2_event_kit::{EKAuthorizationStatus, EKEntityType, EKEventStore};
    use objc2_foundation::{NSArray, NSDate, NSString};
    use std::time::Duration;

    /// What the calendar knows about the meeting being recorded.
    #[derive(Debug, Clone, Default)]
    pub struct CalendarContext {
        pub title: Option<String>,
        /// Display names of attendees, organiser first where known. Empty when
        /// the event is a solo block.
        pub attendees: Vec<String>,
    }

    impl CalendarContext {
        pub fn is_empty(&self) -> bool {
            self.title.is_none() && self.attendees.is_empty()
        }
    }

    /// The event covering `now`, if any. Returns `None` on denied permission,
    /// no matching event, or any failure — all three are the same to a caller
    /// that must not care.
    pub fn current_event() -> Option<CalendarContext> {
        // Safety: every call below is a standard EventKit read on the calling
        // thread. The store is created and dropped here, so nothing outlives
        // this function.
        unsafe {
            let store = EKEventStore::new();

            if !ensure_access(&store) {
                return None;
            }

            // Widen the window slightly: people start recording a minute or two
            // after the event begins, and calendars are not precise about ends.
            let now = NSDate::now();
            let start = NSDate::dateWithTimeIntervalSinceNow(-(GRACE.as_secs() as f64));
            let end = NSDate::dateWithTimeIntervalSinceNow(GRACE.as_secs() as f64);

            let predicate =
                store.predicateForEventsWithStartDate_endDate_calendars(&start, &end, None);
            let events: objc2::rc::Retained<NSArray<objc2_event_kit::EKEvent>> =
                store.eventsMatchingPredicate(&predicate);

            // Prefer a genuine meeting: an event actually in progress, with
            // other people on it, over an all-day banner or a solo focus block.
            let mut best: Option<(u8, CalendarContext)> = None;
            for event in events.iter() {
                if event.isAllDay() {
                    continue;
                }
                let title: Option<String> = {
                    let t: objc2::rc::Retained<NSString> = event.title();
                    let t = t.to_string();
                    if t.trim().is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                };

                let mut attendees: Vec<String> = Vec::new();
                if let Some(participants) = event.attendees() {
                    for p in participants.iter() {
                        if let Some(name) = p.name() {
                            let name = name.to_string();
                            if !name.trim().is_empty() {
                                attendees.push(name);
                            }
                        }
                    }
                }

                let in_progress = {
                    let s = event.startDate();
                    let e = event.endDate();
                    s.timeIntervalSinceDate(&now) <= 0.0 && e.timeIntervalSinceDate(&now) >= 0.0
                };

                // Higher is better.
                let score = match (in_progress, attendees.len() > 1) {
                    (true, true) => 3,
                    (true, false) => 2,
                    (false, true) => 1,
                    (false, false) => 0,
                };

                let ctx = CalendarContext { title, attendees };
                if ctx.is_empty() {
                    continue;
                }
                if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    best = Some((score, ctx));
                }
            }

            best.map(|(_, ctx)| ctx)
        }
    }

    /// ±10 minutes around now. Wide enough for a late start, narrow enough not
    /// to grab the next meeting on a back-to-back day.
    const GRACE: Duration = Duration::from_secs(10 * 60);

    /// Returns true once the app holds calendar read access.
    ///
    /// The request is asynchronous; we wait briefly for the user's answer the
    /// first time and give up rather than hold a recording hostage to a dialog.
    /// macOS remembers the decision, so this only ever costs the first run.
    unsafe fn ensure_access(store: &EKEventStore) -> bool {
        // Values, so nobody has to re-derive them from the deprecation notice:
        // NotDetermined 0, Restricted 1, Denied 2, FullAccess 3, WriteOnly 4.
        // `Authorized` is the pre-macOS-14 spelling of `FullAccess` and really
        // does share its discriminant (3) — matching both would be an
        // unreachable arm, not safety. `WriteOnly` (4) is not enough for us:
        // it permits saving new events, never reading the one we want to name
        // the recording after, so it falls through to the request below.
        let status = EKEventStore::authorizationStatusForEntityType(EKEntityType::Event);
        match status {
            EKAuthorizationStatus::FullAccess => return true,
            EKAuthorizationStatus::Denied | EKAuthorizationStatus::Restricted => {
                log::warn!(
                    "Calendar access refused by macOS (status {}) — recording without a title. \
                     Grant it in System Settings > Privacy & Security > Calendars.",
                    status.0
                );
                return false;
            }
            _ => {}
        }

        log::info!(
            "Requesting calendar access (current status {})",
            status.0
        );

        use std::sync::{Arc, Mutex};
        // The completion block hands back *why*, not just *whether*. A silent
        // "not granted" cost six days of assuming the user had ignored a dialog
        // that macOS never showed: the app was missing the
        // `com.apple.security.personal-information.calendars` entitlement, and
        // under the Hardened Runtime that is refused in-process before TCC is
        // consulted — no prompt, no tccd log line, no TCC.db row. The NSError
        // is the only place that distinguishes it from a real user denial.
        let outcome = Arc::new(Mutex::new(None::<(bool, Option<String>)>));
        let sink = outcome.clone();
        let handler = block2::RcBlock::new(move |ok: objc2::runtime::Bool, err: *mut objc2_foundation::NSError| {
            let detail = if err.is_null() {
                None
            } else {
                let err = &*err;
                Some(format!(
                    "{} {}: {}",
                    err.domain(),
                    err.code(),
                    err.localizedDescription()
                ))
            };
            *sink.lock().unwrap() = Some((ok.as_bool(), detail));
        });
        store.requestFullAccessToEventsWithCompletion(block2::RcBlock::as_ptr(&handler));
        // Deliberately leaked. If the user leaves the permission dialog sitting
        // for longer than the poll below waits, dropping the block would free it
        // while EventKit still holds the pointer — a use-after-free on an answer
        // that arrives late. One small one-off allocation is the cheap side.
        std::mem::forget(handler);

        // Poll rather than block the run loop; the recording thread is not the
        // UI thread, so there is nothing to pump here.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let answer = outcome.lock().unwrap().clone();
            if let Some((ok, detail)) = answer {
                if !ok {
                    log::warn!(
                        "Calendar access not granted — recording without a title. EventKit said: {}",
                        detail.as_deref().unwrap_or("no error returned")
                    );
                }
                return ok;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::info!("Calendar permission prompt unanswered — recording without a title");
        false
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    #[derive(Debug, Clone, Default)]
    pub struct CalendarContext {
        pub title: Option<String>,
        pub attendees: Vec<String>,
    }

    impl CalendarContext {
        pub fn is_empty(&self) -> bool {
            true
        }
    }

    pub fn current_event() -> Option<CalendarContext> {
        None
    }
}

pub use imp::{current_event, CalendarContext};
