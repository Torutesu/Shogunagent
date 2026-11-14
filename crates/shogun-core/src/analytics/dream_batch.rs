//! `dream_batch_sent`: one counts-only anonymous event per night for the Batch send reduction
//! (issue #286 / OPEN-B2; owner decision 2026-09-25 recorded in docs/batch-relay-design.md).
//!
//! The nightly telemetry row (`dream_batch_send_stats`, V54) stays on the device. This is the
//! small, coarse projection of it that answers "how much does the pre-filter save on a
//! representative week", sent through the existing opt-out analytics path
//! ([`AnalyticsHandle`](super::AnalyticsHandle), `analytics.json` `opt_out`): no new egress, no new
//! consent screen, and nothing at all from a device that opted out.
//!
//! What may ride along, and nothing else (CLAUDE.md: analytics carry feature counts only):
//! - `job` — the Batch job, one of [`JOBS`];
//! - `items_considered` / `items_sent` — how many events the night's window had and how many
//!   requests left the device, as a bucket from [`COUNT_BUCKETS`] (not the exact count: the size of
//!   a night's capture is a coarse activity signal, and the question needs only its order).
//!   `items_sent` counts **sessions** — one request per (app, window, hour); the series steps at
//!   2026-10-12 (PR #1022), when it stopped counting pre-filter-surviving rows
//!   (docs/batch-relay-design.md OPEN-B2);
//! - `token_reduction_pct` — the share of estimated input tokens kept on the device, rounded to
//!   the nearest 5 (an integer in `0..=100`).
//!
//! Every property value is a `&'static str` from the lists below or that bounded integer, built
//! from integers only: this module never sees captured text, a hash, an app name or a title.

use serde_json::Value;

use super::Props;

/// The PostHog event name.
pub const EVENT: &str = "dream_batch_sent";

/// The Batch jobs that record send telemetry (`dream_batch_send_stats.job`). A job name outside
/// this list sends nothing.
pub const JOBS: &[&str] = &["consolidation"];

/// The item-count buckets, in order. Coarse on purpose: the event answers "how big is a night",
/// not "exactly how much did this person capture".
pub const COUNT_BUCKETS: &[&str] = &[
    "0",
    "1_9",
    "10_49",
    "50_99",
    "100_249",
    "250_499",
    "500_999",
    "1000_2499",
    "2500_plus",
];

/// The bucket an item count falls into (always a member of [`COUNT_BUCKETS`]).
pub fn count_bucket(n: u64) -> &'static str {
    match n {
        0 => "0",
        1..=9 => "1_9",
        10..=49 => "10_49",
        50..=99 => "50_99",
        100..=249 => "100_249",
        250..=499 => "250_499",
        500..=999 => "500_999",
        1000..=2499 => "1000_2499",
        _ => "2500_plus",
    }
}

/// A reduction rate in `[0, 1]` as a percentage rounded to the nearest 5 (`0..=100`). Anything
/// outside the range (or not a number) clamps rather than reporting a value we never meant.
pub fn reduction_pct_5(rate: f64) -> u64 {
    if !rate.is_finite() || rate <= 0.0 {
        return 0;
    }
    let steps = (rate.min(1.0) * 20.0).round() as u64;
    (steps * 5).min(100)
}

/// The event for one night of `job`, or `None` when `job` is not one of [`JOBS`]. Takes counts
/// only: items considered and sent, and the estimated input tokens before and after the
/// pre-filter.
pub fn event_props(
    job: &str,
    items_considered: u64,
    items_sent: u64,
    est_tokens_before: u64,
    est_tokens_after: u64,
) -> Option<Props> {
    let job = JOBS.iter().copied().find(|j| *j == job)?;
    let rate = crate::dreamcycle::prefilter::reduction_rate(est_tokens_before, est_tokens_after);
    let mut props = Props::new();
    props.insert("job".into(), Value::from(job));
    props.insert("items_considered".into(), Value::from(count_bucket(items_considered)));
    props.insert("items_sent".into(), Value::from(count_bucket(items_sent)));
    props.insert("token_reduction_pct".into(), Value::from(reduction_pct_5(rate)));
    Some(props)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(job: &str, considered: u64, sent: u64, before: u64, after: u64) -> Props {
        event_props(job, considered, sent, before, after).unwrap()
    }

    #[test]
    fn every_property_is_a_fixed_word_or_a_bounded_number() {
        let cases = [
            (0, 0, 0, 0),
            (1, 1, 1_000, 1_000),
            (9, 3, 1_000, 650),
            (120, 80, 10_000, 6_300),
            (5_000, 4_999, 1, 0),
            (u64::MAX, u64::MAX, u64::MAX, 0),
            (10, 20, 100, 500),
        ];
        for (considered, sent, before, after) in cases {
            let p = props("consolidation", considered, sent, before, after);
            let keys: Vec<&str> = p.keys().map(String::as_str).collect();
            assert_eq!(keys, ["items_considered", "items_sent", "job", "token_reduction_pct"]);
            assert!(JOBS.contains(&p["job"].as_str().unwrap()));
            for key in ["items_considered", "items_sent"] {
                assert!(COUNT_BUCKETS.contains(&p[key].as_str().unwrap()), "{key}: {}", p[key]);
            }
            let pct = p["token_reduction_pct"].as_u64().unwrap();
            assert!(pct <= 100 && pct.is_multiple_of(5), "{pct}");
        }
    }

    #[test]
    fn an_unknown_job_sends_nothing() {
        for job in ["", "Consolidation", "consolidation ", "brief", "alice@example.com", "Slack"] {
            assert_eq!(event_props(job, 1, 1, 10, 5), None, "{job:?}");
        }
    }

    #[test]
    fn no_caller_text_can_ride_along() {
        // The job name that is sent is our literal, not the caller's string.
        let caller = String::from("consolidation");
        let p = props(&caller, 3, 2, 100, 50);
        let text = serde_json::Value::Object(p).to_string();
        for forbidden in ["hash", "title", "app", "window", "content", "text", "@"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[test]
    fn counts_fall_into_coarse_buckets() {
        for (n, bucket) in [
            (0, "0"),
            (1, "1_9"),
            (9, "1_9"),
            (10, "10_49"),
            (49, "10_49"),
            (50, "50_99"),
            (100, "100_249"),
            (250, "250_499"),
            (999, "500_999"),
            (1_000, "1000_2499"),
            (2_499, "1000_2499"),
            (2_500, "2500_plus"),
            (u64::MAX, "2500_plus"),
        ] {
            assert_eq!(count_bucket(n), bucket, "{n}");
        }
    }

    #[test]
    fn the_reduction_is_rounded_to_the_nearest_five() {
        for (rate, pct) in [
            (0.0, 0),
            (0.024, 0),
            (0.025, 5),
            (0.37, 35),
            (0.375, 40),
            (0.99, 100),
            (1.0, 100),
            (7.0, 100),
            (-0.3, 0),
            (f64::NAN, 0),
            (f64::INFINITY, 0),
        ] {
            assert_eq!(reduction_pct_5(rate), pct, "{rate}");
        }
        // From the token estimates: no evidence and a growing input both read 0.
        assert_eq!(props("consolidation", 0, 0, 0, 0)["token_reduction_pct"], 0);
        assert_eq!(props("consolidation", 1, 1, 100, 120)["token_reduction_pct"], 0);
        assert_eq!(props("consolidation", 4, 1, 1_000, 250)["token_reduction_pct"], 75);
    }
}
