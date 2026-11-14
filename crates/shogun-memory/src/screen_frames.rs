//! Finite-retention JPEG frames from visual-recall OCR (issue #106, 2026-08-02).
//!
//! Frames live in the encrypted memory DB as BLOBs. A frame links to the `screen_ocr` event read
//! from the focused window and/or the Accessibility capture of the same poll; a preview taken while
//! Accessibility text was enough has no OCR event at all (#969). A background purge drops anything
//! older than the user's selected duration.

use rusqlite::{params, Connection};
use rusqlite::OptionalExtension;

/// OCR text shorter than this triggers `needs_rescan` on recall hits.
pub const THIN_OCR_CHARS: usize = 100;

/// Whether `ocr_text` is thin enough to warrant a Vision re-scan.
///
/// The one place this is decided: search and `get_frame` are two faces onto the same frame and
/// must never disagree about it. Characters, not bytes — 34 chars of Japanese OCR is ~102 UTF-8
/// bytes, and a byte count would skip the re-scan exactly where OCR is weakest (CJK screens).
pub fn needs_rescan(ocr_text: &str) -> bool {
    ocr_text.trim().chars().count() < THIN_OCR_CHARS
}

/// A rectangle in the stored JPEG's pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Which pixels of a frame may be read as the focused window's text (#969, V75).
///
/// ponytail: a window of another app floating above the focused one when the preview was taken
/// sits inside `Window` and would be re-read with it. The window list cannot tell such a window
/// from the always-on-top transparent overlays every Mac has (the Dock alone spans the display),
/// so a stack check would refuse nearly every frame. Mask windows above the focused one here if a
/// reliable opacity signal turns up. Live OCR is unaffected: it captures the focused window alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrRegion {
    /// A frame from before V75: the JPEG is the focused window.
    WholeFrame,
    /// A full-display preview: only this rectangle is the focused window.
    Window(FrameRect),
}

impl OcrRegion {
    fn columns(self) -> [Option<i64>; 4] {
        match self {
            Self::WholeFrame => [None; 4],
            Self::Window(r) => [r.x, r.y, r.width, r.height].map(|v| Some(i64::from(v))),
        }
    }

    fn from_columns(columns: [Option<i64>; 4]) -> Self {
        let [Some(x), Some(y), Some(width), Some(height)] = columns else {
            return Self::WholeFrame;
        };
        let pixel = |v: i64| u32::try_from(v).unwrap_or(0);
        Self::Window(FrameRect { x: pixel(x), y: pixel(y), width: pixel(width), height: pixel(height) })
    }

    /// The focused window as `(x, y, width, height)` fractions of a `width` x `height` frame, top-left
    /// origin: the whole frame for a pre-V75 window capture, `None` when the rectangle misses it.
    pub fn normalized(self, width: u32, height: u32) -> Option<[f64; 4]> {
        match self {
            Self::WholeFrame => Some([0.0, 0.0, 1.0, 1.0]),
            Self::Window(r) if width > 0 && height > 0 => {
                let (fw, fh) = (f64::from(width), f64::from(height));
                let x = f64::from(r.x.min(width)) / fw;
                let y = f64::from(r.y.min(height)) / fh;
                let w = (f64::from(r.x.saturating_add(r.width).min(width)) / fw) - x;
                let h = (f64::from(r.y.saturating_add(r.height).min(height)) / fh) - y;
                (w > 0.0 && h > 0.0).then_some([x, y, w, h])
            }
            Self::Window(_) => None,
        }
    }
}

/// Row to insert for one retained frame.
#[derive(Debug, Clone)]
pub struct NewFrame<'a> {
    pub created_at_ms: i64,
    /// OCR read from the focused window in the same poll. `None` when Accessibility text was
    /// enough and no OCR ran: a saved preview never triggers OCR by itself (#969).
    pub event_id: Option<i64>,
    /// Normal Accessibility capture from the same poll, when one was persisted.
    pub accessibility_event_id: Option<i64>,
    pub app_bundle_id: Option<&'a str>,
    pub window_title: Option<&'a str>,
    pub display_id: Option<i64>,
    pub width: u32,
    pub height: u32,
    pub ocr_region: OcrRegion,
    pub jpeg: &'a [u8],
}

/// Metadata for list/search surfaces — no pixel payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSummary {
    pub id: i64,
    pub created_at_ms: i64,
    pub event_id: Option<i64>,
    pub accessibility_event_id: Option<i64>,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub display_id: Option<i64>,
    pub width: u32,
    pub height: u32,
    pub jpeg_bytes: usize,
    pub ocr_text: String,
    pub accessibility_text: String,
    /// Linked event source: `screen_ocr` (auto) or `user_screenshot` (explicit capture).
    pub source: String,
    pub ocr_region: OcrRegion,
}

impl FrameSummary {
    /// Whether this frame's OCR text is thin enough for an automatic Vision re-scan. A frame with
    /// no OCR event never qualifies: a saved preview must not trigger OCR by itself (#969).
    pub fn needs_rescan(&self) -> bool {
        self.event_id.is_some() && needs_rescan(&self.ocr_text)
    }
}

/// Full frame row including JPEG bytes (for agent re-scan / future vision input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRecord {
    pub summary: FrameSummary,
    pub jpeg: Vec<u8>,
}

/// Stable keyset cursor for chronological Visual Recall browsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineCursor {
    pub created_at_ms: i64,
    pub id: i64,
}

/// Direction from an exclusive timeline cursor. Pages always return newest-first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineDirection {
    #[default]
    Older,
    Newer,
}

/// Filters and pagination controls for one Visual Recall timeline page.
#[derive(Debug, Clone, Copy)]
pub struct TimelineQuery<'a> {
    pub query: &'a str,
    pub from_ms: i64,
    pub to_ms: i64,
    pub app_bundle_id: Option<&'a str>,
    pub cursor: Option<TimelineCursor>,
    pub direction: TimelineDirection,
    pub limit: usize,
}

/// Remove the extra keyset row fetched to detect another page.
///
/// Newer queries are read ascending and reversed before reaching callers, so their sentinel is at
/// the front. Older queries keep their sentinel at the back.
pub fn trim_page_sentinel<T>(
    frames: &mut Vec<T>,
    page_size: usize,
    direction: TimelineDirection,
) -> bool {
    let has_more = frames.len() > page_size;
    if has_more {
        match direction {
            TimelineDirection::Older => frames.truncate(page_size),
            TimelineDirection::Newer => {
                frames.remove(0);
            }
        }
    }
    has_more
}

/// Recall hit for context assembly — metadata + OCR excerpt, no bytes inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRecallHit {
    pub frame_id: i64,
    pub event_id: Option<i64>,
    /// Event that supplied the matching text: Accessibility when available, otherwise OCR.
    pub evidence_event_id: i64,
    pub ts: i64,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub width: u32,
    pub height: u32,
    pub ocr_excerpt: String,
    pub accessibility_excerpt: String,
    pub text_excerpt: String,
    /// `accessibility` or `ocr`; never inferred from app identity.
    pub text_source: String,
    /// True when stored OCR text is thin; caller should re-scan the JPEG (Vision path).
    pub needs_rescan: bool,
    /// Linked event source (`screen_ocr` or `user_screenshot`).
    pub source: String,
}

fn map_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<FrameSummary> {
    Ok(FrameSummary {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        event_id: row.get(2)?,
        accessibility_event_id: row.get(3)?,
        app_bundle_id: row.get(4)?,
        window_title: row.get(5)?,
        display_id: row.get(6)?,
        width: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        height: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
        jpeg_bytes: usize::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        ocr_text: row.get(10)?,
        source: row.get(11)?,
        accessibility_text: row.get(12)?,
        ocr_region: OcrRegion::from_columns([row.get(13)?, row.get(14)?, row.get(15)?, row.get(16)?]),
    })
}

const SUMMARY_SELECT: &str = "SELECT f.id, f.created_at_ms, f.event_id, f.accessibility_event_id,
    f.app_bundle_id, f.window_title, f.display_id, f.width, f.height, length(f.bytes),
    coalesce(e.content, ''), coalesce(e.source, 'screen_ocr'), coalesce(ax.content, ''),
    f.focus_x, f.focus_y, f.focus_width, f.focus_height";

/// Columns in [`SUMMARY_SELECT`]; anything selected after it starts at this index.
const SUMMARY_COLUMNS: usize = 17;

const SUMMARY_JOINS: &str = "LEFT JOIN event_log e ON e.id = f.event_id
    LEFT JOIN event_log ax ON ax.id = f.accessibility_event_id";

// Keep legacy self-captures out of every Recall read surface without deleting user data.
// Match capture::own_app's exact identities and dot-delimited isolated build suffixes.
// NULL attribution is preserved: it does not establish that the frame belongs to SHOGUN.
const RECALL_VISIBLE: &str = "(f.app_bundle_id IS NULL OR (
    lower(f.app_bundle_id) NOT IN ('com.syogun.shogunai', 'dev.shogun.spike',
                                 'shogunai', 'shogun', 'shogun-desktop-spike', '')
    AND lower(f.app_bundle_id) NOT GLOB 'com.syogun.shogunai.*'
    AND lower(f.app_bundle_id) NOT GLOB 'dev.shogun.spike.*'))";

/// Insert one compressed frame. Returns the new row id.
pub fn insert(conn: &Connection, frame: &NewFrame<'_>) -> Result<i64, rusqlite::Error> {
    let [focus_x, focus_y, focus_width, focus_height] = frame.ocr_region.columns();
    conn.execute(
        "INSERT INTO screen_frames
           (created_at_ms, event_id, accessibility_event_id, app_bundle_id, window_title,
            display_id, width, height, mime, bytes, focus_x, focus_y, focus_width, focus_height)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'image/jpeg', ?9, ?10, ?11, ?12, ?13)",
        params![
            frame.created_at_ms,
            frame.event_id,
            frame.accessibility_event_id,
            frame.app_bundle_id,
            frame.window_title,
            frame.display_id,
            i64::from(frame.width),
            i64::from(frame.height),
            frame.jpeg,
            focus_x,
            focus_y,
            focus_width,
            focus_height,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Lookup by frame row id.
pub fn get_summary_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<FrameSummary>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "{SUMMARY_SELECT}
             FROM screen_frames f
             {SUMMARY_JOINS}
             WHERE f.id = ?1 AND {RECALL_VISIBLE}"
        ),
        [id],
        map_summary,
    )
    .optional()
}

/// Lookup by frame row id, including JPEG bytes.
pub fn get_by_id(conn: &Connection, id: i64) -> Result<Option<FrameRecord>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "{SUMMARY_SELECT}, f.bytes
         FROM screen_frames f
         {SUMMARY_JOINS}
         WHERE f.id = ?1 AND {RECALL_VISIBLE}"
    ))?;
    let mut rows = stmt.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let summary = map_summary(row)?;
    let jpeg: Vec<u8> = row.get(SUMMARY_COLUMNS)?;
    Ok(Some(FrameRecord { summary, jpeg }))
}

/// Lookup by linked OCR or Accessibility event id.
pub fn get_by_event_id(conn: &Connection, event_id: i64) -> Result<Option<FrameRecord>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "{SUMMARY_SELECT}, f.bytes
         FROM screen_frames f
         {SUMMARY_JOINS}
         WHERE (f.event_id = ?1 OR f.accessibility_event_id = ?1) AND {RECALL_VISIBLE}
         ORDER BY f.id DESC LIMIT 1"
    ))?;
    let mut rows = stmt.query([event_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let summary = map_summary(row)?;
    let jpeg: Vec<u8> = row.get(SUMMARY_COLUMNS)?;
    Ok(Some(FrameRecord { summary, jpeg }))
}

/// List frames in a time window, newest first.
pub fn list_in_range(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<Vec<FrameSummary>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "{SUMMARY_SELECT}
         FROM screen_frames f
         {SUMMARY_JOINS}
         WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2 AND {RECALL_VISIBLE}
         ORDER BY f.created_at_ms DESC
         LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![from_ms, to_ms, limit as i64], map_summary)?;
    rows.collect()
}

/// The timeline's text filter, evaluated ONCE per request (#727).
/// Non-indexable queries use escaped LIKE over the same bounded event span.
///
/// A frame matches when its OCR event or its linked Accessibility event is in the FTS result.
/// Not a correlated `EXISTS (… event_fts MATCH ? AND rowid IN (f.event_id,
/// f.accessibility_event_id))`: that re-runs the MATCH for every frame in range, ~0.37 ms per
/// frame on the #714 harness, and tens of seconds at 5k frames over a 100k-event index.
///
/// `MATERIALIZED` pins the hit set to one evaluation even though [`TIMELINE_MATCHES`] reads it
/// twice; each uncorrelated `IN` then builds its lookup table once, and every frame costs a b-tree
/// probe against the covering `idx_screen_frames_timeline` (V58), never a read of the frame row.
/// The result is the same set: `x IN (a, b)` over the FTS rowids is `x = a OR x = b`, and a NULL
/// link (no Accessibility capture) matches nothing on that side in both forms.
///
/// The MATCH is also bounded to the rowid span of the events the frames in the time/app window
/// link to. A hit outside that span cannot belong to any of those frames, so the set of matching
/// frames is unchanged; FTS5 seeks within its doclists instead of walking the whole history, which
/// matters because event_log keeps years of every source while frames are kept for days. With no
/// frames in the window the span is NULL and the MATCH returns nothing.
///
/// Uses `?1`/`?2` (time window) and `?3` (app) like the statements it prefixes.
fn timeline_hits_cte(match_param: u8, substring: bool) -> String {
    let (hits, id_column) = if substring {
        (format!("SELECT id FROM event_log WHERE content LIKE ?{match_param} ESCAPE '\\'"), "id")
    } else {
        (format!("SELECT rowid FROM event_fts WHERE event_fts MATCH ?{match_param}"), "rowid")
    };
    format!(
        "WITH timeline_span(lo, hi) AS MATERIALIZED (
             SELECT min(link), max(link) FROM (
                 SELECT f.event_id AS link FROM screen_frames f
                  WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
                    AND (?3 IS NULL OR f.app_bundle_id = ?3)
                 UNION ALL
                 SELECT f.accessibility_event_id FROM screen_frames f
                  WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
                    AND (?3 IS NULL OR f.app_bundle_id = ?3)
             )
         ),
         timeline_hits(id) AS MATERIALIZED (
             {hits}
               AND {id_column} >= (SELECT lo FROM timeline_span)
               AND {id_column} <= (SELECT hi FROM timeline_span)
         )"
    )
}

/// Frame predicate over [`timeline_hits_cte`]. Order, keyset and filters are untouched.
const TIMELINE_MATCHES: &str = "(f.event_id IN (SELECT id FROM timeline_hits)
    OR f.accessibility_event_id IN (SELECT id FROM timeline_hits))";

/// SQL for one timeline page, and whether its rows come back oldest-first (to be reversed).
///
/// Parameters: `?1`/`?2` time window, `?3` app, `?4`/`?5` exclusive cursor, then with FTS `?6`
/// the MATCH expression and `?7` the limit (without FTS, `?6` is the limit).
fn timeline_page_sql(direction: TimelineDirection, fts: bool, substring: bool) -> (String, bool) {
    let (cursor_predicate, order, reverse) = match direction {
        TimelineDirection::Older => (
            "f.created_at_ms < ?4 OR (f.created_at_ms = ?4 AND f.id < ?5)",
            "f.created_at_ms DESC, f.id DESC",
            false,
        ),
        TimelineDirection::Newer => (
            "f.created_at_ms > ?4 OR (f.created_at_ms = ?4 AND f.id > ?5)",
            "f.created_at_ms ASC, f.id ASC",
            true,
        ),
    };
    let where_base = format!(
        "f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
         AND (?3 IS NULL OR f.app_bundle_id = ?3)
         AND (?4 IS NULL OR {cursor_predicate}) AND {RECALL_VISIBLE}"
    );
    let sql = if fts {
        // Pick the page's ids first, off the covering `idx_screen_frames_timeline` (V58) alone,
        // then read summaries for those rows only. Asking for the summary columns in the same
        // pass makes the planner walk the frame rows themselves, and a frame row is its own page
        // or more (the JPEG BLOB spills to overflow): a rare term would read every frame in range.
        format!(
            "{hits},
             timeline_page(id) AS MATERIALIZED (
                 SELECT f.id FROM screen_frames f
                 WHERE {where_base}
                   AND {TIMELINE_MATCHES}
                 ORDER BY {order} LIMIT ?7
             )
             {SUMMARY_SELECT}
             FROM timeline_page
             JOIN screen_frames f ON f.id = timeline_page.id
             {SUMMARY_JOINS}
             ORDER BY {order}",
            hits = timeline_hits_cte(6, substring),
        )
    } else {
        format!(
            "{SUMMARY_SELECT}
             FROM screen_frames f
             {SUMMARY_JOINS}
             WHERE {where_base}
             ORDER BY {order} LIMIT ?6"
        )
    };
    (sql, reverse)
}

/// SQL for [`timeline_count`]: `?1`/`?2` time window, `?3` app, and with FTS `?4` the MATCH.
fn timeline_count_sql(fts: bool, substring: bool) -> String {
    let where_base = format!("f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
        AND (?3 IS NULL OR f.app_bundle_id = ?3) AND {RECALL_VISIBLE}");
    if fts {
        format!(
            "{hits}
             SELECT count(*) FROM screen_frames f
             WHERE {where_base}
               AND {TIMELINE_MATCHES}",
            hits = timeline_hits_cte(4, substring),
        )
    } else {
        format!("SELECT count(*) FROM screen_frames f WHERE {where_base}")
    }
}

// Trigram MATCH cannot serve short or non-indexable input. Search linked text within the
// same retained time/app window instead, with literal SQL wildcards and the same page bound.
fn timeline_expression(query: &str) -> (Option<String>, bool) {
    let query = query.trim();
    if query.is_empty() {
        return (None, false);
    }
    if query.chars().count() >= 3 {
        if let Some(expr) = crate::search::fts_query(query) {
            return (Some(expr), false);
        }
    }
    let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    (Some(format!("%{escaped}%")), true)
}

/// Browse retained frames newest-first without loading JPEG bytes.
///
/// `cursor` is exclusive. Direction handles both sides of timestamp/id ties while returned rows
/// remain newest-first.
pub fn timeline_page(
    conn: &Connection,
    query: &TimelineQuery<'_>,
) -> Result<Vec<FrameSummary>, rusqlite::Error> {
    let cursor_ts = query.cursor.map(|value| value.created_at_ms);
    let cursor_id = query.cursor.map(|value| value.id);
    let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
    let (expr, substring) = timeline_expression(query.query);
    let (sql, reverse) = timeline_page_sql(query.direction, expr.is_some(), substring);
    let mut stmt = conn.prepare(&sql)?;
    let base = (query.from_ms, query.to_ms, query.app_bundle_id, cursor_ts, cursor_id);
    let rows = match &expr {
        Some(expr) => stmt.query_map(
            params![base.0, base.1, base.2, base.3, base.4, expr, limit],
            map_summary,
        )?,
        None => stmt.query_map(params![base.0, base.1, base.2, base.3, base.4, limit], map_summary)?,
    };
    let mut frames: Vec<_> = rows.collect::<Result<_, _>>()?;
    if reverse {
        frames.reverse();
    }
    Ok(frames)
}

/// Count matching timeline rows without reading JPEG bytes.
pub fn timeline_count(
    conn: &Connection,
    query: &str,
    from_ms: i64,
    to_ms: i64,
    app_bundle_id: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    let (expr, substring) = timeline_expression(query);
    match expr {
        Some(expr) => conn.query_row(
            &timeline_count_sql(true, substring),
            params![from_ms, to_ms, app_bundle_id, expr],
            |row| row.get(0),
        ),
        None => conn.query_row(
            &timeline_count_sql(false, false),
            params![from_ms, to_ms, app_bundle_id],
            |row| row.get(0),
        ),
    }
}

/// Apps represented in a timeline filter, for local filter controls.
pub fn timeline_apps(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT f.app_bundle_id FROM screen_frames f
         WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
           AND f.app_bundle_id IS NOT NULL AND {RECALL_VISIBLE}
         ORDER BY f.app_bundle_id COLLATE NOCASE",
    ))?;
    let rows = stmt.query_map(params![from_ms, to_ms], |row| row.get(0))?;
    rows.collect()
}

/// Frame id for a linked OCR or Accessibility event, if any.
pub fn frame_id_for_event(conn: &Connection, event_id: i64) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT f.id FROM screen_frames f
         WHERE (f.event_id = ?1 OR f.accessibility_event_id = ?1) AND {RECALL_VISIBLE}
         ORDER BY f.id DESC LIMIT 1"),
        [event_id],
        |r| r.get(0),
    )
    .optional()
}

/// Latest frame id per event (one query — avoids N+1 in evidence assembly).
pub fn frame_ids_for_events(
    conn: &Connection,
    event_ids: &[i64],
) -> Result<std::collections::HashMap<i64, i64>, rusqlite::Error> {
    if event_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = std::iter::repeat_n("?", event_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT linked_event_id, id FROM (
            SELECT f.event_id AS linked_event_id, f.id FROM screen_frames f
            WHERE f.event_id IN ({placeholders}) AND {RECALL_VISIBLE}
            UNION ALL
            SELECT f.accessibility_event_id AS linked_event_id, f.id FROM screen_frames f
            WHERE f.accessibility_event_id IN ({placeholders}) AND {RECALL_VISIBLE}
         ) ORDER BY id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(
        event_ids.iter().chain(event_ids.iter()),
    ))?;
    let mut out = std::collections::HashMap::new();
    while let Some(row) = rows.next()? {
        let event_id: i64 = row.get(0)?;
        let frame_id: i64 = row.get(1)?;
        out.entry(event_id).or_insert(frame_id);
    }
    Ok(out)
}

/// Every frame's id, age and stored size — the input [`crate::retention::Policy::sweep`] decides
/// over. Metadata only: `length(bytes)` never loads the JPEG.
pub fn retention_items(conn: &Connection) -> Result<Vec<crate::retention::Item>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at_ms, length(bytes) FROM screen_frames ORDER BY created_at_ms, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(crate::retention::Item { id: r.get(0)?, created_at: r.get(1)?, bytes: r.get(2)? })
    })?;
    rows.collect()
}

/// Delete the frames a sweep selected, in ONE transaction. Returns rows removed.
///
/// **Frames only — the linked events stay.** Expiry retires the *image*, not the memory: the OCR
/// text and its provenance live in `event_log` and are the part SHOGUN is allowed to keep. This
/// is the opposite of [`delete_by_id`], where the user asked for a specific capture to be gone
/// and the record of it should go too.
///
/// One transaction so a crash mid-sweep leaves the cache consistent rather than half-expired.
pub fn delete_ids(conn: &mut Connection, ids: &[i64]) -> Result<usize, rusqlite::Error> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut removed = 0;
    for &id in ids {
        removed += tx.execute("DELETE FROM screen_frames WHERE id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(removed)
}

/// Visual recall Off: delete every passive frame (a `screen_ocr` event, or no OCR event at all)
/// together with the OCR text read from it, in ONE transaction. Each frame goes through the same path as a user's
/// single-frame delete, so a linked OCR event goes (with its FTS, search-document, embedding
/// and provenance rows) only once no other frame still references it. Other events for the
/// same app/window and Accessibility capture events are never touched.
///
/// Also deletes every `screen_text` event: the durable OCR copy kept where Accessibility missed a
/// window (frame expiry never removes it; Off does), in the same transaction.
///
/// Returns each removed frame's OCR event id (`None` for a preview without one) and the removed
/// `screen_text` event ids, for hot-memory reconciliation. Every frame of each returned event is
/// removed, so each returned event is deleted by commit.
pub fn purge_auto_only(
    conn: &mut Connection,
) -> Result<(Vec<Option<i64>>, Vec<i64>), rusqlite::Error> {
    let tx = conn.transaction()?;
    let frames = {
        // A preview with no OCR event (#969) is passive too: Off must leave none behind.
        let mut statement = tx.prepare(
            "SELECT f.id FROM screen_frames f
             LEFT JOIN event_log e ON e.id = f.event_id
             WHERE f.event_id IS NULL OR e.source = 'screen_ocr'",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let removed = delete_frames_in(&tx, &frames)?;
    let texts = {
        let mut statement = tx.prepare("SELECT id FROM event_log WHERE source = 'screen_text'")?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for &id in &texts {
        crate::search_documents::delete_canonical_event_in(&tx, id)?;
    }
    tx.commit()?;
    Ok((removed, texts))
}

/// Delete `ids` through [`delete_by_id_in`]: one OCR event id (or `None`) per removed frame.
fn delete_frames_in(
    tx: &rusqlite::Transaction<'_>,
    ids: &[i64],
) -> Result<Vec<Option<i64>>, rusqlite::Error> {
    let mut removed = Vec::with_capacity(ids.len());
    // Off deletes every screen_text event right after; hidden-self frames carry none in practice.
    let mut texts = Vec::new();
    for &id in ids {
        removed.extend(delete_by_id_in(tx, id, &mut texts)?);
    }
    Ok(removed)
}

/// Delete one frame and its now-orphaned visual-recall event atomically.
pub fn delete_by_id(conn: &mut Connection, id: i64) -> Result<bool, rusqlite::Error> {
    Ok(delete_by_id_with_copies(conn, id)?.is_some())
}

/// [`delete_by_id`], plus the frame's durable `screen_text` copy. The frame links it by event id
/// (`screen_text_event_id`), so near-duplicate collapse cannot detach it: deleting a frame that
/// caught something sensitive must not leave the same words searchable. The copy goes only when
/// no other frame still references it. `None` when no such frame exists, else the removed
/// copies' event ids (for hot-memory reconciliation).
pub fn delete_by_id_with_copies(
    conn: &mut Connection,
    id: i64,
) -> Result<Option<Vec<i64>>, rusqlite::Error> {
    let tx = conn.transaction()?;
    let mut copies = Vec::new();
    if delete_by_id_in(&tx, id, &mut copies)?.is_none() {
        return Ok(None);
    }
    tx.commit()?;
    Ok(Some(copies))
}

/// Record the `screen_text` event a just-stored frame produced or touched.
pub fn link_screen_text(conn: &Connection, frame_id: i64, event_id: i64) -> Result<bool, rusqlite::Error> {
    conn.execute(
        "UPDATE screen_frames SET screen_text_event_id = ?2 WHERE id = ?1",
        params![frame_id, event_id],
    )
    .map(|changed| changed == 1)
}

/// `None` when no such frame exists, else the removed frame's OCR event id (if it had one).
fn delete_by_id_in(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    deleted_texts: &mut Vec<i64>,
) -> Result<Option<Option<i64>>, rusqlite::Error> {
    let linked: Option<(Option<i64>, Option<i64>)> = tx
        .query_row(
            "SELECT event_id, screen_text_event_id FROM screen_frames WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((event_id, text_id)) = linked else {
        return Ok(None);
    };
    let removed = tx.execute("DELETE FROM screen_frames WHERE id = ?1", [id])?;
    if removed == 0 {
        return Ok(None);
    }
    if let Some(text_id) = text_id {
        let shared: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM screen_frames WHERE screen_text_event_id = ?1)",
            [text_id],
            |r| r.get(0),
        )?;
        if !shared {
            crate::search_documents::delete_canonical_event_in(tx, text_id)?;
            deleted_texts.push(text_id);
        }
    }
    let Some(event_id) = event_id else {
        return Ok(Some(None));
    };
    // Delete the linked event only when it is a visual-recall event no other frame still
    // references — and through the shared canonical helper: extraction may have linked
    // state_provenance rows to this event (no ON DELETE clause), so a raw event delete would
    // FK-fail and the user could not delete the frame at all.
    let orphaned: bool = tx.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM event_log
              WHERE id = ?1
                AND source IN ('screen_ocr', 'user_screenshot')
                AND NOT EXISTS (SELECT 1 FROM screen_frames WHERE event_id = ?1))",
        [event_id],
        |r| r.get(0),
    )?;
    if orphaned {
        crate::search_documents::delete_canonical_event_in(tx, event_id)?;
    }
    Ok(Some(Some(event_id)))
}

/// Link a frame that had no OCR event to the one a re-read just stored. Returns false when the
/// frame is gone or already linked.
pub fn link_ocr_event(conn: &Connection, frame_id: i64, event_id: i64) -> Result<bool, rusqlite::Error> {
    conn.execute(
        "UPDATE screen_frames SET event_id = ?2 WHERE id = ?1 AND event_id IS NULL",
        params![frame_id, event_id],
    )
    .map(|changed| changed == 1)
}

/// Explicitly delete legacy self-captures and orphaned OCR events in one transaction.
/// Returns each removed frame's OCR event id (`None` without one) for hot-memory reconciliation.
/// Uses the same identity predicate as Recall reads; unknown attribution is preserved.
pub fn delete_hidden_self_frames(conn: &mut Connection) -> Result<Vec<Option<i64>>, rusqlite::Error> {
    let tx = conn.transaction()?;
    let frames = {
        let mut statement =
            tx.prepare(&format!("SELECT f.id FROM screen_frames f WHERE NOT {RECALL_VISIBLE}"))?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let removed = delete_frames_in(&tx, &frames)?;
    tx.commit()?;
    Ok(removed)
}

/// Search frames for visual-recall questions: FTS on linked OCR text, scoped to a time window.
pub fn search_for_recall(
    conn: &Connection,
    query: &str,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
    excerpt_chars: usize,
) -> Result<Vec<FrameRecallHit>, rusqlite::Error> {
    let mut out = Vec::new();
    if let Some(expr) = crate::search::fts_query(query) {
        let mut stmt = conn.prepare(&format!(
            "WITH matches AS (
                SELECT f.id AS frame_id, f.event_id AS evidence_event_id,
                       bm25(event_fts) AS score, 'ocr' AS text_source
                FROM screen_frames f
                INNER JOIN event_fts ON event_fts.rowid = f.event_id
                WHERE event_fts MATCH ?3
                UNION ALL
                SELECT f.id AS frame_id, f.accessibility_event_id AS evidence_event_id,
                       bm25(event_fts) AS score, 'accessibility' AS text_source
                FROM screen_frames f
                INNER JOIN event_fts ON event_fts.rowid = f.accessibility_event_id
                WHERE event_fts MATCH ?3
            ), best AS (
                SELECT frame_id, evidence_event_id, text_source, score,
                       row_number() OVER (
                           PARTITION BY frame_id
                           ORDER BY score, CASE text_source WHEN 'accessibility' THEN 0 ELSE 1 END,
                                    evidence_event_id
                       ) AS choice
                FROM matches
            )
             {SUMMARY_SELECT}, best.evidence_event_id, best.text_source
             FROM screen_frames f
             {SUMMARY_JOINS}
             INNER JOIN best ON best.frame_id = f.id AND best.choice = 1
             WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
               AND (f.event_id IS NULL OR e.source IN ('screen_ocr', 'user_screenshot'))
               AND {RECALL_VISIBLE}
             ORDER BY best.score, f.created_at_ms DESC
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(params![from_ms, to_ms, expr, limit as i64], |row| {
            let summary = map_summary(row)?;
            let evidence_event_id = row.get(SUMMARY_COLUMNS)?;
            let text_source = row.get(SUMMARY_COLUMNS + 1)?;
            Ok(recall_hit_from_summary(
                &summary,
                evidence_event_id,
                text_source,
                query,
                excerpt_chars,
            ))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    if out.len() < limit {
        let seen: std::collections::HashSet<i64> = out.iter().map(|h| h.frame_id).collect();
        let room = limit.saturating_sub(out.len());
        for s in list_in_range(conn, from_ms, to_ms, room + seen.len())? {
            if seen.contains(&s.id) {
                continue;
            }
            let ax = s.accessibility_event_id.filter(|_| !s.accessibility_text.trim().is_empty());
            // A preview with neither text has nothing to offer as evidence.
            let Some((evidence_event_id, text_source)) = ax
                .map(|id| (id, "accessibility"))
                .or_else(|| s.event_id.map(|id| (id, "ocr")))
            else {
                continue;
            };
            out.push(recall_hit_from_summary(
                &s,
                evidence_event_id,
                text_source.to_string(),
                query,
                excerpt_chars,
            ));
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

fn recall_hit_from_summary(
    s: &FrameSummary,
    evidence_event_id: i64,
    text_source: String,
    query: &str,
    excerpt_chars: usize,
) -> FrameRecallHit {
    let needs_rescan = s.needs_rescan();
    let accessibility_excerpt = crate::search::excerpt(&s.accessibility_text, query, excerpt_chars);
    let ocr_excerpt = crate::search::excerpt(&s.ocr_text, query, excerpt_chars);
    let text_excerpt = if text_source == "accessibility" && !accessibility_excerpt.is_empty() {
        accessibility_excerpt.clone()
    } else {
        ocr_excerpt.clone()
    };
    FrameRecallHit {
        frame_id: s.id,
        event_id: s.event_id,
        evidence_event_id,
        ts: s.created_at_ms,
        app_bundle_id: s.app_bundle_id.clone(),
        window_title: s.window_title.clone(),
        width: s.width,
        height: s.height,
        ocr_excerpt,
        accessibility_excerpt,
        text_excerpt,
        text_source,
        needs_rescan,
        source: s.source.clone(),
    }
}

/// Aggregate stats for settings / status surfaces (no pixel payload).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub count: i64,
    pub oldest_ms: Option<i64>,
    pub total_bytes: i64,
}

pub fn stats(conn: &Connection) -> Result<FrameStats, rusqlite::Error> {
    // V87: triggers keep one small row per frame; summing `length(bytes)` read every frame row.
    conn.query_row(
        "SELECT count(*), min(created_at_ms), coalesce(sum(bytes), 0) FROM screen_frame_sizes",
        [],
        |r| {
            Ok(FrameStats {
                count: r.get(0)?,
                oldest_ms: r.get(1)?,
                total_bytes: r.get(2)?,
            })
        },
    )
}

/// Stored frames hidden because their exact attribution belongs to SHOGUN.
pub fn hidden_self_stats(conn: &Connection) -> Result<FrameStats, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT count(*), min(f.created_at_ms), coalesce(sum(length(f.bytes)), 0)
                  FROM screen_frames f WHERE NOT {RECALL_VISIBLE}"),
        [],
        |row| Ok(FrameStats {
            count: row.get(0)?,
            oldest_ms: row.get(1)?,
            total_bytes: row.get(2)?,
        }),
    )
}

/// Total encrypted JPEG bytes stored in a time window. The JPEG payload never leaves SQLite.
pub fn bytes_in_range(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT coalesce(sum(length(bytes)), 0) FROM screen_frames
         WHERE created_at_ms >= ?1 AND created_at_ms <= ?2",
        params![from_ms, to_ms],
        |row| row.get(0),
    )
}

/// #727: parity with the per-frame `EXISTS` and the once-per-request plan.
#[cfg(test)]
#[path = "screen_frames_timeline_tests.rs"]
mod timeline_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};

    fn seed_event(conn: &Connection, ts: i64, content: &str) -> i64 {
        seed_text_event(conn, ts, "screen_ocr", content)
    }

    fn seed_text_event(conn: &Connection, ts: i64, source: &str, content: &str) -> i64 {
        insert_event(
            conn,
            &NewEvent {
                ts,
                source,
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some("Inbox"),
                content,
                content_hash: &format!("h{ts}-{source}"),
                dwell_ms: 1,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn count_events(conn: &Connection, id: i64) -> i64 {
        conn.query_row("SELECT count(*) FROM event_log WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn deleting_a_frame_deletes_its_linked_screen_text_even_when_content_differs() {
        let mut conn = crate::open_in_memory().unwrap();
        let ocr = seed_text_event(&conn, 1_000, "screen_ocr", "zebrafish ocr text");
        // Collapse rewrote the copy: its content no longer equals the OCR text.
        let copy = seed_text_event(&conn, 1_001, "screen_text", "zebrafish ocr text, merged");
        let frame = seed_frame(&conn, 1_000, ocr, b"a");
        assert!(link_screen_text(&conn, frame, copy).unwrap());

        assert_eq!(delete_by_id_with_copies(&mut conn, frame).unwrap(), Some(vec![copy]));
        assert_eq!(count_events(&conn, copy), 0);
    }

    #[test]
    fn screen_text_shared_by_two_frames_survives_deleting_one() {
        let mut conn = crate::open_in_memory().unwrap();
        let ocr = seed_text_event(&conn, 1_000, "screen_ocr", "zebrafish ocr text");
        let copy = seed_text_event(&conn, 1_001, "screen_text", "zebrafish ocr text");
        let first = seed_frame(&conn, 1_000, ocr, b"a");
        let second = seed_frame(&conn, 1_001, ocr, b"b");
        link_screen_text(&conn, first, copy).unwrap();
        link_screen_text(&conn, second, copy).unwrap();

        assert_eq!(delete_by_id_with_copies(&mut conn, first).unwrap(), Some(vec![]));
        assert_eq!(count_events(&conn, copy), 1);
        assert_eq!(delete_by_id_with_copies(&mut conn, second).unwrap(), Some(vec![copy]));
        assert_eq!(count_events(&conn, copy), 0);
    }

    #[test]
    fn hidden_self_cleanup_matches_visibility_preserves_unknown_and_lookalikes() {
        let mut conn = crate::open_in_memory().unwrap();
        let bundles = [
            Some("com.syogun.shogunai"), Some("COM.SYOGUN.SHOGUNAI.qa"),
            Some("dev.shogun.spike"), Some("dev.shogun.spike.qa"), Some("ShogunAI"),
            Some("shogun"), Some("shogun-desktop-spike"), Some(""),
            None, Some("com.syogun.shogunaihelper"), Some("dev.shogun.spikehelper"),
            Some("com.apple.Safari"),
        ];
        let mut preserved = Vec::new();
        for (index, bundle) in bundles.iter().enumerate() {
            let ts = 1_000 + index as i64;
            let event = seed_event(&conn, ts, "screen text");
            let frame = seed_frame(&conn, ts, event, b"jpeg");
            conn.execute("UPDATE screen_frames SET app_bundle_id = ?1 WHERE id = ?2", params![bundle, frame]).unwrap();
            if index >= 8 { preserved.push(frame); }
        }
        assert_eq!(hidden_self_stats(&conn).unwrap().count, 8);
        assert_eq!(hidden_self_stats(&conn).unwrap().total_bytes, 32);
        assert_eq!(stats(&conn).unwrap().count, 12, "status alone does not delete");
        assert_eq!(delete_hidden_self_frames(&mut conn).unwrap().len(), 8);
        assert_eq!(hidden_self_stats(&conn).unwrap(), FrameStats::default());
        assert_eq!(stats(&conn).unwrap().total_bytes, 16);
        for frame in preserved {
            assert!(get_by_id(&conn, frame).unwrap().is_some());
        }
        assert!(delete_hidden_self_frames(&mut conn).unwrap().is_empty());
    }

    #[test]
    fn hidden_self_cleanup_keeps_shared_ocr_and_accessibility_memory() {
        let mut conn = crate::open_in_memory().unwrap();
        let shared = seed_event(&conn, 1_000, "shared OCR");
        let ax = seed_text_event(&conn, 1_001, "capture", "normal AX memory");
        let hidden = seed_frame_with_accessibility(&conn, 1_000, shared, Some(ax), b"hidden");
        let visible = seed_frame(&conn, 1_001, shared, b"visible");
        let second_hidden = seed_frame(&conn, 1_002, shared, b"another hidden");
        for frame in [hidden, second_hidden] {
            conn.execute("UPDATE screen_frames SET app_bundle_id = 'shogun' WHERE id = ?1", [frame]).unwrap();
        }
        assert_eq!(delete_hidden_self_frames(&mut conn).unwrap(), vec![Some(shared), Some(shared)]);
        assert!(get_by_id(&conn, visible).unwrap().is_some());
        for event in [shared, ax] {
            assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)", [event], |row| row.get::<_, bool>(0)).unwrap());
        }
    }

    #[test]
    fn hidden_self_cleanup_rolls_back_all_frames_if_canonical_delete_fails() {
        let mut conn = crate::open_in_memory().unwrap();
        for ts in [1_000, 1_001] {
            let event = seed_event(&conn, ts, "OCR");
            let frame = seed_frame(&conn, ts, event, b"jpeg");
            conn.execute("UPDATE screen_frames SET app_bundle_id = 'shogun' WHERE id = ?1", [frame]).unwrap();
        }
        conn.execute_batch("CREATE TRIGGER reject_event_delete BEFORE DELETE ON event_log BEGIN SELECT RAISE(ABORT, 'test refusal'); END;").unwrap();
        assert!(delete_hidden_self_frames(&mut conn).is_err());
        assert_eq!(hidden_self_stats(&conn).unwrap().count, 2);
        assert_eq!(stats(&conn).unwrap().total_bytes, 8);
    }

    #[test]
    fn legacy_self_frames_are_hidden_from_all_recall_reads_without_deletion() {
        let conn = crate::open_in_memory().unwrap();
        let visible_event = seed_event(&conn, 1_000, "public invoice");
        let visible = seed_frame(&conn, 1_000, visible_event, &[1]);
        let mut hidden = Vec::new();
        for (index, bundle) in [
            "com.syogun.shogunai",
            "COM.SYOGUN.SHOGUNAI.test",
            "dev.shogun.spike.qa",
            "ShogunAI",
        ]
        .iter()
        .enumerate()
        {
            let ts = 2_000 + index as i64;
            let event = seed_event(&conn, ts, "public invoice");
            let frame = seed_frame(&conn, ts, event, &[2]);
            conn.execute(
                "UPDATE screen_frames SET app_bundle_id = ?1 WHERE id = ?2",
                params![bundle, frame],
            )
            .unwrap();
            hidden.push((event, frame));
        }
        assert_eq!(list_in_range(&conn, 0, 10_000, 1).unwrap()[0].id, visible);
        for query in ["", "invoice"] {
            assert_eq!(timeline_count(&conn, query, 0, 10_000, None).unwrap(), 1);
            let page = timeline_page(
                &conn,
                &TimelineQuery {
                    query,
                    from_ms: 0,
                    to_ms: 10_000,
                    app_bundle_id: None,
                    cursor: None,
                    direction: TimelineDirection::Older,
                    limit: 1,
                },
            )
            .unwrap();
            assert_eq!(page[0].id, visible);
            assert_eq!(
                search_for_recall(&conn, query, 0, 10_000, 10, 100)
                    .unwrap()
                    .iter()
                    .map(|hit| hit.frame_id)
                    .collect::<Vec<_>>(),
                vec![visible]
            );
        }
        assert_eq!(
            timeline_apps(&conn, 0, 10_000).unwrap(),
            vec!["com.apple.Safari"]
        );
        for (event, frame) in &hidden {
            assert!(get_summary_by_id(&conn, *frame).unwrap().is_none());
            assert!(get_by_id(&conn, *frame).unwrap().is_none());
            assert!(get_by_event_id(&conn, *event).unwrap().is_none());
            assert!(frame_id_for_event(&conn, *event).unwrap().is_none());
        }
        let mut event_ids = hidden.iter().map(|(event, _)| *event).collect::<Vec<_>>();
        event_ids.push(visible_event);
        assert_eq!(
            frame_ids_for_events(&conn, &event_ids).unwrap(),
            std::collections::HashMap::from([(visible_event, visible)])
        );
        assert_eq!(
            stats(&conn).unwrap().count,
            5,
            "retention and storage accounting still include hidden rows"
        );
        assert_eq!(retention_items(&conn).unwrap().len(), 5);
        conn.execute(
            "UPDATE screen_frames SET app_bundle_id = 'com.syogun.shogunaihelper' WHERE id = ?1",
            [hidden[0].1],
        )
        .unwrap();
        assert_eq!(
            timeline_count(&conn, "", 0, 10_000, None).unwrap(),
            2,
            "lookalike apps remain visible"
        );
    }

    /// Deleting expired frames never touches the `screen_text` copy.
    #[test]
    fn frame_expiry_keeps_screen_text() {
        let mut conn = crate::open_in_memory().unwrap();
        let ocr = seed_event(&conn, 1_000, "expiring zebrafish text");
        let durable = seed_text_event(&conn, 1_000, "screen_text", "zebrafish kept past the frame");
        let frame = seed_frame(&conn, 1_000, ocr, &[1]);

        assert_eq!(delete_ids(&mut conn, &[frame]).unwrap(), 1);

        let exists = |id: i64| -> bool {
            conn.query_row("SELECT EXISTS (SELECT 1 FROM event_log WHERE id = ?1)", [id], |r| r.get(0)).unwrap()
        };
        assert!(exists(durable));
    }

    /// Turning Visual recall Off promises an immediate purge of passive frames (CLAUDE.md
    /// invariant 2 exception, mechanism.mdx). Pin it so a refactor that links frames elsewhere
    /// cannot silently keep screenshots (issue #782). Owner decision: Off also deletes the OCR
    /// text read from those frames — and nothing else.
    #[test]
    fn purge_auto_only_removes_passive_frames_and_their_ocr_text_only() {
        let mut conn = crate::open_in_memory().unwrap();
        let passive = seed_event(&conn, 1_000, "passive zebrafish text");
        let other = seed_text_event(&conn, 2_000, "user", "a user note");
        // Same app/window as the passive frame, but ordinary Accessibility capture.
        let ax = seed_text_event(&conn, 1_000, "capture", "zebrafish via accessibility");
        // An OCR event no frame points at (frame already expired).
        let frameless = seed_event(&conn, 500, "frameless zebrafish ocr");
        seed_frame_with_accessibility(&conn, 1_000, passive, Some(ax), &[1]);
        seed_frame(&conn, 1_500, passive, &[2]);
        let kept = seed_frame(&conn, 2_000, other, &[3]);
        // Durable OCR copy kept where Accessibility missed the window, linked to no frame.
        let durable = seed_text_event(&conn, 900, "screen_text", "zebrafish kept past the frame");

        let (frames, texts) = purge_auto_only(&mut conn).unwrap();
        assert_eq!(frames, vec![Some(passive), Some(passive)]);
        assert_eq!(texts, vec![durable]);
        assert!(
            !conn
                .query_row("SELECT EXISTS (SELECT 1 FROM event_log WHERE id = ?1)", [durable], |r| r.get::<_, bool>(0))
                .unwrap(),
            "Off also deletes the durable screen_text copy"
        );
        let left: Vec<i64> = conn
            .prepare("SELECT id FROM screen_frames")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, vec![kept]);
        let event_exists = |id: i64| -> bool {
            conn.query_row("SELECT EXISTS (SELECT 1 FROM event_log WHERE id = ?1)", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(!event_exists(passive), "OCR text read from the deleted frames goes too");
        assert!(event_exists(other), "a non-passive frame's event stays");
        assert!(event_exists(ax), "Accessibility capture for the same app stays");
        assert!(event_exists(frameless), "OCR text with no deleted frame stays");
        let fts_hits: Vec<i64> = conn
            .prepare("SELECT rowid FROM event_fts WHERE event_fts MATCH ?1 ORDER BY rowid")
            .unwrap()
            .query_map([crate::search::fts_query("zebrafish").unwrap()], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(fts_hits, vec![ax, frameless], "deleted OCR text is not findable");
        assert!(purge_auto_only(&mut conn).unwrap().0.is_empty(), "idempotent");
    }

    fn seed_frame(conn: &Connection, ts: i64, event_id: i64, jpeg: &[u8]) -> i64 {
        seed_frame_with_accessibility(conn, ts, event_id, None, jpeg)
    }

    fn seed_frame_with_accessibility(
        conn: &Connection,
        ts: i64,
        event_id: i64,
        accessibility_event_id: Option<i64>,
        jpeg: &[u8],
    ) -> i64 {
        insert(
            conn,
            &NewFrame {
                created_at_ms: ts,
                event_id: Some(event_id),
                accessibility_event_id,
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some("Inbox"),
                display_id: Some(1),
                width: 100,
                height: 50,
                ocr_region: OcrRegion::WholeFrame,
                jpeg,
            },
        )
        .unwrap()
    }

    /// A full-display preview taken while Accessibility text was enough: no OCR event (#969).
    fn seed_preview(
        conn: &Connection,
        ts: i64,
        accessibility_event_id: Option<i64>,
        ocr_region: OcrRegion,
    ) -> i64 {
        insert(
            conn,
            &NewFrame {
                created_at_ms: ts,
                event_id: None,
                accessibility_event_id,
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some("Inbox"),
                display_id: Some(2),
                width: 1_000,
                height: 500,
                ocr_region,
                jpeg: &[0xFF, 0xD8, 0xFF, 0xD9],
            },
        )
        .unwrap()
    }

    const WINDOW: FrameRect = FrameRect { x: 100, y: 50, width: 400, height: 250 };

    #[test]
    fn preview_without_ocr_is_purged_by_off_and_keeps_accessibility_text() {
        let mut conn = crate::open_in_memory().unwrap();
        let ax = seed_text_event(&conn, 1_000, "capture", "zebrafish via accessibility");
        let preview = seed_preview(&conn, 1_000, Some(ax), OcrRegion::Window(WINDOW));
        let bare = seed_preview(&conn, 1_100, None, OcrRegion::Window(WINDOW));

        assert_eq!(purge_auto_only(&mut conn).unwrap().0, vec![None, None]);
        assert!(get_by_id(&conn, preview).unwrap().is_none());
        assert!(get_by_id(&conn, bare).unwrap().is_none());
        assert!(conn
            .query_row("SELECT EXISTS (SELECT 1 FROM event_log WHERE id = ?1)", [ax], |r| r.get::<_, bool>(0))
            .unwrap());
    }

    #[test]
    fn preview_without_ocr_can_be_deleted_and_hidden_cleanup_handles_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let ax = seed_text_event(&conn, 1_000, "capture", "normal AX memory");
        let preview = seed_preview(&conn, 1_000, Some(ax), OcrRegion::Window(WINDOW));
        assert!(delete_by_id(&mut conn, preview).unwrap());
        assert!(!delete_by_id(&mut conn, preview).unwrap(), "already gone");

        let hidden = seed_preview(&conn, 1_100, Some(ax), OcrRegion::Window(WINDOW));
        conn.execute("UPDATE screen_frames SET app_bundle_id = 'shogun' WHERE id = ?1", [hidden]).unwrap();
        assert_eq!(delete_hidden_self_frames(&mut conn).unwrap(), vec![None]);
        assert!(conn
            .query_row("SELECT EXISTS (SELECT 1 FROM event_log WHERE id = ?1)", [ax], |r| r.get::<_, bool>(0))
            .unwrap());
    }

    #[test]
    fn recall_search_reaches_previews_by_accessibility_text_and_skips_textless_ones() {
        let conn = crate::open_in_memory().unwrap();
        let ax = seed_text_event(&conn, 1_000, "capture", "quarterly zebrafish roadmap");
        let preview = seed_preview(&conn, 1_000, Some(ax), OcrRegion::Window(WINDOW));
        seed_preview(&conn, 1_100, None, OcrRegion::Window(WINDOW));

        let hits = search_for_recall(&conn, "zebrafish", 0, 10_000, 10, 80).unwrap();
        assert_eq!(hits.len(), 1, "a preview with no text is not evidence: {hits:?}");
        assert_eq!(hits[0].frame_id, preview);
        assert_eq!(hits[0].event_id, None);
        assert_eq!(hits[0].evidence_event_id, ax);
        assert_eq!(hits[0].text_source, "accessibility");
        assert!(!hits[0].needs_rescan, "a saved preview never triggers OCR by itself");
    }

    #[test]
    fn ocr_region_round_trips_and_normalizes_to_the_focused_window() {
        let conn = crate::open_in_memory().unwrap();
        let window = seed_preview(&conn, 1_000, None, OcrRegion::Window(WINDOW));
        let event = seed_event(&conn, 900, "legacy window OCR");
        let legacy = seed_frame(&conn, 900, event, &[1]);
        let region = |id| get_summary_by_id(&conn, id).unwrap().unwrap().ocr_region;

        assert_eq!(region(window), OcrRegion::Window(WINDOW));
        assert_eq!(region(legacy), OcrRegion::WholeFrame);
        assert_eq!(OcrRegion::Window(WINDOW).normalized(1_000, 500), Some([0.1, 0.1, 0.4, 0.5]));
        assert_eq!(OcrRegion::WholeFrame.normalized(100, 50), Some([0.0, 0.0, 1.0, 1.0]));
        let past_edge = FrameRect { x: 500, y: 0, width: 1_000, height: 500 };
        assert_eq!(OcrRegion::Window(past_edge).normalized(1_000, 500), Some([0.5, 0.0, 0.5, 1.0]));
        let outside = FrameRect { x: 1_200, y: 0, width: 10, height: 10 };
        assert_eq!(OcrRegion::Window(outside).normalized(1_000, 500), None);
    }

    #[test]
    fn rescan_links_a_preview_once() {
        let conn = crate::open_in_memory().unwrap();
        let preview = seed_preview(&conn, 1_000, None, OcrRegion::Window(WINDOW));
        let ocr = seed_event(&conn, 1_000, "re-read text");
        assert!(link_ocr_event(&conn, preview, ocr).unwrap());
        assert!(!link_ocr_event(&conn, preview, ocr).unwrap(), "already linked");
        let linked = get_summary_by_id(&conn, preview).unwrap().unwrap();
        assert_eq!(linked.event_id, Some(ocr));
        assert!(linked.needs_rescan(), "thin OCR read from the window may be re-read");
    }

    #[test]
    fn insert_and_stats() {
        let conn = crate::open_in_memory().unwrap();
        let event_id = seed_event(&conn, 1_000, "hello");
        seed_frame(&conn, 1_000, event_id, &[0xFF, 0xD8, 0xFF, 0x00]);
        let s = stats(&conn).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.oldest_ms, Some(1_000));
        assert_eq!(s.total_bytes, 4);
    }

    #[test]
    fn timeline_keyset_and_fts_intersect_time_and_app_filters() {
        let conn = crate::open_in_memory().unwrap();
        let old_event = seed_event(&conn, 1_000, "project comet notes");
        let old_id = seed_frame(&conn, 1_000, old_event, b"old");
        let tie_event = seed_event(&conn, 2_000, "project comet mail");
        let tie_id = seed_frame(&conn, 2_000, tie_event, b"tie");
        let mail_event = seed_event(&conn, 2_000, "project comet inbox");
        let mail_id = seed_frame(&conn, 2_000, mail_event, b"mail");
        conn.execute(
            "UPDATE screen_frames SET app_bundle_id = 'com.apple.Mail' WHERE id = ?1",
            [mail_id],
        )
        .unwrap();

        let first = timeline_page(
            &conn,
            &TimelineQuery {
                query: "comet",
                from_ms: 0,
                to_ms: 3_000,
                app_bundle_id: None,
                cursor: None,
                direction: TimelineDirection::Older,
                limit: 1,
            },
        )
        .unwrap();
        assert_eq!(
            first[0].id, mail_id,
            "timestamp ties break by id descending"
        );
        let second = timeline_page(
            &conn,
            &TimelineQuery {
                query: "comet",
                from_ms: 0,
                to_ms: 3_000,
                app_bundle_id: None,
                cursor: Some(TimelineCursor {
                    created_at_ms: first[0].created_at_ms,
                    id: first[0].id,
                }),
                direction: TimelineDirection::Older,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(
            second.iter().map(|frame| frame.id).collect::<Vec<_>>(),
            vec![tie_id, old_id]
        );

        let safari = timeline_page(
            &conn,
            &TimelineQuery {
                query: "comet",
                from_ms: 1_500,
                to_ms: 3_000,
                app_bundle_id: Some("com.apple.Safari"),
                cursor: None,
                direction: TimelineDirection::Older,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(
            safari.iter().map(|frame| frame.id).collect::<Vec<_>>(),
            vec![tie_id]
        );
        assert_eq!(timeline_count(&conn, "comet", 0, 3_000, None).unwrap(), 3);
        assert_eq!(
            timeline_apps(&conn, 0, 3_000).unwrap(),
            vec!["com.apple.Mail", "com.apple.Safari"]
        );

        let newer = timeline_page(
            &conn,
            &TimelineQuery {
                query: "comet",
                from_ms: 0,
                to_ms: 3_000,
                app_bundle_id: None,
                cursor: Some(TimelineCursor {
                    created_at_ms: 1_000,
                    id: old_id,
                }),
                direction: TimelineDirection::Newer,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(
            newer.iter().map(|frame| frame.id).collect::<Vec<_>>(),
            vec![mail_id, tie_id]
        );
    }

    #[test]
    fn timeline_substrings_preserve_filters_pagination_and_literal_wildcards() {
        let conn = crate::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for ts in [1_000, 2_000, 3_000] {
            let event = seed_event(&conn, ts, "日本語 100% a_b \\ !!!");
            ids.push(seed_frame(&conn, ts, event, b"frame"));
        }
        let ax = seed_event(&conn, 2_000, "日本語 accessibility");
        let ocr = seed_event(&conn, 2_000, "unrelated OCR");
        let ax_frame = seed_frame(&conn, 2_000, ocr, b"ax");
        conn.execute("UPDATE screen_frames SET accessibility_event_id = ?1 WHERE id = ?2", params![ax, ax_frame]).unwrap();
        let other = seed_event(&conn, 2_000, "日本語 1000 axb");
        let other_frame = seed_frame(&conn, 2_000, other, b"other");
        conn.execute("UPDATE screen_frames SET app_bundle_id = 'com.apple.Mail' WHERE id = ?1", [other_frame]).unwrap();
        let hidden = seed_event(&conn, 2_000, "日本語");
        let hidden_frame = seed_frame(&conn, 2_000, hidden, b"hidden");
        conn.execute("UPDATE screen_frames SET app_bundle_id = 'com.syogun.shogunai' WHERE id = ?1", [hidden_frame]).unwrap();
        let query = TimelineQuery {
            query: "日本", from_ms: 1_500, to_ms: 2_500,
            app_bundle_id: Some("com.apple.Safari"), cursor: None,
            direction: TimelineDirection::Older, limit: 1,
        };
        let page = timeline_page(&conn, &query).unwrap();
        assert_eq!(page.iter().map(|frame| frame.id).collect::<Vec<_>>(), vec![ax_frame]);
        assert_eq!(timeline_count(&conn, "日本", 1_500, 2_500, query.app_bundle_id).unwrap(), 2);
        let older = timeline_page(&conn, &TimelineQuery {
            cursor: Some(TimelineCursor { created_at_ms: 2_000, id: ax_frame }), ..query
        }).unwrap();
        assert_eq!(older.iter().map(|frame| frame.id).collect::<Vec<_>>(), vec![ids[1]]);
        let newer = timeline_page(&conn, &TimelineQuery {
            cursor: Some(TimelineCursor { created_at_ms: 2_000, id: ids[1] }),
            direction: TimelineDirection::Newer, ..query
        }).unwrap();
        assert_eq!(newer.iter().map(|frame| frame.id).collect::<Vec<_>>(), vec![ax_frame]);
        assert_eq!(timeline_count(&conn, "日", 0, 4_000, None).unwrap(), 5);
        for literal in ["%", "_", "\\", "!!!"] {
            assert_eq!(timeline_count(&conn, literal, 0, 4_000, None).unwrap(), 3, "{literal}");
            let page = timeline_page(&conn, &TimelineQuery {
                query: literal, from_ms: 0, to_ms: 4_000, app_bundle_id: None,
                cursor: None, direction: TimelineDirection::Older, limit: 2,
            }).unwrap();
            assert_eq!(page.iter().map(|frame| frame.id).collect::<Vec<_>>(), vec![ids[2], ids[1]], "{literal}");
        }
    }

    // mem-001: assert actual matching rows for the audit examples, not only empty results.
    #[test]
    fn timeline_audit_short_queries_return_only_matching_frames() {
        let conn = crate::open_in_memory().unwrap();
        let cat = seed_event(&conn, 1_000, "猫の写真");
        let cat_frame = seed_frame(&conn, 1_000, cat, b"cat");
        let name = seed_event(&conn, 2_000, "田中さんの予定");
        let name_frame = seed_frame(&conn, 2_000, name, b"name");
        let stopword = seed_event(&conn, 3_000, "the project is ready");
        let stopword_frame = seed_frame(&conn, 3_000, stopword, b"stopword");
        let ax = seed_text_event(&conn, 4_000, "capture", "猫 the accessibility");
        let unrelated = seed_event(&conn, 4_000, "unrelated OCR");
        let ax_frame = seed_frame_with_accessibility(&conn, 4_000, unrelated, Some(ax), b"ax");
        let unrelated = seed_event(&conn, 5_000, "unrelated window");
        seed_frame(&conn, 5_000, unrelated, b"unrelated");

        assert_eq!(timeline_count(&conn, "", 0, 6_000, None).unwrap(), 5);
        for (query, expected) in [
            ("猫", vec![ax_frame, cat_frame]),
            ("田中", vec![name_frame]),
            ("the", vec![ax_frame, stopword_frame]),
            ("  猫  ", vec![ax_frame, cat_frame]),
            ("犬", vec![]),
        ] {
            let frames = timeline_page(
                &conn,
                &TimelineQuery {
                    query,
                    from_ms: 0,
                    to_ms: 6_000,
                    app_bundle_id: None,
                    cursor: None,
                    direction: TimelineDirection::Older,
                    limit: 10,
                },
            )
            .unwrap();
            assert_eq!(
                frames.iter().map(|frame| frame.id).collect::<Vec<_>>(),
                expected,
                "{query}"
            );
            assert_eq!(
                timeline_count(&conn, query, 0, 6_000, None).unwrap(),
                i64::try_from(expected.len()).unwrap(),
                "{query}"
            );
        }
    }

    #[test]
    fn newer_page_trimming_drops_the_front_sentinel_after_reversal() {
        let mut frames = vec![6, 5, 4];

        let has_more = trim_page_sentinel(&mut frames, 2, TimelineDirection::Newer);

        assert_eq!((has_more, frames), (true, vec![5, 4]));
    }

    #[test]
    fn older_page_trimming_drops_the_back_sentinel() {
        let mut frames = vec![6, 5, 4];

        let has_more = trim_page_sentinel(&mut frames, 2, TimelineDirection::Older);

        assert_eq!((has_more, frames), (true, vec![6, 5]));
    }

    #[test]
    fn a_sweep_expires_only_by_selected_age() {
        use crate::retention::Policy;
        let mut conn = crate::open_in_memory().unwrap();
        // Four frames of 4 bytes each. Only the oldest is past a 1_000 ms window.
        for (ts, body) in [(100i64, b"aaaa"), (5_000, b"bbbb"), (5_100, b"cccc"), (5_200, b"dddd")] {
            let e = seed_event(&conn, ts, "x");
            seed_frame(&conn, ts, e, body);
        }
        let items = retention_items(&conn).unwrap();
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|i| i.bytes == 4), "size comes from length(bytes)");

        let sweep = Policy::frames(1_000).sweep(&items, 5_200);
        assert_eq!(sweep.expired.len(), 1, "the 100 ms frame is past the window");

        let removed = delete_ids(&mut conn, &sweep.all()).unwrap();
        assert_eq!(removed, 1);
        let s = stats(&conn).unwrap();
        assert_eq!(s.count, 3);
        assert_eq!(s.total_bytes, 12, "in-window bytes are never budget-evicted");
        assert_eq!(s.oldest_ms, Some(5_000));
        // The OCR events all survive. Expiry retires the image, not the memory it produced —
        // the text and its provenance are what SHOGUN keeps (V12's rollback note says the same).
        let events: i64 = conn
            .query_row("SELECT count(*) FROM event_log WHERE source = 'screen_ocr'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(events, 4, "expiring a frame must not delete the text it yielded");
    }

    #[test]
    fn deleting_no_ids_touches_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, 100, "x");
        seed_frame(&conn, 100, e, b"jpeg");
        assert_eq!(delete_ids(&mut conn, &[]).unwrap(), 0);
        assert_eq!(stats(&conn).unwrap().count, 1);
    }

    #[test]
    fn get_by_event_and_search() {
        let conn = crate::open_in_memory().unwrap();
        let event_id = seed_event(&conn, 5_000, "quarterly roadmap slide");
        let frame_id = seed_frame(&conn, 5_000, event_id, b"jpeg");
        let rec = get_by_event_id(&conn, event_id).unwrap().expect("frame");
        assert_eq!(rec.summary.id, frame_id);
        assert_eq!(rec.jpeg, b"jpeg");
        let hits = search_for_recall(&conn, "roadmap", 0, 10_000, 5, 80).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frame_id, frame_id);
        assert_eq!(frame_id_for_event(&conn, event_id).unwrap(), Some(frame_id));
    }

    #[test]
    fn accessibility_text_is_linked_searchable_and_keeps_its_provenance() {
        let conn = crate::open_in_memory().unwrap();
        let ocr_event_id = seed_event(&conn, 5_000, "Telegram toolbar");
        let accessibility_event_id =
            seed_text_event(&conn, 5_000, "capture", "Maya owns project Orchid");
        let frame_id = seed_frame_with_accessibility(
            &conn,
            5_000,
            ocr_event_id,
            Some(accessibility_event_id),
            b"jpeg",
        );

        let summary = get_summary_by_id(&conn, frame_id).unwrap().expect("frame");
        assert_eq!(summary.accessibility_event_id, Some(accessibility_event_id));
        assert_eq!(summary.accessibility_text, "Maya owns project Orchid");

        let hits = search_for_recall(&conn, "Orchid", 0, 10_000, 5, 80).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].evidence_event_id, accessibility_event_id);
        assert_eq!(hits[0].text_source, "accessibility");
        assert!(hits[0].text_excerpt.contains("Orchid"));

        let mapped = frame_ids_for_events(&conn, &[ocr_event_id, accessibility_event_id]).unwrap();
        assert_eq!(mapped.get(&ocr_event_id), Some(&frame_id));
        assert_eq!(mapped.get(&accessibility_event_id), Some(&frame_id));
    }

    #[test]
    fn timeline_search_matches_accessibility_and_ocr_without_merging_text() {
        let conn = crate::open_in_memory().unwrap();
        let ocr_event_id = seed_event(&conn, 5_000, "fallback canvas glyphs");
        let accessibility_event_id =
            seed_text_event(&conn, 5_000, "capture", "Maya owns project Orchid");
        let frame_id = seed_frame_with_accessibility(
            &conn,
            5_000,
            ocr_event_id,
            Some(accessibility_event_id),
            b"jpeg",
        );

        for query in ["Orchid", "glyphs"] {
            let frames = timeline_page(
                &conn,
                &TimelineQuery {
                    query,
                    from_ms: 0,
                    to_ms: 10_000,
                    app_bundle_id: None,
                    cursor: None,
                    direction: TimelineDirection::Older,
                    limit: 5,
                },
            )
            .unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, frame_id);
            assert_eq!(timeline_count(&conn, query, 0, 10_000, None).unwrap(), 1);
        }

        let summary = get_summary_by_id(&conn, frame_id).unwrap().unwrap();
        assert_eq!(summary.accessibility_text, "Maya owns project Orchid");
        assert_eq!(summary.ocr_text, "fallback canvas glyphs");
    }

    #[test]
    fn deleting_accessibility_memory_nulls_only_its_optional_frame_link() {
        let conn = crate::open_in_memory().unwrap();
        let ocr_event_id = seed_event(&conn, 5_000, "fallback OCR survives");
        let accessibility_event_id = seed_text_event(&conn, 5_000, "capture", "AX text");
        let frame_id = seed_frame_with_accessibility(
            &conn,
            5_000,
            ocr_event_id,
            Some(accessibility_event_id),
            b"jpeg",
        );

        conn.execute("DELETE FROM event_log WHERE id = ?1", [accessibility_event_id])
            .unwrap();

        let summary = get_summary_by_id(&conn, frame_id).unwrap().expect("frame survives");
        assert_eq!(summary.accessibility_event_id, None);
        assert!(summary.accessibility_text.is_empty());
        assert_eq!(summary.ocr_text, "fallback OCR survives");
    }

    #[test]
    fn deleting_visual_frame_preserves_normal_accessibility_memory() {
        let mut conn = crate::open_in_memory().unwrap();
        let ocr_event_id = seed_event(&conn, 5_000, "OCR text");
        let accessibility_event_id = seed_text_event(&conn, 5_000, "capture", "AX text");
        let frame_id = seed_frame_with_accessibility(
            &conn,
            5_000,
            ocr_event_id,
            Some(accessibility_event_id),
            b"jpeg",
        );

        assert!(delete_by_id(&mut conn, frame_id).unwrap());
        let ax_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [accessibility_event_id],
                |row| row.get(0),
            )
            .unwrap();
        let ocr_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [ocr_event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ax_exists, "normal AX memory has independent retention controls");
        assert!(!ocr_exists, "frame-owned OCR event is removed with explicit deletion");
    }

    #[test]
    fn list_in_range_respects_bounds() {
        let conn = crate::open_in_memory().unwrap();
        let e1 = seed_event(&conn, 1_000, "a");
        let e2 = seed_event(&conn, 5_000, "b");
        seed_frame(&conn, 1_000, e1, b"a");
        seed_frame(&conn, 5_000, e2, b"b");
        let listed = list_in_range(&conn, 2_000, 6_000, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].event_id, Some(e2));
    }

    #[test]
    fn bytes_in_range_uses_only_recent_encrypted_frames() {
        let conn = crate::open_in_memory().unwrap();
        let e1 = seed_event(&conn, 1_000, "a");
        let e2 = seed_event(&conn, 5_000, "b");
        seed_frame(&conn, 1_000, e1, b"old");
        seed_frame(&conn, 5_000, e2, b"recent");
        assert_eq!(bytes_in_range(&conn, 2_000, 6_000).unwrap(), 6);
    }

    #[test]
    fn delete_removes_orphan_event_in_same_transaction() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = seed_event(&conn, 1_000, "private screen text");
        let first = seed_frame(&conn, 1_000, event_id, b"a");
        let second = seed_frame(&conn, 1_001, event_id, b"b");

        assert!(delete_by_id(&mut conn, first).unwrap());
        let event_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(event_exists);

        assert!(delete_by_id(&mut conn, second).unwrap());
        let event_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!event_exists);
    }

    #[test]
    fn delete_survives_extraction_having_cited_the_ocr_event() {
        // Extraction over screen OCR links state_provenance rows (no ON DELETE) to the OCR
        // event. A raw event delete would FK-fail and the user could not delete the frame at
        // all, so the canonical helper clears the citations first.
        use crate::state::{CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = seed_event(&conn, 1_000, "I'll send the deck tomorrow");
        let frame_id = seed_frame(&conn, 1_000, event_id, b"jpeg");
        crate::state::insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the deck",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.4,
                now: 1_100,
            },
            &[Provenance::new(event_id)],
        )
        .unwrap();

        assert!(
            delete_by_id(&mut conn, frame_id).expect("a cited OCR event must not block deletion")
        );
        let event_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!event_exists, "the frame-owned OCR event is gone");
        let cites: i64 = conn
            .query_row(
                "SELECT count(*) FROM state_provenance WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cites, 0, "no dangling provenance survives");
    }

    #[test]
    fn hidden_self_cleanup_survives_extraction_having_cited_the_ocr_event() {
        use crate::state::{CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = seed_event(&conn, 1_000, "I'll send the deck tomorrow");
        let frame_id = seed_frame(&conn, 1_000, event_id, b"jpeg");
        crate::state::insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the deck",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.4,
                now: 1_100,
            },
            &[Provenance::new(event_id)],
        )
        .unwrap();

        conn.execute("UPDATE screen_frames SET app_bundle_id = 'shogun' WHERE id = ?1", [frame_id]).unwrap();
        assert_eq!(delete_hidden_self_frames(&mut conn).unwrap(), vec![Some(event_id)]);
        let event_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_log WHERE id = ?1)",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!event_exists, "the frame-owned OCR event is gone");
        let cites: i64 = conn
            .query_row(
                "SELECT count(*) FROM state_provenance WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cites, 0, "no dangling provenance survives");
    }

    #[test]
    fn thin_ocr_is_measured_in_chars_not_bytes() {
        // ~50 Japanese chars is ~150 UTF-8 bytes: under the 100-CHAR threshold, over the byte
        // one. Both API faces call this fn, so search and get_frame cannot disagree.
        let jp = "\u{753b}\u{9762}".repeat(25);
        assert_eq!(jp.chars().count(), 50);
        assert!(jp.len() > THIN_OCR_CHARS);
        assert!(needs_rescan(&jp), "a thin CJK frame must still be re-scanned");
        assert!(needs_rescan("  short  "));
        assert!(!needs_rescan(&"a".repeat(THIN_OCR_CHARS)));
    }
}
