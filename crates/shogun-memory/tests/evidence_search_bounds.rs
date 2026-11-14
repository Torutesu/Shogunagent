//! The federated local-search SLO fixture (NFR-SLO-04: 500 ms), asserted on **work done**, not on
//! wall-clock time.
//!
//! The fixture is the evidence-validation worst case: 100 matching meetings, so `limit` 100
//! candidates, each carrying 30 persisted evidence rows that must all be validated before the
//! meeting may be answered from. What keeps that inside the SLO is structural, and this file pins
//! the structure deterministically, so it holds the same on a loaded CI runner as on an idle
//! laptop:
//!
//! - every candidate is answer-eligible, and the result is capped at `limit`;
//! - validation is batched: the statements a search prepares grow with the candidates, never with
//!   the evidence rows behind them (no statement per evidence row);
//! - the SQLite VM work grows linearly in evidence rows, with a fixed per-row budget, and the whole
//!   search at the SLO fixture stays under a fixed instruction budget.
//!
//! The wall-clock p95 against 500 ms is a measurement, not a default-run assertion: a parallel
//! `cargo test --workspace`, or any other load, inflates elapsed time with no change to the code
//! (in a debug build it measures ~76 ms alone, and failed 16 of 20 runs next to 20 busy loops on
//! 4 cores). It lives in
//! `tests/search_slo.rs` as an explicit release-mode gate, which `.github/workflows/memory-bench.yml`
//! runs:
//!
//! ```text
//! cargo test -p shogun-memory --release --test search_slo -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use shogun_memory::search::{search_federated, FederatedSearchOptions, FederatedSearchResult};

use support::{add_session_evidence, evidence_heavy_meeting};

const QUERY: &str = "routine evidence";
/// Matching meetings: exactly enough to fill `limit`, as in the wall-clock gate.
const MEETINGS: usize = 100;
const LIMIT: usize = 100;
/// Evidence rows added per candidate in each of the three steps (10, 20, 30: the SLO fixture).
const STEP: usize = 10;
/// SQLite VM instructions per extra evidence row. Measured ~34 (a chunked `IN` probe plus the row
/// it binds); the budget leaves ~1.4x headroom for a SQLite upgrade that reshuffles opcodes, and
/// still fails a per-row query or a second pass over the evidence.
const VM_PER_EVIDENCE_ROW_BUDGET: u64 = 48;
/// The whole search at 100 candidates × 30 evidence rows. Measured 346,505; ~1.5x headroom.
const VM_SLO_FIXTURE_BUDGET: u64 = 520_000;
/// SELECT statements prepared per candidate. Measured 12 (per-candidate metadata and the
/// evidence tables it cites); a budget of 16 fails a new per-candidate query, not a reshuffle.
const SELECTS_PER_CANDIDATE_BUDGET: u64 = 16;

/// The work one search did: the result, SQLite VM instructions, and SELECT statements prepared.
struct Work {
    result: FederatedSearchResult,
    vm: u64,
    selects: u64,
}

/// Run one search and count its work. Both counters are exact and deterministic for a given
/// SQLite build, schema and data, unlike elapsed time: the progress handler fires on every VM
/// instruction, the authorizer once per SELECT (and sub-select) as each statement is prepared.
fn search(conn: &mut rusqlite::Connection, limit: usize) -> Work {
    let vm = Arc::new(AtomicU64::new(0));
    let selects = Arc::new(AtomicU64::new(0));
    let vm_counter = Arc::clone(&vm);
    let select_counter = Arc::clone(&selects);
    conn.progress_handler(
        1,
        Some(move || {
            vm_counter.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .expect("install VM counter");
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(context.action, AuthAction::Select) {
            select_counter.fetch_add(1, Ordering::Relaxed);
        }
        Authorization::Allow
    }))
    .expect("install SELECT counter");
    let result = search_federated(
        conn,
        QUERY,
        None,
        20_000,
        &FederatedSearchOptions::default(),
        limit,
    )
    .unwrap();
    conn.progress_handler(0, None::<fn() -> bool>)
        .expect("remove VM counter");
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove SELECT counter");
    Work {
        result,
        vm: vm.load(Ordering::Relaxed),
        selects: selects.load(Ordering::Relaxed),
    }
}

/// Merge the full-text segments each step's inserts created, so every search walks the same index
/// layout and the difference between steps is evidence validation alone.
fn optimize(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "INSERT INTO event_fts(event_fts) VALUES ('optimize');
         INSERT INTO event_grams_fts(event_grams_fts) VALUES ('optimize');",
    )
    .unwrap();
}

fn evidence_rows(result: &FederatedSearchResult) -> usize {
    result.hits.iter().map(|hit| hit.evidence_ids.len()).sum()
}

#[test]
fn evidence_validation_work_is_capped_by_candidates_and_linear_in_evidence() {
    // The SLO fixture, built in three equal steps of evidence per candidate: 10, 20, then 30.
    let mut conn = shogun_memory::open_in_memory().unwrap();
    let sessions: Vec<i64> = (0..MEETINGS)
        .map(|ordinal| evidence_heavy_meeting(&mut conn, ordinal, STEP))
        .collect();
    optimize(&conn);
    let mut steps = vec![search(&mut conn, LIMIT)];
    for from in [STEP, 2 * STEP] {
        for (ordinal, &session) in sessions.iter().enumerate() {
            add_session_evidence(&mut conn, session, ordinal, from, STEP);
        }
        optimize(&conn);
        steps.push(search(&mut conn, LIMIT));
    }
    let half = search(&mut conn, LIMIT / 2);
    let [at_10, at_20, at_30] = [&steps[0], &steps[1], &steps[2]];
    let rows = [at_10, at_20, at_30].map(|work| evidence_rows(&work.result));
    eprintln!(
        "federated evidence work (limit {LIMIT}): VM {} / {} / {}, SELECTs {} / {} / {}, evidence \
         rows {rows:?} at {STEP} / {} / {} per candidate; limit {}: VM {}, SELECTs {}",
        at_10.vm,
        at_20.vm,
        at_30.vm,
        at_10.selects,
        at_20.selects,
        at_30.selects,
        2 * STEP,
        3 * STEP,
        LIMIT / 2,
        half.vm,
        half.selects,
    );

    // Behaviour at the SLO fixture (and at every step): capped at `limit`, and every candidate
    // validated against complete persisted evidence, so every one is answer-eligible.
    for work in [at_10, at_20, at_30, &half] {
        let hits = &work.result.hits;
        assert!(!hits.is_empty(), "adversarial fixture must produce hits");
        assert!(
            hits.iter().all(|hit| hit.answer_eligible),
            "every candidate has complete persisted evidence"
        );
        assert!(hits.iter().all(|hit| !hit.evidence_ids.is_empty()));
    }
    assert_eq!(at_30.result.hits.len(), LIMIT, "capped at limit with {MEETINGS} matches");
    assert_eq!(half.result.hits.len(), LIMIT / 2);
    assert!(
        rows[2] >= MEETINGS * 3 * STEP,
        "each candidate carries its 30 evidence rows: {rows:?}"
    );

    // Batched validation: tripling the evidence behind the same candidates adds only the bind
    // chunks (900 ids each), never a statement per row. Statements follow the candidates.
    assert!(
        at_30.selects.saturating_sub(at_10.selects) <= 4,
        "statements must not grow with evidence rows: {} -> {}",
        at_10.selects,
        at_30.selects
    );
    let per_candidate = at_30.selects.saturating_sub(half.selects) / (LIMIT / 2) as u64;
    assert!(
        per_candidate <= SELECTS_PER_CANDIDATE_BUDGET,
        "{per_candidate} SELECTs per candidate (budget {SELECTS_PER_CANDIDATE_BUDGET})"
    );
    assert!(at_30.selects <= LIMIT as u64 * SELECTS_PER_CANDIDATE_BUDGET);

    // Linear in evidence rows, at a fixed per-row cost: two equal steps cost the same (±15%).
    let first = at_20.vm - at_10.vm;
    let second = at_30.vm - at_20.vm;
    let step_rows = (rows[2] - rows[1]) as u64;
    let ratio = second as f64 / first as f64;
    assert!(
        (0.85..=1.15).contains(&ratio),
        "evidence validation must be linear: +{first} then +{second} VM instructions (x{ratio:.3})"
    );
    assert!(
        second <= step_rows * VM_PER_EVIDENCE_ROW_BUDGET,
        "{second} VM instructions for {step_rows} more evidence rows (budget \
         {VM_PER_EVIDENCE_ROW_BUDGET}/row)"
    );

    // The whole search at the SLO fixture, under a fixed instruction budget.
    assert!(
        at_30.vm <= VM_SLO_FIXTURE_BUDGET,
        "{} VM instructions at the SLO fixture (budget {VM_SLO_FIXTURE_BUDGET})",
        at_30.vm
    );
}
