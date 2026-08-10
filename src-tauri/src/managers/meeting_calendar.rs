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
    use objc2::rc::{autoreleasepool, Retained};
    use objc2_event_kit::{EKAuthorizationStatus, EKEntityType, EKEventStore};
    use objc2_foundation::{NSArray, NSDate, NSString};
    use std::cell::OnceCell;
    use std::time::Duration;

    thread_local! {
        /// One EventKit store for the life of this thread. See [`with_store`].
        static STORE: OnceCell<Retained<EKEventStore>> = const { OnceCell::new() };
    }

    /// Run `f` against this thread's calendar store, freshened first.
    ///
    /// **Holding one store is the fix for a real bug, not a micro-optimisation.**
    /// Building a throw-away `EKEventStore` per lookup made the menu bar go
    /// blind: EventKit served the first ten stores a process created and then
    /// returned an **empty event list** — not an error — to every one after
    /// that. The tray timer burns one every 30 seconds, so the label counted
    /// down correctly for exactly five minutes after each launch and then
    /// announced "Meetings done" for the rest of the day. Observed five times
    /// over on 2026-08-10, each run failing on the eleventh tick to the second,
    /// with a real 09:00 standup on the calendar throughout. The reason it
    /// surfaced as a confident wrong answer rather than a visible failure is
    /// that an empty list is exactly what a genuinely clear diary looks like.
    ///
    /// Two things went wrong together and both are fixed here. The stores were
    /// never actually being freed — this thread has no autorelease pool, so
    /// every object EventKit autoreleased internally leaked and each store kept
    /// its connection to `CalendarAgent` open forever, until the process hit the
    /// per-app connection ceiling. Hence both the single store *and* the
    /// [`autoreleasepool`] wrapping every lookup below. Apple's own guidance
    /// arrives at the same place from the other direction: create one store and
    /// keep it for the lifetime of the app.
    ///
    /// `reset()` is what makes a long-lived store safe to hold. A store serves
    /// the calendar state it has already loaded, so without it a meeting added
    /// mid-morning would never appear. It invalidates every object previously
    /// fetched through the store, so nothing may be held across calls — nothing
    /// is; each lookup copies out plain Rust values before returning. It resets
    /// the *data*, not the connection, which is why it does not walk back into
    /// the exhaustion above.
    ///
    /// Thread-local rather than one global because `Retained` is not `Sync`, and
    /// per-thread is bounded anyway: the tray timer and the recording path are
    /// the only callers.
    fn with_store<T>(f: impl FnOnce(&EKEventStore) -> T) -> T {
        STORE.with(|cell| {
            let store = cell.get_or_init(|| {
                log::info!("Opening the calendar store (once per thread, kept open)");
                // Safety: a plain EventKit allocation, kept alive by the cell.
                unsafe { EKEventStore::new() }
            });
            // Safety: no object fetched through the store outlives a lookup.
            unsafe { store.reset() };
            autoreleasepool(|_| f(store))
        })
    }

    /// What the calendar knows about the meeting being recorded.
    #[derive(Debug, Clone, Default)]
    pub struct CalendarContext {
        pub title: Option<String>,
        /// Display names of attendees, organiser first where known. Empty when
        /// the event is a solo block.
        pub attendees: Vec<String>,
        /// Event start/end as Unix timestamps, for the card's time-range row.
        /// `None` alongside a `Some` title is possible (title known, either
        /// bound missing) — `format_time_range` needs both, so the card falls
        /// back to the app name rather than a half-formed range.
        pub start_epoch: Option<f64>,
        pub end_epoch: Option<f64>,
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
        // thread, and nothing fetched through the store outlives this function.
        with_store(|store| unsafe {
            if !ensure_access(store) {
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

                let start_date = event.startDate();
                let end_date = event.endDate();
                let in_progress = {
                    start_date.timeIntervalSinceDate(&now) <= 0.0
                        && end_date.timeIntervalSinceDate(&now) >= 0.0
                };

                // Higher is better.
                let score = match (in_progress, attendees.len() > 1) {
                    (true, true) => 3,
                    (true, false) => 2,
                    (false, true) => 1,
                    (false, false) => 0,
                };

                let ctx = CalendarContext {
                    title,
                    attendees,
                    start_epoch: Some(start_date.timeIntervalSince1970()),
                    end_epoch: Some(end_date.timeIntervalSince1970()),
                };
                if ctx.is_empty() {
                    continue;
                }
                if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    best = Some((score, ctx));
                }
            }

            best.map(|(_, ctx)| ctx)
        })
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
        // Safety as in `current_event`: reads only, nothing outlives the call.
        if !unsafe { has_access() } {
            return NextMeeting::Unavailable;
        }

        with_store(|store| unsafe {
            // Only today. "Nothing left today" is a meaningful, restful state;
            // showing tomorrow's 9am standup all evening is not.
            let start = NSDate::dateWithTimeIntervalSinceNow(-(GRACE.as_secs() as f64));
            let end = NSDate::dateWithTimeIntervalSinceNow(secs_until_local_midnight());

            let predicate =
                store.predicateForEventsWithStartDate_endDate_calendars(&start, &end, None);
            let events: objc2::rc::Retained<NSArray<objc2_event_kit::EKEvent>> =
                store.eventsMatchingPredicate(&predicate);

            log_event_count(events.iter().count());

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
        })
    }

    /// How many events the store handed back, before any filtering.
    ///
    /// This is the tripwire for the exhaustion bug described on [`with_store`],
    /// and it exists because the bug is otherwise unfalsifiable: a broken
    /// lookup and a genuinely clear afternoon produce the identical menu bar
    /// label, so "Meetings done" can never confirm the fix on its own. The raw
    /// count can. It counts what the predicate returned rather than what
    /// survived the filters, so all-day holiday banners and birthdays keep it
    /// above zero on days with no meetings at all — a store that has stopped
    /// answering drops it to zero and pins it there.
    ///
    /// Logged only when the number moves, which is a handful of lines a day.
    fn log_event_count(count: usize) {
        use std::sync::Mutex;
        static LAST: Mutex<Option<usize>> = Mutex::new(None);

        let mut last = match LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *last == Some(count) {
            return;
        }
        *last = Some(count);
        log::debug!("Calendar returned {} events for the rest of today", count);
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
        pub start_epoch: Option<f64>,
        pub end_epoch: Option<f64>,
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

/// "1:00 PM – 2:00 PM" — 12-hour, no leading zero on the hour, en dash.
/// Matches the Granola-pop card's time row exactly (design ref:
/// `Lark/design/2026-08-08-meeting-card-granola-pop-mockup.html`).
///
/// Pure formatting, no EventKit — same split as `next_meeting()`'s own
/// `local_hm` (SKILL.md's "make Lark log the value" pattern applies here
/// too: this is the one piece of the calendar path unit-testable without a
/// live calendar or a macOS build).
pub fn format_time_range(start_epoch: f64, end_epoch: f64) -> Option<String> {
    let start = local_h12(start_epoch)?;
    let end = local_h12(end_epoch)?;
    Some(format!("{start} – {end}"))
}

fn local_h12(epoch: f64) -> Option<String> {
    let dt = chrono::DateTime::from_timestamp(epoch as i64, 0)?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%-I:%M %p")
            .to_string(),
    )
}

#[cfg(test)]
mod format_tests {
    use super::format_time_range;

    #[test]
    fn formats_a_one_hour_range() {
        // 2026-08-08 13:00:00 UTC and 14:00:00 UTC — assert only that both
        // halves parse and land either side of an en dash; the actual
        // AM/PM text depends on the machine's local timezone, which this
        // test must not assume.
        let start = 1786280400.0_f64; // 2026-08-08 13:00:00 UTC
        let end = 1786284000.0_f64; // 2026-08-08 14:00:00 UTC
        let out = format_time_range(start, end).expect("both bounds present");
        let parts: Vec<&str> = out.split(" – ").collect();
        assert_eq!(
            parts.len(),
            2,
            "expected exactly one en-dash-separated range: {out}"
        );
        assert!(parts[0].contains(':') && (parts[0].contains("AM") || parts[0].contains("PM")));
        assert!(parts[1].contains(':') && (parts[1].contains("AM") || parts[1].contains("PM")));
    }

    #[test]
    fn same_instant_still_formats_both_sides() {
        let t = 1786280400.0_f64;
        assert!(format_time_range(t, t).is_some());
    }
}
