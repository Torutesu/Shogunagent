//! The Batch relay's bearer on this Mac, including during the 7-day trial (issue #763).
//!
//! The service-funded Batch lane (Dream Cycle, Morning Brief, meeting recaps, mail triage) goes
//! through the relay, which takes a signed token as its bearer. A paid Mac presents its licence
//! token from `billing.json`. A trial Mac has no licence, so without a trial token it would
//! present nothing and run every one of those degraded for the whole trial, though the trial
//! promises everything.
//!
//! The licence API issues a trial token per device (`POST /api/license/trial`). The desktop
//! keeps it in `trial_relay.json` next to `billing.json`. It is plan `trial`, which
//! `shogun_license::verify` refuses, so it never grants a plan here. What it does carry into plan
//! resolution is the end of this device's trial, signed (`period_end`, verified by
//! `shogun_license::DeviceBinding::verify_trial`; issue #1031): without it no trial runs, and at
//! or past it the trial is over whatever `onboarding.json` says ([`trial_end_ms`]). The plain
//! `trial_ends_at` recorded beside the token (audit 2026-10-10 H2) and a sign-out can only end the
//! trial sooner, so editing, deleting or re-stamping the files cannot start or stretch a trial.
//! The device id the desktop asks with is the hardware-derived one
//! (`shogun_license::hardware_device_id`), so to the server a Mac stays one device across
//! reinstalls. The token is only ever handed to the relay, and only while the trial runs. Like the
//! licence token it is signed, device-bound and expires within a day, so it is not a secret
//! (CLAUDE.md invariant 7, the licence-token exception).

use std::path::PathBuf;

use shogun_agents::entitlement::{entitlements, resolve_plan_from, PlanInputs, PlanStatus};
use shogun_license::{trusted_keys, Clock, DeviceBinding, LicenceFacts, TrustedKeys};

use crate::plan_source::{
    billing_json_path, onboarding_json_path, parse_billing_snapshot, parse_max_seen_ms,
    parse_trial_started_at_ms, BillingSnapshot,
};

/// Ask again this long before the token expires, so a Mac never runs a night without one.
pub const REFRESH_BEFORE_EXPIRY_SECS: u64 = 6 * 3600;

/// After an answer without a token (the trial ended on the server, or the request failed), wait
/// this long before asking again.
pub const RETRY_AFTER_SECS: u64 = 3600;

/// The relay refuses a token at or past its `exp` with 401 (`apps/api/src/auth.ts` checks only
/// `exp`), even while the licence still entitles this Mac through its 14-day offline grace. A
/// token is presented only with at least this long left, so a submit and its first poll do not
/// straddle the expiry (issue #777).
pub const RELAY_EXPIRY_MARGIN_SECS: u64 = 5 * 60;

/// Before a Batch-lane use, a licence token this close to `exp` (or past it) is re-verified first,
/// so the night's jobs present a fresh token instead of one the relay is about to refuse
/// (issue #777).
pub const LICENCE_REFRESH_BEFORE_EXPIRY_SECS: u64 = 3 * 3600;

/// The Batch lane asks the licence API for that refresh at most this often. Several Batch jobs
/// run in one night, and during a licence-API outage each would otherwise make its own request.
pub const LICENCE_REFRESH_MIN_INTERVAL_SECS: u64 = 15 * 60;

/// Where the trial's relay token lives: the `SHOGUN_TRIAL_RELAY_JSON` env override (debug builds
/// only), else the macOS app-data path next to `billing.json`.
pub fn trial_relay_json_path() -> Option<PathBuf> {
    crate::plan_source::app_data_file("SHOGUN_TRIAL_RELAY_JSON", "trial_relay.json")
}

/// What `trial_relay.json` holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialRelay {
    /// The anonymous device id the licence API bound the token to. Kept so the Mac keeps asking
    /// as the same device: a new id would look like a new trial to the server.
    pub device_id: String,
    /// The last token issued, or `None` when the last answer carried none.
    pub token: Option<String>,
    /// When `token` expires, unix seconds.
    pub expires_at: Option<u64>,
    /// When the licence API was last asked, unix seconds.
    pub checked_at: u64,
    /// The server's end of this device's trial (`trial_ends_at`), unix seconds, from the last
    /// answer that carried one (audit H2). Authoritative for plan resolution; see
    /// [`trial_end_ms`]. Absent in files written before it was recorded.
    pub trial_ends_at: Option<u64>,
    /// When the account signed out on this Mac, unix seconds (#148 §7.4). Until the next sign-in
    /// the trial is over here: plan resolution reads it as the server's end, so all four faces
    /// show the signed-out, locked state at once. A sign-in rewrites the file without it.
    pub signed_out_at: Option<u64>,
    /// Why the last answer gave this Mac no trial, one of [`REFUSAL_REASONS`] (`None` after a
    /// grant). Display only ([`trial_refusal`]): it never feeds a gate.
    pub reason: Option<String>,
}

/// The refusals the licence API's trial answer can carry and the faces show: this Mac's trial
/// went to another account (`trial_used`), the account's trial already reached its Mac cap
/// (`trial_device_limit`), the trial starts at Checkout with a card (`checkout_required`), or a
/// sign-in comes first (`account_required`).
pub const REFUSAL_REASONS: [&str; 4] = ["trial_used", "trial_device_limit", "checkout_required", "account_required"];

/// The `trial_ends_at` to record after the licence API answered, given the one held. An answer
/// that carries the server's end only ever brings the held one forward (issue #1031). A refusal
/// replaces it: the held end can be another account's trial on this Mac, and keeping it would
/// show this account a trial that ended instead of the reason it has none. Dropping it unlocks
/// nothing — with no signed token no trial runs ([`trial_end_ms`]).
pub fn recorded_end_after_answer(held: Option<u64>, answer: Option<u64>, refused: bool) -> Option<u64> {
    if refused {
        return answer;
    }
    match (held, answer) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Why this Mac has no trial, for display, while it has neither a trial nor a plan: the reason
/// the last answer recorded. `None` once a trial or a plan runs, whatever the file still says.
pub fn trial_refusal(trial: Option<&TrialRelay>, status: PlanStatus) -> Option<&str> {
    if matches!(status, PlanStatus::TrialNotStarted | PlanStatus::TrialExpired) {
        trial?.reason.as_deref()
    } else {
        None
    }
}

/// [`trial_refusal`] for this Mac, read from `trial_relay.json` at its usual place.
pub fn read_trial_refusal(status: PlanStatus) -> Option<String> {
    let text = std::fs::read_to_string(trial_relay_json_path()?).ok()?;
    trial_refusal(parse_trial_relay(&text).as_ref(), status).map(str::to_string)
}

/// Parse `trial_relay.json`. Anything unreadable is `None`, the same as no file.
pub fn parse_trial_relay(text: &str) -> Option<TrialRelay> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? < 1 {
        return None;
    }
    let device_id = v.get("device_id")?.as_str()?.trim().to_string();
    if device_id.is_empty() {
        return None;
    }
    let token = v
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some(TrialRelay {
        device_id,
        token,
        expires_at: v.get("expires_at").and_then(serde_json::Value::as_u64),
        checked_at: v
            .get("checked_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        trial_ends_at: v.get("trial_ends_at").and_then(serde_json::Value::as_u64),
        signed_out_at: v.get("signed_out_at").and_then(serde_json::Value::as_u64),
        reason: v
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .filter(|r| REFUSAL_REASONS.contains(r))
            .map(str::to_string),
    })
}

/// The end of this device's trial that the record states without proof, in unix **ms**: the
/// plain `trial_ends_at` (audit H2) or a sign-out on this Mac (#148 §7.4), whichever is earlier.
/// Plan resolution uses it only to end a trial sooner ([`trial_end_ms`]).
pub fn recorded_trial_end_ms(trial: Option<&TrialRelay>) -> Option<u64> {
    let trial = trial?;
    let end = match (trial.trial_ends_at, trial.signed_out_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    end?.checked_mul(1000)
}

/// The end of this device's trial as signed by the licence API: the `period_end` of the trial
/// token in the record, when it verifies against `keys` and is bound to this Mac (issue #1031).
/// The token's `exp` does not matter here — a Mac offline past it keeps its trial until this end.
pub fn signed_trial_end_ms(
    trial: Option<&TrialRelay>,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> Option<u64> {
    let trial = trial?;
    let token = trial.token.as_deref()?;
    let verified = binding.verify_trial(token, keys?, &trial.device_id).ok()?;
    Some(verified.period_end_ms())
}

/// The trial end plan resolution reads (`PlanInputs::server_trial_ends_at_ms`), unix ms, at
/// `now_ms` (issue #1031):
/// - a signed end ([`signed_trial_end_ms`]), brought forward by the recorded one when that is
///   earlier;
/// - with no signed end, a recorded end already reached (the server said the trial is over, or
///   this Mac signed out): the trial is over;
/// - otherwise `None`: nothing proves a trial yet (before the first token, after a sign-in, or a
///   file edited or deleted), so no trial runs until the licence API answers.
pub fn trial_end_ms(
    trial: Option<&TrialRelay>,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
    now_ms: u64,
) -> Option<u64> {
    let recorded = recorded_trial_end_ms(trial);
    match signed_trial_end_ms(trial, keys, binding) {
        Some(signed) => Some(recorded.map_or(signed, |r| r.min(signed))),
        None => recorded.filter(|&end| end <= now_ms),
    }
}

/// [`trial_end_ms`] for this Mac, read from `trial_relay.json` at its usual place, now.
pub fn read_server_trial_ends_at_ms() -> Option<u64> {
    let text = std::fs::read_to_string(trial_relay_json_path()?).ok()?;
    trial_end_ms(
        parse_trial_relay(&text).as_ref(),
        trusted_keys().as_ref(),
        DeviceBinding::this_mac(),
        crate::desktop_heartbeat::now_ms(),
    )
}

/// Serialise for the desktop writer; kept beside the parser so the field names cannot drift.
pub fn serialize_trial_relay(relay: &TrialRelay) -> String {
    serde_json::json!({
        "version": 1,
        "device_id": relay.device_id,
        "token": relay.token,
        "expires_at": relay.expires_at,
        "checked_at": relay.checked_at,
        "trial_ends_at": relay.trial_ends_at,
        "signed_out_at": relay.signed_out_at,
        "reason": relay.reason,
    })
    .to_string()
}

/// Is the 7-day trial running at `now_ms`, with no active licence? The local stamp bounded by
/// the signed trial end in `trial` ([`trial_end_ms`]).
pub fn trial_running(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    now_ms: u64,
) -> bool {
    trial_running_with_key(
        billing,
        trial_started_at_ms,
        trial,
        Clock::at(now_ms),
        trusted_keys().as_ref(),
        DeviceBinding::this_mac(),
    )
}

/// [`trial_running`] with the verifying keys and this Mac's device binding passed in, so tests
/// can sign licences, at a [`Clock`] that may carry the high-water mark (audit M7).
fn trial_running_with_key(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> bool {
    trial_status_with_key(billing, trial_started_at_ms, trial, clock, keys, binding)
        == PlanStatus::Trial
}

fn trial_status_with_key(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> PlanStatus {
    let facts = match (billing, keys) {
        (Some(snap), Some(keys)) => binding.plan_facts(&snap.token, keys, &snap.device_id, clock),
        _ => LicenceFacts::default(),
    };
    let inputs = PlanInputs {
        trial_started_at_ms,
        server_trial_ends_at_ms: trial_end_ms(trial, keys, binding, clock.now()),
        billing: facts.state,
        licence_trial_until_ms: facts.trial_until_ms,
    };
    entitlements(resolve_plan_from(inputs), clock.now()).status
}

/// The `exp` (unix seconds) of the licence token in `billing`, while that licence entitles this
/// Mac at `now_ms` — fresh, or past `exp` inside the offline grace window. `None` with no
/// licence, one this build cannot verify, or one past its grace.
pub fn active_licence_expiry(billing: Option<&BillingSnapshot>, now_ms: u64) -> Option<u64> {
    active_licence_expiry_with_key(
        billing,
        Clock::at(now_ms),
        trusted_keys().as_ref(),
        DeviceBinding::this_mac(),
    )
}

fn active_licence_expiry_with_key(
    billing: Option<&BillingSnapshot>,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> Option<u64> {
    let snap = billing?;
    let token = binding.verify(&snap.token, keys?, &snap.device_id).ok()?;
    token.freshness_at(clock).is_valid().then_some(token.expires_at)
}

/// Is this Mac provisioned for the Batch relay at `now_ms`: a licence that entitles it (fresh, or
/// past `exp` inside the offline grace), or the local 7-day trial? This is what the Dream status
/// shows as its Batch lane. It does not ask whether a bearer is in hand this minute: a licence
/// near `exp` is refreshed and a trial token is fetched before the night uses them
/// ([`licence_refresh_due`], [`should_ask`]), and whether a night actually ran on the relay is the
/// night report's to say. Not the Select KK key in the Keychain: a release build never holds
/// one, so that would read "local" on every shipping Mac.
pub fn batch_provisioned(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    now_ms: u64,
) -> bool {
    batch_provisioned_with_key(
        billing,
        trial_started_at_ms,
        trial,
        Clock::at(now_ms),
        trusted_keys().as_ref(),
        DeviceBinding::this_mac(),
    )
}

fn batch_provisioned_with_key(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> bool {
    active_licence_expiry_with_key(billing, clock, keys, binding).is_some()
        || trial_running_with_key(billing, trial_started_at_ms, trial, clock, keys, binding)
}

/// Would the relay still accept a token expiring at `expires_at` (unix seconds), with
/// [`RELAY_EXPIRY_MARGIN_SECS`] to spare?
pub fn usable_at_relay(expires_at: u64, now_secs: u64) -> bool {
    expires_at > now_secs.saturating_add(RELAY_EXPIRY_MARGIN_SECS)
}

/// Should the Batch lane re-verify the licence before using it? Yes when its token expires within
/// [`LICENCE_REFRESH_BEFORE_EXPIRY_SECS`] (or already has), unless the Batch lane already asked
/// less than [`LICENCE_REFRESH_MIN_INTERVAL_SECS`] ago. A last attempt stamped in the future (the
/// clock stepped back) does not hold the refresh off.
pub fn licence_refresh_due(expires_at: u64, now_secs: u64, last_attempt_secs: Option<u64>) -> bool {
    let near = now_secs.saturating_add(LICENCE_REFRESH_BEFORE_EXPIRY_SECS) >= expires_at;
    near && refresh_rested(now_secs, last_attempt_secs)
}

/// Has the Batch lane waited [`LICENCE_REFRESH_MIN_INTERVAL_SECS`] since its last refresh
/// attempt? Also the limit on the refresh a relay 401 asks for, whatever the token's expiry.
pub fn refresh_rested(now_secs: u64, last_attempt_secs: Option<u64>) -> bool {
    last_attempt_secs.is_none_or(|at| {
        at > now_secs || now_secs - at >= LICENCE_REFRESH_MIN_INTERVAL_SECS
    })
}

/// The bearer the Batch relay takes from this Mac at `now_ms`, or `None`, in which case the
/// caller runs its local lane. The licence token while it entitles the device and the relay would
/// still accept it; otherwise the trial token, while the local trial runs and the token has not
/// expired.
///
/// A paid Mac whose token is past `exp` (offline grace) keeps its plan locally but gets `None`
/// here: the relay would answer 401 and the night would read as a subscription error (issue #777).
/// The caller refreshes the licence first (see [`licence_refresh_due`]).
pub fn relay_bearer(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    now_ms: u64,
) -> Option<String> {
    relay_bearer_with_key(
        billing,
        trial_started_at_ms,
        trial,
        Clock::at(now_ms),
        trusted_keys().as_ref(),
        DeviceBinding::this_mac(),
    )
}

fn relay_bearer_with_key(
    billing: Option<&BillingSnapshot>,
    trial_started_at_ms: Option<u64>,
    trial: Option<&TrialRelay>,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> Option<String> {
    let now_ms = clock.now();
    if let Some(exp) = active_licence_expiry_with_key(billing, clock, keys, binding) {
        // A Mac with an entitling licence presents that licence: its token or, past
        // `exp`, nothing — never the trial token in its place.
        return billing
            .filter(|_| usable_at_relay(exp, now_ms / 1000))
            .map(|snap| snap.token.clone());
    }
    if !trial_running_with_key(billing, trial_started_at_ms, trial, clock, keys, binding) {
        return None;
    }
    let trial = trial?;
    let expires_at_ms = trial.expires_at?.saturating_mul(1000);
    trial.token.clone().filter(|_| expires_at_ms > now_ms)
}

/// Should a running trial ask the licence API for a token now? Yes with no record; with a token
/// that expires within [`REFRESH_BEFORE_EXPIRY_SECS`], or none at all, once
/// [`RETRY_AFTER_SECS`] have passed since the last ask.
pub fn should_ask(trial: Option<&TrialRelay>, now_secs: u64) -> bool {
    let Some(trial) = trial else {
        return true;
    };
    let fresh = trial.token.is_some()
        && trial
            .expires_at
            .is_some_and(|exp| exp > now_secs.saturating_add(REFRESH_BEFORE_EXPIRY_SECS));
    !fresh && now_secs.saturating_sub(trial.checked_at) >= RETRY_AFTER_SECS
}

/// The files on this Mac, read once for one decision.
#[derive(Debug, Clone, Default)]
pub struct RelayFiles {
    pub billing: Option<BillingSnapshot>,
    pub trial_started_at_ms: Option<u64>,
    pub trial: Option<TrialRelay>,
    /// The time high-water mark from `billing.json` (audit M7); every `now_ms` handed to the
    /// methods below is clamped to it.
    pub max_seen_ms: Option<u64>,
}

impl RelayFiles {
    /// Read `billing.json`, `onboarding.json` and `trial_relay.json` from their usual places.
    pub fn read() -> Self {
        let read = |path: Option<PathBuf>| path.and_then(|p| std::fs::read_to_string(p).ok());
        let billing_text = read(billing_json_path());
        Self {
            billing: billing_text.as_deref().and_then(parse_billing_snapshot),
            trial_started_at_ms: read(onboarding_json_path())
                .and_then(|t| parse_trial_started_at_ms(&t)),
            trial: read(trial_relay_json_path()).and_then(|t| parse_trial_relay(&t)),
            max_seen_ms: billing_text.as_deref().and_then(parse_max_seen_ms),
        }
    }

    fn clock(&self, now_ms: u64) -> Clock {
        Clock::with_mark(now_ms, self.max_seen_ms)
    }

    /// [`relay_bearer`] over these files.
    pub fn bearer(&self, now_ms: u64) -> Option<String> {
        relay_bearer_with_key(
            self.billing.as_ref(),
            self.trial_started_at_ms,
            self.trial.as_ref(),
            self.clock(now_ms),
            trusted_keys().as_ref(),
            DeviceBinding::this_mac(),
        )
    }

    /// [`trial_running`] over these files.
    pub fn trial_running(&self, now_ms: u64) -> bool {
        trial_running_with_key(
            self.billing.as_ref(),
            self.trial_started_at_ms,
            self.trial.as_ref(),
            self.clock(now_ms),
            trusted_keys().as_ref(),
            DeviceBinding::this_mac(),
        )
    }

    /// Should this Mac ask the licence API for a trial token (subject to [`should_ask`])? While
    /// the trial runs, and while it is stamped here but not yet proven by a signed token
    /// (issue #1031): right after sign-in, after a failed first ask, or after the record was
    /// edited or deleted. Not once the trial is over or before it was ever stamped.
    pub fn trial_token_wanted(&self, now_ms: u64) -> bool {
        match trial_status_with_key(
            self.billing.as_ref(),
            self.trial_started_at_ms,
            self.trial.as_ref(),
            self.clock(now_ms),
            trusted_keys().as_ref(),
            DeviceBinding::this_mac(),
        ) {
            PlanStatus::Trial => true,
            PlanStatus::TrialNotStarted => self.trial_started_at_ms.is_some(),
            _ => false,
        }
    }

    /// Explicit onboarding account checks may ask before the local trial clock exists (#148 D2).
    /// This permits checking eligibility only; bearer use still requires a running trial.
    pub fn trial_check_allowed(&self, now_ms: u64) -> bool {
        matches!(
            trial_status_with_key(
                self.billing.as_ref(),
                self.trial_started_at_ms,
                self.trial.as_ref(),
                self.clock(now_ms),
                trusted_keys().as_ref(),
                DeviceBinding::this_mac(),
            ),
            PlanStatus::TrialNotStarted | PlanStatus::Trial
        )
    }

    /// [`batch_provisioned`] over these files.
    pub fn batch_provisioned(&self, now_ms: u64) -> bool {
        batch_provisioned_with_key(
            self.billing.as_ref(),
            self.trial_started_at_ms,
            self.trial.as_ref(),
            self.clock(now_ms),
            trusted_keys().as_ref(),
            DeviceBinding::this_mac(),
        )
    }

    /// [`active_licence_expiry`] over these files.
    pub fn licence_expiry(&self, now_ms: u64) -> Option<u64> {
        active_licence_expiry_with_key(
            self.billing.as_ref(),
            self.clock(now_ms),
            trusted_keys().as_ref(),
            DeviceBinding::this_mac(),
        )
    }

    /// The trial end plan resolution reads ([`trial_end_ms`]), unix ms, at `now_ms`.
    pub fn server_trial_ends_at_ms(&self, now_ms: u64) -> Option<u64> {
        let now = self.clock(now_ms).now();
        trial_end_ms(self.trial.as_ref(), trusted_keys().as_ref(), DeviceBinding::this_mac(), now)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use shogun_agents::entitlement::TRIAL_DURATION_MS;

    const STARTED_MS: u64 = 1_760_000_000_000;
    const NOW_MS: u64 = STARTED_MS + 24 * 3600 * 1000;
    const NOW_SECS: u64 = NOW_MS / 1000;

    fn trial(token: Option<&str>, expires_at: Option<u64>, checked_at: u64) -> TrialRelay {
        TrialRelay {
            device_id: "device-abc12345".to_string(),
            token: token.map(str::to_string),
            expires_at,
            checked_at,
            trial_ends_at: None,
            signed_out_at: None,
            reason: None,
        }
    }

    fn files(
        billing: Option<BillingSnapshot>,
        started: Option<u64>,
        trial: Option<TrialRelay>,
        max_seen_ms: Option<u64>,
    ) -> RelayFiles {
        RelayFiles { billing, trial_started_at_ms: started, trial, max_seen_ms }
    }

    /// The test key's half of the licence API: what `trusted_keys()` holds in a release build.
    fn signing() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])
    }

    fn test_keys() -> TrustedKeys {
        TrustedKeys::single(signing().verifying_key().to_bytes())
    }

    /// The tests stand for a Mac with no hardware id: the id recorded beside the token decides.
    const BIND: DeviceBinding<'static> = DeviceBinding::RECORDED_ONLY;

    /// The trial's end on the server for [`STARTED_MS`], unix seconds.
    const TRIAL_END_SECS: u64 = (STARTED_MS + TRIAL_DURATION_MS) / 1000;

    /// Sign a trial token the way `/api/license/trial` does (`buildTrialTokenPayload`).
    fn trial_token(device: &str, iat: u64, exp: u64, period_end: u64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;
        let body = serde_json::json!({
            "v": 1, "lic": format!("trial_{device}"), "plan": "trial", "status": "trialing",
            "device": device, "iat": iat, "exp": exp, "period_end": period_end,
            "cancel_at_period_end": true, "grace_days": 0,
        })
        .to_string();
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing().sign(body.as_bytes()).to_bytes())
        )
    }

    /// The record after a successful ask: a signed token expiring at `expires_at`, issued two
    /// hours before it, for the trial that started at [`STARTED_MS`].
    fn signed(expires_at: u64) -> TrialRelay {
        let token = trial_token(DEVICE, expires_at - 2 * 3600, expires_at, TRIAL_END_SECS);
        trial(Some(&token), Some(expires_at), NOW_SECS)
    }

    fn status(started: Option<u64>, relay: Option<&TrialRelay>, clock: Clock) -> PlanStatus {
        trial_status_with_key(None, started, relay, clock, Some(&test_keys()), BIND)
    }

    fn running(started: Option<u64>, relay: Option<&TrialRelay>, now_ms: u64) -> bool {
        trial_running_with_key(None, started, relay, Clock::at(now_ms), Some(&test_keys()), BIND)
    }

    fn bearer(started: Option<u64>, relay: Option<&TrialRelay>, now_ms: u64) -> Option<String> {
        relay_bearer_with_key(None, started, relay, Clock::at(now_ms), Some(&test_keys()), BIND)
    }

    fn provisioned(started: Option<u64>, relay: Option<&TrialRelay>, now_ms: u64) -> bool {
        batch_provisioned_with_key(None, started, relay, Clock::at(now_ms), Some(&test_keys()), BIND)
    }

    #[test]
    fn account_required_check_runs_without_a_trial_stamp_but_grants_no_bearer() {
        let files = RelayFiles::default();
        assert!(files.trial_check_allowed(NOW_MS));
        assert!(!files.trial_running(NOW_MS));
        assert!(!files.trial_token_wanted(NOW_MS));
        // An eligibility response can even contain a token: it cannot start the local trial.
        let relay = signed(NOW_SECS + 3600);
        assert_eq!(status(None, Some(&relay), Clock::at(NOW_MS)), PlanStatus::TrialNotStarted);
        assert_eq!(bearer(None, Some(&relay), NOW_MS), None);
        assert!(!provisioned(None, Some(&relay), NOW_MS));
        // Stamped here and proven by the token: the trial runs, until the signed end.
        assert_eq!(status(Some(STARTED_MS), Some(&relay), Clock::at(NOW_MS)), PlanStatus::Trial);
        assert_eq!(
            status(Some(STARTED_MS), Some(&relay), Clock::at(STARTED_MS + TRIAL_DURATION_MS)),
            PlanStatus::TrialExpired
        );
    }

    /// Issue #1031: a stamp in `onboarding.json` is a file anyone can edit or recreate. Without a
    /// signed, device-bound trial token it runs no trial, and with one the token's end wins.
    #[test]
    fn only_a_signed_trial_token_runs_the_trial() {
        let fresh_stamp = Some(NOW_MS - 3600 * 1000);
        // No record at all (`trial_relay.json` deleted), or one without a token.
        for relay in [None, Some(trial(None, None, NOW_SECS))] {
            assert!(!running(fresh_stamp, relay.as_ref(), NOW_MS));
            assert_eq!(status(fresh_stamp, relay.as_ref(), Clock::at(NOW_MS)), PlanStatus::TrialNotStarted);
            assert_eq!(trial_end_ms(relay.as_ref(), Some(&test_keys()), BIND, NOW_MS), None);
        }
        // The plain `trial_ends_at` edited into the future proves nothing either.
        let mut edited = trial(None, None, NOW_SECS);
        edited.trial_ends_at = Some(NOW_SECS + 30 * 86_400);
        assert!(!running(fresh_stamp, Some(&edited), NOW_MS));
        // A forged token, a token for another device, and a licence in its place prove nothing.
        let forged = trial_token(DEVICE, NOW_SECS - 60, NOW_SECS + 3600, TRIAL_END_SECS)
            .replace("v1.", "v1.x");
        let other = trial_token("device-other999", NOW_SECS - 60, NOW_SECS + 3600, TRIAL_END_SECS);
        let licence_token = licence(&signing(), NOW_SECS - 60, NOW_SECS + 3600).token;
        for token in [forged, other, licence_token, "v1.trial.sig".to_string()] {
            let relay = trial(Some(&token), Some(NOW_SECS + 3600), NOW_SECS);
            assert!(!running(fresh_stamp, Some(&relay), NOW_MS), "{token}");
            assert_eq!(bearer(fresh_stamp, Some(&relay), NOW_MS), None);
        }
        // A token signed by a key this build does not trust.
        let relay = signed(NOW_SECS + 3600);
        let stranger = TrustedKeys::single(ed25519_dalek::SigningKey::from_bytes(&[6u8; 32]).verifying_key().to_bytes());
        assert_eq!(trial_end_ms(Some(&relay), Some(&stranger), BIND, NOW_MS), None);
        assert_eq!(trial_end_ms(Some(&relay), None, BIND, NOW_MS), None);
        // With the signed token, a stamp edited to an hour ago still ends at the server's end.
        assert!(running(fresh_stamp, Some(&relay), NOW_MS));
        assert_eq!(
            trial_end_ms(Some(&relay), Some(&test_keys()), BIND, NOW_MS),
            Some(TRIAL_END_SECS * 1000)
        );
        assert!(!running(fresh_stamp, Some(&relay), TRIAL_END_SECS * 1000));
        // Offline past the token's `exp`: the trial runs to its signed end, with no bearer.
        let lapsed_token = signed(NOW_SECS - 60);
        assert!(running(Some(STARTED_MS), Some(&lapsed_token), NOW_MS));
        assert_eq!(bearer(Some(STARTED_MS), Some(&lapsed_token), NOW_MS), None);
    }

    /// Audit H2 / #148 §7.4: the recorded end and a sign-out can only end the trial sooner. With
    /// no signed token, a recorded end already passed reads as over (the server said so, or this
    /// Mac signed out), not as "not started".
    #[test]
    fn the_recorded_end_and_a_sign_out_only_bring_the_end_forward() {
        let mut relay = signed(NOW_SECS + 3600);
        relay.trial_ends_at = Some(NOW_SECS);
        assert_eq!(bearer(Some(STARTED_MS), Some(&relay), NOW_MS), None);
        assert!(!running(Some(STARTED_MS), Some(&relay), NOW_MS));
        assert!(!provisioned(Some(STARTED_MS), Some(&relay), NOW_MS));
        // One second before the recorded end it still runs.
        assert_eq!(
            bearer(Some(STARTED_MS), Some(&relay), NOW_MS - 1000),
            relay.token.clone()
        );
        assert_eq!(recorded_trial_end_ms(Some(&relay)), Some(NOW_MS));
        assert_eq!(recorded_trial_end_ms(None), None);
        // A recorded end later than the signed one does not extend it.
        relay.trial_ends_at = Some(NOW_SECS + 30 * 86_400);
        assert_eq!(
            trial_end_ms(Some(&relay), Some(&test_keys()), BIND, NOW_MS),
            Some(TRIAL_END_SECS * 1000)
        );
        // A sign-out ends the trial on this Mac at once, and round-trips.
        let mut out = trial(None, None, NOW_SECS);
        out.trial_ends_at = Some(NOW_SECS + 5 * 86_400);
        out.signed_out_at = Some(NOW_SECS - 60);
        assert_eq!(recorded_trial_end_ms(Some(&out)), Some((NOW_SECS - 60) * 1000));
        assert_eq!(parse_trial_relay(&serialize_trial_relay(&out)), Some(out.clone()));
        assert_eq!(status(Some(STARTED_MS), Some(&out), Clock::at(NOW_MS)), PlanStatus::TrialExpired);
        // The server said the trial is over (no token, its end passed): over, too.
        let mut over = trial(None, None, NOW_SECS);
        over.trial_ends_at = Some(NOW_SECS - 1);
        assert_eq!(status(Some(STARTED_MS), Some(&over), Clock::at(NOW_MS)), PlanStatus::TrialExpired);
        // A sign-in takes the sign-out mark off before the token comes back: not proven yet, so
        // not started (the desktop asks again), rather than over.
        out.signed_out_at = None;
        assert_eq!(status(Some(STARTED_MS), Some(&out), Clock::at(NOW_MS)), PlanStatus::TrialNotStarted);
        // A missing local stamp still fails closed.
        assert!(!running(None, Some(&signed(NOW_SECS + 3600)), NOW_MS));
    }

    #[test]
    fn the_desktop_asks_for_a_token_while_the_trial_runs_or_waits_for_proof() {
        // Stamped, nothing on disk: ask (the first ask after sign-in failed, or the file went).
        let waiting = files(None, Some(STARTED_MS), None, None);
        assert!(waiting.trial_token_wanted(NOW_MS));
        assert!(waiting.trial_check_allowed(NOW_MS));
        assert!(!waiting.trial_running(NOW_MS));
        // Never stamped: only the explicit account check asks.
        assert!(!files(None, None, None, None).trial_token_wanted(NOW_MS));
        // Over: no more asking.
        let mut over = trial(None, None, NOW_SECS);
        over.trial_ends_at = Some(NOW_SECS - 1);
        assert!(!files(None, Some(STARTED_MS), Some(over), None).trial_token_wanted(NOW_MS));
    }

    #[test]
    fn the_recorded_mark_clamps_the_batch_lane_clock() {
        let relay = signed(NOW_SECS + 3600);
        let over = STARTED_MS + TRIAL_DURATION_MS;
        let at = |mark: Option<u64>| Clock::with_mark(NOW_MS, mark);
        // The desktop saw the trial end; the clock now reads yesterday.
        let rolled_back = |c| relay_bearer_with_key(None, Some(STARTED_MS), Some(&relay), c, Some(&test_keys()), BIND);
        assert_eq!(rolled_back(at(Some(over))), None);
        assert!(!trial_running_with_key(None, Some(STARTED_MS), Some(&relay), at(Some(over)), Some(&test_keys()), BIND));
        assert!(!batch_provisioned_with_key(None, Some(STARTED_MS), Some(&relay), at(Some(over)), Some(&test_keys()), BIND));
        // Without the mark the same reading is a running trial.
        assert_eq!(rolled_back(at(None)), relay.token.clone());
        assert!(batch_provisioned_with_key(None, Some(STARTED_MS), Some(&relay), at(None), Some(&test_keys()), BIND));
    }

    #[test]
    fn a_stamped_running_trial_resolves_a_batch_credential() {
        let relay = signed(NOW_SECS + 3600);
        assert_eq!(bearer(Some(STARTED_MS), Some(&relay), NOW_MS), relay.token.clone());
    }

    #[test]
    fn the_trial_token_is_not_used_outside_the_local_trial() {
        let relay = signed(NOW_SECS + 40 * 3600);
        // Past the trial: the token's own expiry does not extend it.
        let after = STARTED_MS + TRIAL_DURATION_MS + 1;
        assert_eq!(bearer(Some(STARTED_MS), Some(&relay), after), None);
        // No trial clock on this Mac: not a trial at all.
        assert_eq!(bearer(None, Some(&relay), NOW_MS), None);
    }

    #[test]
    fn an_expired_or_missing_trial_token_is_no_credential() {
        let expired = signed(NOW_SECS);
        assert_eq!(bearer(Some(STARTED_MS), Some(&expired), NOW_MS), None);
        let refused = trial(None, None, NOW_SECS);
        assert_eq!(bearer(Some(STARTED_MS), Some(&refused), NOW_MS), None);
        assert_eq!(bearer(Some(STARTED_MS), None, NOW_MS), None);
    }

    #[test]
    fn an_unverifiable_licence_does_not_hide_the_trial_token() {
        // A billing.json this build cannot verify reads as no licence: the trial rules apply.
        let billing = BillingSnapshot {
            device_id: DEVICE.to_string(),
            token: "not-a-licence".to_string(),
            verified_at: None,
            max_seen_ms: None,
        };
        let relay = signed(NOW_SECS + 3600);
        assert_eq!(
            relay_bearer_with_key(Some(&billing), Some(STARTED_MS), Some(&relay), Clock::at(NOW_MS), Some(&test_keys()), BIND),
            relay.token.clone()
        );
    }

    #[test]
    fn asks_when_there_is_no_fresh_token_and_not_more_than_hourly() {
        assert!(should_ask(None, NOW_SECS));
        // A token good for more than six hours: nothing to do.
        let fresh = trial(
            Some("t"),
            Some(NOW_SECS + REFRESH_BEFORE_EXPIRY_SECS + 1),
            0,
        );
        assert!(!should_ask(Some(&fresh), NOW_SECS));
        // Expiring soon, last asked long ago: ask.
        let soon = trial(Some("t"), Some(NOW_SECS + 60), NOW_SECS - RETRY_AFTER_SECS);
        assert!(should_ask(Some(&soon), NOW_SECS));
        // Refused or failed a minute ago: wait out the hour.
        let refused = trial(None, None, NOW_SECS - 60);
        assert!(!should_ask(Some(&refused), NOW_SECS));
        let later = trial(None, None, NOW_SECS - RETRY_AFTER_SECS);
        assert!(should_ask(Some(&later), NOW_SECS));
    }

    const DEVICE: &str = "device-abc12345";

    /// Mint a licence token the way the licence API does (issue #777 tests).
    fn licence(signing: &ed25519_dalek::SigningKey, iat_secs: u64, exp_secs: u64) -> BillingSnapshot {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;

        let body = serde_json::json!({
            "v": 1, "lic": "lic-relay", "plan": "standard", "status": "active", "device": DEVICE,
            "iat": iat_secs, "exp": exp_secs, "period_end": 2_000_000_000i64,
            "cancel_at_period_end": false, "grace_days": 14,
        })
        .to_string();
        let token = format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
        );
        BillingSnapshot { device_id: DEVICE.to_string(), token, verified_at: None, max_seen_ms: None }
    }

    /// Issue #777: the relay refuses a token at or past `exp`, so a paid Mac on its offline grace
    /// gets no bearer (local lane) instead of a night of 401s — and never the trial token either.
    #[test]
    fn a_licence_token_past_exp_is_never_sent_to_the_relay() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let keys = TrustedKeys::single(key);
        let exp = NOW_SECS + 2 * 3600;
        let snap = licence(&signing, exp - 26 * 3600, exp);
        let relay = trial(Some("v1.trial.sig"), Some(NOW_SECS + 3600), NOW_SECS);
        let at = |secs: u64| {
            relay_bearer_with_key(Some(&snap), Some(STARTED_MS), Some(&relay), Clock::at(secs * 1000), Some(&keys),
                DeviceBinding::RECORDED_ONLY,
            )
        };

        // Fresh: the licence token.
        assert_eq!(at(NOW_SECS).as_deref(), Some(snap.token.as_str()));
        // Inside the margin, at exp, and in the grace window: nothing.
        assert_eq!(at(exp - RELAY_EXPIRY_MARGIN_SECS), None);
        assert_eq!(at(exp), None);
        assert_eq!(at(exp + 3 * 24 * 3600), None);
        // The licence still entitles the Mac in grace, which is why the trial token stays unused.
        assert_eq!(
            active_licence_expiry_with_key(Some(&snap), Clock::at((exp + 3 * 24 * 3600) * 1000), Some(&keys),
                DeviceBinding::RECORDED_ONLY
            ),
            Some(exp)
        );
        // Past the grace window the licence is gone and the trial rules decide again.
        let past_grace_ms = (exp - 26 * 3600 + 15 * 24 * 3600) * 1000;
        let recorded = DeviceBinding::RECORDED_ONLY;
        assert_eq!(active_licence_expiry_with_key(Some(&snap), Clock::at(past_grace_ms), Some(&keys), recorded), None);
        // A licence this build cannot verify has no expiry to act on.
        let other = TrustedKeys::single(ed25519_dalek::SigningKey::from_bytes(&[6u8; 32]).verifying_key().to_bytes());
        assert_eq!(active_licence_expiry_with_key(Some(&snap), Clock::at(NOW_MS), Some(&other), recorded), None);
        assert_eq!(active_licence_expiry_with_key(Some(&snap), Clock::at(NOW_MS), None, recorded), None);
    }

    /// Issue #777: a `billing.json` copied from a paying Mac is no licence here — the relay is not
    /// handed its token, and a running trial keeps presenting the trial token instead.
    #[test]
    fn a_copied_licence_is_not_presented_to_the_relay() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let keys = TrustedKeys::single(key);
        let copied = licence(&signing, NOW_SECS - 60, NOW_SECS + 26 * 3600);
        const OTHER_HW: &str = "YLBItJt5pOm21XXUG5u3Yp_iuTj6PjOY-uvskMDp9Ig";
        // The other Mac's own trial token, bound to its hardware id.
        let trial_token = trial_token(OTHER_HW, NOW_SECS - 60, NOW_SECS + 3600, TRIAL_END_SECS);
        let relay = trial(Some(&trial_token), Some(NOW_SECS + 3600), NOW_SECS);
        let other_mac = DeviceBinding { hardware: Some(OTHER_HW), legacy_cutoff_secs: NOW_SECS - 3600 };
        assert_eq!(active_licence_expiry_with_key(Some(&copied), Clock::at(NOW_MS), Some(&keys), other_mac), None);
        assert_eq!(
            relay_bearer_with_key(Some(&copied), Some(STARTED_MS), Some(&relay), Clock::at(NOW_MS), Some(&keys), other_mac)
                .as_deref(),
            Some(trial_token.as_str())
        );
        // On the Mac it came from (no hardware id, so the saved id decides) it is the licence.
        assert_eq!(
            relay_bearer_with_key(Some(&copied), Some(STARTED_MS), Some(&relay), Clock::at(NOW_MS), Some(&keys),
                DeviceBinding::RECORDED_ONLY
            ),
            Some(copied.token.clone())
        );
    }

    /// The Dream status's Batch lane: a licence (fresh or in grace) or a running trial, whether
    /// or not a bearer is in hand this minute; nothing else.
    #[test]
    fn missing_recap_bearer_distinguishes_eligibility_from_refresh_failure() {
        use crate::meeting_recap_status::missing_credential_status;
        let signing = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let keys = TrustedKeys::single(key);
        let binding = DeviceBinding::RECORDED_ONLY;
        let expired = licence(&signing, NOW_SECS - 26 * 3600, NOW_SECS - 3600);
        // A running trial whose token lapsed (the refresh failed), and a licence in its grace.
        let lapsed_trial = signed(NOW_SECS - 60);
        for (billing, started, relay) in
            [(None, Some(STARTED_MS), Some(&lapsed_trial)), (Some(&expired), None, None)]
        {
            assert!(relay_bearer_with_key(billing, started, relay, Clock::at(NOW_MS), Some(&keys), binding).is_none());
            assert_eq!(missing_credential_status(batch_provisioned_with_key(billing, started, relay, Clock::at(NOW_MS), Some(&keys), binding)), "unavailable");
        }
        // Never started, over, or stamped without proof.
        for started in [None, Some(STARTED_MS - TRIAL_DURATION_MS), Some(STARTED_MS)] {
            assert!(relay_bearer_with_key(None, started, None, Clock::at(NOW_MS), Some(&keys), binding).is_none());
            assert_eq!(missing_credential_status(batch_provisioned_with_key(None, started, None, Clock::at(NOW_MS), Some(&keys), binding)), "needs_plan");
        }
    }

    #[test]
    fn batch_provisioning_follows_the_licence_or_the_trial() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let keys = TrustedKeys::single(key);
        let recorded = DeviceBinding::RECORDED_ONLY;
        let exp = NOW_SECS + 2 * 3600;
        let snap = licence(&signing, exp - 26 * 3600, exp);
        let relay = signed(NOW_SECS + 3600);
        let at = |billing: Option<&BillingSnapshot>, started: Option<u64>, now_ms: u64| {
            batch_provisioned_with_key(billing, started, Some(&relay), Clock::at(now_ms), Some(&keys), recorded)
        };

        // A licence, fresh and in its offline grace (no bearer then, but a refresh comes first).
        assert!(at(Some(&snap), None, NOW_MS));
        assert!(at(Some(&snap), None, (exp + 3 * 24 * 3600) * 1000));
        // Past the grace, with no trial: not provisioned.
        let past_grace_ms = (exp - 26 * 3600 + 15 * 24 * 3600) * 1000;
        assert!(!at(Some(&snap), None, past_grace_ms));
        // A running trial, proven by its signed token; stamped with nothing on disk is not.
        assert!(at(None, Some(STARTED_MS), NOW_MS));
        assert!(!batch_provisioned_with_key(None, Some(STARTED_MS), None, Clock::at(NOW_MS), Some(&keys), recorded));
        // The trial over, or never started: not provisioned.
        assert!(!at(None, Some(STARTED_MS), STARTED_MS + TRIAL_DURATION_MS + 1));
        assert!(!at(None, None, NOW_MS));
        // A licence copied from another Mac is not a licence here.
        let other_mac = DeviceBinding {
            hardware: Some("YLBItJt5pOm21XXUG5u3Yp_iuTj6PjOY-uvskMDp9Ig"),
            legacy_cutoff_secs: NOW_SECS - 3600,
        };
        assert!(!batch_provisioned_with_key(Some(&snap), None, None, Clock::at(NOW_MS), Some(&keys), other_mac));
    }

    #[test]
    fn the_relay_margin_is_strict() {
        assert!(usable_at_relay(NOW_SECS + RELAY_EXPIRY_MARGIN_SECS + 1, NOW_SECS));
        assert!(!usable_at_relay(NOW_SECS + RELAY_EXPIRY_MARGIN_SECS, NOW_SECS));
        assert!(!usable_at_relay(NOW_SECS, NOW_SECS));
        assert!(!usable_at_relay(0, u64::MAX));
    }

    /// Issue #777: refresh before a Batch-lane use when the token is near or past `exp`, but not
    /// once per Batch job during an outage.
    #[test]
    fn the_batch_lane_refreshes_a_licence_near_expiry_at_most_every_quarter_hour() {
        let exp = NOW_SECS + 10 * 3600;
        // Far from expiry: nothing to do.
        assert!(!licence_refresh_due(exp, NOW_SECS, None));
        // Within three hours of expiry, and past it.
        let near = exp - LICENCE_REFRESH_BEFORE_EXPIRY_SECS;
        assert!(licence_refresh_due(exp, near, None));
        assert!(licence_refresh_due(exp, exp + 3600, None));
        // Just asked: wait out the interval, then ask again.
        assert!(!licence_refresh_due(exp, exp + 3600, Some(exp + 3600 - 60)));
        assert!(licence_refresh_due(
            exp,
            exp + 3600,
            Some(exp + 3600 - LICENCE_REFRESH_MIN_INTERVAL_SECS)
        ));
        // A stamp from the future (clock stepped back) does not block the refresh.
        assert!(licence_refresh_due(exp, exp + 3600, Some(exp + 99_999)));
        // The rest interval alone (the relay-401 refresh) ignores the expiry.
        assert!(refresh_rested(NOW_SECS, None));
        assert!(!refresh_rested(NOW_SECS, Some(NOW_SECS - 1)));
        assert!(refresh_rested(NOW_SECS, Some(NOW_SECS - LICENCE_REFRESH_MIN_INTERVAL_SECS)));
    }

    #[test]
    fn the_file_round_trips_and_garbage_reads_as_none() {
        let relay = trial(Some("v1.trial.sig"), Some(123), 45);
        assert_eq!(
            parse_trial_relay(&serialize_trial_relay(&relay)),
            Some(relay)
        );
        let refused = TrialRelay { reason: Some("trial_used".into()), ..trial(None, None, 45) };
        assert_eq!(
            parse_trial_relay(&serialize_trial_relay(&refused)),
            Some(refused)
        );
        // A reason this build does not know reads as none; a file from before it was recorded too.
        let odd = r#"{"version":1,"device_id":"device-abc12345","checked_at":1,"reason":"<b>hi</b>"}"#;
        assert_eq!(parse_trial_relay(odd).and_then(|t| t.reason), None);
        for text in [
            "",
            "not json",
            r#"{"device_id":"device-abc12345"}"#,
            r#"{"version":1,"device_id":"  "}"#,
            r#"{"version":0,"device_id":"device-abc12345"}"#,
        ] {
            assert_eq!(parse_trial_relay(text), None, "{text:?}");
        }
    }

    /// Audit 2026-10-20 M7: the trial's end is read from the signed trial token; the unsigned
    /// fields can only bring it earlier.
    #[test]
    fn the_trial_end_comes_from_the_signed_token_and_unsigned_fields_only_shorten_it() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;
        let signing = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let keys = TrustedKeys::single(signing.verifying_key().to_bytes());
        let iat = 1_800_000_000u64;
        let end = iat + 3 * 24 * 3600;
        let body = serde_json::json!({
            "v": 1, "lic": "trial_dev-m7", "plan": "trial", "status": "trialing",
            "device": "dev-m7", "iat": iat, "exp": iat + 3600, "period_end": end,
            "cancel_at_period_end": true, "grace_days": 0,
        })
        .to_string();
        let token = format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
        );
        let record = |trial_ends_at: Option<u64>, token: Option<String>| TrialRelay {
            device_id: "dev-m7".into(),
            token,
            expires_at: None,
            checked_at: iat,
            trial_ends_at,
            signed_out_at: None,
            reason: None,
        };
        let binding = DeviceBinding::RECORDED_ONLY;
        // An unsigned end pushed a year out cannot outlive the signed one.
        let edited = record(Some(end + 365 * 24 * 3600), Some(token.clone()));
        assert_eq!(trial_end_ms(Some(&edited), Some(&keys), binding, iat * 1000), Some(end * 1000));
        // Dropping the unsigned field changes nothing.
        let unsigned_gone = record(None, Some(token.clone()));
        assert_eq!(trial_end_ms(Some(&unsigned_gone), Some(&keys), binding, iat * 1000), Some(end * 1000));
        // An earlier unsigned end still ends the trial sooner.
        let earlier = record(Some(end - 3600), Some(token.clone()));
        assert_eq!(trial_end_ms(Some(&earlier), Some(&keys), binding, iat * 1000), Some((end - 3600) * 1000));
        // A token signed by someone else carries no end.
        let other = TrustedKeys::single(ed25519_dalek::SigningKey::from_bytes(&[22u8; 32]).verifying_key().to_bytes());
        assert_eq!(trial_end_ms(Some(&unsigned_gone), Some(&other), binding, iat * 1000), None);
        // Another Mac's token carries no end.
        let elsewhere = DeviceBinding { hardware: Some("another-mac"), legacy_cutoff_secs: 0 };
        assert_eq!(trial_end_ms(Some(&unsigned_gone), Some(&keys), elsewhere, iat * 1000), None);
        // Issue #1031: with no signed token, a pushed-out unsigned end proves nothing.
        let no_token = record(Some(end + 365 * 24 * 3600), None);
        assert_eq!(trial_end_ms(Some(&no_token), Some(&keys), binding, iat * 1000), None);
    }

    /// PR #1314 audit: account B on a Mac whose trial ran under account A is told why it has no
    /// trial, not shown A's ended one.
    #[test]
    fn a_refusal_replaces_the_held_end_and_a_grant_only_brings_it_forward() {
        let a_end = Some(NOW_SECS - 3600);
        assert_eq!(recorded_end_after_answer(a_end, None, true), None, "trial_used: A's end goes");
        assert_eq!(recorded_end_after_answer(a_end, None, false), a_end, "a plain answer keeps it");
        assert_eq!(recorded_end_after_answer(a_end, Some(NOW_SECS), false), a_end, "never later");
        assert_eq!(recorded_end_after_answer(Some(NOW_SECS), a_end, false), a_end, "sooner wins");
        assert_eq!(recorded_end_after_answer(None, Some(NOW_SECS), false), Some(NOW_SECS));
        // Dropping the end unlocks nothing: no signed token, no trial.
        let refused = TrialRelay { reason: Some("trial_used".into()), ..trial(None, None, NOW_SECS) };
        assert_eq!(status(Some(STARTED_MS), Some(&refused), Clock::at(NOW_MS)), PlanStatus::TrialNotStarted);
    }

    #[test]
    fn the_refusal_shows_only_while_no_trial_or_plan_runs() {
        let refused = TrialRelay { reason: Some("trial_device_limit".into()), ..trial(None, None, NOW_SECS) };
        for shown in [PlanStatus::TrialNotStarted, PlanStatus::TrialExpired] {
            assert_eq!(trial_refusal(Some(&refused), shown), Some("trial_device_limit"));
        }
        for hidden in [PlanStatus::Trial, PlanStatus::Standard, PlanStatus::Pro] {
            assert_eq!(trial_refusal(Some(&refused), hidden), None);
        }
        assert_eq!(trial_refusal(Some(&trial(None, None, NOW_SECS)), PlanStatus::TrialNotStarted), None);
        assert_eq!(trial_refusal(None, PlanStatus::TrialNotStarted), None);
    }
}
