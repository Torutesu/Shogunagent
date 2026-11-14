//! Sound cues — and, first, the rules for staying silent (Issue #49, `docs/sound-design.md`).
//!
//! SHOGUN reads the microphone and system audio. Anything it *plays* through the built-in
//! speaker is picked up again by the built-in microphone, which means a UI chime can
//!
//! 1. land in the user's own transcript as `Speaker::Me` and propagate into the recap, and
//! 2. be heard by everyone else on the call, with no way for them to tell where it came from.
//!
//! The system tap already excludes our own process (`audio::capture::system_tap`), so that third
//! path is closed. The two above cannot be closed by an API — only by not playing. That is why
//! this module is a *silence* policy with a sound attached, and why the decision lives here as a
//! pure function instead of at each call site.
//!
//! The four rules (design doc §3):
//!
//! - **S1 Hot-mic silence** — any process holding an input device, on built-in speakers: only the
//!   user-triggered dictation boundary pair plays; every unattended, failure and meeting cue stays
//!   silent.
//! - **S2 Unattended silence** — cues that are not `Ask`/`Fail` need the user to have asked for
//!   them (`Pref::Full`).
//! - **S3 L1 silence** — automatic execution never makes a sound. Enforced structurally: there is
//!   no [`Cue`] for an L1 outcome, so a call site cannot ask for one.
//! - **S4 Quiet hours** — inside the user's quiet window everything is silent, `Ask`/`Fail`
//!   included. The Dream Cycle runs at night; a sleeping Mac stays quiet.
//!
//! The shell (`apps/desktop/src-tauri/src/sound.rs`) senses [`Env`] and performs the playback;
//! every "should this make a noise" question is answered here, where it can be tested without a
//! Mac.

/// What a cue is *for*. Drives the default policy, not the file that gets played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// We received something you did. Answers "did that register?".
    Ack,
    /// Something you were waiting for is ready. Answers "is it done yet?".
    Ready,
    /// Your judgement is required (L3). Answers "is anything waiting on me?".
    Ask,
    /// Something broke in a way that costs you work if ignored. Answers "is it still working?".
    Fail,
    /// The one-off identity cue. Plays when the user finishes setting SHOGUN up, and on launch
    /// only if they explicitly asked for it.
    Signature,
}

/// Every sound the product can make. Deliberately short: each addition costs a file, a policy
/// question and a chance to annoy someone every day.
///
/// There is intentionally no cue for L1 execution, sync progress, indexing or the Dream Cycle
/// (design doc §4 "anti-categories" and rule S3). The daily summaries are the one recorded
/// exception (issue #10, owner decision 2026-08-15): `SummaryReady` marks the morning/evening
/// card arriving, and being `Ready`-tier it stays opt-in (Pref::Full) and quiet-hours-gated.
///
/// The right-Option draft is the second recorded exception (owner decision 2026-09-27): the key
/// press is acknowledged, a soft tick/tock repeats while the draft is being written, and a
/// landing cue plays when it is inserted. It is progress, which §4 otherwise leaves silent, but it
/// is progress the user just asked for with a key and is waiting on with their cursor in the
/// field — so, like dictation, it plays on Essential. It still yields to a live microphone (S1),
/// quiet hours (S4) and the OS setting, and the repeat stops at [`THINKING_CAP_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// The notch was summoned by hotkey.
    Summon,
    /// Push-to-talk started listening.
    VoiceStart,
    /// Push-to-talk stopped listening.
    VoiceEnd,
    /// Push-to-talk could not run at all (mic denied, no model, ASR failed).
    VoiceFailed,
    /// An L3 send is waiting for a human. The first reason this module exists.
    ApprovalPending,
    /// A meeting was detected and note-taking starts unless the user declines.
    MeetingOffered,
    /// A recap finished generating.
    RecapReady,
    /// Capture stopped in a way the user has to fix (accessibility revoked, DB unavailable).
    CaptureStopped,
    /// A model the user was waiting on finished downloading.
    ModelReady,
    /// A connector finished authenticating (the landing point when they come back from a browser).
    ConnectorLinked,
    /// A daily summary card (morning / evening) arrived in the notch (issue #10).
    SummaryReady,
    /// Onboarding completed. Once in the life of an install.
    OnboardingComplete,
    /// The app launched. Silent unless the user turned the startup sound on (D1).
    AppLaunched,
    /// Right Option started a draft.
    DraftStarted,
    /// One beat of the repeat while the draft is being written. Alternates two assets
    /// ([`thinking_asset`]); [`Cue::asset`] names the first.
    DraftThinking,
    /// The draft landed in the field.
    DraftInserted,
}

impl Cue {
    pub fn category(self) -> Category {
        match self {
            Cue::Summon
            | Cue::VoiceStart
            | Cue::VoiceEnd
            | Cue::DraftStarted
            | Cue::DraftThinking => Category::Ack,
            Cue::RecapReady
            | Cue::ModelReady
            | Cue::ConnectorLinked
            | Cue::SummaryReady
            | Cue::DraftInserted => Category::Ready,
            Cue::ApprovalPending | Cue::MeetingOffered => Category::Ask,
            Cue::VoiceFailed | Cue::CaptureStopped => Category::Fail,
            Cue::OnboardingComplete | Cue::AppLaunched => Category::Signature,
        }
    }

    /// Base name of the audio file this cue plays, without extension.
    ///
    /// Several cues share one file on purpose — the set is a family of six, not one sound per
    /// event (design doc §7.1).
    pub fn asset(self) -> &'static str {
        match self {
            // Opening motion: something began.
            Cue::Summon | Cue::DraftStarted => "ack-open",
            Cue::DraftThinking => "thinking-tick",
            Cue::DraftInserted => "draft-landed",
            // Dictation has its own stronger paired pulse, selected by the owner in the sound lab.
            Cue::VoiceStart => "voice-start",
            Cue::VoiceEnd => "voice-end",
            Cue::RecapReady | Cue::ModelReady | Cue::ConnectorLinked | Cue::SummaryReady => "ready",
            Cue::ApprovalPending | Cue::MeetingOffered => "ask",
            Cue::VoiceFailed | Cue::CaptureStopped => "fail",
            Cue::OnboardingComplete | Cue::AppLaunched => "signature",
        }
    }

    /// Stable id for logs and analytics. Never carries content — only which cue fired.
    pub fn id(self) -> &'static str {
        match self {
            Cue::Summon => "summon",
            Cue::VoiceStart => "voice_start",
            Cue::VoiceEnd => "voice_end",
            Cue::VoiceFailed => "voice_failed",
            Cue::ApprovalPending => "approval_pending",
            Cue::MeetingOffered => "meeting_offered",
            Cue::RecapReady => "recap_ready",
            Cue::CaptureStopped => "capture_stopped",
            Cue::ModelReady => "model_ready",
            Cue::ConnectorLinked => "connector_linked",
            Cue::SummaryReady => "summary_ready",
            Cue::OnboardingComplete => "onboarding_complete",
            Cue::AppLaunched => "app_launched",
            Cue::DraftStarted => "draft_started",
            Cue::DraftThinking => "draft_thinking",
            Cue::DraftInserted => "draft_inserted",
        }
    }

    /// The right-Option draft cues, which have their own on/off switch ([`Settings::draft_sounds`]).
    pub fn is_draft(self) -> bool {
        matches!(self, Cue::DraftStarted | Cue::DraftThinking | Cue::DraftInserted)
    }

    /// Cues the user asked for with an explicit key press and is waiting on: the dictation
    /// boundary pair and the right-Option draft. They play on Essential and are never
    /// repeat-throttled (a tick every 600 ms is the point). Only the dictation pair is also exempt
    /// from the hot-mic rule; the draft cues are not.
    pub fn user_requested(self) -> bool {
        matches!(
            self,
            Cue::VoiceStart
                | Cue::VoiceEnd
                | Cue::DraftStarted
                | Cue::DraftThinking
                | Cue::DraftInserted
        )
    }

    /// Every cue, for the preview list in Settings and for exhaustiveness in tests.
    pub const ALL: [Cue; 16] = [
        Cue::Summon,
        Cue::VoiceStart,
        Cue::VoiceEnd,
        Cue::VoiceFailed,
        Cue::ApprovalPending,
        Cue::MeetingOffered,
        Cue::RecapReady,
        Cue::CaptureStopped,
        Cue::ModelReady,
        Cue::ConnectorLinked,
        Cue::SummaryReady,
        Cue::OnboardingComplete,
        Cue::AppLaunched,
        Cue::DraftStarted,
        Cue::DraftThinking,
        Cue::DraftInserted,
    ];
}

/// Delay from the right-Option acknowledgement to the first thinking beat, so the two do not
/// run together.
pub const THINKING_LEAD_MS: u64 = 350;
/// One thinking beat every 600 ms (owner choice "C2 時計 0.6秒", 2026-09-27).
pub const THINKING_PERIOD_MS: u64 = 600;
/// Hard stop for the repeat. An inline draft has no cancel and its HTTP call may run for
/// minutes; a tick that outlives the user's patience is worse than silence.
pub const THINKING_CAP_MS: u64 = 20_000;

/// How many beats fit between [`THINKING_LEAD_MS`] and [`THINKING_CAP_MS`].
pub const fn thinking_max_beats() -> u64 {
    (THINKING_CAP_MS - THINKING_LEAD_MS) / THINKING_PERIOD_MS + 1
}

/// The asset for beat `n` (0-based): a higher tick and a lower tock, alternating like a clock.
pub fn thinking_asset(beat: u64) -> &'static str {
    if beat.is_multiple_of(2) {
        "thinking-tick"
    } else {
        "thinking-tock"
    }
}

/// The audio files that ship in the bundle.
pub const ASSETS: [&str; 11] = [
    "ack-open",
    "ack-close",
    "voice-start",
    "voice-end",
    "ready",
    "ask",
    "fail",
    "signature",
    "thinking-tick",
    "thinking-tock",
    "draft-landed",
];

/// How much the user wants to hear. Default is [`Pref::Essential`]: dictation boundaries plus the
/// two categories that protect their work — a decision they owe, and a failure they must see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pref {
    /// Never play anything.
    Off,
    #[default]
    /// Voice start/end plus `Ask` and `Fail`.
    Essential,
    /// Everything, including `Ack` and `Ready`.
    Full,
}

impl Pref {
    pub fn tag(self) -> &'static str {
        match self {
            Pref::Off => "off",
            Pref::Essential => "essential",
            Pref::Full => "full",
        }
    }

    /// Parse the tag written by the UI. Anything unrecognised falls back to the default rather
    /// than to "loudest" — a corrupt settings file must not make the app noisier.
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "off" => Pref::Off,
            "full" => Pref::Full,
            _ => Pref::Essential,
        }
    }
}

/// A quiet window in local wall-clock minutes-from-midnight. Wraps midnight when `start > end`,
/// which is the normal case (22:00 → 08:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuietHours {
    pub enabled: bool,
    /// Minutes since local midnight. 0..=1439; the shell clamps before storing.
    pub start_min: u16,
    pub end_min: u16,
}

impl Default for QuietHours {
    fn default() -> Self {
        // 22:00–08:00. The Dream Cycle runs inside this window by design.
        Self { enabled: true, start_min: 22 * 60, end_min: 8 * 60 }
    }
}

impl QuietHours {
    /// Whether `now_min` (minutes since local midnight) falls inside the window.
    pub fn contains(&self, now_min: u16) -> bool {
        if !self.enabled {
            return false;
        }
        if self.start_min == self.end_min {
            // A zero-width window silences nothing. The alternative reading ("always quiet") would
            // turn a mis-set field into a feature that appears broken.
            return false;
        }
        if self.start_min < self.end_min {
            now_min >= self.start_min && now_min < self.end_min
        } else {
            now_min >= self.start_min || now_min < self.end_min
        }
    }
}

/// The user's persisted sound settings.
/// Defaults (asserted in `defaults_are_the_documented_ones`): Essential, startup sound on,
/// right-Option draft sounds on, quiet hours 22:00–08:00 on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub pref: Pref,
    /// Play [`Cue::AppLaunched`] on every launch. On by default (owner decision 2026-10-20,
    /// replacing D1's off default).
    pub startup_sound: bool,
    /// The right-Option draft cues (start, the thinking tick/tock, landing). On by default (owner
    /// decision 2026-09-27); a settings file written before this field existed reads as on.
    pub draft_sounds: bool,
    pub quiet_hours: QuietHours,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            pref: Pref::default(),
            startup_sound: true,
            draft_sounds: true,
            quiet_hours: QuietHours::default(),
        }
    }
}

/// Everything outside the user's settings that a decision depends on. The shell keeps these
/// cached and refreshed by its existing watchers — [`should_play`] must never be the thing that
/// goes and asks Core Audio (design doc §8.5, SLO: first open frame ≤ 150 ms, idle CPU ≤ 5%).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub settings: Settings,
    /// macOS System Settings → Sound → "Play user interface sound effects".
    pub os_ui_sounds_enabled: bool,
    /// Any input device running in any process (`mic::input_in_use`).
    pub mic_in_use: bool,
    /// Default output is the built-in speaker, so the built-in mic can hear it. On failure the
    /// shell reports `true` — the safe side (design doc §8.3).
    pub output_is_builtin_speaker: bool,
    /// Local minutes since midnight.
    pub now_min: u16,
    /// Time since this same *sound* last played, if it has this run. `None` = never.
    /// Keyed by asset, not by cue: two cues that share a file are one sound to the ear.
    pub ms_since_same_sound: Option<u64>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            os_ui_sounds_enabled: true,
            mic_in_use: false,
            output_is_builtin_speaker: true,
            now_min: 12 * 60,
            ms_since_same_sound: None,
        }
    }
}

/// Two plays of the same sound closer together than this collapse into one. An approval burst
/// is one event to a human, not five.
pub const MIN_GAP_MS: u64 = 2_000;

/// Why a cue did not play. Logged (never with content) so "why was it silent?" has an answer
/// that does not require a debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silence {
    /// macOS UI sound effects are off. We do not override the system setting (D5).
    OsSoundEffectsOff,
    /// The user chose Off.
    PrefOff,
    /// Inside quiet hours (S4).
    QuietHours,
    /// A microphone is live and we are on the built-in speaker (S1). Explicit dictation boundary
    /// cues are exempt; they tell the user exactly when capture begins and ends.
    HotMic,
    /// `Ack`/`Ready` while the user is on Essential (S2).
    NotEssential,
    /// The startup sound is off (D1).
    StartupSoundOff,
    /// The user turned the right-Option draft sounds off.
    DraftSoundsOff,
    /// The same sound played moments ago.
    Throttled,
}

impl Silence {
    pub fn tag(self) -> &'static str {
        match self {
            Silence::OsSoundEffectsOff => "os_sound_effects_off",
            Silence::PrefOff => "pref_off",
            Silence::QuietHours => "quiet_hours",
            Silence::HotMic => "hot_mic",
            Silence::NotEssential => "not_essential",
            Silence::StartupSoundOff => "startup_sound_off",
            Silence::DraftSoundsOff => "draft_sounds_off",
            Silence::Throttled => "throttled",
        }
    }
}

/// The answer: play this asset, or stay silent for this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Play(&'static str),
    Silent(Silence),
}

impl Verdict {
    pub fn is_play(self) -> bool {
        matches!(self, Verdict::Play(_))
    }
}

/// The whole policy, in evaluation order. Ordering is deliberate: the reason reported is the
/// *first* rule that silences the cue, and the earlier rules are the ones the user is more likely
/// to be asking about ("I turned sounds off and it still…").
pub fn should_play(cue: Cue, env: &Env) -> Verdict {
    if !env.os_ui_sounds_enabled {
        return Verdict::Silent(Silence::OsSoundEffectsOff);
    }
    if env.settings.pref == Pref::Off {
        return Verdict::Silent(Silence::PrefOff);
    }
    if cue.is_draft() && !env.settings.draft_sounds {
        return Verdict::Silent(Silence::DraftSoundsOff);
    }
    // S4 — before the category split, so quiet hours also covers Ask/Fail and Signature.
    if env.settings.quiet_hours.contains(env.now_min) {
        return Verdict::Silent(Silence::QuietHours);
    }
    // S1 — keep background and failure sounds out of another live microphone. The explicit
    // dictation boundary pair is the one product exception: the start cue plays into SHOGUN's own
    // dictation mic, which subtracts it (#951), with any meeting mic already silenced by the
    // dictation claim; end playback waits for SHOGUN's capture to close.
    if env.mic_in_use
        && env.output_is_builtin_speaker
        && !matches!(cue, Cue::VoiceStart | Cue::VoiceEnd)
    {
        return Verdict::Silent(Silence::HotMic);
    }
    if cue == Cue::AppLaunched && !env.settings.startup_sound {
        return Verdict::Silent(Silence::StartupSoundOff);
    }
    // S2 — Ack/Ready are opt-in except the cues the user asked for with a key (dictation and the
    // right-Option draft). Ask/Fail and the Signature are not: the first two protect work, and
    // the Signature is user-triggered.
    match cue.category() {
        Category::Ack | Category::Ready
            if env.settings.pref != Pref::Full && !cue.user_requested() =>
        {
            return Verdict::Silent(Silence::NotEssential);
        }
        _ => {}
    }
    if !cue.user_requested() && env.ms_since_same_sound.is_some_and(|ms| ms < MIN_GAP_MS)
    {
        return Verdict::Silent(Silence::Throttled);
    }
    Verdict::Play(cue.asset())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment where everything plays: sounds on, no mic, midday, Full.
    fn loud() -> Env {
        Env {
            settings: Settings { pref: Pref::Full, startup_sound: true, ..Settings::default() },
            ..Env::default()
        }
    }

    #[test]
    fn explicit_dictation_boundaries_are_not_repeat_throttled() {
        let env = Env { ms_since_same_sound: Some(0), ..loud() };
        for cue in [Cue::VoiceStart, Cue::VoiceEnd] {
            assert_eq!(should_play(cue, &env), Verdict::Play(cue.asset()));
        }
    }

    #[test]
    fn draft_cues_play_on_essential_unthrottled_but_not_into_a_hot_mic() {
        let essential = Env { ms_since_same_sound: Some(0), ..Env::default() };
        assert_eq!(should_play(Cue::DraftStarted, &essential), Verdict::Play("ack-open"));
        assert_eq!(should_play(Cue::DraftThinking, &essential), Verdict::Play("thinking-tick"));
        assert_eq!(should_play(Cue::DraftInserted, &essential), Verdict::Play("draft-landed"));

        let hot = Env { mic_in_use: true, output_is_builtin_speaker: true, ..Env::default() };
        for cue in [Cue::DraftStarted, Cue::DraftThinking, Cue::DraftInserted] {
            assert_eq!(should_play(cue, &hot), Verdict::Silent(Silence::HotMic), "{}", cue.id());
        }
    }

    #[test]
    fn draft_switch_silences_only_the_draft_cues() {
        let env = Env {
            settings: Settings { draft_sounds: false, ..loud().settings },
            ..loud()
        };
        for cue in Cue::ALL {
            let verdict = should_play(cue, &env);
            if cue.is_draft() {
                assert_eq!(verdict, Verdict::Silent(Silence::DraftSoundsOff), "{}", cue.id());
            } else {
                assert!(verdict.is_play(), "{} was silenced by the draft switch", cue.id());
            }
        }
    }

    #[test]
    fn settings_written_before_the_draft_switch_read_as_on() {
        let old = r#"{"pref":"essential","startup_sound":false,"quiet_hours":{"enabled":true,"start_min":1320,"end_min":480}}"#;
        let s: Settings = serde_json::from_str(old).expect("old sound.json parses");
        assert!(s.draft_sounds);
    }

    #[test]
    fn thinking_alternates_tick_and_tock_and_stops_at_the_cap() {
        assert_eq!(thinking_asset(0), "thinking-tick");
        assert_eq!(thinking_asset(1), "thinking-tock");
        assert_eq!(thinking_asset(2), "thinking-tick");
        assert!(ASSETS.contains(&thinking_asset(0)) && ASSETS.contains(&thinking_asset(1)));
        let last = THINKING_LEAD_MS + (thinking_max_beats() - 1) * THINKING_PERIOD_MS;
        assert!(last <= THINKING_CAP_MS, "last beat at {last} ms is past the cap");
        assert!(last + THINKING_PERIOD_MS > THINKING_CAP_MS, "one more beat would still fit");
    }

    #[test]
    fn only_dictation_and_the_draft_are_user_requested() {
        let requested: Vec<&str> =
            Cue::ALL.iter().filter(|c| c.user_requested()).map(|c| c.id()).collect();
        assert_eq!(
            requested,
            ["voice_start", "voice_end", "draft_started", "draft_thinking", "draft_inserted"]
        );
    }

    #[test]
    fn every_cue_maps_to_a_shipped_asset() {
        for cue in Cue::ALL {
            assert!(
                ASSETS.contains(&cue.asset()),
                "{} points at {}, which is not in ASSETS",
                cue.id(),
                cue.asset()
            );
        }
    }

    #[test]
    fn cue_ids_are_unique() {
        let mut ids: Vec<&str> = Cue::ALL.iter().map(|c| c.id()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "two cues share an id");
    }

    #[test]
    fn os_setting_wins_over_everything() {
        let env = Env { os_ui_sounds_enabled: false, ..loud() };
        for cue in Cue::ALL {
            assert_eq!(
                should_play(cue, &env),
                Verdict::Silent(Silence::OsSoundEffectsOff),
                "{} played with UI sound effects off",
                cue.id()
            );
        }
    }

    #[test]
    fn pref_off_silences_everything() {
        let env = Env { settings: Settings { pref: Pref::Off, ..loud().settings }, ..loud() };
        for cue in Cue::ALL {
            assert_eq!(should_play(cue, &env), Verdict::Silent(Silence::PrefOff), "{}", cue.id());
        }
    }

    #[test]
    fn hot_mic_on_builtin_speaker_silences_non_dictation_cues() {
        let env = Env { mic_in_use: true, output_is_builtin_speaker: true, ..loud() };
        for cue in Cue::ALL
            .into_iter()
            .filter(|cue| !matches!(cue, Cue::VoiceStart | Cue::VoiceEnd))
        {
            assert_eq!(should_play(cue, &env), Verdict::Silent(Silence::HotMic), "{}", cue.id());
        }
    }

    #[test]
    fn hot_mic_still_plays_explicit_dictation_boundaries() {
        let env = Env { mic_in_use: true, output_is_builtin_speaker: true, ..loud() };
        assert_eq!(should_play(Cue::VoiceStart, &env), Verdict::Play("voice-start"));
        assert_eq!(should_play(Cue::VoiceEnd, &env), Verdict::Play("voice-end"));
    }

    #[test]
    fn hot_mic_on_headphones_still_plays() {
        let env = Env { mic_in_use: true, output_is_builtin_speaker: false, ..loud() };
        assert!(should_play(Cue::ApprovalPending, &env).is_play());
        assert!(should_play(Cue::CaptureStopped, &env).is_play());
    }

    #[test]
    fn builtin_speaker_without_a_live_mic_plays() {
        let env = Env { mic_in_use: false, output_is_builtin_speaker: true, ..loud() };
        assert!(should_play(Cue::ApprovalPending, &env).is_play());
    }

    #[test]
    fn essential_plays_voice_boundaries_ask_and_fail() {
        let env = Env { settings: Settings::default(), ..Env::default() };
        assert_eq!(should_play(Cue::VoiceStart, &env), Verdict::Play("voice-start"));
        assert_eq!(should_play(Cue::VoiceEnd, &env), Verdict::Play("voice-end"));
        assert_eq!(should_play(Cue::ApprovalPending, &env), Verdict::Play("ask"));
        assert_eq!(should_play(Cue::MeetingOffered, &env), Verdict::Play("ask"));
        assert_eq!(should_play(Cue::CaptureStopped, &env), Verdict::Play("fail"));
        assert_eq!(should_play(Cue::VoiceFailed, &env), Verdict::Play("fail"));
        for quiet in [Cue::Summon, Cue::RecapReady, Cue::ModelReady, Cue::ConnectorLinked] {
            assert_eq!(
                should_play(quiet, &env),
                Verdict::Silent(Silence::NotEssential),
                "{} should need Full",
                quiet.id()
            );
        }
    }

    #[test]
    fn full_adds_ack_and_ready() {
        let env = loud();
        assert_eq!(should_play(Cue::VoiceStart, &env), Verdict::Play("voice-start"));
        assert_eq!(should_play(Cue::VoiceEnd, &env), Verdict::Play("voice-end"));
        assert_eq!(should_play(Cue::RecapReady, &env), Verdict::Play("ready"));
    }

    #[test]
    fn launch_is_silent_unless_asked_for() {
        let env = Env {
            settings: Settings { pref: Pref::Full, startup_sound: false, ..Settings::default() },
            ..Env::default()
        };
        assert_eq!(should_play(Cue::AppLaunched, &env), Verdict::Silent(Silence::StartupSoundOff));
        // …while finishing onboarding, which the user definitely did, plays on the same settings.
        assert_eq!(should_play(Cue::OnboardingComplete, &env), Verdict::Play("signature"));
    }

    /// The Signature is not an Ack/Ready, so Essential does not suppress it.
    #[test]
    fn onboarding_signature_survives_essential() {
        let env = Env::default();
        assert_eq!(should_play(Cue::OnboardingComplete, &env), Verdict::Play("signature"));
    }

    #[test]
    fn quiet_hours_silence_even_failures() {
        // 23:30, inside the default 22:00–08:00 window.
        let env = Env { now_min: 23 * 60 + 30, ..loud() };
        for cue in Cue::ALL {
            assert_eq!(should_play(cue, &env), Verdict::Silent(Silence::QuietHours), "{}", cue.id());
        }
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let q = QuietHours::default();
        assert!(q.contains(22 * 60), "start is inclusive");
        assert!(q.contains(3 * 60), "after midnight is inside");
        assert!(q.contains(7 * 60 + 59));
        assert!(!q.contains(8 * 60), "end is exclusive");
        assert!(!q.contains(12 * 60));
        assert!(!q.contains(21 * 60 + 59));
    }

    #[test]
    fn quiet_hours_same_day_window() {
        let q = QuietHours { enabled: true, start_min: 9 * 60, end_min: 17 * 60 };
        assert!(!q.contains(8 * 60 + 59));
        assert!(q.contains(9 * 60));
        assert!(q.contains(16 * 60 + 59));
        assert!(!q.contains(17 * 60));
        assert!(!q.contains(23 * 60));
    }

    #[test]
    fn quiet_hours_disabled_or_zero_width_silence_nothing() {
        let off = QuietHours { enabled: false, ..QuietHours::default() };
        assert!(!off.contains(23 * 60));
        let zero = QuietHours { enabled: true, start_min: 60, end_min: 60 };
        assert!(!zero.contains(60));
        assert!(!zero.contains(0));
    }

    #[test]
    fn repeat_of_the_same_sound_is_throttled() {
        let env = Env { ms_since_same_sound: Some(MIN_GAP_MS - 1), ..loud() };
        assert_eq!(should_play(Cue::ApprovalPending, &env), Verdict::Silent(Silence::Throttled));

        let env = Env { ms_since_same_sound: Some(MIN_GAP_MS), ..loud() };
        assert!(should_play(Cue::ApprovalPending, &env).is_play());

        let env = Env { ms_since_same_sound: None, ..loud() };
        assert!(should_play(Cue::ApprovalPending, &env).is_play());
    }

    #[test]
    fn first_matching_rule_is_the_reported_reason() {
        let env = Env {
            os_ui_sounds_enabled: false,
            settings: Settings { pref: Pref::Off, ..Settings::default() },
            mic_in_use: true,
            ..Env::default()
        };
        assert_eq!(should_play(Cue::Summon, &env), Verdict::Silent(Silence::OsSoundEffectsOff));

        let env = Env { mic_in_use: true, ..Env::default() }; // Essential + hot mic
        assert_eq!(should_play(Cue::Summon, &env), Verdict::Silent(Silence::HotMic));
    }

    #[test]
    fn pref_tags_round_trip_and_fall_back_quietly() {
        for p in [Pref::Off, Pref::Essential, Pref::Full] {
            assert_eq!(Pref::from_tag(p.tag()), p);
        }
        assert_eq!(Pref::from_tag(""), Pref::Essential);
        assert_eq!(Pref::from_tag("LOUD"), Pref::Essential, "garbage must not be louder");
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let s = Settings::default();
        assert_eq!(s.pref, Pref::Essential);
        assert!(s.startup_sound, "startup sound is on by default (2026-10-20)");
        assert!(s.draft_sounds, "right-Option draft sounds are on by default");
        assert!(s.quiet_hours.enabled);
        assert_eq!((s.quiet_hours.start_min, s.quiet_hours.end_min), (22 * 60, 8 * 60));
    }
}
