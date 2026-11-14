//! Explicit, bounded inventory of canonical event text. No payload leaves this module.

use std::collections::HashMap;

use rusqlite::{Connection, MAIN_DB};
use serde::Serialize;

use crate::fts_compact::{self, DbPages};

const MAX_ROWS: i64 = 10_000;
const MAX_BYTES: i64 = 8 * 1024 * 1024;
const MAX_ROW_BYTES: i64 = 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct StoragePreview {
    pub complete: bool,
    pub partial_reason: Option<&'static str>,
    pub scanned_rows: i64,
    pub scanned_canonical_bytes: i64,
    pub retained_one_copy_bytes: i64,
    pub extra_duplicate_copy_bytes: i64,
    /// Gross duplicate bytes in groups with user/import provenance or active frozen Dream input.
    pub protected_duplicate_bytes: i64,
    /// All other duplicate bytes remain uncertain until reader/deletion contracts are mapped.
    pub uncertain_duplicate_bytes: i64,
    /// Gross duplicate opportunity in groups containing an active frozen Dream source.
    pub active_frozen_overlap_duplicate_bytes: i64,
    pub snapshot_at_ms: i64,
    pub limits: PreviewLimits,
    pub database_pages: DbPages,
    /// These quantities have different storage contracts and are intentionally not estimated.
    pub frame_payload_bytes: Option<i64>,
    pub wal_bytes: Option<i64>,
    pub backup_bytes: Option<i64>,
    pub fts_bytes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PreviewLimits {
    pub rows: i64,
    pub bytes: i64,
    pub single_row_bytes: i64,
}

/// Snapshot-consistent, read-only preview. The savepoint also works when the daemon has an
/// active Dream transaction. Incremental BLOB access checks byte size before reading text.
pub fn preview(conn: &Connection) -> Result<StoragePreview, rusqlite::Error> {
    conn.execute_batch("SAVEPOINT shogun_storage_preview")?;
    let result = preview_in_snapshot(conn);
    let release =
        conn.execute_batch("ROLLBACK TO shogun_storage_preview; RELEASE shogun_storage_preview");
    release?;
    result
}

fn preview_in_snapshot(conn: &Connection) -> Result<StoragePreview, rusqlite::Error> {
    let pages = fts_compact::db_pages(conn)?;
    let snapshot_at_ms: i64 =
        conn.query_row("SELECT unixepoch('now') * 1000", [], |row| row.get(0))?;
    let mut groups: HashMap<Vec<u8>, (i64, bool, bool)> = HashMap::new();
    let mut scanned_rows = 0;
    let mut scanned_bytes: i64 = 0;
    let mut partial_reason = None;
    let candidates = {
        let mut statement = conn.prepare(
            "SELECT e.id,
                    EXISTS (SELECT 1 FROM dream_event_arrivals a
                        JOIN dream_cycle_inputs i ON i.arrival_seq = a.arrival_seq
                        JOIN dream_cycles c ON c.cycle_id = i.cycle_id
                        WHERE a.event_id = e.id AND i.disposition = 'active'
                          AND c.status IN ('running', 'failed')),
                    (e.source = 'user' OR e.source LIKE 'imported:%')
             FROM event_log e ORDER BY e.id LIMIT ?1",
        )?;
        let rows = statement.query_map([MAX_ROWS + 1], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (id, frozen, explicitly_protected) in candidates {
        if scanned_rows == MAX_ROWS {
            partial_reason = Some("row_limit");
            break;
        }
        // sqlite3_blob_bytes works on TEXT columns and does not materialize the whole value.
        let blob = conn.blob_open(MAIN_DB, "event_log", "content", id, true)?;
        let size = i64::from(blob.size());
        if size > MAX_ROW_BYTES {
            partial_reason = Some("single_row_byte_limit");
            break;
        }
        if scanned_bytes.saturating_add(size) > MAX_BYTES {
            partial_reason = Some("total_byte_limit");
            break;
        }
        let mut content = vec![0; blob.len()];
        blob.read_at_exact(&mut content, 0)?;
        let group = groups.entry(content).or_insert((0, false, false));
        group.0 += 1;
        group.1 |= frozen;
        group.2 |= explicitly_protected || frozen;
        scanned_rows += 1;
        scanned_bytes += size;
    }
    let retained_one_copy_bytes = groups.keys().map(|content| content.len() as i64).sum();
    let extra_duplicate_copy_bytes = scanned_bytes - retained_one_copy_bytes;
    let duplicate_bytes = |predicate: fn(bool, bool) -> bool| -> i64 {
        groups
            .iter()
            .filter(|(_, (_, frozen, protected))| predicate(*frozen, *protected))
            .map(|(content, (count, _, _))| (content.len() as i64) * (count - 1))
            .sum()
    };
    let protected_duplicate_bytes = duplicate_bytes(|_, protected| protected);
    let active_frozen_overlap_duplicate_bytes = duplicate_bytes(|frozen, _| frozen);
    Ok(StoragePreview {
        complete: partial_reason.is_none(),
        partial_reason,
        scanned_rows,
        scanned_canonical_bytes: scanned_bytes,
        retained_one_copy_bytes,
        extra_duplicate_copy_bytes,
        protected_duplicate_bytes,
        uncertain_duplicate_bytes: extra_duplicate_copy_bytes - protected_duplicate_bytes,
        active_frozen_overlap_duplicate_bytes,
        snapshot_at_ms,
        limits: PreviewLimits {
            rows: MAX_ROWS,
            bytes: MAX_BYTES,
            single_row_bytes: MAX_ROW_BYTES,
        },
        database_pages: pages,
        frame_payload_bytes: None,
        wal_bytes: None,
        backup_bytes: None,
        fts_bytes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_equality_unicode_and_frozen_overlap() {
        let conn = crate::open(":memory:").unwrap();
        conn.execute_batch(
            "INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
             VALUES (1,'user','text','é','collision',1),
                    (2,'capture','text','é','collision',2),
                    (3,'capture','text','e','collision',3);
             INSERT INTO dream_cycles(cycle_id,input_from_ts,input_to_ts,status,batch_stage,
                 snapshot_max_arrival_seq,created_at,updated_at)
             VALUES ('cycle',1,3,'running','pending',3,1,1);
             INSERT INTO dream_cycle_inputs(cycle_id,arrival_seq,event_id,event_ts,source,content)
             VALUES ('cycle',2,2,2,'capture','é');",
        )
        .unwrap();
        let result = preview(&conn).unwrap();
        assert!(result.complete);
        assert_eq!(result.scanned_canonical_bytes, 5);
        assert_eq!(result.retained_one_copy_bytes, 3);
        assert_eq!(result.extra_duplicate_copy_bytes, 2);
        assert_eq!(result.protected_duplicate_bytes, 2);
        assert_eq!(result.uncertain_duplicate_bytes, 0);
        assert_eq!(result.active_frozen_overlap_duplicate_bytes, 2);
    }

    #[test]
    fn row_limit_never_claims_database_wide_savings() {
        let conn = crate::open(":memory:").unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x <= 10000)
            INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
            SELECT x,'capture','text','a','same',x FROM n;",
        )
        .unwrap();
        let result = preview(&conn).unwrap();
        assert!(!result.complete);
        assert_eq!(result.partial_reason, Some("row_limit"));
        assert_eq!(result.scanned_rows, MAX_ROWS);
    }

    #[test]
    fn byte_limits_stop_before_reading_more_text() {
        let conn = crate::open(":memory:").unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 9)
             INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
             SELECT x,'capture','text',CAST(zeroblob(1048576) AS TEXT),'same',x FROM n;",
        )
        .unwrap();
        let result = preview(&conn).unwrap();
        assert_eq!(
            (result.scanned_rows, result.partial_reason),
            (8, Some("total_byte_limit"))
        );
        conn.execute_batch(
            "DELETE FROM event_log;
             INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
             VALUES (1,'capture','text',CAST(zeroblob(1048577) AS TEXT),'large',1);",
        )
        .unwrap();
        let result = preview(&conn).unwrap();
        assert_eq!(
            (result.scanned_rows, result.partial_reason),
            (0, Some("single_row_byte_limit"))
        );
    }
}
