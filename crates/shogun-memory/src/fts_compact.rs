//! Bounded, resumable compaction of the trigram FTS5 indexes (#714, step 3 of
//! `docs/fts-storage-714.md` §7).
//!
//! Two things make an FTS5 index larger than its content needs:
//! - **Stale versions.** Re-indexing an older row (pre-V41 dedup touches, OCR re-scans) leaves its
//!   old postings in a deeper segment until that segment is merged. The hourly `('merge', 64)`
//!   never merges them: a positive N only merges a level holding ≥ `usermerge` (4) segments.
//! - **Leaves written at FTS5's default `pgsz` (4050).** Under SQLCipher those spill into an
//!   overflow page each; V57 set 3960 for new leaves, but existing leaves keep their size until a
//!   merge rewrites them.
//!
//! This module holds the SQL-level primitives the core's compaction job drives, one short
//! transaction at a time:
//! - [`Structure`] decodes the FTS5 structure record (`%_data` rowid 10) of the shipped engine
//!   (SQLite 3.54.1 = SQLCipher 4.20.0, the V1 record format; FTS5 writes V2 only for
//!   `contentless_delete` tables, which this schema has none of). It tells whether a merge is already
//!   in progress, how many leaf pages sit outside the largest segment, and so what is left to do.
//! - [`plan`] decides the next move for one table: begin a merge of every segment (a negative
//!   `'merge'`), continue the merge in progress (a positive `'merge'`, so the in-progress merge is
//!   never abandoned), or nothing.
//! - [`merge_step`] runs one bounded `'merge'` command. It is a single autocommit statement, so
//!   each step is its own transaction: a crash or a stop between steps leaves a valid index, and
//!   FTS5 keeps any merge in progress in the structure record, so a later run picks it up.
//!
//! Never a one-shot `'optimize'` or `'rebuild'`: either is one transaction whose WAL is about the
//! size of the whole index (757 MB for a 747 MB index in the #714 measurements).
//!
//! Every value this module reports is a count or a size. It never reads indexed text.

use rusqlite::{Connection, OptionalExtension};

/// The trigram FTS5 tables the job compacts, in the order it visits them: the capture index first
/// (by far the largest), then the derived-document and knowledge-entity indexes.
pub const FTS_TABLES: [&str; 3] = ["event_fts", "search_documents_fts", "knowledge_entities_fts"];

/// `FTS5_STRUCTURE_ROWID`: the structure record's rowid in `%_data`.
const STRUCTURE_ROWID: i64 = 10;
/// `%_data` rowids put the segment id above 37 bits of dlidx flag / height / page number
/// (`FTS5_SEGMENT_ROWID` in fts5_index.c); a leaf has height 0 and no dlidx flag.
const SEGMENT_ID_SHIFT: u32 = 37;
/// FTS5's default leaf size, used when a table has no `pgsz` row.
pub const DEFAULT_PGSZ: i64 = 4050;
/// FTS5 can end a leaf a few bytes past `pgsz` (the varints it appends after its "page full"
/// check). A sampled leaf longer than `pgsz` plus this slack was written at a larger `pgsz`.
const LEAF_SLACK: i64 = 32;
/// Leaves sampled from the start of the largest segment to tell its leaf size.
const LEAF_SAMPLE: i64 = 32;
/// The largest segment counts as "the index" when every other segment together holds at most this
/// share of the leaf pages (in percent). Above it, the job has stale versions or recent segments
/// worth folding in; below it, rewriting the whole index is not worth the write amplification.
pub const OUTSIDE_LARGEST_MAX_PERCENT: u64 = 10;
/// `FTS5_DEFAULT_USERMERGE`. Restored after a stalled continue step (see [`merge_step`]).
const DEFAULT_USERMERGE: i64 = 4;

/// Whether `table` is one of [`FTS_TABLES`]. Every function here refuses any other name, so a
/// table name is never built from anything but these constants.
fn checked(table: &str) -> Result<&'static str, rusqlite::Error> {
    FTS_TABLES
        .iter()
        .copied()
        .find(|t| *t == table)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName(table.to_string()))
}

/// One segment of an FTS5 index: its id and its current leaf-page range. A segment that is an
/// input of a merge in progress has had its consumed leaves trimmed, so the range shrinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub id: u32,
    pub first_page: u32,
    pub last_page: u32,
}

impl Segment {
    /// Leaf pages in the segment.
    pub fn pages(&self) -> u64 {
        u64::from(self.last_page.saturating_sub(self.first_page)) + 1
    }
}

/// One level of the structure record. `merging` is the number of this level's oldest segments
/// that are inputs of a merge in progress (0 = none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    pub merging: u32,
    pub segments: Vec<Segment>,
}

/// The decoded FTS5 structure record (V1 format; re-verified on SQLite 3.54.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    pub write_counter: u64,
    pub levels: Vec<Level>,
}

/// SQLite's varint (big-endian 7-bit groups, a full 8-bit ninth byte), which FTS5 uses throughout.
fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if i == 8 {
            return Some((value << 8) | u64::from(byte));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn varint32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    varint(buf, pos).and_then(|v| u32::try_from(v).ok())
}

impl Structure {
    /// Decode a structure record. `None` for anything that is not a well-formed V1 record (a
    /// future engine's format included): the caller then falls back to "did the command change
    /// anything", which needs no decoding.
    pub fn decode(block: &[u8]) -> Option<Structure> {
        // 4-byte configuration cookie, then nLevel, nSegment, nWriteCounter.
        let mut pos = 4usize;
        if block.len() < pos {
            return None;
        }
        let n_level = varint32(block, &mut pos)?;
        let n_segment = varint32(block, &mut pos)?;
        // FTS5_MAX_SEGMENT is 2000; FTS5_MAX_LEVEL 64. Anything larger is not this format.
        if n_level > 64 || n_segment > 2000 {
            return None;
        }
        let write_counter = varint(block, &mut pos)?;
        let mut levels = Vec::with_capacity(n_level as usize);
        let mut seen = 0u32;
        for _ in 0..n_level {
            let merging = varint32(block, &mut pos)?;
            let n_seg = varint32(block, &mut pos)?;
            if merging > n_seg || seen.checked_add(n_seg)? > n_segment {
                return None;
            }
            seen += n_seg;
            let mut segments = Vec::with_capacity(n_seg as usize);
            for _ in 0..n_seg {
                let id = varint32(block, &mut pos)?;
                let first_page = varint32(block, &mut pos)?;
                let last_page = varint32(block, &mut pos)?;
                if last_page < first_page {
                    return None;
                }
                segments.push(Segment { id, first_page, last_page });
            }
            levels.push(Level { merging, segments });
        }
        if seen != n_segment || pos != block.len() {
            return None;
        }
        Some(Structure { write_counter, levels })
    }

    fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.levels.iter().flat_map(|l| l.segments.iter())
    }

    /// Segments in the index.
    pub fn segment_count(&self) -> usize {
        self.levels.iter().map(|l| l.segments.len()).sum()
    }

    /// Leaf pages across every segment.
    pub fn leaf_pages(&self) -> u64 {
        self.segments().map(Segment::pages).sum()
    }

    /// The segment with the most leaf pages.
    pub fn largest(&self) -> Option<Segment> {
        self.segments().copied().max_by_key(Segment::pages)
    }

    /// Leaf pages outside the largest segment.
    pub fn pages_outside_largest(&self) -> u64 {
        self.leaf_pages() - self.largest().map_or(0, |s| s.pages())
    }

    /// Whether some level has a merge in progress.
    pub fn merge_in_progress(&self) -> bool {
        self.levels.iter().any(|l| l.merging > 0)
    }

    /// Leaf pages still to be read by the merges in progress (their inputs' untrimmed leaves).
    pub fn pending_merge_input_pages(&self) -> u64 {
        self.levels
            .iter()
            .flat_map(|l| l.segments.iter().take(l.merging as usize))
            .map(Segment::pages)
            .sum()
    }
}

/// The raw structure record of `table`, if the index has one yet.
pub fn structure_record(conn: &Connection, table: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    let table = checked(table)?;
    conn.query_row(
        &format!("SELECT block FROM \"{table}_data\" WHERE id = ?1"),
        [STRUCTURE_ROWID],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .optional()
}

/// The `pgsz` the table writes new leaves at ([`DEFAULT_PGSZ`] without a config row).
pub fn configured_pgsz(conn: &Connection, table: &str) -> Result<i64, rusqlite::Error> {
    let table = checked(table)?;
    let value: Option<i64> = conn
        .query_row(&format!("SELECT v FROM \"{table}_config\" WHERE k = 'pgsz'"), [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(value.unwrap_or(DEFAULT_PGSZ))
}

/// Whether `segment`'s first leaves were written at a larger `pgsz` than `pgsz`. Reads only the
/// lengths of up to [`LEAF_SAMPLE`] leaf blobs through a rowid range seek (`length()` of a blob
/// column does not load the blob), never their bytes.
fn leaves_larger_than(
    conn: &Connection,
    table: &'static str,
    segment: Segment,
    pgsz: i64,
) -> Result<bool, rusqlite::Error> {
    let base = i64::from(segment.id) << SEGMENT_ID_SHIFT;
    let first = base + i64::from(segment.first_page);
    let last = (base + i64::from(segment.last_page)).min(first + LEAF_SAMPLE - 1);
    let largest: Option<i64> = conn.query_row(
        &format!("SELECT max(length(block)) FROM \"{table}_data\" WHERE id BETWEEN ?1 AND ?2"),
        [first, last],
        |r| r.get(0),
    )?;
    Ok(largest.is_some_and(|len| len > pgsz + LEAF_SLACK))
}

/// What the job should do next with one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Move {
    /// Nothing worth doing: one segment, or the largest segment already holds almost every leaf
    /// at the configured leaf size.
    Done,
    /// A merge is in progress: continue it with a positive `'merge'`. A negative one here would
    /// fold any segment written since into a fresh merge and redo the progress made so far.
    Continue,
    /// Start merging every segment into one (a negative `'merge'`).
    Begin,
}

/// One table's index shape: counts only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TableState {
    pub table: &'static str,
    /// `false` when the structure record could not be decoded; the job then runs negative merges
    /// until one changes nothing.
    pub structure_known: bool,
    pub segments: usize,
    pub leaf_pages: u64,
    pub pages_outside_largest: u64,
    pub merge_in_progress: bool,
    /// The largest segment's leaves were written at a larger `pgsz` than the table's setting.
    pub oversized_leaves: bool,
    /// Leaf pages the job still expects to write for this table (an estimate).
    pub pages_to_write: u64,
    pub next: Move,
}

/// Decide the next move for `table` from its structure record. Reads the record, the `pgsz`
/// setting and at most [`LEAF_SAMPLE`] leaf lengths: cheap enough for a status call.
pub fn plan(conn: &Connection, table: &str) -> Result<TableState, rusqlite::Error> {
    let table = checked(table)?;
    let empty = TableState {
        table,
        structure_known: true,
        segments: 0,
        leaf_pages: 0,
        pages_outside_largest: 0,
        merge_in_progress: false,
        oversized_leaves: false,
        pages_to_write: 0,
        next: Move::Done,
    };
    let Some(block) = structure_record(conn, table)? else {
        return Ok(empty);
    };
    let Some(structure) = Structure::decode(&block) else {
        return Ok(TableState { structure_known: false, next: Move::Begin, ..empty });
    };
    let segments = structure.segment_count();
    let leaf_pages = structure.leaf_pages();
    let outside = structure.pages_outside_largest();
    let in_progress = structure.merge_in_progress();
    let oversized = match structure.largest() {
        Some(largest) if segments > 1 => {
            leaves_larger_than(conn, table, largest, configured_pgsz(conn, table)?)?
        }
        _ => false,
    };
    let next = if in_progress {
        Move::Continue
    } else if segments <= 1 {
        Move::Done
    } else if outside * 100 > leaf_pages * OUTSIDE_LARGEST_MAX_PERCENT || oversized {
        Move::Begin
    } else {
        Move::Done
    };
    let pages_to_write = match next {
        Move::Done => 0,
        // An in-progress merge still has to read its inputs' remaining leaves; if what it leaves
        // behind still needs work, that is the rest of the index again.
        Move::Continue => structure.pending_merge_input_pages(),
        Move::Begin => leaf_pages,
    };
    Ok(TableState {
        table,
        structure_known: true,
        segments,
        leaf_pages,
        pages_outside_largest: outside,
        merge_in_progress: in_progress,
        oversized_leaves: oversized,
        pages_to_write,
        next,
    })
}

/// What one [`merge_step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    /// The structure record changed: FTS5 merged something. `false` means FTS5 had nothing to
    /// merge for this command — the completion signal (see the tests for why not
    /// `total_changes()`).
    pub progressed: bool,
}

/// Run one bounded merge command on `table`: `('merge', -pages)` for [`Move::Begin`] (and for an
/// undecodable structure), `('merge', pages)` for [`Move::Continue`]. `pages` bounds the leaf
/// pages written, which bounds the time the write lock is held and the WAL written.
///
/// A positive merge picks the level with the most segments below a level that is merging, and
/// does nothing if that level holds fewer than `usermerge` (4). So a continue step can stall when
/// 2–3 recent segments sit below a merge of 2–3 inputs. Then the same command runs once more with
/// `usermerge` 2, inside one transaction that restores the previous value, rather than a negative
/// merge that would restart the merge in progress.
///
/// Must not run inside an open transaction.
pub fn merge_step(
    conn: &Connection,
    table: &str,
    next: Move,
    pages: u32,
) -> Result<StepOutcome, rusqlite::Error> {
    let table = checked(table)?;
    let pages = i64::from(pages.max(1));
    let before = structure_record(conn, table)?;
    let command = format!("INSERT INTO \"{table}\"(\"{table}\", rank) VALUES ('merge', ?1)");
    match next {
        Move::Done => return Ok(StepOutcome { progressed: false }),
        Move::Begin => {
            conn.execute(&command, [-pages])?;
        }
        Move::Continue => {
            conn.execute(&command, [pages])?;
            if structure_record(conn, table)? == before {
                let usermerge: Option<i64> = conn
                    .query_row(
                        &format!("SELECT v FROM \"{table}_config\" WHERE k = 'usermerge'"),
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                let set = format!("INSERT INTO \"{table}\"(\"{table}\", rank) VALUES ('usermerge', ?1)");
                let tx = conn.unchecked_transaction()?;
                tx.execute(&set, [2i64])?;
                tx.execute(&command, [pages])?;
                tx.execute(&set, [usermerge.unwrap_or(DEFAULT_USERMERGE)])?;
                tx.commit()?;
            }
        }
    }
    Ok(StepOutcome { progressed: structure_record(conn, table)? != before })
}

/// Page accounting for the whole database file: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DbPages {
    pub page_size: i64,
    pub page_count: i64,
    pub freelist_count: i64,
    /// `PRAGMA auto_vacuum`: 0 none, 1 full, 2 incremental.
    pub auto_vacuum: i64,
}

impl DbPages {
    /// The file's size in bytes.
    pub fn file_bytes(&self) -> i64 {
        self.page_count.saturating_mul(self.page_size)
    }

    /// Bytes in pages that hold data.
    pub fn used_bytes(&self) -> i64 {
        (self.page_count - self.freelist_count).max(0).saturating_mul(self.page_size)
    }

    /// Bytes in free pages: reused by later writes, and returned to the filesystem only by
    /// `incremental_vacuum` (incremental databases) or a `VACUUM`.
    pub fn free_bytes(&self) -> i64 {
        self.freelist_count.saturating_mul(self.page_size)
    }

    /// Whether freed pages can be handed back to the filesystem without a `VACUUM`.
    pub fn returns_free_pages(&self) -> bool {
        self.auto_vacuum == 2
    }
}

/// Read [`DbPages`] (four pragmas; no table scan).
pub fn db_pages(conn: &Connection) -> Result<DbPages, rusqlite::Error> {
    let pragma = |name: &str| conn.query_row(&format!("PRAGMA {name}"), [], pragma_integer);
    Ok(DbPages {
        page_size: pragma("page_size")?,
        page_count: pragma("page_count")?,
        freelist_count: pragma("freelist_count")?,
        auto_vacuum: pragma("auto_vacuum")?,
    })
}

/// An integer pragma result. On a keyed connection SQLCipher answers `PRAGMA page_size` itself
/// (its codec page size, the same number) and returns it as text, so accept a decimal string too.
fn pragma_integer(row: &rusqlite::Row<'_>) -> Result<i64, rusqlite::Error> {
    use rusqlite::types::{Type, ValueRef};
    match row.get_ref(0)? {
        ValueRef::Integer(value) => Ok(value),
        ValueRef::Text(text) => std::str::from_utf8(text)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| rusqlite::Error::InvalidColumnType(0, "pragma".into(), Type::Text)),
        other => Err(rusqlite::Error::InvalidColumnType(0, "pragma".into(), other.data_type())),
    }
}

/// Hand up to `max_pages` free pages back to the filesystem (`auto_vacuum = INCREMENTAL` only; a
/// harmless no-op otherwise). Bounded like every other step. Returns the pages released.
pub fn release_free_pages(conn: &Connection, max_pages: i64) -> Result<i64, rusqlite::Error> {
    let before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    {
        // One (empty) result row per page freed: step it to exhaustion, like `reclaim_space`.
        let mut stmt = conn.prepare(&format!("PRAGMA incremental_vacuum({})", max_pages.max(1)))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
    }
    let after: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    Ok((before - after).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLCipher answers `PRAGMA page_size` on a keyed connection as text; the plaintext page
    /// size must still come back as the same integer.
    #[test]
    fn db_pages_reads_keyed_and_plaintext_databases_alike() {
        let path = crate::fts_page_size_tests::temp("db-pages-keyed");
        let keyed = crate::open_encrypted(&path, &crate::fts_page_size_tests::key()).unwrap();
        let plain = crate::open_in_memory().unwrap();
        let expected: i64 = plain.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
        let keyed_pages = db_pages(&keyed).unwrap();
        assert_eq!(keyed_pages.page_size, expected);
        assert!(keyed_pages.page_count > 0);
        assert_eq!(db_pages(&plain).unwrap().page_size, expected);
        drop(keyed);
        crate::fts_page_size_tests::remove(&path);
    }

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        // Test-only encoder for values below 2^56 (7-bit groups, big-endian).
        let mut groups = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            groups.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        groups.reverse();
        out.extend(groups);
    }

    /// (segment id, first page, last page).
    type Seg = (u32, u32, u32);

    fn record(levels: &[(u32, &[Seg])]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 7];
        let n_seg: usize = levels.iter().map(|(_, s)| s.len()).sum();
        encode_varint(levels.len() as u64, &mut out);
        encode_varint(n_seg as u64, &mut out);
        encode_varint(300, &mut out);
        for (merging, segs) in levels {
            encode_varint(u64::from(*merging), &mut out);
            encode_varint(segs.len() as u64, &mut out);
            for (id, first, last) in *segs {
                encode_varint(u64::from(*id), &mut out);
                encode_varint(u64::from(*first), &mut out);
                encode_varint(u64::from(*last), &mut out);
            }
        }
        out
    }

    #[test]
    fn decodes_levels_segments_and_merges_in_progress() {
        let block = record(&[(0, &[(3, 1, 4)]), (2, &[(1, 10, 209), (2, 1, 100), (5, 1, 1)]), (0, &[(4, 1, 1000)])]);
        let s = Structure::decode(&block).unwrap();
        assert_eq!(s.write_counter, 300);
        assert_eq!(s.segment_count(), 5);
        assert_eq!(s.leaf_pages(), 4 + 200 + 100 + 1 + 1000);
        assert_eq!(s.largest().unwrap().id, 4);
        assert_eq!(s.pages_outside_largest(), 305);
        assert!(s.merge_in_progress());
        assert_eq!(s.pending_merge_input_pages(), 300, "the two oldest segments of level 1");
    }

    #[test]
    fn rejects_anything_that_is_not_a_v1_record() {
        let good = record(&[(0, &[(1, 1, 2)])]);
        assert!(Structure::decode(&good).is_some());
        // Truncated, trailing bytes, segment-count mismatch, merge larger than the level.
        assert!(Structure::decode(&good[..good.len() - 1]).is_none());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(Structure::decode(&trailing).is_none());
        let mut miscount = good.clone();
        miscount[5] = 2;
        assert!(Structure::decode(&miscount).is_none());
        assert!(Structure::decode(&record(&[(2, &[(1, 1, 2)])])).is_none());
        assert!(Structure::decode(&[0, 0]).is_none());
        // A 3.45+ V2 record puts a 0xFF000001 marker after the cookie; it is not guessed at.
        assert!(Structure::decode(&[0, 0, 0, 7, 0xff, 0, 0, 1, 1, 1, 0, 0, 1, 1, 1, 2]).is_none());
    }

    #[test]
    fn varint_matches_sqlite_encoding() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, 2_097_151, 1 << 40] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf);
            let mut pos = 0;
            assert_eq!(varint(&buf, &mut pos), Some(v));
            assert_eq!(pos, buf.len());
        }
        // Nine bytes: the ninth carries a full 8 bits.
        let nine = [0xffu8; 9];
        let mut pos = 0;
        assert_eq!(varint(&nine, &mut pos), Some(u64::MAX));
    }

    #[test]
    fn only_the_known_tables_are_accepted() {
        let conn = crate::open_in_memory().unwrap();
        assert!(plan(&conn, "event_fts").is_ok());
        assert!(plan(&conn, "event_log").is_err());
        assert!(merge_step(&conn, "x\"; DROP TABLE event_log; --", Move::Begin, 8).is_err());
        assert_eq!(plan(&conn, "knowledge_entities_fts").unwrap().next, Move::Done);
    }
}
