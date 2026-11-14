//! Remembering a "no" for longer than an instant (FR-MT-02c).
//!
//! Detection runs on a timer, so the state machine returning to Idle after a decline is not the
//! end of the story: the meeting app is still frontmost on the next tick, and the offer comes
//! straight back. Without this gate, "Not now" buys the user one second, and Stop is followed by
//! a fresh offer that starts recording again ten seconds later — the decline is real in the state
//! machine and meaningless in practice.
//!
//! Declines follow call presence, not application focus or a ten-minute app cooldown.
//! Strong identities distinguish back-to-back calls; a short floor absorbs signal flaps.
//! Decisions live only in memory and never change the per-app prompt setting.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum delay before a call-end signal can lift a decline.
pub const DECLINE_FLOOR_MS: i64 = 30_000;
/// Safety bound when no call-end signal ever arrives; longer than any normal call.
pub const DECLINE_MAX_MS: i64 = 2 * 60 * 60 * 1_000;
/// Room codes can recur. Keep one occurrence's answer for at most twelve hours.
pub const ROOM_DECISION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// In-memory record of what the user has said no to, and until when.
#[derive(Debug, Default, Clone)]
pub struct OfferGate {
    meetings: HashMap<String, (MeetingDecision, Instant)>,
    calls: HashMap<String, DeclinedCall>,
}

/// Presence evidence available for a declined call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSurface {
    Native,
    Meet,
    Huddle,
}

/// Snapshot retained independently of the recording lane, including after it returns to Idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclinedCall {
    pub identity: Option<String>,
    pub occurrence_id: Option<i64>,
    pub surface: CallSurface,
    declined_at: i64,
    mic_closed_since: Option<i64>,
    surface_lost_since: Option<i64>,
    ended: bool,
}

/// A tick's evidence; an unreadable surface is unknown, not absent.
pub struct CallPresence {
    pub surface_present: Option<bool>,
    pub mic_open: bool,
    pub app_running: bool,
    pub now: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingDecision {
    Missed,
    Declined,
    QuietExpired,
}

impl OfferGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn meeting_decision(&mut self, meeting: &str) -> Option<MeetingDecision> {
        if self
            .meetings
            .get(meeting)
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.meetings.remove(meeting);
        }
        self.meetings.get(meeting).map(|(decision, _)| *decision)
    }

    fn prune_meetings(&mut self) {
        self.meetings
            .retain(|_, (_, until)| Instant::now() < *until);
    }

    pub fn miss_meeting(&mut self, meeting: &str) {
        self.prune_meetings();
        self.meetings
            .entry(meeting.to_owned())
            .or_insert((MeetingDecision::Missed, Instant::now() + ROOM_DECISION_TTL));
    }

    pub fn decline_meeting(&mut self, meeting: &str) {
        self.prune_meetings();
        self.meetings.insert(
            meeting.to_owned(),
            (
                MeetingDecision::Declined,
                Instant::now() + ROOM_DECISION_TTL,
            ),
        );
    }

    pub fn expire_quiet_meeting(&mut self, meeting: &str) {
        self.prune_meetings();
        if let Some((decision, _)) = self.meetings.get_mut(meeting) {
            *decision = MeetingDecision::QuietExpired;
        }
    }

    #[cfg(test)]
    fn expire_meeting_for_test(&mut self, meeting: &str) {
        if let Some((_, until)) = self.meetings.get_mut(meeting) {
            *until = Instant::now();
        }
    }

    /// Decline an unidentified native call (also used by core-only callers).
    pub fn decline(&mut self, bundle_id: &str, now: i64) {
        self.decline_call(bundle_id, None, None, CallSurface::Native, now);
    }

    /// Record Not now, Stop, or sleep-end for this call, without changing settings.
    pub fn decline_call(
        &mut self,
        bundle_id: &str,
        identity: Option<&str>,
        occurrence_id: Option<i64>,
        surface: CallSurface,
        now: i64,
    ) {
        self.calls.insert(
            bundle_id.to_owned(),
            DeclinedCall {
                identity: identity.map(str::to_owned),
                occurrence_id,
                surface,
                declined_at: now,
                mic_closed_since: None,
                surface_lost_since: None,
                ended: false,
            },
        );
    }

    pub fn may_offer(&self, bundle_id: &str, now: i64) -> bool {
        self.may_offer_call(bundle_id, None, None, now)
    }

    /// A known different identity bypasses even the flap floor.
    pub fn may_offer_call(
        &self,
        bundle_id: &str,
        identity: Option<&str>,
        occurrence_id: Option<i64>,
        now: i64,
    ) -> bool {
        self.calls.get(bundle_id).is_none_or(|call| {
            let different = identity
                .zip(call.identity.as_deref())
                .is_some_and(|(new, old)| new != old)
                || occurrence_id
                    .zip(call.occurrence_id)
                    .is_some_and(|(new, old)| new != old);
            let age = now.saturating_sub(call.declined_at);
            // ponytail: if the end signal is never seen, the decline still lapses after two hours.
            different || (call.ended && age >= DECLINE_FLOOR_MS) || age >= DECLINE_MAX_MS
        })
    }

    /// Focus alone cannot prove that a call ended. Kept for core detection callers.
    pub fn observe_front(&mut self, _bundle_id: &str, _now: i64) {}

    pub fn declined_calls(&self) -> impl Iterator<Item = (&String, &DeclinedCall)> {
        self.calls.iter()
    }

    /// Apply evidence only to the decline that was snapshotted before native probing.
    pub fn observe_call(&mut self, bundle_id: &str, snapshot: &DeclinedCall, o: CallPresence) {
        let Some(call) = self.calls.get_mut(bundle_id) else {
            return;
        };
        if &*call != snapshot {
            return;
        }
        if o.mic_open {
            call.mic_closed_since = None;
        } else {
            call.mic_closed_since.get_or_insert(o.now);
        }
        match o.surface_present {
            Some(true) => call.surface_lost_since = None,
            Some(false) => {
                call.surface_lost_since.get_or_insert(o.now);
            }
            None => {}
        }
        let present = o.app_running
            && match call.surface {
                CallSurface::Meet => super::detect::meet_url_session_present(
                    call.surface_lost_since,
                    o.now,
                    o.mic_open,
                    call.mic_closed_since,
                ),
                CallSurface::Huddle => super::detect::huddle_session_present(
                    call.surface_lost_since,
                    o.now,
                    o.mic_open,
                    call.mic_closed_since,
                ),
                CallSurface::Native => {
                    o.mic_open
                        || call.mic_closed_since.is_none_or(|since| {
                            o.now.saturating_sub(since) < super::detect::MIC_QUIET_AFTER_URL_LEFT_MS
                        })
                }
            };
        // A signal that recovers during the floor was a flap, not a new call.
        call.ended = !present;
        if call.ended && o.now.saturating_sub(call.declined_at) >= DECLINE_FLOOR_MS {
            self.calls.remove(bundle_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZOOM: &str = "us.zoom.xos";

    #[test]
    fn meet_decline_survives_focus_but_does_not_silence_another_room() {
        let mut gate = OfferGate::new();
        gate.miss_meeting("abc-defg-hij");
        assert_eq!(
            gate.meeting_decision("abc-defg-hij"),
            Some(MeetingDecision::Missed)
        );
        gate.decline_meeting("abc-defg-hij");
        gate.observe_front("com.apple.Notes", 1_200_000);
        gate.miss_meeting("abc-defg-hij");
        assert_eq!(
            gate.meeting_decision("abc-defg-hij"),
            Some(MeetingDecision::Declined)
        );
        assert_eq!(gate.meeting_decision("xyz-abcd-efg"), None);
        assert!(gate.may_offer("com.google.Chrome", 1));
    }

    #[test]
    fn room_decision_expires_after_twelve_hours() {
        let mut gate = OfferGate::new();
        gate.decline_meeting("abc-defg-hij");
        assert_eq!(
            gate.meeting_decision("abc-defg-hij"),
            Some(MeetingDecision::Declined)
        );
        gate.expire_meeting_for_test("abc-defg-hij");
        assert_eq!(gate.meeting_decision("abc-defg-hij"), None);
    }

    #[test]
    fn quiet_expiry_is_distinct_from_an_explicit_decline() {
        let mut gate = OfferGate::new();
        gate.miss_meeting("abc-defg-hij");
        gate.expire_quiet_meeting("abc-defg-hij");
        assert_eq!(
            gate.meeting_decision("abc-defg-hij"),
            Some(MeetingDecision::QuietExpired)
        );
        gate.expire_meeting_for_test("abc-defg-hij");
        assert_eq!(gate.meeting_decision("abc-defg-hij"), None);
        gate.miss_meeting("abc-defg-hij");
        assert_eq!(
            gate.meeting_decision("abc-defg-hij"),
            Some(MeetingDecision::Missed)
        );
    }

    #[test]
    fn an_app_that_was_never_declined_may_be_offered() {
        assert!(OfferGate::new().may_offer(ZOOM, 1_000));
    }

    #[test]
    fn a_decline_survives_the_next_detection_tick() {
        // The bug this type exists to prevent: the state machine is back in Idle one second
        // later, the meeting app is still frontmost, and the offer returns as if nothing was
        // said. One second of "no" is not a "no" (FR-MT-02c).
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 1_000);

        gate.observe_front(ZOOM, 2_000);
        assert!(
            !gate.may_offer(ZOOM, 2_000),
            "one second later it must still be declined"
        );
    }

    #[test]
    fn declining_one_app_does_not_silence_another() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 1_000);

        assert!(gate.may_offer("com.microsoft.teams", 2_000));
    }

    fn observe(gate: &mut OfferGate, now: i64, mic_open: bool) {
        let snapshot = gate.calls[ZOOM].clone();
        gate.observe_call(
            ZOOM,
            &snapshot,
            CallPresence {
                surface_present: Some(true),
                mic_open,
                app_running: true,
                now,
            },
        );
    }

    #[test]
    fn ended_call_allows_next_call_in_same_app_within_ten_minutes() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 0);
        observe(&mut gate, 31_000, false);
        observe(&mut gate, 40_000, false);
        assert!(gate.may_offer(ZOOM, 41_000));
    }

    #[test]
    fn same_live_call_stays_declined_past_ten_minutes_and_focus_changes() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 0);
        gate.observe_front("com.apple.Notes", 1_000);
        gate.observe_front("com.apple.Notes", 90_000);
        observe(&mut gate, 700_000, true);
        assert!(!gate.may_offer(ZOOM, 700_000));
    }

    #[test]
    fn different_meet_identity_offers_immediately() {
        let mut gate = OfferGate::new();
        gate.decline_call(ZOOM, Some("meet:abc-defg-hij"), None, CallSurface::Meet, 0);
        assert!(!gate.may_offer_call(ZOOM, Some("meet:abc-defg-hij"), None, 1));
        assert!(gate.may_offer_call(ZOOM, Some("meet:xyz-abcd-efg"), None, 1));
    }

    #[test]
    fn different_calendar_occurrence_offers_immediately() {
        let mut gate = OfferGate::new();
        gate.decline_call(
            ZOOM,
            Some("same-meet-room"),
            Some(100),
            CallSurface::Native,
            0,
        );
        assert!(gate.may_offer_call(ZOOM, Some("same-meet-room"), Some(200), 1));
        assert!(!gate.may_offer_call(ZOOM, None, None, 1));
    }

    #[test]
    fn flap_within_thirty_seconds_does_not_offer() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 0);
        observe(&mut gate, 1_000, false);
        observe(&mut gate, 10_000, false);
        observe(&mut gate, 11_000, true);
        assert!(!gate.may_offer(ZOOM, DECLINE_FLOOR_MS - 1));
        assert!(!gate.may_offer(ZOOM, DECLINE_FLOOR_MS));
    }

    #[test]
    fn meet_and_huddle_end_on_surface_loss_and_mic_release() {
        for surface in [CallSurface::Meet, CallSurface::Huddle] {
            let mut gate = OfferGate::new();
            gate.decline_call(ZOOM, Some("room"), None, surface, 0);
            for now in [31_000, 60_000] {
                let snapshot = gate.calls[ZOOM].clone();
                gate.observe_call(
                    ZOOM,
                    &snapshot,
                    CallPresence {
                        surface_present: Some(false),
                        mic_open: true,
                        app_running: true,
                        now,
                    },
                );
                assert!(!gate.may_offer_call(ZOOM, Some("room"), None, now));
            }
            for now in [61_000, 70_000] {
                let snapshot = gate.calls[ZOOM].clone();
                gate.observe_call(
                    ZOOM,
                    &snapshot,
                    CallPresence {
                        surface_present: Some(false),
                        mic_open: false,
                        app_running: true,
                        now,
                    },
                );
            }
            assert!(gate.may_offer_call(ZOOM, Some("room"), None, 71_000));
        }
    }

    #[test]
    fn app_exit_lifts_decline_after_floor_even_if_mic_is_held_elsewhere() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 0);
        let snapshot = gate.calls[ZOOM].clone();
        gate.observe_call(
            ZOOM,
            &snapshot,
            CallPresence {
                surface_present: None,
                mic_open: true,
                app_running: false,
                now: 31_000,
            },
        );
        assert!(gate.may_offer(ZOOM, 31_000));
    }

    #[test]
    fn stale_presence_does_not_clear_a_replacement_decline() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 0);
        let snapshot = gate.calls[ZOOM].clone();
        gate.decline(ZOOM, 1);
        gate.observe_call(
            ZOOM,
            &snapshot,
            CallPresence {
                surface_present: Some(false),
                mic_open: false,
                app_running: false,
                now: 31_000,
            },
        );
        assert!(!gate.may_offer(ZOOM, 31_000));
    }

    #[test]
    fn a_momentary_flick_to_another_window_does_not_wipe_the_decline() {
        // Guards `observe_front` against keeping only the frontmost app's entry: glancing at
        // notes for one tick during a declined call would wipe the "no", and the return would
        // re-offer, with recording auto-starting ten seconds later.
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 1_000);

        gate.observe_front("com.apple.Notes", 2_000);
        gate.observe_front(ZOOM, 3_000);

        assert!(
            !gate.may_offer(ZOOM, 4_000),
            "a one-second glance elsewhere is not leaving"
        );
    }

    #[test]
    fn staying_in_the_app_does_not_clear_the_decline() {
        let mut gate = OfferGate::new();
        gate.decline(ZOOM, 1_000);

        for tick in 1..=30 {
            gate.observe_front(ZOOM, 1_000 + tick * 1_000);
            assert!(
                !gate.may_offer(ZOOM, 1_000 + tick * 1_000),
                "tick {tick} re-offered"
            );
        }
    }
}
