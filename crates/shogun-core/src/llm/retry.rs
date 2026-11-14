//! Shared retry / backoff for LLM calls (FR-HN-09 / #315).
//!
//! 429 and 5xx are the only classes a second attempt can fix. Credentials, quota, parse
//! failures, and 4xx-other are not: the same request gets the same answer. The schedule is the
//! one live translation already used (`meeting_translate.rs`): four attempts, 0 / 1s / 2s / 4s
//! between them, and a 30s cooldown after a 429 so a shared Select-KK lane is not hammered.
//!
//! Chat streaming still retries **once**, and only before the first token — that is an SLO
//! choice, not a second policy. New call sites must use this module rather than inventing
//! another schedule.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use super::LlmError;

/// Four attempts including the first. Matches live translation.
pub const MAX_ATTEMPTS: u32 = 4;

/// After a 429, new requests on the same lane wait this long.
pub const COOLDOWN_AFTER_429: Duration = Duration::from_secs(30);

const BACKOFF_MS: [u64; 4] = [0, 1_000, 2_000, 4_000];

/// Sleep before `attempt` (0-based). Attempt 0 is immediate.
pub fn backoff(attempt: u32) -> Duration {
    let ms = BACKOFF_MS
        .get(attempt as usize)
        .copied()
        .unwrap_or(*BACKOFF_MS.last().unwrap_or(&4_000));
    Duration::from_millis(ms)
}

/// Whether this error class is worth another attempt.
pub fn is_retryable(e: &LlmError) -> bool {
    match e {
        LlmError::RateLimited(..) | LlmError::Transport(_) => true,
        // `status_error` renders every non-401/403/429 status as "… HTTP <code>".
        LlmError::Provider(msg) => msg
            .rsplit("HTTP ")
            .next()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .is_some_and(|c| c >= 500),
        LlmError::Unauthorized(..)
        | LlmError::InvalidRequest
        | LlmError::QuotaExhausted(_)
        | LlmError::NeedsUpdate(_)
        | LlmError::NotConfigured
        | LlmError::Parse(_)
        | LlmError::PolicyStopped
        // A ledger that would not take the row a moment ago will not take it on an immediate
        // replay either, and a burst of attempts against a locked database is the wrong answer.
        | LlmError::TraceUnrecorded => false,
    }
}

/// Drive `op` up to [`MAX_ATTEMPTS`], returning the first success or the last retryable
/// error. Non-retryable errors abort immediately. Before every attempt after the first, `sleep`
/// is handed that attempt's [`backoff`] (1s / 2s / 4s): an immediate replay of a 429 is the
/// burst this policy exists to prevent. The sleeper is injected so the policy stays free of a
/// runtime and tests stay fast. The lane-wide pause after a 429 ([`COOLDOWN_AFTER_429`]) is the
/// caller's [`Cooldown`], armed when this returns `RateLimited`.
pub fn run<T, F, S>(mut sleep: S, mut op: F) -> Result<T, LlmError>
where
    F: FnMut(u32) -> Result<T, LlmError>,
    S: FnMut(Duration),
{
    let mut last = LlmError::RateLimited(429, "exhausted retries".into());
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            sleep(backoff(attempt));
        }
        match op(attempt) {
            Ok(v) => return Ok(v),
            Err(e) if is_retryable(&e) => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Async counterpart of [`run`]: same attempt limit, same backoff, through the injected async
/// `sleep` (`tokio::time::sleep` in production).
pub async fn run_async<T, F, Fut, S, SleepFut>(mut sleep: S, mut op: F) -> Result<T, LlmError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, LlmError>>,
    S: FnMut(Duration) -> SleepFut,
    SleepFut: std::future::Future<Output = ()>,
{
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            sleep(backoff(attempt)).await;
        }
        match op(attempt).await {
            Err(error) if is_retryable(&error) && attempt + 1 < MAX_ATTEMPTS => {}
            result => return result,
        }
    }
    Err(LlmError::Provider("retry budget is empty".into()))
}

/// Bounded retry for safe Batch reads. Never use for submission: a lost create response may
/// already have incurred provider work. Callers retain their saved handle after exhaustion.
pub async fn batch_read<T, F, Fut>(mut op: F) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LlmError>>,
{
    run_async(tokio::time::sleep, |_| op()).await
}

/// [`batch_read`], but `stop` can end the quick retries early. The shared-service poll uses it
/// to hand a throttled answer (a `Retry-After` the server asked for, or a daily cap) straight
/// back to its own backoff instead of re-asking one, two and four seconds later — that burst is
/// exactly the load a `Retry-After` exists to prevent (issue #29 FR-SVC-02).
pub async fn batch_read_unless<T, F, Fut, S>(mut op: F, stop: S) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LlmError>>,
    S: Fn(&LlmError) -> bool,
{
    let mut last = None;
    for attempt in 0..MAX_ATTEMPTS {
        tokio::time::sleep(backoff(attempt)).await;
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if stop(&error) || !is_retryable(&error) => return Err(error),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| LlmError::Provider("retry budget is empty".into())))
}

// shared-service polling (issue #29 FR-SVC-02)
//
// Every device's nightly Batch poll (`GET /v1/batch/{id}` on the relay) draws on one shared
// service. A fixed one-minute cadence would make each device ask ~120 times a night, and a fleet
// in one time zone asks in lockstep. The policy below is the one schedule every such poll uses — kept
// here, next to the transport, rather than per route (FR-HN-09 discipline).

/// The wait before each poll, in minutes: 1 → 2 → 4 → 8, then every 15 (the cap).
pub const POLL_BACKOFF_MINUTES: [u64; 5] = [1, 2, 4, 8, 15];

/// The per-device, per-night poll target (spec §3: ≤ 40). The schedule above reaches it only
/// after ~9 hours of continuous waiting, longer than any night's window.
pub const MAX_POLLS_PER_NIGHT: u32 = 40;

/// Consecutive transient throttles (429/503 carrying `Retry-After`) tolerated in one run before
/// the device gives the night up and falls back to the local lane (spec §4-2).
pub const TRANSIENT_THROTTLE_LIMIT: u32 = 3;

/// The wait before poll number `poll_index` (0-based) under [`POLL_BACKOFF_MINUTES`].
pub fn poll_delay(poll_index: u32) -> Duration {
    let last = POLL_BACKOFF_MINUTES[POLL_BACKOFF_MINUTES.len() - 1];
    let minutes = POLL_BACKOFF_MINUTES
        .get(poll_index as usize)
        .copied()
        .unwrap_or(last);
    Duration::from_secs(minutes * 60)
}

/// The wait before the next poll: the server's `Retry-After` when it sent one — the server
/// knows its own load; the device only guesses — otherwise the schedule.
pub fn next_poll_delay(poll_index: u32, retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or_else(|| poll_delay(poll_index))
}

/// How many polls fit in `budget` of waiting under the schedule (a poll happens after each wait
/// that completes inside the budget). `const` so a caller can size its budget at compile time.
pub const fn polls_within(budget: Duration) -> u32 {
    let budget = budget.as_secs();
    let mut spent = 0u64;
    let mut polls = 0u32;
    loop {
        let idx = if (polls as usize) < POLL_BACKOFF_MINUTES.len() {
            polls as usize
        } else {
            POLL_BACKOFF_MINUTES.len() - 1
        };
        let wait = POLL_BACKOFF_MINUTES[idx] * 60;
        if spent + wait > budget {
            return polls;
        }
        spent += wait;
        polls += 1;
    }
}

/// How a shared service said "not now" (spec §4-2, `docs/batch-relay-design.md` §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedThrottle {
    /// The plan's daily cap is spent. Nothing the device can do tonight changes that, so the
    /// first one ends the night: the local lane runs.
    CapReached,
    /// A momentary overload (429 rate limit, or 503 with `Retry-After`). Wait as told, then ask
    /// again — at most [`TRANSIENT_THROTTLE_LIMIT`] times in a row.
    Transient(Duration),
}

/// Cap markers in a relay 429 body. The relay contract (`docs/batch-relay-design.md` §4.5, PR
/// #753) is that a cap answer carries **no** `Retry-After` and the device branches on that alone;
/// the reason codes are informational. They are still honoured here for one case: a relay that
/// predates that contract sent the daily cap *with* a `Retry-After` pointing at UTC midnight,
/// and reading that as transient would park the night on an hours-long wait.
const CAP_MARKERS: [&str; 3] = ["daily chunk cap reached", "\"daily_cap\"", "\"spend_cap\""];

/// Classify a non-success shared-service answer. `None` is "not a throttle" (an ordinary 4xx or
/// a 5xx with no `Retry-After`), which keeps its existing handling.
///
/// The rule (relay contract, PR #753): **branch on the presence of `Retry-After`.** A 429 without
/// one is a spent cap (daily chunks, or the daily spend breaker) — give the night up. A 429 or
/// 503 with one is transient — wait and ask again. A cap reason code in the body also reads as
/// the cap (see [`CAP_MARKERS`]).
pub fn classify_shared_throttle(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> Option<SharedThrottle> {
    match (status, retry_after) {
        (429, _) if CAP_MARKERS.iter().any(|marker| body.contains(marker)) => {
            Some(SharedThrottle::CapReached)
        }
        (429 | 503, Some(wait)) => Some(SharedThrottle::Transient(wait)),
        (429, None) => Some(SharedThrottle::CapReached),
        _ => None,
    }
}

/// Spread a server-requested wait: `wait + unit × wait` for `unit` in `0..=1` ("full jitter" on
/// top of the floor the server set). The relay answers an overloaded moment with the same
/// `Retry-After: 1` to everyone; a fleet that retries at exactly +1 s re-forms the burst it was
/// told to break up (PR #753's load test). The floor is never shortened — the server's interval
/// is a minimum.
pub fn spread_retry_after(wait: Duration, unit: f64) -> Duration {
    let unit = if unit.is_finite() { unit.clamp(0.0, 1.0) } else { 0.0 };
    wait.saturating_add(wait.mul_f64(unit))
}

/// A uniform draw in `0..=1` for [`spread_retry_after`]. Not for anything security-sensitive:
/// std's per-process random hasher keys, fed a counter and the clock, are plenty for spreading
/// retries without a new dependency.
pub fn jitter_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    h.write_u32(nanos);
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Wall-clock gate: after a 429, refuse new work until [`COOLDOWN_AFTER_429`] elapses.
#[derive(Debug)]
pub struct Cooldown {
    until_ms: AtomicI64,
}

impl Cooldown {
    pub const fn new() -> Self {
        Self {
            until_ms: AtomicI64::new(0),
        }
    }

    pub fn active(&self, now_ms: i64) -> bool {
        self.until_ms.load(Ordering::Acquire) > now_ms
    }

    pub fn arm(&self, now_ms: i64) {
        let extra = COOLDOWN_AFTER_429.as_millis() as i64;
        self.until_ms
            .store(now_ms.saturating_add(extra), Ordering::Release);
    }
}

impl Default for Cooldown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "net")]
    #[tokio::test]
    async fn async_retry_stops_at_budget_and_does_not_retry_auth_errors() {
        let mut calls = 0;
        let result = run_async(|_| std::future::ready(()), |_| {
            calls += 1;
            std::future::ready(Err::<(), _>(LlmError::RateLimited(429, String::new())))
        })
        .await;
        assert!(matches!(result, Err(LlmError::RateLimited(..))));
        assert_eq!(calls, MAX_ATTEMPTS);

        let mut calls = 0;
        let result = run_async(|_| std::future::ready(()), |_| {
            calls += 1;
            std::future::ready(Err::<(), _>(LlmError::Unauthorized(401, String::new())))
        })
        .await;
        assert!(matches!(result, Err(LlmError::Unauthorized(..))));
        assert_eq!(calls, 1);
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(is_retryable(&LlmError::RateLimited(429, String::new())));
        assert!(is_retryable(&LlmError::Provider(
            "messages HTTP 529".into()
        )));
        assert!(is_retryable(&LlmError::Provider(
            "messages HTTP 503".into()
        )));
        assert!(is_retryable(&LlmError::Transport(
            crate::llm::transport::TransportError::Io("reset".into())
        )));
        assert!(!is_retryable(&LlmError::Provider(
            "messages HTTP 400".into()
        )));
        assert!(!is_retryable(&LlmError::InvalidRequest));
        assert!(!is_retryable(&LlmError::Unauthorized(401, String::new())));
        assert!(!is_retryable(&LlmError::QuotaExhausted("x".into())));
        assert!(!is_retryable(&LlmError::Parse("x".into())));
        assert!(!is_retryable(&LlmError::NotConfigured));
    }

    #[test]
    fn backoff_is_zero_then_one_two_four_seconds() {
        assert_eq!(backoff(0), Duration::from_millis(0));
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(99), Duration::from_secs(4));
    }

    fn no_sleep(_: Duration) {}

    /// The schedule is slept, not just written down: 1s / 2s / 4s before attempts 2-4, nothing
    /// before the first, and nothing after a failure that will not be retried.
    #[test]
    fn retries_wait_out_the_backoff_schedule() {
        let mut slept = Vec::new();
        let _ = run(
            |d| slept.push(d),
            |_| Err::<(), _>(LlmError::RateLimited(429, String::new())),
        );
        assert_eq!(
            slept,
            vec![Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)]
        );

        let mut slept = Vec::new();
        let _ = run(
            |d| slept.push(d),
            |_| Err::<(), _>(LlmError::Unauthorized(401, String::new())),
        );
        assert!(slept.is_empty());

        let slept = std::cell::RefCell::new(Vec::new());
        let mut calls = 0;
        let got = block_on_ready(run_async(
            |d| {
                slept.borrow_mut().push(d);
                std::future::ready(())
            },
            |_| {
                calls += 1;
                std::future::ready(if calls < 3 {
                    Err(LlmError::Provider("messages HTTP 503".into()))
                } else {
                    Ok(7)
                })
            },
        ));
        assert_eq!(got.ok(), Some(7));
        assert_eq!(slept.into_inner(), vec![Duration::from_secs(1), Duration::from_secs(2)]);
    }

    /// Poll a future whose every await is already ready (the test sleeper and op above).
    fn block_on_ready<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn four_429s_exhaust_the_budget() {
        let mut hits = 0u32;
        let err = run(no_sleep, |_attempt| {
            hits += 1;
            Err::<(), _>(LlmError::RateLimited(429, String::new()))
        })
        .unwrap_err();
        assert_eq!(hits, MAX_ATTEMPTS);
        assert!(matches!(err, LlmError::RateLimited(429, _)));
    }

    #[test]
    fn a_success_after_two_429s_stops() {
        let mut hits = 0u32;
        let got = run(no_sleep, |_attempt| {
            hits += 1;
            if hits < 3 {
                Err(LlmError::RateLimited(429, String::new()))
            } else {
                Ok(7)
            }
        })
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(hits, 3);
    }

    #[test]
    fn a_5xx_is_retried_and_a_401_is_not() {
        let mut hits = 0u32;
        let _ = run(no_sleep, |_attempt| {
            hits += 1;
            Err::<(), _>(LlmError::Provider("messages HTTP 500".into()))
        });
        assert_eq!(hits, MAX_ATTEMPTS);

        hits = 0;
        let err = run(no_sleep, |_attempt| {
            hits += 1;
            Err::<(), _>(LlmError::Unauthorized(401, String::new()))
        })
        .unwrap_err();
        assert_eq!(hits, 1);
        assert!(matches!(err, LlmError::Unauthorized(401, _)));
    }

    #[test]
    fn poll_backoff_is_one_two_four_eight_then_fifteen_minutes() {
        let minutes: Vec<u64> = (0..8).map(|i| poll_delay(i).as_secs() / 60).collect();
        assert_eq!(minutes, vec![1, 2, 4, 8, 15, 15, 15, 15]);
        assert_eq!(poll_delay(u32::MAX), Duration::from_secs(15 * 60));
    }

    #[test]
    fn retry_after_overrides_the_schedule() {
        assert_eq!(next_poll_delay(0, None), Duration::from_secs(60));
        assert_eq!(next_poll_delay(0, Some(Duration::from_secs(7))), Duration::from_secs(7));
        assert_eq!(next_poll_delay(9, Some(Duration::from_secs(3_600))), Duration::from_secs(3_600));
    }

    /// Spec §3: at most 40 polls per device per night. Simulate the schedule over realistic
    /// Batch durations — a quick night, a slow one, and a whole night spent waiting (02:00 until
    /// 11:00) — and count the polls a device makes before it sees `ended`.
    #[test]
    fn nightly_poll_count_stays_within_the_target() {
        fn polls_until_ended(batch_minutes: u64) -> u32 {
            let mut waited = 0u64;
            let mut polls = 0u32;
            while waited < batch_minutes * 60 {
                waited += poll_delay(polls).as_secs();
                polls += 1;
            }
            polls
        }
        assert_eq!(polls_until_ended(1), 1);
        assert_eq!(polls_until_ended(15), 4);
        assert_eq!(polls_until_ended(60), 7);
        assert_eq!(polls_until_ended(120), 11);
        // A fixed one-minute cadence would make 540 polls over this night.
        let whole_night = polls_until_ended(9 * 60);
        assert!(whole_night <= MAX_POLLS_PER_NIGHT, "{whole_night} polls in a 9h night");
        assert_eq!(polls_within(Duration::from_secs(9 * 3_600)), whole_night);
    }

    #[test]
    fn a_retry_after_is_spread_but_never_shortened() {
        let one = Duration::from_secs(1);
        assert_eq!(spread_retry_after(one, 0.0), one);
        assert_eq!(spread_retry_after(one, 1.0), Duration::from_secs(2));
        assert_eq!(spread_retry_after(one, 0.5), Duration::from_millis(1_500));
        assert_eq!(spread_retry_after(one, -3.0), one, "clamped, never shorter");
        assert_eq!(spread_retry_after(one, f64::NAN), one);
        // Draws land in range and differ: 3,000 devices told "+1 s" do not all come back at +1 s.
        let draws: Vec<f64> = (0..200).map(|_| jitter_unit()).collect();
        assert!(draws.iter().all(|u| (0.0..=1.0).contains(u)));
        let distinct: std::collections::BTreeSet<u64> =
            draws.iter().map(|u| (u * 1_000.0) as u64).collect();
        assert!(distinct.len() > 100, "only {} distinct draws", distinct.len());
    }

    #[test]
    fn polls_within_counts_whole_waits_only() {
        assert_eq!(polls_within(Duration::ZERO), 0);
        assert_eq!(polls_within(Duration::from_secs(59)), 0);
        assert_eq!(polls_within(Duration::from_secs(60)), 1);
        assert_eq!(polls_within(Duration::from_secs(15 * 60)), 4);
        assert_eq!(polls_within(Duration::from_secs(2 * 3_600)), 11);
    }

    #[test]
    fn cap_and_transient_throttles_are_told_apart() {
        let secs = Duration::from_secs;
        // The relay's cap: reason code, with or without a Retry-After.
        let cap = r#"{"error":"daily chunk cap reached"}"#;
        assert_eq!(classify_shared_throttle(429, cap, None), Some(SharedThrottle::CapReached));
        assert_eq!(
            classify_shared_throttle(429, cap, Some(secs(6_400))),
            Some(SharedThrottle::CapReached)
        );
        // A rate limit with an interval is transient.
        let busy = r#"{"error":"too many requests"}"#;
        assert_eq!(
            classify_shared_throttle(429, busy, Some(secs(30))),
            Some(SharedThrottle::Transient(secs(30)))
        );
        // The relay contract (PR #753): a 429 with no Retry-After is a cap, whatever its body.
        assert_eq!(classify_shared_throttle(429, busy, None), Some(SharedThrottle::CapReached));
        let spend = r#"{"error":"daily chunk cap reached","reason":"spend_cap","resets_at":"2026-09-30T00:00:00Z"}"#;
        assert_eq!(classify_shared_throttle(429, spend, None), Some(SharedThrottle::CapReached));
        let limited = r#"{"error":"too many requests","reason":"rate_limited"}"#;
        assert_eq!(
            classify_shared_throttle(429, limited, Some(secs(2))),
            Some(SharedThrottle::Transient(secs(2)))
        );
        // 503 with Retry-After is transient; without, it is an ordinary 5xx.
        let metering = r#"{"error":"metering unavailable"}"#;
        assert_eq!(
            classify_shared_throttle(503, metering, Some(secs(1))),
            Some(SharedThrottle::Transient(secs(1)))
        );
        assert_eq!(classify_shared_throttle(503, metering, None), None);
        assert_eq!(classify_shared_throttle(500, "", Some(secs(5))), None);
        assert_eq!(classify_shared_throttle(404, "not found", None), None);
    }

    #[tokio::test(start_paused = true)]
    async fn batch_read_unless_stops_quick_retries_when_told() {
        let mut calls = 0;
        let result = batch_read_unless(
            || {
                calls += 1;
                std::future::ready(Err::<(), _>(LlmError::RateLimited(429, String::new())))
            },
            |_| true,
        )
        .await;
        assert!(matches!(result, Err(LlmError::RateLimited(..))));
        assert_eq!(calls, 1, "a throttled answer is handed back, not re-asked");

        let mut calls = 0;
        let result = batch_read_unless(
            || {
                calls += 1;
                std::future::ready(Err::<(), _>(LlmError::Provider("poll HTTP 502".into())))
            },
            |_| false,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls, MAX_ATTEMPTS, "an ordinary 5xx keeps its quick retries");
    }

    #[test]
    fn cooldown_blocks_until_the_window_elapses() {
        let gate = Cooldown::new();
        assert!(!gate.active(1_000));
        gate.arm(1_000);
        assert!(gate.active(1_000));
        assert!(gate.active(1_000 + 29_000));
        assert!(!gate.active(1_000 + 30_000));
    }
}
