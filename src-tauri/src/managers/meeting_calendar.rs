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

    /// What the menu bar needs to know about the meeting coming up.
    #[derive(Debug, Clone, PartialEq)]
    pub struct UpcomingEvent {
        pub title: String,
        /// Seconds until it starts; zero or negative once it is under way.
        pub starts_in_secs: i64,
        /// Local start time, 24-hour, e.g. `14:30`.
        pub start_hm: String,
    }

    /// The state of the day ahead.
    ///
    /// `Clear` and `Unavailable` are deliberately distinct. Both would render as
    /// an empty menu bar, but only one of them can honestly claim the day is
    /// done — telling someone their meetings are over because we were never
    /// allowed to look is the one failure here that could actually cost them
    /// something.
    #[derive(Debug, Clone, PartialEq)]
    pub enum NextMeeting {
        /// Calendar readable, and something is still to come.
        Upcoming(UpcomingEvent),
        /// Calendar readable, nothing left today.
        Clear,
        /// No calendar access, so the day is genuinely unknown.
        Unavailable,
    }

    /// The next event still to come today — in progress or upcoming, whichever
    /// starts soonest.
    ///
    /// **Never prompts.** Unlike [`current_event`], this runs on a timer in the
    /// background rather than at a moment the user chose, so it only reads an
    /// access decision that has already been made. Asking here would pop the
    /// permission dialog unbidden, thirty seconds after launch, over whatever
    /// the user was actually doing.
    pub fn next_meeting() -> NextMeeting {
        unsafe {
            if !has_access() {
                return NextMeeting::Unavailable;
            }

            let store = EKEventStore::new();

            // Only today. "Nothing left today" is a meaningful, restful state;
            // showing tomorrow's 9am standup all evening is not.
            let start = NSDate::dateWithTimeIntervalSinceNow(-(GRACE.as_secs() as f64));
            let end = NSDate::dateWithTimeIntervalSinceNow(secs_until_local_midnight());

            let predicate =
                store.predicateForEventsWithStartDate_endDate_calendars(&start, &end, None);
            let events: objc2::rc::Retained<NSArray<objc2_event_kit::EKEvent>> =
                store.eventsMatchingPredicate(&predicate);

            let now_epoch = NSDate::now().timeIntervalSince1970();

            let mut best: Option<(f64, UpcomingEvent)> = None;
            for event in events.iter() {
                // An all-day banner is not a meeting, and a declined invite is
                // one the user has already said they are not attending.
                if event.isAllDay() || is_declined(&event) {
                    continue;
                }

                let title = {
                    let t: objc2::rc::Retained<NSString> = event.title();
                    let t = t.to_string();
                    if t.trim().is_empty() {
                        continue;
                    }
                    t.trim().to_string()
                };

                let start_epoch = event.startDate().timeIntervalSince1970();
                let end_epoch = event.endDate().timeIntervalSince1970();

                // Already finished — the grace window above can still catch one.
                if end_epoch < now_epoch {
                    continue;
                }

                if best.as_ref().map(|(s, _)| start_epoch < *s).unwrap_or(true) {
                    let Some(start_hm) = local_hm(start_epoch) else {
                        continue;
                    };
                    best = Some((
                        start_epoch,
                        UpcomingEvent {
                            title,
                            starts_in_secs: (start_epoch - now_epoch).round() as i64,
                            start_hm,
                        },
                    ));
                }
            }

            match best {
                Some((_, ev)) => NextMeeting::Upcoming(ev),
                None => NextMeeting::Clear,
            }
        }
    }

    /// True only when access is already granted. Reads the decision, never asks.
    unsafe fn has_access() -> bool {
        matches!(
            EKEventStore::authorizationStatusForEntityType(EKEntityType::Event),
            EKAuthorizationStatus::FullAccess
        )
    }

    /// Whether the user has declined this invite. EventKit reports the status of
    /// every participant, so we look for the one flagged as us.
    unsafe fn is_declined(event: &objc2_event_kit::EKEvent) -> bool {
        let Some(participants) = event.attendees() else {
            return false;
        };
        participants.iter().any(|p| {
            p.isCurrentUser()
                && p.participantStatus() == objc2_event_kit::EKParticipantStatus::Declined
        })
    }

    /// Seconds from now to the next local midnight.
    ///
    /// Derived from the wall clock rather than by constructing tomorrow's date,
    /// so a DST boundary shifts the answer by an hour instead of panicking on a
    /// local time that does not exist.
    fn secs_until_local_midnight() -> f64 {
        use chrono::Timelike;
        let now = chrono::Local::now();
        let elapsed = now.num_seconds_from_midnight() as f64;
        (86_400.0 - elapsed).max(60.0)
    }

    /// A Unix timestamp as local `HH:MM`.
    fn local_hm(epoch: f64) -> Option<String> {
        let dt = chrono::DateTime::from_timestamp(epoch as i64, 0)?;
        Some(dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
    }

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

        log::info!("Requesting calendar access (current status {})", status.0);

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
        let handler = block2::RcBlock::new(
            move |ok: objc2::runtime::Bool, err: *mut objc2_foundation::NSError| {
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
            },
        );
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

    #[derive(Debug, Clone, PartialEq)]
    pub struct UpcomingEvent {
        pub title: String,
        pub starts_in_secs: i64,
        pub start_hm: String,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum NextMeeting {
        Upcoming(UpcomingEvent),
        Clear,
        Unavailable,
    }

    pub fn next_meeting() -> NextMeeting {
        NextMeeting::Unavailable
    }
}

pub use imp::{current_event, next_meeting, CalendarContext, NextMeeting, UpcomingEvent};
