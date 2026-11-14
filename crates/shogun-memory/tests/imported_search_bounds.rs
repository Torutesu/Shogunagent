//! #391: the imported-claim lane at scale, asserted on **work done**, not on wall-clock time.
//!
//! `search_imported` scans `memory_import_items` with `instr` (there is no FTS table or projection
//! to keep in sync), so its cost is one pass over the committed claims. What keeps that inside the
//! local-search SLO is structural, and this file pins the structure deterministically so it holds
//! the same on a loaded CI runner as on an idle laptop:
//!
//! - the lane is capped (`LIMIT`) — 20,000 matching claims still hand back `limit` rows;
//! - the pass is linear: SQLite VM instructions grow in proportion to the claims, with a fixed
//!   per-claim budget (no per-row subquery, no second pass, no full sort);
//! - federated search admits at most `source_limit` imported candidates, never answer-eligible,
//!   ranked below observed memory, and `memory.import.delete` removes them at once.
//!
//! The wall-clock p95 against the 500 ms SLO (NFR-SLO-04) is a measurement, not a unit-test
//! assertion — a parallel `cargo test --workspace` inflates elapsed time from CPU contention alone.
//! It lives in `tests/search_slo.rs` as an explicit, release-mode gate:
//!
//! ```text
//! cargo test -p shogun-memory --release --test search_slo -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use shogun_memory::event_log::{insert, NewEvent};
use shogun_memory::memory_import::{delete_import, search_imported, CONFIDENCE_MAX};
use shogun_memory::search::{
    lexical_terms, search_federated, CanonicalEvidenceId, FederatedSearchOptions,
    FederatedSourceKind,
};

use support::insert_matching_imported_claims;

const QUERY: &str = "routine evidence";
const LANE_LIMIT: usize = 100;
const CLAIMS_PER_IMPORT: usize = 500;
/// SQLite VM instructions the lane may spend per committed claim. The shipped statement measures
/// 129 per claim (plus ~1,000 fixed); the budget leaves ~1.5x headroom for a SQLite upgrade that
/// reshuffles opcodes, and still fails a change that adds a correlated subquery or a second pass.
const VM_PER_CLAIM_BUDGET: u64 = 192;

/// Exact SQLite VM instruction count for `f` on `conn` (progress handler at granularity 1).
/// Deterministic for a given SQLite build, schema and data — unlike elapsed time.
fn vm_instructions<T>(
    conn: &mut rusqlite::Connection,
    f: impl FnOnce(&rusqlite::Connection) -> T,
) -> (T, u64) {
    let count = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&count);
    conn.progress_handler(
        1,
        Some(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .expect("install VM counter");
    let out = f(conn);
    conn.progress_handler(0, None::<fn() -> bool>)
        .expect("remove VM counter");
    (out, count.load(Ordering::Relaxed))
}

fn observed_event(conn: &rusqlite::Connection, ts: i64, text: &str, hash: &str) -> i64 {
    insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: Some("Evidence"),
            content: text,
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

#[test]
fn imported_claim_lane_work_is_capped_and_linear_at_twenty_thousand_items() {
    let mut conn = shogun_memory::open_in_memory().unwrap();
    let observed: Vec<i64> = (0..20)
        .map(|i| {
            observed_event(
                &conn,
                9_000 + i,
                "Routine evidence review",
                &format!("observed-{i}"),
            )
        })
        .collect();
    let terms = lexical_terms(QUERY);

    // 10,000 claims, then 20,000: the lane's instruction count must scale with the claims.
    let mut import_ids = insert_matching_imported_claims(&mut conn, 20, CLAIMS_PER_IMPORT);
    let (half_lane, half_ops) = vm_instructions(&mut conn, |conn| {
        search_imported(conn, &terms, 0.5, LANE_LIMIT).unwrap()
    });
    import_ids.extend(insert_matching_imported_claims(
        &mut conn,
        20,
        CLAIMS_PER_IMPORT,
    ));
    let (lane, full_ops) = vm_instructions(&mut conn, |conn| {
        search_imported(conn, &terms, 0.5, LANE_LIMIT).unwrap()
    });
    let claims = (import_ids.len() * CLAIMS_PER_IMPORT) as u64;
    eprintln!(
        "imported lane VM instructions: {half_ops} at {} claims, {full_ops} at {claims} claims \
         ({} per claim)",
        claims / 2,
        full_ops / claims
    );

    // Capped: every claim matches, the lane returns exactly `limit`, best coverage then newest.
    assert_eq!(half_lane.len(), LANE_LIMIT);
    assert_eq!(lane.len(), LANE_LIMIT);
    assert!(lane.iter().all(|m| m.matched_terms == 2));
    assert!(
        lane.windows(2).all(|w| w[0].item_id > w[1].item_id),
        "newest first on ties"
    );
    assert_eq!(lane[0].import_id, *import_ids.last().unwrap());
    assert!(lane.iter().all(|m| m.confidence <= CONFIDENCE_MAX));

    // Linear: one pass, fixed per-claim cost. Doubling the claims doubles the work (±5%).
    let ratio = full_ops as f64 / half_ops as f64;
    assert!(
        (1.9..=2.1).contains(&ratio),
        "lane work must be linear in claims: {half_ops} -> {full_ops} (x{ratio:.3})"
    );
    assert!(
        full_ops <= claims * VM_PER_CLAIM_BUDGET,
        "lane spent {full_ops} VM instructions on {claims} claims (budget {VM_PER_CLAIM_BUDGET}/claim)"
    );

    // Federated: the lane contributes at most `source_limit` candidates, never as fact, and
    // observed memory with the same match outranks every imported claim.
    let result = search_federated(
        &conn,
        QUERY,
        None,
        20_000,
        &FederatedSearchOptions::default(),
        LANE_LIMIT,
    )
    .unwrap();
    let imported: Vec<_> = result
        .hits
        .iter()
        .filter(|hit| hit.source_kind.is_imported())
        .collect();
    assert!(!imported.is_empty(), "imported claims are recalled");
    assert!(imported.len() <= LANE_LIMIT);
    for hit in &imported {
        assert_eq!(
            hit.source_kind,
            FederatedSourceKind::Imported(
                shogun_memory::memory_import::package::Platform::ComputerHistory
            )
        );
        assert!(
            !hit.answer_eligible,
            "an imported claim is never stated as fact"
        );
        assert!(hit.confidence <= CONFIDENCE_MAX);
        assert!(matches!(
            hit.evidence_ids.as_slice(),
            [CanonicalEvidenceId::MemoryImport(id)] if import_ids.contains(id)
        ));
    }
    let first_imported = result
        .hits
        .iter()
        .position(|hit| hit.source_kind.is_imported())
        .unwrap();
    let last_observed = result
        .hits
        .iter()
        .rposition(|hit| !hit.source_kind.is_imported())
        .unwrap();
    assert!(
        last_observed < first_imported,
        "observed memory ranks above imported claims at equal match"
    );
    assert_eq!(
        result.hits.len() - imported.len(),
        observed.len(),
        "every observed match is kept"
    );

    // Undo the newest import: its claims leave the lane and federated search in the same step.
    let newest = *import_ids.last().unwrap();
    assert!(delete_import(&mut conn, newest).unwrap());
    let lane = search_imported(&conn, &terms, 0.5, LANE_LIMIT).unwrap();
    assert_eq!(lane.len(), LANE_LIMIT);
    assert!(lane.iter().all(|m| m.import_id != newest));
    let after = search_federated(
        &conn,
        QUERY,
        None,
        20_000,
        &FederatedSearchOptions::default(),
        LANE_LIMIT,
    )
    .unwrap();
    assert!(after.hits.iter().all(|hit| !hit
        .evidence_ids
        .contains(&CanonicalEvidenceId::MemoryImport(newest))));
}
