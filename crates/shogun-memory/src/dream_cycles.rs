//! Durable Full Dream Cycle identity and provider Batch lifecycle.

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::event_log::{
    push_partitioned_event, BatchEventMeta, EventText, PartitionedEvents, BATCH_EXCLUDED_SOURCES,
};

/// Whether required effects for a frozen Full cycle are still in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleStatus {
    Running,
    Failed,
    Abandoned,
    Done,
}

impl CycleStatus {
    fn parse(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "running" => Ok(Self::Running),
            "failed" => Ok(Self::Failed),
            "abandoned" => Ok(Self::Abandoned),
            "done" => Ok(Self::Done),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// Durable provider lifecycle for a Full cycle's cloud partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStage {
    /// Range frozen; no provider handle has been recorded.
    Pending,
    /// One leased runner owns the right to call the provider, before a handle exists.
    Submitting,
    /// Provider accepted the Batch and its handle plus expected ids are durable.
    Submitted,
    /// Exact-once, terminal, parseable results were fetched and validated.
    ResultsReady,
    /// This cycle intentionally uses the existing local fallback and has no provider Batch.
    Local,
    /// Retry budget exhausted; frozen text was released and coverage moves to the next cycle.
    Abandoned,
    /// Every required job effect completed.
    Complete,
}

impl BatchStage {
    fn parse(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "submitting" => Ok(Self::Submitting),
            "submitted" => Ok(Self::Submitted),
            "results_ready" => Ok(Self::ResultsReady),
            "local" => Ok(Self::Local),
            "abandoned" => Ok(Self::Abandoned),
            "complete" => Ok(Self::Complete),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// One frozen Full cycle and the provider information needed after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamCycleRow {
    pub cycle_id: String,
    pub input_from_ts: i64,
    pub input_to_ts: i64,
    pub status: CycleStatus,
    pub batch_stage: BatchStage,
    pub provider_batch_id: Option<String>,
    pub run_owner: Option<String>,
    pub run_generation: i64,
    pub run_lease_until: Option<i64>,
    pub failure_count: i64,
    pub snapshot_max_arrival_seq: i64,
    pub expected_custom_ids: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Frozen active inputs plus cloud ids that explicit user deletion tombstoned after submission.
#[derive(Debug, Clone, Default)]
pub struct FrozenInputs {
    pub partitioned: PartitionedEvents,
    pub deleted_cloud_event_ids: Vec<i64>,
}

/// Failed owned attempts allowed before a cycle releases its frozen input and becomes terminal.
pub const MAX_FAILURE_ATTEMPTS: i64 = 3;

/// Stable trigger message used to distinguish an intentional Dream pin from a storage failure.
pub const PINNED_EVENT_ERROR: &str = "event pinned by unfinished Dream cycle";

/// Freeze a Full cycle before any provider submission. Repeating the call preserves the first
/// range rather than refreshing it from a later scheduler tick.
pub fn freeze_full_cycle(
    conn: &mut Connection,
    cycle_id: &str,
    input_from_ts: i64,
    input_to_ts: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    let carry_from: Option<i64> = tx.query_row(
        "SELECT MIN(input_from_ts) FROM dream_cycles
         WHERE status = 'abandoned' AND coverage_carried = 0",
        [],
        |row| row.get(0),
    )?;
    let effective_input_from_ts = carry_from
        .map(|abandoned_from| abandoned_from.min(input_from_ts))
        .unwrap_or(input_from_ts);
    let previous_max: i64 = tx.query_row(
        "SELECT COALESCE(MAX(snapshot_max_arrival_seq), 0) FROM dream_cycles",
        [],
        |row| row.get(0),
    )?;
    let has_previous_cycle: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM dream_cycles)", [], |row| {
            row.get(0)
        })?;
    let snapshot_max: i64 = tx.query_row(
        "SELECT COALESCE(MAX(arrival_seq), 0) FROM dream_event_arrivals",
        [],
        |row| row.get(0),
    )?;
    let inserted = tx.execute(
        "INSERT INTO dream_cycles
           (cycle_id, input_from_ts, input_to_ts, status, batch_stage, provider_batch_id,
            snapshot_max_arrival_seq, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'running', 'pending', NULL, ?4, ?5, ?5)
         ON CONFLICT (cycle_id) DO NOTHING",
        params![
            cycle_id,
            effective_input_from_ts,
            input_to_ts,
            snapshot_max,
            now
        ],
    )?;
    if inserted == 1 {
        tx.execute(
            "INSERT INTO dream_cycle_inputs
                 (cycle_id, arrival_seq, event_id, event_ts, source, content)
             SELECT ?1, a.arrival_seq, e.id, e.ts, e.source, e.content
             FROM dream_event_arrivals a
             JOIN event_log e ON e.id = a.event_id
             WHERE a.arrival_seq <= ?2
               AND a.arrival_seq = (
                   SELECT MAX(latest.arrival_seq)
                   FROM dream_event_arrivals latest
                   WHERE latest.event_id = a.event_id
               )
               AND e.ts < ?3
               AND ((e.ts >= ?4) OR (?5 AND a.arrival_seq > ?6))
               AND NOT EXISTS (
                   SELECT 1 FROM dream_cycle_inputs claimed
                   WHERE claimed.arrival_seq = a.arrival_seq
               )
             ORDER BY e.ts, e.id",
            params![
                cycle_id,
                snapshot_max,
                input_to_ts,
                effective_input_from_ts,
                has_previous_cycle,
                previous_max
            ],
        )?;
        tx.execute(
            "UPDATE dream_cycles SET coverage_carried = 1
             WHERE status = 'abandoned' AND coverage_carried = 0",
            [],
        )?;
    }
    tx.commit()
}

/// Read the minimized local snapshot. Canonical retention/mutation cannot alter this text; an
/// explicit deletion removes the text and leaves only the cloud id needed for result accounting.
///
/// The snapshot holds the text, time and source; the session-grouping columns (`kind`,
/// `app_bundle_id`, `window_title`) are read from the live `event_log` row, which the
/// `dream_cycle_inputs_pin_delete` trigger keeps in place for every active input of an unfinished
/// cycle. The pin protects the row's existence and (`dream_cycle_inputs_pin_update`) its `ts`,
/// `source`, `content` and `content_hash` — **not** `kind`, `app_bundle_id` or `window_title`:
/// nothing in the product rewrites those today, but an update to them on a pinned row would not
/// be refused. If one ever changed between submission and resume, the affected rows would fold
/// into a session with a different key, hence a `custom_id` the submitting binary never sent, and
/// `submitted_ids_still_valid` fails that night closed (no anchor is resolved against lines the
/// model did not see) rather than misattributing a span. A row that is gone anyway (only possible
/// after an explicit deletion, which tombstones the input first) is grouped on the snapshot's own
/// columns.
pub fn frozen_inputs(conn: &Connection, cycle_id: &str) -> Result<FrozenInputs, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT i.event_id, i.content, i.source, i.disposition, i.event_ts,
                COALESCE(e.kind, 'text'), e.app_bundle_id, e.window_title, e.thread_key
         FROM dream_cycle_inputs i
         LEFT JOIN event_log e ON e.id = i.event_id
         WHERE i.cycle_id = ?1
         ORDER BY i.event_ts, i.event_id",
    )?;
    let mut rows = stmt.query([cycle_id])?;
    let mut out = FrozenInputs::default();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let content: Option<String> = row.get(1)?;
        let source: String = row.get(2)?;
        let disposition: String = row.get(3)?;
        if disposition == "deleted_explicit" {
            if !BATCH_EXCLUDED_SOURCES.contains(&source.as_str()) {
                out.deleted_cloud_event_ids.push(id);
            }
            continue;
        }
        let content = content.ok_or(rusqlite::Error::InvalidQuery)?;
        let meta = BatchEventMeta {
            id,
            ts: row.get(4)?,
            source,
            kind: row.get(5)?,
            app_bundle_id: row.get(6)?,
            window_title: row.get(7)?,
            thread_key: row.get(8)?,
        };
        push_partitioned_event(&mut out.partitioned, meta, content);
    }
    Ok(out)
}

/// The same verified snapshot in original event order for local Consolidation. This preserves
/// deterministic candidate/provenance precedence when cloud-eligible and device-only sources were
/// interleaved in the captured stream.
pub fn frozen_events(conn: &Connection, cycle_id: &str) -> Result<Vec<EventText>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT event_id, content
         FROM dream_cycle_inputs
         WHERE cycle_id = ?1 AND disposition = 'active'
         ORDER BY event_ts, event_id",
    )?;
    let rows = stmt
        .query_map([cycle_id], |row| {
            Ok(EventText {
                id: row.get(0)?,
                content: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Honor an explicit user deletion without stranding accepted Batch work. Snapshot text is erased
/// immediately; the surviving id is only result-accounting evidence and is removed at completion.
pub fn mark_event_deleted_explicit(
    conn: &Connection,
    event_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE dream_cycle_inputs
         SET content = NULL, disposition = 'deleted_explicit'
         WHERE event_id = ?1 AND disposition = 'active'",
        [event_id],
    )?;
    Ok(())
}

/// Prefer actionable cycles; rotate unknown creates so one cannot hide later recoverable jobs.
pub fn oldest_unfinished(conn: &Connection) -> Result<Option<DreamCycleRow>, rusqlite::Error> {
    oldest_unfinished_matching(conn, |_| true)
}

/// [`oldest_unfinished`] among the hourly windows only: a nightly cycle (failed, parked or held)
/// never stands in for the hourly lane's own backlog.
pub fn oldest_unfinished_hourly(conn: &Connection) -> Result<Option<DreamCycleRow>, rusqlite::Error> {
    oldest_unfinished_matching(conn, crate::jobs::is_hourly_cycle_id)
}

fn oldest_unfinished_matching(
    conn: &Connection,
    keep: impl Fn(&str) -> bool,
) -> Result<Option<DreamCycleRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT cycle_id,
                provider_batch_id IS NULL AND failure_count >= ?1 AND EXISTS (
                    SELECT 1 FROM dream_batch_items i WHERE i.cycle_id = dream_cycles.cycle_id
                ) AS parked
         FROM dream_cycles
         WHERE status IN ('running', 'failed')
         ORDER BY parked ASC,
                  CASE WHEN parked THEN updated_at ELSE created_at END ASC,
                  failure_count ASC, created_at ASC, cycle_id ASC",
    )?;
    let ids = stmt.query_map([MAX_FAILURE_ATTEMPTS], |row| row.get::<_, String>(0))?;
    for id in ids {
        let id = id?;
        if keep(&id) {
            return get(conn, &id);
        }
    }
    Ok(None)
}

/// The newest night whose Full cycle completed (`YYYYMMDD` ids sort as dates) — the night the
/// result on screen describes (issue #285).
pub fn latest_done(conn: &Connection) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT cycle_id FROM dream_cycles WHERE status = 'done'
         ORDER BY cycle_id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

/// Read one cycle, including the exact set of submitted custom ids.
pub fn get(conn: &Connection, cycle_id: &str) -> Result<Option<DreamCycleRow>, rusqlite::Error> {
    let mut cycle = conn
        .query_row(
            "SELECT cycle_id, input_from_ts, input_to_ts, status, batch_stage,
                    provider_batch_id, run_owner, run_generation, run_lease_until,
                    failure_count, snapshot_max_arrival_seq,
                    created_at, updated_at
             FROM dream_cycles WHERE cycle_id = ?1",
            [cycle_id],
            |row| {
                let status = CycleStatus::parse(&row.get::<_, String>(3)?)?;
                let batch_stage = BatchStage::parse(&row.get::<_, String>(4)?)?;
                Ok(DreamCycleRow {
                    cycle_id: row.get(0)?,
                    input_from_ts: row.get(1)?,
                    input_to_ts: row.get(2)?,
                    status,
                    batch_stage,
                    provider_batch_id: row.get(5)?,
                    run_owner: row.get(6)?,
                    run_generation: row.get(7)?,
                    run_lease_until: row.get(8)?,
                    failure_count: row.get(9)?,
                    snapshot_max_arrival_seq: row.get(10)?,
                    expected_custom_ids: Vec::new(),
                    created_at: row.get(11)?,
                    updated_at: row.get(12)?,
                })
            },
        )
        .optional()?;
    if let Some(ref mut cycle) = cycle {
        let mut stmt = conn.prepare(
            "SELECT custom_id FROM dream_batch_items WHERE cycle_id = ?1 ORDER BY custom_id",
        )?;
        cycle.expected_custom_ids = stmt
            .query_map([cycle_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
    }
    Ok(cycle)
}

/// Claim exclusive execution of an unfinished cycle. An expired `submitting` lease is reset to
/// pending: the provider-accept-before-handle-persist interval is irreducible without provider
/// idempotency, but ordinary concurrent runners cannot enter it together.
pub fn claim_run(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    now: i64,
    lease_until: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "UPDATE dream_cycles
         SET status = 'running',
             batch_stage = CASE
                 WHEN batch_stage = 'submitting'
                  AND (run_owner IS NULL OR run_lease_until <= ?3)
                 THEN 'pending'
                 ELSE batch_stage
             END,
             run_owner = ?2, run_generation = run_generation + 1,
             run_lease_until = ?4, updated_at = ?3
         WHERE cycle_id = ?1 AND status IN ('running', 'failed')
           AND (run_owner IS NULL OR run_lease_until <= ?3)
         RETURNING run_generation",
        params![cycle_id, owner, now, lease_until],
        |row| row.get(0),
    )
    .optional()
}

/// Renew a live lease only for its exact owner and fencing generation.
pub fn renew_run(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
    lease_until: i64,
) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE dream_cycles
         SET run_lease_until = ?5, updated_at = ?4
         WHERE cycle_id = ?1 AND status IN ('running', 'failed') AND run_owner = ?2
           AND run_generation = ?3 AND run_lease_until > ?4",
        params![cycle_id, owner, generation, now, lease_until],
    )?;
    Ok(changed == 1)
}

/// Check one live fenced owner while the caller holds the same connection lock as its following
/// effect operation.
pub fn owns_live_run(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM dream_cycles
             WHERE cycle_id = ?1 AND status IN ('running', 'failed') AND run_owner = ?2
               AND run_generation = ?3 AND run_lease_until > ?4
         )",
        params![cycle_id, owner, generation, now],
        |row| row.get(0),
    )
}

/// Fail and release only the exact fenced owner. A stale generation cannot release its successor.
/// The final allowed failure atomically abandons the cycle and erases all text-bearing snapshots.
pub fn release_run(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    release_run_inner(conn, cycle_id, owner, generation, now, None, false)
}

/// [`release_run`] for a window refused for its content: at the limit its inputs stay claimed
/// (text dropped, as an explicit delete does) and its coverage is not carried, so the next
/// freeze never re-sends what was refused. Those events keep their local-rule candidates.
pub fn release_run_refused(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    release_run_inner(conn, cycle_id, owner, generation, now, None, true)
}

/// Like [`release_run`] but never abandons: for a shared-service outage (relay down, throttled,
/// poll budget spent) the paid-for window must stay resumable, not be dropped after a few tries.
pub fn release_run_retryable(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    release_run_inner(conn, cycle_id, owner, generation, now, Some(i64::MAX), false)
}

fn release_run_inner(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
    forced_limit: Option<i64>,
    refused: bool,
) -> Result<bool, rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    // A lost create response is not a failed provider job. Keep its original identity beyond
    // the normal retry budget; abandoning it would send the same frozen input under a new id.
    // A broken snapshot read cannot prove non-submission. Preserve identity, but still release
    // the worker lease so repairing that table allows an immediate fenced retry.
    let unresolved: bool = tx.query_row(
        "SELECT provider_batch_id IS NULL AND EXISTS (
             SELECT 1 FROM dream_batch_items WHERE cycle_id = ?1
         ) FROM dream_cycles WHERE cycle_id = ?1",
        [cycle_id],
        |row| row.get(0),
    ).optional().map(|value| value.unwrap_or(false)).unwrap_or(true);
    let failure_limit = match forced_limit {
        Some(limit) => limit,
        None if unresolved => i64::MAX,
        None => MAX_FAILURE_ATTEMPTS,
    };
    let status = tx
        .query_row(
            "UPDATE dream_cycles
         SET failure_count = failure_count + 1,
             status = CASE
                 WHEN failure_count + 1 >= ?5 THEN 'abandoned'
                 ELSE 'failed'
             END,
             batch_stage = CASE
                 WHEN failure_count + 1 >= ?5 THEN 'abandoned'
                 WHEN batch_stage = 'submitting' THEN 'pending'
                 ELSE batch_stage
             END,
             run_owner = NULL, run_lease_until = NULL, updated_at = ?4
         WHERE cycle_id = ?1 AND status IN ('running', 'failed') AND run_owner = ?2
           AND run_generation = ?3
         RETURNING status",
            params![cycle_id, owner, generation, now, failure_limit],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(status) = status else {
        return Ok(false);
    };
    if status == "abandoned" && refused {
        tx.execute(
            "UPDATE dream_cycle_inputs SET content = NULL, disposition = 'deleted_explicit' WHERE cycle_id = ?1",
            [cycle_id],
        )?;
        tx.execute("UPDATE dream_cycles SET coverage_carried = 1 WHERE cycle_id = ?1", [cycle_id])?;
    } else if status == "abandoned" {
        tx.execute(
            "DELETE FROM dream_cycle_inputs WHERE cycle_id = ?1",
            [cycle_id],
        )?;
        tx.execute(
            "DELETE FROM dream_batch_items WHERE cycle_id = ?1",
            [cycle_id],
        )?;
        tx.execute(
            "DELETE FROM dream_brief_effects WHERE cycle_id = ?1",
            [cycle_id],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Free every lease whose owner does not match `mine` (a SQL LIKE pattern for this process).
/// A `submitting` stage returns to `pending` exactly as an expired lease does in [`claim_run`].
pub fn release_foreign_leases(conn: &Connection, mine: &str) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "UPDATE dream_cycles
         SET batch_stage = CASE WHEN batch_stage = 'submitting' THEN 'pending' ELSE batch_stage END,
             run_owner = NULL, run_lease_until = NULL
         WHERE run_owner IS NOT NULL AND run_owner NOT LIKE ?1
           AND status IN ('running', 'failed')",
        [mine],
    )
}

/// Shrink an owned, not-yet-submitted cycle so it holds at most `cap` Batch-eligible inputs: keep
/// every input strictly before the `cap + 1`-th eligible one (by time), release the rest, and pull
/// `input_to_ts` back to that boundary. The released events stay unconsumed, so the next window
/// (cursor = this cycle's end) picks them up in order. `false` = nothing to split, or no clean
/// time boundary (the caller then truncates the submission instead).
pub fn split_oversized(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    cap: usize,
) -> Result<bool, rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    let from: Option<i64> = tx
        .query_row(
            "SELECT input_from_ts FROM dream_cycles
             WHERE cycle_id = ?1 AND status = 'running' AND run_owner = ?2 AND run_generation = ?3
               AND batch_stage = 'pending' AND provider_batch_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM dream_batch_items WHERE cycle_id = ?1)",
            params![cycle_id, owner, generation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(from) = from else {
        return Ok(false);
    };
    let marks = vec!["?"; BATCH_EXCLUDED_SOURCES.len()].join(",");
    let eligible = format!(
        "FROM dream_cycle_inputs WHERE cycle_id = ?1 AND disposition = 'active' AND source NOT IN ({marks})"
    );
    let args = |extra: Option<i64>| -> Vec<rusqlite::types::Value> {
        let mut v: Vec<rusqlite::types::Value> = vec![cycle_id.to_string().into()];
        v.extend(BATCH_EXCLUDED_SOURCES.iter().map(|s| rusqlite::types::Value::from(s.to_string())));
        v.extend(extra.map(Into::into));
        v
    };
    let boundary: Option<i64> = tx
        .query_row(
            &format!("SELECT event_ts {eligible} ORDER BY event_ts, event_id LIMIT 1 OFFSET {cap}"),
            rusqlite::params_from_iter(args(None)),
            |row| row.get(0),
        )
        .optional()?;
    let Some(boundary) = boundary else {
        return Ok(false);
    };
    let kept: i64 = tx.query_row(
        &format!("SELECT count(*) {eligible} AND event_ts < ?{}", BATCH_EXCLUDED_SOURCES.len() + 2),
        rusqlite::params_from_iter(args(Some(boundary))),
        |row| row.get(0),
    )?;
    if kept == 0 || boundary <= from {
        return Ok(false);
    }
    tx.execute(
        "DELETE FROM dream_cycle_inputs WHERE cycle_id = ?1 AND event_ts >= ?2",
        params![cycle_id, boundary],
    )?;
    tx.execute(
        "UPDATE dream_cycles SET input_to_ts = ?2 WHERE cycle_id = ?1",
        params![cycle_id, boundary],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Reserve the sole right to call the provider before making the non-transactional request.
pub fn claim_submission(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
    expected_custom_ids: &[String],
) -> Result<bool, rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE dream_cycles
         SET batch_stage = 'submitting', updated_at = ?4
         WHERE cycle_id = ?1 AND status IN ('running', 'failed') AND batch_stage = 'pending'
           AND provider_batch_id IS NULL AND run_owner = ?2 AND run_generation = ?3
           AND run_lease_until > ?4",
        params![cycle_id, owner, generation, now],
    )?;
    if changed == 1 {
        replace_expected_ids(&tx, cycle_id, expected_custom_ids)?;
    }
    tx.commit()?;
    Ok(changed == 1)
}

/// Atomically persist the provider handle and the exact submitted ids.
pub fn mark_submitted(
    conn: &mut Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    provider_batch_id: &str,
    expected_custom_ids: &[String],
    now: i64,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE dream_cycles
         SET status = 'running', batch_stage = 'submitted', provider_batch_id = ?2,
             updated_at = ?5
         WHERE cycle_id = ?1 AND batch_stage = 'submitting' AND provider_batch_id IS NULL
           AND run_owner = ?3 AND run_generation = ?4 AND run_lease_until > ?5",
        params![cycle_id, provider_batch_id, owner, generation, now],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    replace_expected_ids(&tx, cycle_id, expected_custom_ids)?;
    tx.commit()
}

fn replace_expected_ids(
    tx: &Transaction<'_>,
    cycle_id: &str,
    expected_custom_ids: &[String],
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "DELETE FROM dream_batch_items WHERE cycle_id = ?1",
        [cycle_id],
    )?;
    for custom_id in expected_custom_ids {
        tx.execute(
            "INSERT INTO dream_batch_items (cycle_id, custom_id) VALUES (?1, ?2)",
            params![cycle_id, custom_id],
        )?;
    }
    Ok(())
}

/// Record that this cycle intentionally uses the local classifier fallback.
pub fn mark_local(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE dream_cycles
         SET status = 'running', batch_stage = 'local', updated_at = ?4
         WHERE cycle_id = ?1 AND batch_stage = 'pending' AND provider_batch_id IS NULL
           AND run_owner = ?2 AND run_generation = ?3 AND run_lease_until > ?4",
        params![cycle_id, owner, generation, now],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::StatementChangedRows(changed))
    }
}

/// Record that provider results passed exact-once completeness validation.
pub fn mark_results_ready(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE dream_cycles
         SET status = 'running', batch_stage = 'results_ready', updated_at = ?4
         WHERE cycle_id = ?1 AND batch_stage IN ('submitted', 'results_ready')
           AND provider_batch_id IS NOT NULL AND run_owner = ?2 AND run_generation = ?3
           AND run_lease_until > ?4",
        params![cycle_id, owner, generation, now],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::StatementChangedRows(changed))
    }
}

/// Fail an owned unfinished cycle and release its lease. Before the retry limit, a submitting state
/// returns to Pending so a later lease may recover; the provider-accept-before-handle-persist caveat
/// still applies. At the limit, [`release_run`] makes the cycle terminal and releases its pins.
pub fn mark_failed(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let changed = release_run(conn, cycle_id, owner, generation, now)? as usize;
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}

/// Results lost at the provider (relay HTTP 410): abandon at once, releasing the frozen inputs so
/// the same events are re-frozen into a fresh window and a NEW batch (no point retrying a read of
/// files that no longer exist).
pub fn mark_abandoned(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    if release_run_inner(conn, cycle_id, owner, generation, now, Some(1), false)? {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}

/// [`mark_failed`] for an outage: counts the failure (ordering) but never abandons.
pub fn mark_failed_retryable(
    conn: &Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    if release_run_retryable(conn, cycle_id, owner, generation, now)? {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}

/// Mark every cycle-level effect complete only after all per-job effects reached `done`.
pub fn mark_complete(
    conn: &mut Connection,
    cycle_id: &str,
    owner: &str,
    generation: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    // Capture the lane before completion replaces batch_stage. Receipts and completion commit
    // together; a lost lease rolls the receipts back too. Explicitly deleted inputs get none.
    tx.execute(
        "INSERT INTO event_processing_receipts (event_id, arrival_seq, reviewed_at, lane)
         SELECT i.event_id, i.arrival_seq, ?2,
                CASE WHEN c.batch_stage = 'local' THEN 'local' ELSE 'batch' END
         FROM dream_cycle_inputs i JOIN dream_cycles c ON c.cycle_id = i.cycle_id
         JOIN event_log e ON e.id = i.event_id
         WHERE i.cycle_id = ?1 AND i.disposition = 'active'
         ON CONFLICT(event_id) DO UPDATE SET arrival_seq = excluded.arrival_seq,
             reviewed_at = excluded.reviewed_at, lane = excluded.lane",
        params![cycle_id, now],
    )?;
    let changed = tx.execute(
        "UPDATE dream_cycles
         SET status = 'done', batch_stage = 'complete', run_owner = NULL,
             run_lease_until = NULL, updated_at = ?4
         WHERE cycle_id = ?1 AND status IN ('running', 'failed') AND run_owner = ?2
           AND run_generation = ?3 AND run_lease_until > ?4
           AND batch_stage IN ('pending', 'submitted', 'results_ready', 'local')
           AND 8 = (
               SELECT COUNT(*) FROM job_runs
               WHERE cycle_id = ?1 AND state = 'done'
                 AND kind IN (
                     'consolidation', 'compression', 'state_update', 'confidence_recalc',
                     'cold_demotion', 'morning_brief', 'lesson_distillation',
                     'pattern_distillation'
                 )
           )",
        params![cycle_id, owner, generation, now],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    tx.execute(
        "DELETE FROM dream_cycle_inputs WHERE cycle_id = ?1",
        [cycle_id],
    )?;
    tx.execute(
        "DELETE FROM dream_batch_items WHERE cycle_id = ?1",
        [cycle_id],
    )?;
    tx.execute(
        "DELETE FROM dream_brief_effects WHERE cycle_id = ?1",
        [cycle_id],
    )?;
    tx.commit()
}

/// Whether MorningBrief's effect already committed for this cycle.
pub fn brief_effect_exists(conn: &Connection, cycle_id: &str) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dream_brief_effects WHERE cycle_id = ?1)",
        [cycle_id],
        |row| row.get(0),
    )
}

/// Save MorningBrief, consume its one-shot notice, and associate the effect with the cycle in one
/// transaction. An existing association makes retry a no-op until the job ledger reaches Done.
pub fn save_brief_effect(
    conn: &mut Connection,
    cycle_id: &str,
    date: &str,
    payload_json: &str,
    generated: bool,
    notice_id: Option<i64>,
    now: i64,
) -> Result<(), rusqlite::Error> {
    let tx = conn.savepoint()?;
    let exists = brief_effect_exists(&tx, cycle_id)?;
    if exists {
        return tx.commit();
    }
    crate::briefs::upsert_brief_in_transaction(&tx, date, payload_json, generated, now)?;
    if let Some(id) = notice_id {
        let changed = tx.execute(
            "UPDATE work_patterns SET noticed_at = ?2 WHERE id = ?1 AND noticed_at IS NULL",
            params![id, now],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
    }
    tx.execute(
        "INSERT INTO dream_brief_effects (cycle_id, date, notice_id) VALUES (?1, ?2, ?3)",
        params![cycle_id, date, notice_id],
    )?;
    tx.commit()?;
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Brief(date.to_owned()),
        now,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn event<'a>(ts: i64, text: &'a str, hash: &'a str) -> crate::event_log::NewEvent<'a> {
        crate::event_log::NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: text,
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        }
    }

    fn claim(conn: &Connection, cycle_id: &str) -> i64 {
        claim_run(conn, cycle_id, "owner", 2, 10_000)
            .unwrap()
            .unwrap()
    }

    fn submit(conn: &mut Connection, cycle_id: &str, batch_id: &str, ids: &[String]) {
        let generation = claim(conn, cycle_id);
        assert!(claim_submission(conn, cycle_id, "owner", generation, 3, &[]).unwrap());
        mark_submitted(conn, cycle_id, "owner", generation, batch_id, ids, 4).unwrap();
    }

    fn complete(conn: &mut Connection, cycle_id: &str) {
        for kind in [
            "consolidation",
            "compression",
            "state_update",
            "confidence_recalc",
            "cold_demotion",
            "morning_brief",
            "lesson_distillation",
            "pattern_distillation",
        ] {
            crate::jobs::upsert(conn, cycle_id, kind, "done", 10, 20, 4).unwrap();
        }
        let cycle = get(conn, cycle_id).unwrap().unwrap();
        let generation = if cycle.run_owner.as_deref() == Some("owner") {
            cycle.run_generation
        } else {
            claim(conn, cycle_id)
        };
        mark_complete(conn, cycle_id, "owner", generation, 5).unwrap();
    }

    #[test]
    fn unknown_submission_never_rolls_over_to_a_new_cycle() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "unknown", 0, 100, 100).unwrap();
        let first = claim_run(&conn, "unknown", "worker", 101, 10_000).unwrap().unwrap();
        assert!(claim_submission(&conn, "unknown", "worker", first, 101, &["1".into()]).unwrap());
        mark_failed(&conn, "unknown", "worker", first, 102).unwrap();
        for now in 103..113 {
            let generation = claim_run(&conn, "unknown", "worker", now, 10_000).unwrap().unwrap();
            mark_failed(&conn, "unknown", "worker", generation, now).unwrap();
        }
        let pending = oldest_unfinished(&conn).unwrap().unwrap();
        assert_eq!(pending.cycle_id, "unknown");
        assert_eq!(pending.expected_custom_ids, vec!["1".to_string()]);
        assert_eq!(pending.batch_stage, BatchStage::Pending);
    }

    #[test]
    fn unknown_queue_rotates_and_never_hides_a_bound_job() {
        let mut conn = crate::open_in_memory().unwrap();
        for (id, now) in [("old", 100), ("later", 200)] {
            freeze_full_cycle(&mut conn, id, 0, now, now).unwrap();
            for attempt in 0..MAX_FAILURE_ATTEMPTS {
                let generation = claim_run(&conn, id, "owner", now + attempt, 10_000).unwrap().unwrap();
                assert!(claim_submission(&conn, id, "owner", generation, now + attempt, &["1".into()]).unwrap());
                mark_failed(&conn, id, "owner", generation, now + attempt).unwrap();
            }
        }
        assert_eq!(oldest_unfinished(&conn).unwrap().unwrap().cycle_id, "old");
        let generation = claim_run(&conn, "old", "owner", 300, 10_000).unwrap().unwrap();
        mark_failed(&conn, "old", "owner", generation, 300).unwrap();
        assert_eq!(oldest_unfinished(&conn).unwrap().unwrap().cycle_id, "later");
        freeze_full_cycle(&mut conn, "bound", 200, 400, 400).unwrap();
        submit(&mut conn, "bound", "accepted", &["2".into()]);
        assert_eq!(oldest_unfinished(&conn).unwrap().unwrap().cycle_id, "bound");
        complete(&mut conn, "bound");
        assert_eq!(oldest_unfinished(&conn).unwrap().unwrap().cycle_id, "later");
        assert!(get(&conn, "old").unwrap().unwrap().provider_batch_id.is_none());
        assert_eq!(get(&conn, "old").unwrap().unwrap().expected_custom_ids, ["1"]);
    }

    #[test]
    fn latest_done_is_the_newest_completed_night() {
        let mut conn = crate::open_in_memory().unwrap();
        assert_eq!(latest_done(&conn).unwrap(), None);
        freeze_full_cycle(&mut conn, "20260920", 0, 10, 1).unwrap();
        freeze_full_cycle(&mut conn, "20260921", 10, 20, 2).unwrap();
        assert_eq!(latest_done(&conn).unwrap(), None, "unfinished cycles are not results");
        complete(&mut conn, "20260920");
        assert_eq!(latest_done(&conn).unwrap().as_deref(), Some("20260920"));
        complete(&mut conn, "20260921");
        assert_eq!(latest_done(&conn).unwrap().as_deref(), Some("20260921"));
    }

    #[test]
    fn freeze_preserves_the_first_range() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        freeze_full_cycle(&mut conn, "c1", 100, 200, 2).unwrap();

        let cycle = get(&conn, "c1").unwrap().unwrap();

        assert_eq!((cycle.input_from_ts, cycle.input_to_ts), (10, 20));
    }

    #[test]
    fn submitted_handle_and_expected_ids_are_read_back() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "batch-7", &["2".into(), "1".into()]);

        let cycle = get(&conn, "c1").unwrap().unwrap();

        assert_eq!(cycle.provider_batch_id.as_deref(), Some("batch-7"));
        assert_eq!(cycle.expected_custom_ids, vec!["1", "2"]);
        assert_eq!(cycle.batch_stage, BatchStage::Submitted);
    }

    #[test]
    fn completed_cycle_is_not_returned_for_resume() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        complete(&mut conn, "c1");

        assert!(oldest_unfinished(&conn).unwrap().is_none());
    }

    #[test]
    fn submitted_cycle_cannot_be_converted_to_local() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "batch-7", &["1".into()]);

        assert!(mark_local(&conn, "c1", "owner", 1, 5).is_err());
        let cycle = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(cycle.batch_stage, BatchStage::Submitted);
        assert_eq!(cycle.provider_batch_id.as_deref(), Some("batch-7"));
    }

    #[test]
    fn lifecycle_transitions_require_owner_and_legal_predecessor() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();

        let generation = claim_run(&conn, "c1", "winner", 2, 100).unwrap().unwrap();
        assert!(claim_run(&conn, "c1", "loser", 2, 100).unwrap().is_none());
        assert!(claim_submission(&conn, "c1", "winner", generation, 3, &[]).unwrap());
        assert!(mark_results_ready(&conn, "c1", "winner", generation, 4).is_err());
        assert!(mark_submitted(
            &mut conn,
            "c1",
            "loser",
            generation,
            "batch-7",
            &["1".into()],
            4,
        )
        .is_err());
        mark_submitted(
            &mut conn,
            "c1",
            "winner",
            generation,
            "batch-7",
            &["1".into()],
            4,
        )
        .unwrap();
        assert!(mark_local(&conn, "c1", "winner", generation, 5).is_err());
        mark_results_ready(&conn, "c1", "winner", generation, 5).unwrap();
        assert!(mark_complete(&mut conn, "c1", "winner", generation, 6).is_err());
        for kind in [
            "consolidation",
            "compression",
            "state_update",
            "confidence_recalc",
            "cold_demotion",
            "morning_brief",
            "lesson_distillation",
            "pattern_distillation",
        ] {
            crate::jobs::upsert(&conn, "c1", kind, "done", 10, 20, 5).unwrap();
        }
        assert!(mark_complete(&mut conn, "c1", "loser", generation, 6).is_err());
        mark_complete(&mut conn, "c1", "winner", generation, 6).unwrap();
    }

    #[test]
    fn expired_submitting_lease_returns_to_pending_for_recovery() {
        let mut conn = crate::open_in_memory().unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        let first = claim_run(&conn, "c1", "first", 2, 3).unwrap().unwrap();
        assert!(claim_submission(&conn, "c1", "first", first, 2, &[]).unwrap());

        let recovery = claim_run(&conn, "c1", "recovery", 4, 100).unwrap().unwrap();
        let cycle = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(cycle.batch_stage, BatchStage::Pending);
        assert_eq!(cycle.run_owner.as_deref(), Some("recovery"));
        assert!(recovery > first);
    }

    #[test]
    fn ordinary_delete_and_mutation_fail_loudly_until_successful_completion() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::event_log::insert(&conn, &event(15, "submitted", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "batch-7", &[id.to_string()]);
        let delete_error = conn
            .execute("DELETE FROM event_log WHERE id = ?1", [id])
            .unwrap_err();
        let update_error = conn
            .execute(
                "UPDATE event_log SET content = 'mutated', content_hash = 'h2' WHERE id = ?1",
                [id],
            )
            .unwrap_err();

        assert!(delete_error.to_string().contains(PINNED_EVENT_ERROR));
        assert!(update_error.to_string().contains(PINNED_EVENT_ERROR));

        let frozen = frozen_inputs(&conn, "c1").unwrap();
        assert_eq!(frozen.partitioned.cloud[0].content, "submitted");
        let canonical: String = conn
            .query_row("SELECT content FROM event_log WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(canonical, "submitted");

        complete(&mut conn, "c1");
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM dream_cycle_inputs WHERE cycle_id = 'c1'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute("DELETE FROM event_log WHERE id = ?1", [id])
            .unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn a_relaunch_frees_the_leases_the_previous_process_left_behind() {
        let mut conn = crate::open_in_memory().unwrap();
        crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        claim_run(&conn, "c1", "111-1", 2, 1_000_000).unwrap().unwrap();
        assert!(claim_run(&conn, "c1", "222-1", 3, 1_000_000).unwrap().is_none(), "held");
        assert_eq!(release_foreign_leases(&conn, "222-%").unwrap(), 1);
        assert!(claim_run(&conn, "c1", "222-1", 4, 1_000_000).unwrap().is_some());
        assert_eq!(release_foreign_leases(&conn, "222-%").unwrap(), 0, "own lease stays");
    }

    #[test]
    fn an_oversized_cycle_splits_at_a_time_boundary_and_leaves_the_rest_unclaimed() {
        let mut conn = crate::open_in_memory().unwrap();
        for ts in 11..=20 {
            crate::event_log::insert(&conn, &event(ts, &format!("event {ts}"), &format!("h{ts}"))).unwrap();
        }
        freeze_full_cycle(&mut conn, "c1", 10, 100, 1).unwrap();
        let generation = claim_run(&conn, "c1", "owner", 2, 10_000).unwrap().unwrap();
        assert!(split_oversized(&conn, "c1", "owner", generation, 4).unwrap());
        let row = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(row.input_to_ts, 15, "ends before the 5th eligible event");
        assert_eq!(frozen_inputs(&conn, "c1").unwrap().partitioned.cloud.len(), 4);
        // Not oversized any more; a stale owner cannot split either.
        assert!(!split_oversized(&conn, "c1", "owner", generation, 4).unwrap());
        assert!(!split_oversized(&conn, "c1", "other", generation, 1).unwrap());
        // The released events are claimed again by the next window, starting at the boundary.
        complete(&mut conn, "c1");
        freeze_full_cycle(&mut conn, "c2", 15, 100, 3).unwrap();
        assert_eq!(frozen_inputs(&conn, "c2").unwrap().partitioned.cloud.len(), 6);
    }

    #[test]
    fn outage_failures_never_abandon_a_submitted_cycle() {
        let mut conn = crate::open_in_memory().unwrap();
        crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "msgbatch_1", &["1".to_string()]);
        for attempt in 1..=MAX_FAILURE_ATTEMPTS + 3 {
            let generation = claim_run(&conn, "c1", "owner", attempt * 100_000, attempt * 100_000 + 50_000).unwrap().unwrap();
            mark_failed_retryable(&conn, "c1", "owner", generation, attempt * 100_000).unwrap();
        }
        let row = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(row.status, CycleStatus::Failed);
        assert_eq!(row.provider_batch_id.as_deref(), Some("msgbatch_1"));
        assert_eq!(row.failure_count, MAX_FAILURE_ATTEMPTS + 3);
    }

    #[test]
    fn lost_results_abandon_a_submitted_cycle_at_once() {
        let mut conn = crate::open_in_memory().unwrap();
        crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "msgbatch_1", &["1".to_string()]);
        let generation = claim_run(&conn, "c1", "owner", 100_000, 150_000).unwrap().unwrap();
        mark_abandoned(&conn, "c1", "owner", generation, 100_000).unwrap();
        assert_eq!(get(&conn, "c1").unwrap().unwrap().status, CycleStatus::Abandoned);
    }

    #[test]
    fn failed_cycle_abandons_at_the_limit_and_carries_coverage_forward_once() {
        let mut conn = crate::open_in_memory().unwrap();
        let first = crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        let released_delete =
            crate::event_log::insert(&conn, &event(16, "delete after abandon", "h-delete"))
                .unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();

        for attempt in 1..=MAX_FAILURE_ATTEMPTS {
            let generation = claim_run(&conn, "c1", "owner", attempt + 1, 10_000)
                .unwrap()
                .unwrap();
            mark_failed(&conn, "c1", "owner", generation, attempt + 1).unwrap();
            if attempt < MAX_FAILURE_ATTEMPTS {
                assert!(oldest_unfinished(&conn).unwrap().is_some());
            }
        }

        let abandoned = get(&conn, "c1").unwrap().unwrap();
        assert_eq!(abandoned.status, CycleStatus::Abandoned);
        assert_eq!(abandoned.batch_stage, BatchStage::Abandoned);
        assert_eq!(abandoned.failure_count, MAX_FAILURE_ATTEMPTS);
        assert!(oldest_unfinished(&conn).unwrap().is_none());
        let frozen_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM dream_cycle_inputs WHERE cycle_id = 'c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(frozen_rows, 0);
        conn.execute(
            "UPDATE event_log SET content = 'updated after abandon', content_hash = 'h3'
             WHERE id = ?1",
            [first],
        )
        .unwrap();
        conn.execute("DELETE FROM event_log WHERE id = ?1", [released_delete])
            .unwrap();

        let second = crate::event_log::insert(&conn, &event(25, "second", "h2")).unwrap();
        freeze_full_cycle(&mut conn, "c2", 20, 30, 10).unwrap();
        let replacement = get(&conn, "c2").unwrap().unwrap();
        assert_eq!(
            (replacement.input_from_ts, replacement.input_to_ts),
            (10, 30)
        );
        let ids: Vec<i64> = frozen_inputs(&conn, "c2")
            .unwrap()
            .partitioned
            .cloud
            .into_iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(ids, vec![first, second]);

        complete(&mut conn, "c2");
        freeze_full_cycle(&mut conn, "c3", 30, 40, 20).unwrap();
        assert_eq!(get(&conn, "c3").unwrap().unwrap().input_from_ts, 30);
    }

    #[test]
    fn frozen_meeting_text_stays_in_the_local_partition() {
        let mut conn = crate::open_in_memory().unwrap();
        let mut meeting = event(15, "private transcript", "h1");
        meeting.source = "meeting";
        let id = crate::event_log::insert(&conn, &meeting).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();

        let frozen = frozen_inputs(&conn, "c1").unwrap();

        assert!(frozen.partitioned.cloud.is_empty());
        assert_eq!(
            frozen.partitioned.local_only,
            vec![EventText {
                id,
                content: "private transcript".to_string(),
            }]
        );
    }

    #[test]
    fn explicit_delete_tombstones_content_but_keeps_result_identity() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::event_log::insert(&conn, &event(15, "submitted", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        submit(&mut conn, "c1", "batch-7", &[id.to_string()]);

        crate::search_documents::delete_canonical_event(&mut conn, id).unwrap();

        let frozen = frozen_inputs(&conn, "c1").unwrap();
        assert!(frozen.partitioned.cloud.is_empty());
        assert_eq!(frozen.deleted_cloud_event_ids, vec![id]);
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn late_insert_before_the_high_water_is_claimed_by_the_next_cycle() {
        let mut conn = crate::open_in_memory().unwrap();
        let first = crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        let first_ids: Vec<i64> = frozen_inputs(&conn, "c1")
            .unwrap()
            .partitioned
            .cloud
            .into_iter()
            .map(|event| event.id)
            .collect();
        let late = crate::event_log::insert(&conn, &event(16, "late", "h2")).unwrap();
        complete(&mut conn, "c1");

        freeze_full_cycle(&mut conn, "c2", 20, 30, 3).unwrap();

        let second_ids: Vec<i64> = frozen_inputs(&conn, "c2")
            .unwrap()
            .partitioned
            .cloud
            .into_iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(first_ids, vec![first]);
        assert_eq!(second_ids, vec![late]);
    }

    #[test]
    fn late_arrival_cursor_survives_sqlite_event_id_reuse() {
        let mut conn = crate::open_in_memory().unwrap();
        let original = crate::event_log::insert(&conn, &event(15, "first", "h1")).unwrap();
        freeze_full_cycle(&mut conn, "c1", 10, 20, 1).unwrap();
        complete(&mut conn, "c1");
        conn.execute("DELETE FROM event_log WHERE id = ?1", [original])
            .unwrap();
        let reused = crate::event_log::insert(&conn, &event(16, "late", "h2")).unwrap();
        assert_eq!(reused, original, "fixture must exercise SQLite rowid reuse");

        freeze_full_cycle(&mut conn, "c2", 20, 30, 3).unwrap();

        let ids: Vec<i64> = frozen_inputs(&conn, "c2")
            .unwrap()
            .partitioned
            .cloud
            .into_iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(ids, vec![reused]);
    }
}
