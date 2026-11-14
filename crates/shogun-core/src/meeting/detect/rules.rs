//! Detection policy, microphone observation, and active-session lifecycle rules.

use super::matching::host_of;
use super::{
    bundle_hint, decide, is_media_url, is_meeting_app, is_meeting_url, is_suppressed_title,
    title_hint, Decision, MeetingHint, Signals,
};

/// Adapter-side facts the pure detector needs to apply product policy (FR-MT-04).
#[derive(Debug, Clone, Copy, Default)]
pub struct DetectionCtx<'a> {
    /// Frontmost app is a browser (Chrome, Safari, Arc, …).
    pub is_browser: bool,
    /// Parsed host of the frontmost browser tab, when a URL was read.
    pub page_host: Option<&'a str>,
    /// A **Strong** meeting URL is open (`meet.google.com`, `app.zoom.us`).
    pub has_meet_url: bool,
    /// A **Strong** meeting app (native Zoom) is frontmost.
    pub has_strong_bundle: bool,
    /// A **Weak** meeting surface is frontmost — a Teams/Webex bundle or a Weak host.
    /// One corroborating vote, never an opener: the adapter must *not* also fold this into
    /// [`Signals::meeting_app_frontmost`], or one observation would count twice.
    pub has_weak_meeting_signal: bool,
    /// Slack's window/AX text looks like a huddle in progress (see [`huddle_hint`]).
    /// One corroborating vote, exactly like a Weak bundle — alone it does nothing, and with
    /// sustained mic use it produces an offer, never an auto-start.
    pub has_huddle_hint: bool,
    /// Focused window title, when AX returned one.
    pub window_title: Option<&'a str>,
}

/// User settings that gate how aggressively mic-only evidence may open an offer.
#[derive(Debug, Clone, Copy, Default)]
pub struct OfferPolicy {
    /// When `false`, sustained mic use alone never opens an interval.
    pub allow_mic_only: bool,
}

/// Count how many independent signals fired (calendar included when present). A Weak meeting
/// surface and a huddle hint are each **one** vote: they can corroborate a mic or
/// controls signal into an offer, but two votes are needed and neither is ever an opener.
fn corroborating_count(signals: &Signals, ctx: &DetectionCtx<'_>) -> usize {
    usize::from(signals.mic_in_use)
        + usize::from(signals.meeting_app_frontmost)
        + usize::from(signals.meeting_controls_visible)
        + usize::from(signals.occurrence_now)
        + usize::from(ctx.has_weak_meeting_signal)
        + usize::from(ctx.has_huddle_hint)
}

/// Whether the observation already proves a meeting surface that exists only for meetings
/// (a Strong URL or the native Zoom app). **Strong only** — a Weak surface or a huddle hint
/// must corroborate, never open.
fn has_strong_opener(ctx: &DetectionCtx<'_>) -> bool {
    ctx.has_meet_url || ctx.has_strong_bundle
}

/// Browser tab with no readable host — cannot prove a meeting unless another product-specific
/// Weak signal (for example an exact Zoom/Meet/Teams title) corroborates the sustained mic.
/// Generic titles never set that signal; PiP/media titles are rejected before this policy runs.
fn browser_lacks_meeting_proof(ctx: &DetectionCtx<'_>) -> bool {
    ctx.is_browser
        && !ctx.has_meet_url
        && !ctx.has_weak_meeting_signal
        && ctx.page_host.is_none_or(str::is_empty)
}

/// Apply FR-MT-04 policy on top of raw signals, then score.
///
/// Biases toward fewer false positives: mic-only is opt-in, PiP/media titles are dropped,
/// browsers with an empty host cannot corroborate, and non-URL offers need two agreeing signals
/// unless Meet or Zoom is already proven. A Weak surface or huddle hint (Plan A) is one of those
/// two signals — Teams frontmost plus sustained mic offers; Teams frontmost alone never does.
pub fn evaluate_offer(signals: &Signals, ctx: &DetectionCtx<'_>, policy: &OfferPolicy) -> Decision {
    if ctx.window_title.is_some_and(is_suppressed_title) {
        return Decision::Ignore;
    }

    let mut effective = *signals;

    if browser_lacks_meeting_proof(ctx) {
        effective.mic_in_use = false;
        effective.meeting_app_frontmost = false;
    }

    // A Weak surface or a huddle hint corroborates the mic, so the mic is no longer "only" —
    // without this the mic-only opt-in gate would zero the very signal a Weak vote
    // pairs with, and Teams-plus-sustained-mic could never offer under the default policy.
    let mic_is_only_opener = effective.mic_in_use
        && !effective.meeting_app_frontmost
        && !effective.meeting_controls_visible
        && !ctx.has_weak_meeting_signal
        && !ctx.has_huddle_hint;
    if mic_is_only_opener && !policy.allow_mic_only {
        effective.mic_in_use = false;
    }

    let d = decide(&effective);
    let Decision::Offer {
        confidence,
        provenance,
    } = d
    else {
        return d;
    };

    if !has_strong_opener(ctx) && corroborating_count(&effective, ctx) < 2 {
        let mic_only_allowed = policy.allow_mic_only
            && effective.mic_in_use
            && !effective.meeting_app_frontmost
            && !effective.meeting_controls_visible;
        if !mic_only_allowed {
            return Decision::Ignore;
        }
    }

    let mut ctx_evidence: Vec<&str> = Vec::new();
    if ctx.has_weak_meeting_signal {
        ctx_evidence.push("weak_meeting_signal");
    }
    if ctx.has_huddle_hint {
        ctx_evidence.push("huddle_hint");
    }
    let provenance = if ctx_evidence.is_empty() {
        provenance
    } else {
        with_ctx_evidence(&provenance, &ctx_evidence)
    };

    Decision::Offer {
        confidence,
        provenance,
    }
}

/// Record ctx-side evidence (Weak surface, huddle hint) in the provenance [`decide`] built, so a
/// wrong Weak offer is explainable from the stored interval like any other (FR-MT-04).
fn with_ctx_evidence(provenance: &str, extra: &[&str]) -> String {
    let mut map = serde_json::from_str::<serde_json::Value>(provenance)
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    let signals = map
        .entry("signals")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let serde_json::Value::Array(fired) = signals {
        for name in extra {
            fired.push(serde_json::Value::String((*name).to_string()));
        }
    }
    serde_json::Value::Object(map).to_string()
}

/// How long the microphone must stay in use before it counts when something else already says
/// "meeting" (a Meet URL, Zoom, calendar corroboration). Ten seconds separates "hey Siri" and a
/// voice memo from a call without making the offer feel late.
pub const MIC_SUSTAIN_MS: i64 = 10_000;

/// Mic-only is the weakest opener (FR-MT-04: ② alone is medium, not certain). When nothing else
/// corroborates, wait longer so speaker bleed on a media tab or a stray open input does not
/// surface the offer.
pub const MIC_ONLY_SUSTAIN_MS: i64 = 30_000;

/// Whether sustained microphone use should count as signal ② this tick.
///
/// Suppressed on known media pages unless [`meeting_context`] is already true (Meet URL / Zoom).
/// Shorter sustain when meeting context is present; longer when mic is the only evidence.
pub fn mic_counts_as_signal(sustained_ms: i64, meeting_context: bool, on_media_page: bool) -> bool {
    if sustained_ms == 0 {
        return false;
    }
    if on_media_page && !meeting_context {
        return false;
    }
    let threshold = if meeting_context {
        MIC_SUSTAIN_MS
    } else {
        MIC_ONLY_SUSTAIN_MS
    };
    sustained_ms >= threshold
}

/// SHOGUN's own bundle id. Its ASR holds the input during a meeting it is already noting, so a
/// holder-attributed signal must never count our own capture as evidence of a *new* meeting.
const SELF_BUNDLE_ID: &str = "com.selectkk.shogun";

/// How many distinct non-meeting apps may be frontmost during one unbroken stretch of microphone
/// use before the coarse signal is written off as stuck.
///
/// Three, because a real call tolerates tabbing away — to notes, to a browser, to the thing being
/// discussed — but a meeting coming into view clears the tally. A signal still "in use" after the
/// user has moved through three unrelated apps with no meeting in sight is not describing this
/// user's meeting; it is describing some daemon holding the device.
pub const MIC_STUCK_DISTINCT_APPS: usize = 3;

/// How long a stretch must have run before the app tally is allowed to condemn it.
///
/// Without a floor, joining a call and immediately opening the agenda, the calendar and a
/// scratchpad would look identical to a stuck daemon. Two minutes costs a genuinely stuck signal
/// almost nothing (it has been true since login) and protects the opening minutes of a real call,
/// which is exactly when people fetch the things they need.
pub const MIC_STUCK_MIN_MS: i64 = 2 * 60 * 1_000;

/// How the platform reported microphone use — and therefore how much the report is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicSource<'a> {
    /// The input is attributed to a specific process (macOS 14.4+ CoreAudio process objects).
    /// This is the signal we want: it can answer *who* is talking through the machine.
    Holder { bundle_id: &'a str },
    /// Only "some process on this machine is using an input device" is known (older macOS, or
    /// attribution unavailable). Correct but coarse: an always-on utility that never releases the
    /// microphone makes this true forever, which is what [`MicWatch`]'s stuck check exists for.
    SystemWide,
}

/// One microphone observation, with the context needed to judge whether it means anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicObservation<'a> {
    /// Whether an input device is running. **Truth value only — no samples are read.**
    pub in_use: bool,
    /// How that truth value was obtained.
    pub source: MicSource<'a>,
    /// The frontmost app at this tick.
    pub frontmost_bundle_id: &'a str,
    /// Whether the frontmost context looks like a meeting (known app, or a meeting URL).
    pub meeting_context: bool,
}

/// Turns "the microphone is open right now" into "a call is happening".
///
/// The single most useful meeting signal is not which page is open — a Meet URL is the same in
/// the lobby, in the call and after everyone has left — it is whether anyone is actually
/// talking through the machine. That is app-agnostic: it catches a call in an app nobody thought
/// to add to a bundle-id table.
///
/// It reads *whether the device is in use* and nothing else. No audio is sampled (FR-MT-04).
#[derive(Debug, Clone, Copy, Default)]
pub struct MicWatch {
    since_ms: Option<i64>,
    /// Distinct non-meeting apps seen in this stretch (hashed, so the watch stays `Copy`).
    seen: [u64; MIC_STUCK_DISTINCT_APPS],
    seen_len: usize,
    /// Set once the coarse signal is written off for this stretch; cleared only by a release or
    /// by a meeting coming into view.
    stuck: bool,
    /// Hash of the frontmost bundle the current stretch is attributed to — the meeting surface
    /// that explained the open device. Leftover mic must not carry into the *next* candidate
    /// (#378): a stretch earned on a Meet tab is not evidence that a Teams splash is a call.
    meeting_holder: Option<u64>,
}

/// FNV-1a. Only ever compared against other hashes in the same process, so stability across
/// releases is irrelevant — determinism within a run (and in tests) is what matters.
fn hash_bundle_id(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

impl MicWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the coarse signal has been written off for the current stretch (diagnostics).
    pub fn is_stuck(&self) -> bool {
        self.stuck
    }

    /// Feed one observation. Returns whether the microphone has been continuously in use for
    /// at least [`MIC_SUSTAIN_MS`] **and** that use is attributable to what the user is doing.
    pub fn observe(&mut self, obs: &MicObservation<'_>, now: i64) -> bool {
        if !obs.in_use {
            *self = Self::new();
            return false;
        }

        match obs.source {
            MicSource::Holder { bundle_id } => {
                // Our own ASR holding the input during a meeting we are already noting is not
                // evidence that a meeting is starting.
                let ours = bundle_id == SELF_BUNDLE_ID;
                let relevant =
                    !ours && (is_meeting_app(bundle_id) || bundle_id == obs.frontmost_bundle_id);
                if !relevant {
                    // Someone else's background use. Not a signal, and not a stretch either —
                    // the clock restarts if the relevant holder starts later.
                    self.since_ms = None;
                    return false;
                }
            }
            MicSource::SystemWide => {
                if obs.meeting_context {
                    let holder = hash_bundle_id(obs.frontmost_bundle_id);
                    if self.meeting_holder != Some(holder) {
                        // A different meeting surface took the front — or this is the first
                        // explained app after an unattributed stretch. Either way the open
                        // device has a *new* explanation, and the old stretch is not evidence
                        // for it: the incoming candidate earns its own sustain (#378).
                        self.since_ms = None;
                        self.meeting_holder = Some(holder);
                    }
                    // A meeting is in view, so the open device now has an explanation. Forget the
                    // tally *and* the verdict: "stuck" means "no explanation was ever offered",
                    // and one has been. Leaving the meeting re-accumulates it, so a daemon that
                    // really is holding the device is condemned again on the way out.
                    self.seen_len = 0;
                    self.stuck = false;
                } else {
                    self.note_unrelated_app(obs.frontmost_bundle_id, now);
                }
                if self.stuck {
                    return false;
                }
            }
        }

        let since = *self.since_ms.get_or_insert(now);
        // `saturating_sub` so a clock that jumps backwards restarts the wait instead of
        // reporting a meeting that has been running for negative time.
        now.saturating_sub(since) >= MIC_SUSTAIN_MS
    }

    /// Record a distinct non-meeting app, condemning the stretch once the signal has outlived
    /// [`MIC_STUCK_DISTINCT_APPS`] of them *and* [`MIC_STUCK_MIN_MS`].
    fn note_unrelated_app(&mut self, bundle_id: &str, now: i64) {
        let h = hash_bundle_id(bundle_id);
        if !self.seen[..self.seen_len].contains(&h) && self.seen_len < MIC_STUCK_DISTINCT_APPS {
            self.seen[self.seen_len] = h;
            self.seen_len += 1;
        }
        let long_enough = self
            .since_ms
            .is_some_and(|since| now.saturating_sub(since) >= MIC_STUCK_MIN_MS);
        if self.seen_len >= MIC_STUCK_DISTINCT_APPS && long_enough {
            self.stuck = true;
        }
    }

    /// Continuous in-use duration in milliseconds, or zero when the mic is closed.
    pub fn sustained_ms(&self, now: i64) -> i64 {
        self.since_ms
            .map(|since| now.saturating_sub(since))
            .unwrap_or(0)
    }
}

/// FR-MT-11: after the meeting page leaves the frontmost browser tab, wait this long before
/// wrapping. Covers a quick alt-tab or lobby flicker without keeping the pill alive for hours
/// because Chrome is still open on Gmail.
pub const MEETING_URL_LEFT_GRACE_MS: i64 = 20_000;
/// After the meeting page is gone past grace, the mic must stay closed this long before the
/// session ends. Separates "tab switched away but call still running" (mic open) from hang-up
/// flicker at the device layer.
pub const MIC_QUIET_AFTER_URL_LEFT_MS: i64 = 8_000;

/// Whether the frontmost browser tab still looks like an active meeting (FR-MT-11).
///
/// Used while a session opened on a Meet URL is recording: quitting Chrome is handled by
/// [`LiveSignals::meeting_app_present`]; navigating to mail or closing the Meet tab is this.
pub fn browser_meeting_page_present(page_url: Option<&str>, window_title: Option<&str>) -> bool {
    if page_url.is_some_and(is_meeting_url) {
        return true;
    }
    if let Some(url) = page_url {
        if is_media_url(url) {
            return false;
        }
        if host_of(url).is_some_and(|h| !h.is_empty()) {
            return false;
        }
    }
    if let Some(title) = window_title {
        if title_hint(title).is_some() {
            return true;
        }
        // PiP / AX gaps: an unreadable URL with a media title is still in-call, not "left".
        if is_suppressed_title(title) {
            return true;
        }
    }
    false
}

/// Whether a URL-tracked session has been off the meeting page long enough to wrap (FR-MT-11).
pub fn meeting_url_left_past_grace(lost_since_ms: Option<i64>, now: i64) -> bool {
    lost_since_ms.is_some_and(|since| now.saturating_sub(since) >= MEETING_URL_LEFT_GRACE_MS)
}

/// The word that names a Slack huddle, in the two languages Slack's UI shows it in.
const HUDDLE_WORDS: &[&str] = &["huddle", "ハドル"];

/// Call-control vocabulary that appears in a huddle's UI but not in chat about huddles.
const HUDDLE_CONTROL_VOCAB: &[&str] = &[
    "mute",
    "unmute",
    "leave",
    "share screen",
    "ミュート",
    "退出",
    "画面共有",
];

fn mentions_huddle(text: &str) -> bool {
    let lower = text.to_lowercase();
    HUDDLE_WORDS.iter().any(|w| lower.contains(w))
}

fn mentions_call_controls(text: &str) -> bool {
    let lower = text.to_lowercase();
    HUDDLE_CONTROL_VOCAB.iter().any(|w| lower.contains(w))
}

/// Whether Slack's window title / AX text looks like a huddle in progress.
///
/// Slack ships huddles inside the ordinary Slack window under the ordinary bundle id
/// ([`SLACK_BUNDLE_ID`]), so no bundle table can see them. The hint instead reads what
/// Accessibility already exposes: "Huddle" (or the Japanese UI's "ハドル") **co-occurring with
/// call-control vocabulary** (mute / leave / 退出 …). Co-occurrence is the point — "huddle" alone
/// is ordinary chat ("let's huddle tomorrow"), and a hint that fired on message text would put
/// the offer over every conversation that mentions the word.
///
/// Policy lives in [`evaluate_offer`]: the hint alone does nothing; with sustained mic use
/// (≥ [`MIC_SUSTAIN_MS`]) it is a Weak offer, never an auto-start.
pub fn huddle_hint(window_title: Option<&str>, ax_snippets: &[&str]) -> bool {
    let huddle_seen =
        window_title.is_some_and(mentions_huddle) || ax_snippets.iter().any(|s| mentions_huddle(s));
    if !huddle_seen {
        return false;
    }
    window_title.is_some_and(mentions_call_controls)
        || ax_snippets.iter().any(|s| mentions_call_controls(s))
}

/// After the huddle hint disappears from the title/AX text, wait this long before
/// treating the huddle as left. Same shape and length as [`MEETING_URL_LEFT_GRACE_MS`] — a
/// redraw or a moment on another channel must not wrap a huddle mid-sentence.
pub const HUDDLE_HINT_LOST_GRACE_MS: i64 = 20_000;

/// Whether a huddle session has been without its hint long enough to wrap.
/// Mirrors [`meeting_url_left_past_grace`].
pub fn huddle_hint_lost_past_grace(lost_since_ms: Option<i64>, now: i64) -> bool {
    lost_since_ms.is_some_and(|since| now.saturating_sub(since) >= HUDDLE_HINT_LOST_GRACE_MS)
}

/// Whether a huddle session should still count as present. Mirrors
/// [`meet_url_session_present`]: past the hint-loss grace an open microphone still means the call
/// is running (the user alt-tabbed away, and Slack's huddle UI is only visible while Slack is
/// frontmost); once the mic closes, wait [`MIC_QUIET_AFTER_URL_LEFT_MS`] so a hang-up flicker does
/// not end the session mid-word. The end watch's idle limit still applies
/// independently, so the huddle ends on whichever fires first.
pub fn huddle_session_present(
    lost_since_ms: Option<i64>,
    now: i64,
    mic_open: bool,
    mic_closed_since_ms: Option<i64>,
) -> bool {
    if !huddle_hint_lost_past_grace(lost_since_ms, now) {
        return true;
    }
    if mic_open {
        return true;
    }
    mic_closed_since_ms.is_none_or(|since| {
        now.saturating_sub(since) < MIC_QUIET_AFTER_URL_LEFT_MS
    })
}

/// Whether a browser Meet session should still count as present (FR-MT-11).
///
/// Past the URL-leave grace the frontmost tab no longer looks like a meeting, but an open
/// microphone almost always means the call is still running on a background tab — the user may be
/// on another Chrome tab, reading mail, or looking at the SHOGUN overlay. Recording continues
/// while the mic is open; once it closes, wait [`MIC_QUIET_AFTER_URL_LEFT_MS`] so a hang-up
/// flicker does not end the session mid-word.
pub fn meet_url_session_present(
    lost_since_ms: Option<i64>,
    now: i64,
    mic_open: bool,
    mic_closed_since_ms: Option<i64>,
) -> bool {
    if !meeting_url_left_past_grace(lost_since_ms, now) {
        return true;
    }
    if mic_open {
        return true;
    }
    mic_closed_since_ms.is_none_or(|since| {
        now.saturating_sub(since) < MIC_QUIET_AFTER_URL_LEFT_MS
    })
}

/// Whether the user has clearly left the call (FR-MT-11). Used to shorten Recap auto-dismiss.
pub fn call_clearly_ended(
    opened_via_meet_url: bool,
    url_lost_since_ms: Option<i64>,
    now: i64,
    mic_open: bool,
    mic_closed_since_ms: Option<i64>,
    zoom_bundle_id: Option<&str>,
    zoom_running: bool,
) -> bool {
    if opened_via_meet_url {
        meeting_url_left_past_grace(url_lost_since_ms, now)
            && !meet_url_session_present(url_lost_since_ms, now, mic_open, mic_closed_since_ms)
    } else if zoom_bundle_id.is_some_and(|b| bundle_hint(b) == Some(MeetingHint::Strong)) {
        // Strong only: quitting Zoom is leaving the call. A Weak bundle (Teams, Webex) stays
        // resident after the call, so its presence proves nothing — fall through to the mic.
        !zoom_running
    } else {
        !mic_open
            && mic_closed_since_ms
                .is_some_and(|since| now.saturating_sub(since) >= MIC_QUIET_AFTER_URL_LEFT_MS)
    }
}
