//! State-tables repository (§6.4, FR-ST-01/02).
//!
//! Every state row is inserted together with at least one provenance event, atomically. The
//! guard [`insert_with_provenance`] rejects an empty provenance list (FR-ST-02: "no code path
//! inserts a state row with empty provenance") — this is the repository-layer half of that
//! invariant; the schema keeps provenance in a separate `state_provenance` table so it can be
//! many-to-many. Typed inserts (e.g. [`insert_person`]) are thin wrappers over the guard.

use rusqlite::{params, Connection, OptionalExtension};

use crate::actor::{current_read, current_write, ReadScope, WriteContext};
use crate::MemoryError;

/// The four state tables (FR-ST-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTable {
    People,
    Projects,
    Commitments,
    OpenLoops,
}

impl StateTable {
    pub fn as_str(self) -> &'static str {
        match self {
            StateTable::People => "people",
            StateTable::Projects => "projects",
            StateTable::Commitments => "commitments",
            StateTable::OpenLoops => "open_loops",
        }
    }

    /// Parse a wire / table name (`open-loops` is accepted for the CLI spelling).
    pub fn parse(name: &str) -> Option<StateTable> {
        Some(match name {
            "people" => StateTable::People,
            "projects" => StateTable::Projects,
            "commitments" => StateTable::Commitments,
            "open_loops" | "open-loops" => StateTable::OpenLoops,
            _ => return None,
        })
    }
}

/// Every state table, for exhaustive iteration.
pub const ALL_STATE_TABLES: [StateTable; 4] =
    [StateTable::People, StateTable::Projects, StateTable::Commitments, StateTable::OpenLoops];

/// Count state rows inserted or updated at or after `ts`, summed across all four state tables — the
/// number of state records a Dream Cycle changed during a run (FR-DC-06), given the run's start
/// time. `updated_at` is set on insert and on every change to a row's content (recompute,
/// consolidation); confidence ageing (`recompute::decay_confidence`) leaves it alone.
pub fn count_changed_since(conn: &Connection, ts: i64) -> Result<i64, rusqlite::Error> {
    let mut total = 0i64;
    for table in ALL_STATE_TABLES {
        // The table name is from a fixed enum (never user input) — safe to inline.
        let sql = format!("SELECT count(*) FROM {} WHERE updated_at >= ?1", table.as_str());
        total += conn.query_row(&sql, params![ts], |r| r.get::<_, i64>(0))?;
    }
    total += conn.query_row("SELECT count(*) FROM knowledge_entities WHERE updated_at >= ?1", [ts], |r| r.get::<_,i64>(0))?;
    Ok(total)
}

/// One provenance link: the event that evidences a state row, with a weight (FR-ST-02).
#[derive(Debug, Clone, Copy)]
pub struct Provenance {
    pub event_id: i64,
    pub weight: f64,
}

impl Provenance {
    pub fn new(event_id: i64) -> Self {
        Self { event_id, weight: 1.0 }
    }
}

/// Insert a state row and its provenance atomically. `insert_row` performs the table-specific
/// INSERT inside the transaction and returns the new row id; this guard then writes one
/// `state_provenance` row per link. An empty `provenance` is rejected before any write
/// (FR-ST-02), so a provenance-less state row can never be committed.
pub fn insert_with_provenance<F>(
    conn: &mut Connection,
    table: StateTable,
    provenance: &[Provenance],
    insert_row: F,
) -> Result<i64, MemoryError>
where
    F: FnOnce(&Connection) -> Result<i64, rusqlite::Error>,
{
    insert_with_quote(conn, table, provenance, None, &current_write(), insert_row)
}

/// [`insert_with_provenance`] as `ctx` (E2-T4): the row's `actor_last` and `namespace` (V93)
/// record the writer and where it lands. [`insert_with_provenance`] itself writes as the call's
/// current context ([`current_write`]: the owner unless a face set one).
pub fn insert_with_provenance_as<F>(
    conn: &mut Connection,
    table: StateTable,
    provenance: &[Provenance],
    ctx: &WriteContext,
    insert_row: F,
) -> Result<i64, MemoryError>
where
    F: FnOnce(&Connection) -> Result<i64, rusqlite::Error>,
{
    insert_with_quote(conn, table, provenance, None, ctx, insert_row)
}

/// Stamp the writer and namespace on a freshly inserted state row (V93). Every INSERT into a
/// state table funnels through here or writes the two columns itself.
pub(crate) fn stamp_writer(
    conn: &Connection,
    table: StateTable,
    id: i64,
    ctx: &WriteContext,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        &format!("UPDATE {} SET actor_last = ?2, namespace = ?3 WHERE id = ?1", table.as_str()),
        params![id, ctx.actor_sql(), ctx.namespace_sql()],
    )?;
    Ok(())
}

/// Insert a validated quote with its existing provenance links in the same savepoint.
pub(crate) fn insert_with_quote<F>(
    conn: &mut Connection,
    table: StateTable,
    provenance: &[Provenance],
    quote: Option<&str>,
    ctx: &WriteContext,
    insert_row: F,
) -> Result<i64, MemoryError>
where
    F: FnOnce(&Connection) -> Result<i64, rusqlite::Error>,
{
    if provenance.is_empty() {
        return Err(MemoryError::EmptyProvenance);
    }
    let tx = conn.savepoint()?;
    let id = insert_row(&tx)?;
    stamp_writer(&tx, table, id, ctx)?;
    for p in provenance {
        tx.execute(
            "INSERT INTO state_provenance (state_table, state_id, event_id, weight)
             VALUES (?1, ?2, ?3, ?4)",
            params![table.as_str(), id, p.event_id, p.weight],
        )?;
        if let Some(quote) = quote {
            retain_quote(&tx, table, id, p.event_id, quote)?;
        }
    }
    tx.commit()?;
    Ok(id)
}

/// A new person (§6.4.2). JSON fields are pre-serialized strings (the caller owns the shape).
#[derive(Debug, Clone, Default)]
pub struct NewPerson<'a> {
    pub display_name: &'a str,
    pub aliases_json: Option<&'a str>,
    pub emails_json: Option<&'a str>,
    pub handles_json: Option<&'a str>,
    pub relationship_summary: Option<&'a str>,
    pub confidence: f64,
    pub now: i64,
}

/// Insert a person with provenance (FR-ST-02). `confidence` must be in 0.0..=1.0 (the schema
/// CHECK enforces it; an out-of-range value returns an error and rolls back).
pub fn insert_person(
    conn: &mut Connection,
    person: &NewPerson<'_>,
    provenance: &[Provenance],
) -> Result<i64, MemoryError> {
    let id = insert_with_provenance(conn, StateTable::People, provenance, |tx| {
        tx.execute(
            "INSERT INTO people
               (display_name, aliases, emails, handles, relationship_summary,
                confidence, base_confidence, created_at, updated_at, last_evidence_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?7, ?7)",
            params![
                person.display_name,
                person.aliases_json,
                person.emails_json,
                person.handles_json,
                person.relationship_summary,
                person.confidence,
                person.now,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Person(id),
        person.now,
    );
    Ok(id)
}

/// Project status (§6.4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    Active,
    Waiting,
    Paused,
    Done,
}

impl ProjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStatus::Active => "active",
            ProjectStatus::Waiting => "waiting",
            ProjectStatus::Paused => "paused",
            ProjectStatus::Done => "done",
        }
    }
}

/// A new project (§6.4.3).
#[derive(Debug, Clone)]
pub struct NewProject<'a> {
    pub name: &'a str,
    pub status: ProjectStatus,
    pub summary: Option<&'a str>,
    pub participants_json: Option<&'a str>,
    pub sources_json: Option<&'a str>,
    pub confidence: f64,
    pub now: i64,
}

/// Insert a project with provenance (FR-ST-02).
pub fn insert_project(
    conn: &mut Connection,
    project: &NewProject<'_>,
    provenance: &[Provenance],
) -> Result<i64, MemoryError> {
    let id = insert_with_provenance(conn, StateTable::Projects, provenance, |tx| {
        tx.execute(
            "INSERT INTO projects
               (name, status, summary, participants, sources,
                confidence, base_confidence, created_at, updated_at, last_evidence_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?7, ?7)",
            params![
                project.name,
                project.status.as_str(),
                project.summary,
                project.participants_json,
                project.sources_json,
                project.confidence,
                project.now,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Project(id),
        project.now,
    );
    Ok(id)
}

/// Commitment direction and status (§6.4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentDirection {
    Mine,
    Theirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentStatus {
    Open,
    Done,
    Overdue,
    Cancelled,
}

impl CommitmentDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitmentDirection::Mine => "mine",
            CommitmentDirection::Theirs => "theirs",
        }
    }
}

impl CommitmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitmentStatus::Open => "open",
            CommitmentStatus::Done => "done",
            CommitmentStatus::Overdue => "overdue",
            CommitmentStatus::Cancelled => "cancelled",
        }
    }
}

/// A new commitment (§6.4.4). Only for explicit-promise evidence (FR-ST-11).
#[derive(Debug, Clone)]
pub struct NewCommitment<'a> {
    pub direction: CommitmentDirection,
    pub counterparty_id: Option<i64>,
    pub description: &'a str,
    pub due_at: Option<i64>,
    pub status: CommitmentStatus,
    pub project_id: Option<i64>,
    pub confidence: f64,
    pub now: i64,
}

/// Insert a commitment with provenance (FR-ST-02).
pub fn insert_commitment(
    conn: &mut Connection,
    c: &NewCommitment<'_>,
    provenance: &[Provenance],
) -> Result<i64, MemoryError> {
    insert_commitment_with_quote(conn, c, provenance, None)
}

pub(crate) fn insert_commitment_with_quote(
    conn: &mut Connection,
    c: &NewCommitment<'_>,
    provenance: &[Provenance],
    quote: Option<&str>,
) -> Result<i64, MemoryError> {
    let id = insert_with_quote(conn, StateTable::Commitments, provenance, quote, &current_write(), |tx| {
        tx.execute(
            "INSERT INTO commitments
               (direction, counterparty_id, description, due_at, status, project_id,
                confidence, base_confidence, created_at, updated_at, last_evidence_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8, ?8)",
            params![
                c.direction.as_str(),
                c.counterparty_id,
                c.description,
                c.due_at,
                c.status.as_str(),
                c.project_id,
                c.confidence,
                c.now,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Commitment(id),
        c.now,
    );
    Ok(id)
}

/// Open-loop kind and status (§6.4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenLoopKind {
    ReplyNeeded,
    WaitingOnThem,
    ReviewPending,
    DecisionPending,
    FollowUp,
    Other,
}

impl OpenLoopKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpenLoopKind::ReplyNeeded => "reply_needed",
            OpenLoopKind::WaitingOnThem => "waiting_on_them",
            OpenLoopKind::ReviewPending => "review_pending",
            OpenLoopKind::DecisionPending => "decision_pending",
            OpenLoopKind::FollowUp => "follow_up",
            OpenLoopKind::Other => "other",
        }
    }
}

/// A new open loop (§6.4.5). `open` status; close is a later state change with its own
/// provenance (FR-ST-13).
#[derive(Debug, Clone)]
pub struct NewOpenLoop<'a> {
    pub kind: OpenLoopKind,
    pub description: &'a str,
    pub counterparty_id: Option<i64>,
    pub project_id: Option<i64>,
    pub opened_at: i64,
    pub confidence: f64,
    pub now: i64,
}

/// Insert an open loop with provenance (FR-ST-02), status `open`.
pub fn insert_open_loop(
    conn: &mut Connection,
    l: &NewOpenLoop<'_>,
    provenance: &[Provenance],
) -> Result<i64, MemoryError> {
    insert_open_loop_with_quote(conn, l, provenance, None)
}

pub(crate) fn insert_open_loop_with_quote(
    conn: &mut Connection,
    l: &NewOpenLoop<'_>,
    provenance: &[Provenance],
    quote: Option<&str>,
) -> Result<i64, MemoryError> {
    let id = insert_with_quote(conn, StateTable::OpenLoops, provenance, quote, &current_write(), |tx| {
        tx.execute(
            "INSERT INTO open_loops
               (kind, description, counterparty_id, project_id, opened_at, staleness_days,
                status, confidence, base_confidence, created_at, updated_at, last_evidence_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 'open', ?6, ?6, ?7, ?7, ?7)",
            params![
                l.kind.as_str(),
                l.description,
                l.counterparty_id,
                l.project_id,
                l.opened_at,
                l.confidence,
                l.now,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    })?;
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::OpenLoop(id),
        l.now,
    );
    Ok(id)
}

// state mutations (user actions)

/// Set a commitment's status (e.g. `Done` when the user resolves it from the panel). Bumps
/// `updated_at`. Returns the number of rows changed (0 if the id is unknown).
pub fn set_commitment_status(
    conn: &Connection,
    id: i64,
    status: CommitmentStatus,
    now: i64,
) -> Result<usize, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE commitments SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status.as_str(), now, id],
    )?;
    if changed != 0 {
        let _ = crate::search_documents::project_source_after_write(
            conn,
            &crate::search_documents::SourceRef::Commitment(id),
            now,
        );
    }
    Ok(changed)
}

/// Close an open loop (the user resolved it). Bumps `updated_at`. Returns rows changed.
pub fn close_open_loop(conn: &Connection, id: i64, now: i64) -> Result<usize, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE open_loops SET status = 'closed', updated_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    if changed != 0 {
        let _ = crate::search_documents::project_source_after_write(
            conn,
            &crate::search_documents::SourceRef::OpenLoop(id),
            now,
        );
    }
    Ok(changed)
}

/// Delete ALL extracted state — commitments, open loops, and their provenance links — leaving the
/// event log, people, and projects intact. The "clear extracted state" reset when the low-
/// confidence local-rule extraction has accumulated noise. Runs in one transaction.
pub fn clear_state(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM state_provenance WHERE state_table IN ('commitments', 'open_loops')",
        [],
    )?;
    tx.execute("DELETE FROM state_disputes WHERE state_table = 'commitments'", [])?;
    tx.execute("DELETE FROM commitments", [])?;
    tx.execute("DELETE FROM open_loops", [])?;
    // The review signals they are consolidated from (V83) go with them.
    tx.execute("DELETE FROM loop_signal_reads", [])?;
    tx.execute("DELETE FROM loop_signals", [])?;
    // And the outcomes read off later messages (V104).
    tx.execute("DELETE FROM loop_updates", [])?;
    tx.commit()
}

// reads (Fusion / Brief supply)

/// A commitment read back for Fusion / Morning Brief. `first_event_id` is the earliest provenance
/// event (the source link, FR-ST-02) — `None` only if somehow unlinked.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitmentRow {
    pub id: i64,
    pub description: String,
    pub due_at: Option<i64>,
    pub status: String,
    pub confidence: f64,
    pub first_event_id: Option<i64>,
    /// Snoozed until this Unix-ms instant (V80); `None` = not snoozed.
    pub deferred_until: Option<i64>,
    /// Last change (Unix ms): when a done row was resolved.
    pub updated_at: i64,
}

impl CommitmentRow {
    /// Snoozed at `now`: the user hid it until a later time. Surfaces that list open work skip it.
    pub fn is_deferred(&self, now: i64) -> bool { self.deferred_until.is_some_and(|t| t > now) }

    /// Unresolved and in the action band (confidence ≥ 0.5): what action surfaces may show. This
    /// is a confidence test, not a review flag (issue #926). Locally extracted commitments only
    /// cross 0.5 after the Batch review or a user edit (owner decision 2026-10-02).
    pub fn is_actionable(&self) -> bool { self.confidence >= 0.5 && self.status != "done" && self.status != "cancelled" }
}

/// List commitments with their source event: same-day items first, then the most recently seen
/// (V83 display order). Rows the horizon timer aged or a merge folded away are left out; `get`
/// still reads them.
///
/// A commitment in an open contradiction (issue #253) is read back with its confidence capped
/// under the fact band ([`crate::disputes::DISPUTED_CONFIDENCE_CAP`]), so Fusion passes it weakly.
pub fn list_commitments(conn: &Connection) -> Result<Vec<CommitmentRow>, rusqlite::Error> {
    list_commitments_scoped(conn, current_read())
}

/// [`list_commitments`] within `scope` (E2-T5): world rows only unless `include_scratch`.
/// [`list_commitments`] itself reads the call's current scope ([`current_read`]).
pub fn list_commitments_scoped(conn: &Connection, scope: ReadScope) -> Result<Vec<CommitmentRow>, rusqlite::Error> {
    commitments_where(conn, LISTED, 1, scope)
}

/// Rows the lists show: not aged by the horizon timer, not folded into another row (V83).
const LISTED: &str = "?1 = 1 AND aged_at IS NULL AND merged_into IS NULL";
/// Same-day items first, then the most recently seen.
const DISPLAY_ORDER: &str = "(horizon IS 'same_day') DESC, last_evidence_at DESC, id DESC";

fn commitments_where(conn: &Connection, predicate: &str, value: i64, scope: ReadScope) -> Result<Vec<CommitmentRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, description, due_at, status, {},
                NULL, deferred_until, updated_at
         FROM commitments WHERE ({predicate}) AND {}
         ORDER BY {DISPLAY_ORDER}",
        crate::disputes::COMMITMENT_CONFIDENCE_SQL,
        scope.predicate("")
    ))?;
    let rows = stmt.query_map([value], |r| {
        Ok(CommitmentRow {
            id: r.get(0)?,
            description: r.get(1)?,
            due_at: r.get(2)?,
            status: r.get(3)?,
            confidence: r.get(4)?,
            first_event_id: r.get(5)?,
            deferred_until: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    let mut visible = Vec::new();
    for row in rows {
        let mut row = row?;
        if let Some(links) = supported_provenance(conn, StateTable::Commitments, row.id, 1)? {
            row.first_event_id = links.first().map(|(id, _)| *id);
            visible.push(row);
        }
    }
    Ok(visible)
}

// Evening Wrap day-window reads (§6.17, FR-EB-01)
// The Wrap's sections are day-window aggregations. The window boundary (local midnight) is the
// caller's concern; these functions only take the cut-off timestamp.

/// Commitments marked done since `since` (Wrap "Today's outcome").
pub fn count_commitments_done_since(conn: &Connection, since: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM commitments WHERE status = 'done' AND updated_at >= ?1",
        [since],
        |r| r.get(0),
    )
}

/// Open loops closed since `since` (Wrap "Today's outcome").
pub fn count_open_loops_closed_since(conn: &Connection, since: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM open_loops WHERE status = 'closed' AND updated_at >= ?1",
        [since],
        |r| r.get(0),
    )
}

/// Unresolved commitments that saw activity since `since` (Wrap "Still open"). Aged and merged
/// rows are left out, as in [`list_commitments`].
pub fn list_commitments_active_since(
    conn: &Connection,
    since: i64,
) -> Result<Vec<CommitmentRow>, rusqlite::Error> {
    commitments_where(
        conn,
        "status NOT IN ('done', 'cancelled') AND updated_at >= ?1 AND aged_at IS NULL AND merged_into IS NULL",
        since,
        current_read(),
    )
}

/// An open loop read back for Fusion / Morning Brief. `kind` is the loop category
/// (`reply_needed` / `waiting_on_them` / …) that Context Fusion maps to an action.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenLoopRow {
    pub id: i64,
    pub kind: String,
    pub description: String,
    pub staleness_days: i64,
    pub status: String,
    pub confidence: f64,
    pub first_event_id: Option<i64>,
    /// The first email address stored for the loop's counterparty, when it names a person who
    /// has one. A reply can only be drafted to an address, never to the loop's description
    /// (#764). Not validated here; the reply producer checks it.
    pub counterparty_email: Option<String>,
    /// A guess from the on-device mail triage (issue #926): the loop came from a thread whose
    /// verdict is still the local first pass — not refined by the Batch review, not corrected by
    /// the user — and the user has not edited the loop. Its confidence is how likely the guess is,
    /// not whether anyone checked it, so consumers rank it a little lower and word it as a
    /// possibility. It stays visible and usable everywhere.
    pub inferred: bool,
    /// Snoozed until this Unix-ms instant (V80); `None` = not snoozed.
    pub deferred_until: Option<i64>,
    /// Last change (Unix ms): when a closed loop was resolved.
    pub updated_at: i64,
}

impl OpenLoopRow {
    /// Snoozed at `now`: the user hid it until a later time. Surfaces that list open work skip it.
    pub fn is_deferred(&self, now: i64) -> bool { self.deferred_until.is_some_and(|t| t > now) }

    /// Open and in the action band (confidence ≥ 0.5): what action surfaces may show. This is a
    /// confidence test, not a review flag (issue #926) — a mail guess passes it and is marked
    /// [`inferred`](Self::inferred) instead of being hidden.
    pub fn is_actionable(&self) -> bool { self.confidence >= 0.5 && self.status != "closed" }
}

/// Whether an open loop is an unreviewed mail guess ([`OpenLoopRow::inferred`]). A correlated
/// subquery over `mail_triage.open_loop_id`; `stage = 'local'` is the on-device first pass.
const OPEN_LOOP_INFERRED_SQL: &str = "(open_loops.user_pinned = 0 AND EXISTS (
            SELECT 1 FROM mail_triage m WHERE m.open_loop_id = open_loops.id AND m.stage = 'local'))";

/// The loop counterparty's first stored email (`people.emails` is a JSON array). A malformed
/// value reads as none rather than failing the whole list; `CASE` evaluates only the branch it
/// takes, so `json_type` never sees invalid JSON.
const COUNTERPARTY_EMAIL_SQL: &str = "(SELECT e.value FROM people p,
            json_each(CASE WHEN json_valid(p.emails)
                           THEN (CASE WHEN json_type(p.emails) = 'array' THEN p.emails ELSE '[]' END)
                           ELSE '[]' END) e
          WHERE p.id = open_loops.counterparty_id AND e.type = 'text' LIMIT 1)";

fn open_loop_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpenLoopRow> {
    Ok(OpenLoopRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        description: r.get(2)?,
        staleness_days: r.get(3)?,
        status: r.get(4)?,
        confidence: r.get(5)?,
        first_event_id: r.get(6)?,
        counterparty_email: r.get(7)?,
        inferred: r.get(8)?,
        deferred_until: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

/// List open loops with their source event, in the display order of [`list_commitments`] (Brief
/// takes the top-N). Aged and merged rows are left out.
pub fn list_open_loops(conn: &Connection) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    list_open_loops_scoped(conn, current_read())
}

/// [`list_open_loops`] within `scope` (E2-T5): world rows only unless `include_scratch`.
pub fn list_open_loops_scoped(conn: &Connection, scope: ReadScope) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    open_loops_where_scoped(conn, LISTED, 1, DISPLAY_ORDER, scope)
}

/// Open loops with activity since `since`, still open (Wrap "Still open"). Display order; aged
/// and merged rows are left out, as in [`list_open_loops`].
pub fn list_open_loops_active_since(
    conn: &Connection,
    since: i64,
) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    open_loops_where(
        conn,
        "status != 'closed' AND updated_at >= ?1 AND aged_at IS NULL AND merged_into IS NULL",
        since,
        DISPLAY_ORDER,
    )
}

/// Open loops that appeared since `since` and are still open (Wrap "Loose ends"), newest first;
/// aged and merged rows are left out.
pub fn list_open_loops_opened_since(
    conn: &Connection,
    since: i64,
) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    open_loops_where(
        conn,
        "status != 'closed' AND opened_at >= ?1 AND aged_at IS NULL AND merged_into IS NULL",
        since,
        "opened_at DESC, id DESC",
    )
}

/// Shared SELECT for the open-loop day-window reads. `where_sql` and `order_sql` are static SQL
/// over columns of this module's own table — never caller input.
fn open_loops_where(
    conn: &Connection,
    where_sql: &str,
    since: i64,
    order_sql: &str,
) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    open_loops_where_scoped(conn, where_sql, since, order_sql, current_read())
}

fn open_loops_where_scoped(
    conn: &Connection,
    where_sql: &str,
    since: i64,
    order_sql: &str,
    scope: ReadScope,
) -> Result<Vec<OpenLoopRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, kind, description, staleness_days, status, confidence,
                NULL,
                {COUNTERPARTY_EMAIL_SQL},
                {OPEN_LOOP_INFERRED_SQL},
                deferred_until, updated_at
         FROM open_loops
         WHERE ({where_sql}) AND {}
         ORDER BY {order_sql}",
        scope.predicate("")
    ))?;
    let rows = stmt.query_map([since], open_loop_row)?;
    let mut visible = Vec::new();
    for row in rows {
        let mut row = row?;
        if let Some(links) = supported_provenance(conn, StateTable::OpenLoops, row.id, 1)? {
            row.first_event_id = links.first().map(|(id, _)| *id);
            visible.push(row);
        }
    }
    Ok(visible)

}

/// A person read back for Fusion / the Memory API.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonRow {
    pub id: i64,
    pub display_name: String,
    pub confidence: f64,
    pub first_event_id: Option<i64>,
    /// `last_evidence_at` (ms) and the number of provenance links, for "last seen" glances.
    pub last_evidence_at: i64,
    pub evidence_count: i64,
}

/// List the people worth showing ([`crate::entity_pages::visible_sql`]) with their source event,
/// ranked by engagement ([`crate::entity_pages::rank_sql`]).
pub fn list_people(conn: &Connection) -> Result<Vec<PersonRow>, rusqlite::Error> {
    list_people_page(conn, i64::MAX, 0)
}

/// Ranked SQL page for the desktop viewer.
pub fn list_people_page(conn: &Connection, limit: i64, offset: i64) -> Result<Vec<PersonRow>, rusqlite::Error> {
    list_people_page_scoped(conn, limit, offset, current_read())
}

/// [`list_people_page`] within `scope` (E2-T5).
pub fn list_people_page_scoped(conn: &Connection, limit: i64, offset: i64, scope: ReadScope) -> Result<Vec<PersonRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, display_name, {},
                NULL, last_evidence_at,
                (SELECT COUNT(*) FROM state_provenance WHERE state_table='people' AND state_id=people.id)
         FROM people WHERE ({}) AND {} ORDER BY {} LIMIT ?1 OFFSET ?2",
        crate::entity_pages::listed_confidence_sql(StateTable::People, "people"),
        crate::entity_pages::visible_sql(StateTable::People, "people"),
        scope.predicate("people."),
        crate::entity_pages::rank_sql(StateTable::People, "people"),
    ))?;
    let rows = stmt.query_map(params![limit, offset], |r| {
        Ok(PersonRow { id: r.get(0)?, display_name: r.get(1)?, confidence: r.get(2)?, first_event_id: r.get(3)?, last_evidence_at: r.get(4)?, evidence_count: r.get(5)? })
    })?;
    let mut visible = Vec::new();
    for row in rows {
        let mut row = row?;
        if let Some(links) = supported_provenance(conn, StateTable::People, row.id, 1)? {
            row.first_event_id = links.first().map(|(id, _)| *id);
            visible.push(row);
        }
    }
    Ok(visible)
}

/// Fetch one person by id.
pub fn get_person(conn: &Connection, id: i64) -> Result<Option<PersonRow>, rusqlite::Error> {
    get_person_scoped(conn, id, current_read())
}

/// [`get_person`] within `scope` (E2-T5): a scratch row reads as absent under the world scope.
pub fn get_person_scoped(conn: &Connection, id: i64, scope: ReadScope) -> Result<Option<PersonRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, display_name, {},
                NULL, last_evidence_at,
                (SELECT COUNT(*) FROM state_provenance WHERE state_table='people' AND state_id=people.id)
         FROM people WHERE id = ?1 AND {}",
        crate::entity_pages::listed_confidence_sql(StateTable::People, "people"),
        scope.predicate("people."),
    ))?;
    let mut rows = stmt.query_map([id], |r| {
        Ok(PersonRow { id: r.get(0)?, display_name: r.get(1)?, confidence: r.get(2)?, first_event_id: r.get(3)?, last_evidence_at: r.get(4)?, evidence_count: r.get(5)? })
    })?;
    let Some(mut row) = rows.next().transpose()? else {
        return Ok(None);
    };
    let Some(links) = supported_provenance(conn, StateTable::People, id, 1)? else {
        return Ok(None);
    };
    row.first_event_id = links.first().map(|(id, _)| *id);
    Ok(Some(row))
}

/// Where a person's or project's evidence comes from, for the "seen in …" label (issue #1362):
/// one row per `event_log.source`, with captures (`capture`, `screen_ocr`) split by app so the
/// label can name the app. Most-cited first. `count` is provenance links, the same count the
/// row's `evidence_count` sums.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EvidenceSource {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_bundle_id: Option<String>,
    pub count: i64,
}

/// The sources of a person's or project's provenance, most-cited first and at most `limit` of
/// them. Content is never read: only the event's source, app and the link count leave the store.
pub fn evidence_sources(conn: &Connection, table: StateTable, id: i64, limit: usize) -> Result<Vec<EvidenceSource>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.source,
                CASE WHEN e.source IN ('capture', 'screen_ocr') THEN e.app_bundle_id END,
                COUNT(*) AS n
           FROM state_provenance p JOIN event_log e ON e.id = p.event_id
          WHERE p.state_table = ?1 AND p.state_id = ?2
          GROUP BY e.source, CASE WHEN e.source IN ('capture', 'screen_ocr') THEN e.app_bundle_id END
          ORDER BY n DESC, e.source, 2
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![table.as_str(), id, i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
        Ok(EvidenceSource { source: r.get(0)?, app_bundle_id: r.get(1)?, count: r.get(2)? })
    })?;
    rows.collect()
}

/// A project read back for Fusion / the Memory API.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub confidence: f64,
    pub first_event_id: Option<i64>,
    /// `last_evidence_at` (ms) and the number of provenance links, for "last seen" glances.
    pub last_evidence_at: i64,
    pub evidence_count: i64,
}

/// List the projects worth showing with their source event, ranked like [`list_people`].
pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectRow>, rusqlite::Error> {
    list_projects_page(conn, i64::MAX, 0)
}

/// Ranked SQL page for the desktop viewer.
pub fn list_projects_page(conn: &Connection, limit: i64, offset: i64) -> Result<Vec<ProjectRow>, rusqlite::Error> {
    list_projects_page_scoped(conn, limit, offset, current_read())
}

/// [`list_projects_page`] within `scope` (E2-T5).
pub fn list_projects_page_scoped(conn: &Connection, limit: i64, offset: i64, scope: ReadScope) -> Result<Vec<ProjectRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, name, status, {},
                NULL, last_evidence_at,
                (SELECT COUNT(*) FROM state_provenance WHERE state_table='projects' AND state_id=projects.id)
         FROM projects WHERE ({}) AND {} ORDER BY {} LIMIT ?1 OFFSET ?2",
        crate::entity_pages::listed_confidence_sql(StateTable::Projects, "projects"),
        crate::entity_pages::visible_sql(StateTable::Projects, "projects"),
        scope.predicate("projects."),
        crate::entity_pages::rank_sql(StateTable::Projects, "projects"),
    ))?;
    let rows = stmt.query_map(params![limit, offset], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            name: r.get(1)?,
            status: r.get(2)?,
            confidence: r.get(3)?,
            first_event_id: r.get(4)?,
            last_evidence_at: r.get(5)?,
            evidence_count: r.get(6)?,
        })
    })?;
    let mut visible = Vec::new();
    for row in rows {
        let mut row = row?;
        if let Some(links) = supported_provenance(conn, StateTable::Projects, row.id, 1)? {
            row.first_event_id = links.first().map(|(id, _)| *id);
            visible.push(row);
        }
    }
    Ok(visible)
}

/// Fetch one project by id.
pub fn get_project(conn: &Connection, id: i64) -> Result<Option<ProjectRow>, rusqlite::Error> {
    get_project_scoped(conn, id, current_read())
}

/// [`get_project`] within `scope` (E2-T5): a scratch row reads as absent under the world scope.
pub fn get_project_scoped(conn: &Connection, id: i64, scope: ReadScope) -> Result<Option<ProjectRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, name, status, {},
                NULL, last_evidence_at,
                (SELECT COUNT(*) FROM state_provenance WHERE state_table='projects' AND state_id=projects.id)
         FROM projects WHERE id = ?1 AND {}",
        crate::entity_pages::listed_confidence_sql(StateTable::Projects, "projects"),
        scope.predicate("projects."),
    ))?;
    let mut rows = stmt.query_map([id], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            name: r.get(1)?,
            status: r.get(2)?,
            confidence: r.get(3)?,
            first_event_id: r.get(4)?,
            last_evidence_at: r.get(5)?,
            evidence_count: r.get(6)?,
        })
    })?;
    let Some(mut row) = rows.next().transpose()? else {
        return Ok(None);
    };
    let Some(links) = supported_provenance(conn, StateTable::Projects, id, 1)? else {
        return Ok(None);
    };
    row.first_event_id = links.first().map(|(id, _)| *id);
    Ok(Some(row))
}

/// Fetch one commitment by id.
pub fn get_commitment(conn: &Connection, id: i64) -> Result<Option<CommitmentRow>, rusqlite::Error> {
    get_commitment_scoped(conn, id, current_read())
}

/// [`get_commitment`] within `scope` (E2-T5): a scratch row reads as absent under the world scope.
pub fn get_commitment_scoped(conn: &Connection, id: i64, scope: ReadScope) -> Result<Option<CommitmentRow>, rusqlite::Error> {
    Ok(commitments_where(conn, "id = ?1", id, scope)?.into_iter().next())
}

/// Fetch one open loop by id.
pub fn get_open_loop(conn: &Connection, id: i64) -> Result<Option<OpenLoopRow>, rusqlite::Error> {
    get_open_loop_scoped(conn, id, current_read())
}

/// [`get_open_loop`] within `scope` (E2-T5): a scratch row reads as absent under the world scope.
pub fn get_open_loop_scoped(conn: &Connection, id: i64, scope: ReadScope) -> Result<Option<OpenLoopRow>, rusqlite::Error> {
    Ok(open_loops_where_scoped(conn, "id = ?1", id, DISPLAY_ORDER, scope)?.into_iter().next())
}

/// Match contiguous source text using the same AX whitespace rule at ingestion and read time.
/// Recall adapters share this check so they cannot return a stale or unsupported quote.
pub fn source_contains_quote(source: &str, quote: &str) -> bool {
    source_quote_start(source, quote).is_some()
}

fn source_quote_start(source: &str, quote: &str) -> Option<usize> {
    let mut words = quote.split_whitespace();
    let first = words.next()?;
    if let Some(offset) = source.find(quote) { return Some(offset); }
    // Search the first literal span, then consume whitespace in place. No full-source copy.
    let mut remaining = source;
    while let Some(offset) = remaining.find(first) {
        let candidate = &remaining[offset..];
        let mut rest = &candidate[first.len()..];
        let matched = words.clone().all(|word| {
            if !rest.chars().next().is_some_and(char::is_whitespace) { return false; }
            rest = rest.trim_start_matches(char::is_whitespace);
            if let Some(next) = rest.strip_prefix(word) { rest = next; true } else { false }
        });
        if matched { return Some(source.len() - remaining.len() + offset); }
        // Include overlapping first-word matches ("aaa b" contains "aa b").
        remaining = &candidate[first.chars().next().map_or(0, char::len_utf8)..];
    }
    None
}

// Exact source identity and explicit connected threads. Screen titles alone are never identity.
pub(crate) const RECAPTURE_CONTEXT_SQL: &str = "(previous.id=current.id
    OR (previous.source NOT IN ('capture','screen_ocr','screen_text') AND current.source NOT IN ('capture','screen_ocr','screen_text')
        AND previous.thread_key=current.thread_key)
    OR (previous.content=current.content AND previous.thread_key IS current.thread_key
        AND previous.app_bundle_id IS current.app_bundle_id
        AND previous.window_title IS current.window_title
        AND ABS(previous.ts-current.ts)<=300000))";

// A changed capture body may still show the same message. The caller must also match the quote
// and its adjacent attribution; this scope alone must never merge descriptions. No time bound:
// an unresolved item stays one memory however much later the message is read again.
//
// This is a PREFILTER. Callers must also require
// `memory_identity::same_screen_conversation(previous, current)` in Rust (same window once counters
// are removed, and no explicit thread identity that tells the two apart).
//
// The window test compares the derived `thread_key`, not the raw title: titles carry unread
// counters — `(2) Inbox` and `(5) Inbox` are one window — so a raw comparison writes the same
// memory again on every counter change (dedup-bench: 2.67 rows per memory). The key is cheap and
// keeps other windows out of the scan: it runs the title through `thread::normalise_window_title`,
// which drops a leading `(N)` counter and the trailing segment where Mail and similar apps put
// their counts. Without it, every same-app row reaches Rust, and a candidate in an unrelated
// window measured ~5x slower at 5,000 rows. Every match exact-title equality would make also has
// equal thread keys, so this never loses one. What it does not admit is a counter in the *middle*
// of a title; those still split.
pub(crate) const CAPTURE_RECAPTURE_SCOPE_SQL: &str = "(previous.source IN ('capture','screen_ocr','screen_text')
    AND current.source IN ('capture','screen_ocr','screen_text')
    AND previous.app_bundle_id=current.app_bundle_id AND previous.app_bundle_id!=''
    AND previous.window_title!='' AND current.window_title!=''
    AND previous.thread_key IS current.thread_key)";

// A PREFILTER for the same message reaching the log through two surfaces: a screen capture on one
// side, a connector item on the other. Callers must accept a pair only when
// `memory_identity::screen_shows_thread` resolves the screen window — in the connector's own app —
// to the connector's thread, `memory_identity::screen_shows_message` finds the synced body on
// screen, the quote is identical, and the row is unresolved. The connector-only arm of RECAPTURE_CONTEXT_SQL
// (same thread, same quote) is the rule being extended; the screen side cannot meet it directly,
// because a capture has no thread id of its own (dedup-bench: slack-app-and-connector).
pub(crate) const CROSS_SURFACE_SCOPE_SQL: &str = "((previous.source IN ('capture','screen_ocr','screen_text'))
    != (current.source IN ('capture','screen_ocr','screen_text')))";

// A resolved item absorbs a recapture only while its source was still on screen. An unchanged
// window is touched, not re-linked, so the anchor is the last sighting and not the first.
pub(crate) const CAPTURE_STILL_VISIBLE_SQL: &str =
    "(current.ts BETWEEN previous.ts-300000 AND MAX(previous.ts,previous.last_seen_at)+300000)";

/// Compare the attribution before one unambiguous occurrence of a quote. A capture that shows no
/// attribution (an AX delta, or the quote at the top of the body) is compatible with any.
/// ponytail: screen text has no universal message ID; ambiguous/moved attribution stays separate.
pub(crate) fn same_capture_quote(previous: &str, current: &str, quote: &str) -> bool {
    // Clock, relative-time and relative-day tokens change between captures of one message; the
    // sender does not. "Sam Reed, Today 4:02 PM" and "Sam Reed, Yesterday 4:02 PM" are one header.
    fn stable(token: &&str) -> bool {
        crate::memory_identity::attribution_token_is_stable(token)
    }
    // Every place the quote occurs, overlapping matches included.
    fn occurrences(source: &str, quote: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = source.get(from..).and_then(|rest| source_quote_start(rest, quote)) {
            let at = from + rel;
            out.push(at);
            let Some(advance) = source[at..].chars().next().map(char::len_utf8) else { break };
            from = at + advance;
        }
        out
    }
    fn attribution(source: &str, quote: &str) -> Option<String> {
        let all = occurrences(source, quote);
        // A `>`-quoted copy is a reference to the message, not a second sending of it. Counting it
        // would make the original ambiguous the moment a reply quoting it comes into view, and the
        // original would be written again (dedup-bench: quoted-reply-on-screen).
        let own: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&at| !crate::memory_identity::on_quoted_line(source, at))
            .collect();
        let start = match own.as_slice() {
            [one] => *one,
            // Only quoted copies on screen: compatible with the original's sender, whoever it was.
            [] if !all.is_empty() => return Some(String::new()),
            // Absent, or genuinely said twice in this body: ambiguous.
            _ => return None,
        };
        // The quote's own line prefix, else the nearest of the two lines above that names someone.
        Some(source[..start].lines().rev().take(3)
            .map(|line| line.split_whitespace().filter(stable).collect::<Vec<_>>().join(" "))
            .find(|line| !line.is_empty()).unwrap_or_default())
    }
    match (attribution(previous, quote), attribution(current, quote)) {
        (Some(a), Some(b)) => a.is_empty() || b.is_empty() || a == b,
        _ => false,
    }
}

/// Identity ignores layout whitespace, terminal punctuation (ASCII and full-width), reply-quote
/// markers and trailing mentions, while retained evidence stays verbatim. `> I'll send the deck` in
/// a reply is the promise it quotes, and so is `明日修正します @toru` the sentence `明日修正します。`
/// — one surface renders the mention inline, another drops it or puts it on its own line.
pub(crate) fn quote_identity(quote: &str) -> String {
    quote_identity_parts(quote).0
}

/// Sentence-final punctuation identity ignores.
const TERMINAL_PUNCTUATION: [char; 7] = ['.', '!', '?', '。', '！', '？', '．'];

/// A chat mention token: `@toru`, `@sam.reed`, `＠佐伯`.
fn is_handle(token: &str) -> bool {
    let Some(name) = token.strip_prefix('@').or_else(|| token.strip_prefix('＠')) else {
        return false;
    };
    let len = name.chars().count();
    (1..=40).contains(&len) && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// [`quote_identity`] plus the trailing mentions it stripped, lowercased, sorted and deduplicated.
///
/// Mentions are stripped from the end only, one or more, with a `cc` that introduces them
/// (`… tomorrow cc @dana @lee`). A quote that is nothing but mentions keeps them: there would be
/// no sentence left to be the identity. The handles are returned so callers can refuse to treat
/// two quotes as one when *both* name someone and the names differ ([`handles_compatible`]).
pub(crate) fn quote_identity_parts(quote: &str) -> (String, Vec<String>) {
    let joined = crate::memory_identity::strip_quote_markers(quote)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut text = joined.as_str();
    let mut handles: Vec<String> = Vec::new();
    loop {
        let trimmed = text.trim_end_matches(TERMINAL_PUNCTUATION).trim_end();
        let Some((head, last)) = trimmed.rsplit_once(' ') else {
            text = trimmed;
            break;
        };
        let head = head.trim_end();
        if head.is_empty() {
            text = trimmed;
            break;
        }
        if is_handle(last) {
            handles.push(last.chars().skip(1).collect::<String>().to_lowercase());
            text = head;
            continue;
        }
        if !handles.is_empty() && matches!(last.to_ascii_lowercase().as_str(), "cc" | "cc:") {
            text = head;
            continue;
        }
        text = trimmed;
        break;
    }
    handles.sort();
    handles.dedup();
    (text.to_owned(), handles)
}

/// Two quotes' trailing mentions agree: one side names nobody, or both name the same people.
/// `I'll review it @dana` and `I'll review it @lee` are two promises to two people.
pub(crate) fn handles_compatible(a: &[String], b: &[String]) -> bool {
    a.is_empty() || b.is_empty() || a == b
}

/// The same quoted sentence: equal [`quote_identity`] and [`handles_compatible`] mentions.
pub(crate) fn same_quote_identity(a: &str, b: &str) -> bool {
    let (ka, ha) = quote_identity_parts(a);
    let (kb, hb) = quote_identity_parts(b);
    ka == kb && handles_compatible(&ha, &hb)
}

/// Retain an already validated quote on its existing link. Call inside the owner's savepoint.
/// Legacy/local text that does not match the live source stays unknown rather than inventing proof.
pub(crate) fn retain_quote(
    conn: &Connection,
    table: StateTable,
    id: i64,
    event_id: i64,
    quote: &str,
) -> Result<bool, rusqlite::Error> {
    let Some(clean) = crate::sanitize::persist_generated(quote) else {
        return Ok(false);
    };
    let quote = clean.text.as_ref();
    let source: Option<String> = conn
        .query_row(
            "SELECT content FROM event_log WHERE id=?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    if !source.is_some_and(|source| source_contains_quote(&source, quote)) {
        return Ok(false);
    }
    let changed = conn.execute(
        "UPDATE state_provenance SET quote=?4, batch_reviewed=0
         WHERE state_table=?1 AND state_id=?2 AND event_id=?3 AND quote IS NOT ?4",
        params![table.as_str(), id, event_id, quote],
    )?;
    if changed != 0 {
        // V21 requires owner freshness to advance after a provenance change, not evidence age.
        conn.execute(
            &format!(
                "UPDATE {} SET updated_at=updated_at+1 WHERE id=?1",
                table.as_str()
            ),
            [id],
        )?;
    }
    Ok(changed != 0)
}

pub(crate) fn mark_quote_reviewed(conn: &Connection, table: StateTable, id: i64, event_id: i64) -> Result<(), rusqlite::Error> {
    conn.execute("UPDATE state_provenance SET batch_reviewed=1 WHERE state_table=?1 AND state_id=?2 AND event_id=?3 AND quote IS NOT NULL", params![table.as_str(), id, event_id])?;
    Ok(())
}

/// Live support for automatic state. Wording corrections stay visible without claiming old proof.
/// Legacy null links remain unknown context; they are not upgraded to verified quotes.
pub(crate) fn supported_provenance(
    conn: &Connection,
    table: StateTable,
    id: i64,
    limit: usize,
) -> Result<Option<Vec<(i64, f64)>>, rusqlite::Error> {
    let owner: Option<(bool, bool)> = conn
        .prepare_cached(&format!("SELECT user_pinned, wording_edited FROM {} WHERE id=?1", table.as_str()))?
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((pinned, wording_edited)) = owner else { return Ok(None); };
    if pinned && wording_edited { return Ok(Some(Vec::new())); }
    let mut stmt = conn.prepare_cached(
        "SELECT p.event_id, p.weight, CASE WHEN p.quote IS NULL THEN '' ELSE e.content END, p.quote
         FROM state_provenance p JOIN event_log e ON e.id=p.event_id
         WHERE p.state_table=?1 AND p.state_id=?2 ORDER BY p.event_id",
    )?;
    let mut rows = stmt.query(params![table.as_str(), id])?;
    let mut links = Vec::new();
    while let Some(row) = rows.next()? {
        let source = row.get_ref(2)?.as_str()?;
        let quote = row.get_ref(3)?.as_str_or_null()?;
        if quote.is_none_or(|quote| source_contains_quote(source, quote))
        {
            links.push((row.get(0)?, row.get(1)?));
            if links.len() == limit { break; }
        }
    }
    Ok((pinned || !links.is_empty()).then_some(links))
}

/// The quote is source text, not proof that the user accepted the extracted claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    SourceQuote,
    LegacyUnknown,
    /// A retained source predates corrected wording and must not be presented as its proof.
    EditedContext,
}

/// One retained source excerpt linked to an extracted memory.
#[derive(Debug, serde::Serialize)]
pub struct EvidenceRow {
    pub event_id: i64,
    pub ts: i64,
    pub source: String,
    pub app_bundle_id: Option<String>,
    pub excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_kind: Option<EvidenceKind>,
}

/// Invalid non-null quotes never fall back to a raw excerpt. Null means unknown legacy context.
pub(crate) fn resolve_quote(source: &str, quote: Option<&str>) -> Option<(String, EvidenceKind)> {
    match quote {
        Some(quote) if source_contains_quote(source, quote) => {
            Some((quote.chars().take(800).collect(), EvidenceKind::SourceQuote))
        }
        Some(_) => None,
        None => Some((
            source.chars().take(800).collect(),
            EvidenceKind::LegacyUnknown,
        )),
    }
}

/// Bounded evidence from live provenance links, never a name-based search presented as evidence.
pub fn evidence(
    conn: &Connection,
    table: StateTable,
    state_id: i64,
) -> Result<Vec<EvidenceRow>, rusqlite::Error> {
    let wording_edited: bool = conn
        .query_row(
            &format!("SELECT wording_edited FROM {} WHERE id=?1", table.as_str()),
            [state_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    let mut stmt = conn.prepare(
        "SELECT e.id, e.ts, e.source, e.app_bundle_id, e.content, p.quote
         FROM state_provenance p JOIN event_log e ON e.id = p.event_id
         WHERE p.state_table = ?1 AND p.state_id = ?2
         ORDER BY e.ts DESC, e.id DESC",
    )?;
    let mut rows = stmt.query(params![table.as_str(), state_id])?;
    let mut evidence = Vec::new();
    while let Some(row) = rows.next()? {
        let source: String = row.get(4)?;
        let quote: Option<String> = row.get(5)?;
        if let Some((excerpt, kind)) = resolve_quote(&source, quote.as_deref()) {
            evidence.push(EvidenceRow {
                event_id: row.get(0)?,
                ts: row.get(1)?,
                source: row.get(2)?,
                app_bundle_id: row.get(3)?,
                excerpt,
                evidence_kind: Some(if wording_edited {
                    EvidenceKind::EditedContext
                } else {
                    kind
                }),
            });
            if evidence.len() == 50 {
                break;
            }
        }
    }
    Ok(evidence)
}

/// Another memory sharing at least one retained provenance event.
#[derive(Debug)]
pub struct RelatedState {
    pub kind: String,
    pub id: i64,
    pub text: String,
    pub confidence: f64,
    pub shared_sources: i64,
}

/// A bounded evidence neighborhood. Shared provenance is not a semantic relationship.
pub fn related_state(conn: &Connection, table: StateTable, id: i64) -> Result<Vec<RelatedState>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT n.kind, n.id, n.name, n.confidence, count(DISTINCT b.event_id) AS shared
         FROM state_provenance a
         JOIN event_log e ON e.id = a.event_id
         JOIN state_provenance b ON b.event_id = a.event_id
         JOIN (SELECT 'people' AS kind, p.id, p.display_name AS name, {} AS confidence FROM people p WHERE {}
               UNION ALL SELECT 'projects', p.id, p.name, {} FROM projects p WHERE {}) n
           ON n.kind = b.state_table AND n.id = b.state_id
         WHERE a.state_table = ?1 AND a.state_id = ?2
           AND NOT (b.state_table = ?1 AND b.state_id = ?2)
         GROUP BY n.kind, n.id, n.name, n.confidence ORDER BY shared DESC, n.kind, n.id LIMIT 12",
        crate::entity_pages::listed_confidence_sql(StateTable::People, "p"),
        crate::entity_pages::visible_sql(StateTable::People, "p"),
        crate::entity_pages::listed_confidence_sql(StateTable::Projects, "p"),
        crate::entity_pages::visible_sql(StateTable::Projects, "p"),
    ))?;
    let rows = stmt.query_map(params![table.as_str(), id], |row| Ok(RelatedState {
        kind: row.get(0)?, id: row.get(1)?, text: row.get(2)?, confidence: row.get(3)?, shared_sources: row.get(4)?,
    }))?;
    rows.collect()
}

/// Count provenance links for a state row (test / diagnostics helper).
pub fn provenance_count(conn: &Connection, table: StateTable, state_id: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM state_provenance WHERE state_table = ?1 AND state_id = ?2",
        params![table.as_str(), state_id],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod quote_identity_tests {
    use super::*;

    #[test]
    fn trailing_mentions_and_full_width_terminators_are_not_identity() {
        let plain = "アプリ全体で30以上のバグを見つけましたので、明日修正します";
        for variant in [
            "アプリ全体で30以上のバグを見つけましたので、明日修正します @kaito",
            "アプリ全体で30以上のバグを見つけましたので、明日修正します。",
            "アプリ全体で30以上のバグを見つけましたので、明日修正します。 @kaito",
            "アプリ全体で30以上のバグを見つけましたので、明日修正します @kaito @rin",
            "アプリ全体で30以上のバグを見つけましたので、明日修正します ＠佐伯",
            "> アプリ全体で30以上のバグを見つけましたので、明日修正します！",
        ] {
            assert_eq!(quote_identity(variant), plain, "{variant}");
        }
        assert_eq!(quote_identity("I'll send the deck tomorrow cc @dana @lee"), "I'll send the deck tomorrow");
        assert_eq!(quote_identity("I'll send the deck tomorrow CC: @dana."), "I'll send the deck tomorrow");
        assert_eq!(quote_identity("I'll send the deck tomorrow?"), "I'll send the deck tomorrow");
    }

    #[test]
    fn only_trailing_mentions_are_stripped() {
        // A mention inside the sentence is part of what was said.
        assert_eq!(quote_identity("@dana I'll send the deck"), "@dana I'll send the deck");
        assert_eq!(quote_identity("I'll ask @dana for the deck"), "I'll ask @dana for the deck");
        // An email address is not a mention, nor is a bare "cc" that introduces nobody.
        assert_eq!(quote_identity("I'll send it to dana@acme.example"), "I'll send it to dana@acme.example");
        assert_eq!(quote_identity("I'll add them in cc"), "I'll add them in cc");
        // Nothing but mentions: there is no sentence to be the identity, so they stay.
        assert_eq!(quote_identity("@dana"), "@dana");
        assert_eq!(quote_identity("cc @dana"), "cc");
    }

    #[test]
    fn different_people_named_at_the_end_are_different_quotes() {
        assert!(same_quote_identity("I'll review it today @dana", "I'll review it today"));
        assert!(same_quote_identity("I'll review it today @Dana", "I'll review it today @dana."));
        assert!(same_quote_identity("I'll review it today @lee @dana", "I'll review it today @dana @lee"));
        assert!(!same_quote_identity("I'll review it today @dana", "I'll review it today @lee"));
        let (_, handles) = quote_identity_parts("明日修正します @Kaito cc @rin");
        assert_eq!(handles, vec!["kaito".to_string(), "rin".to_string()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};

    #[test]
    fn evidence_follows_only_the_requested_memory_links() -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = crate::open_in_memory()?;
        let first = seed_event(&conn, "wiki-evidence-one");
        let unrelated = seed_event(&conn, "wiki-evidence-two");
        let alice = insert_person(&mut conn, &person("Alice", 0.6), &[Provenance::new(first)])?;
        insert_person(&mut conn, &person("Bob", 0.9), &[Provenance::new(unrelated)])?;
        let carol = insert_person(&mut conn, &person("Carol", 0.6), &[Provenance::new(first)])?;
        let found = evidence(&conn, StateTable::People, alice)?;
        assert_eq!(found.iter().map(|row| row.event_id).collect::<Vec<_>>(), vec![first]);
        assert!(evidence(&conn, StateTable::Projects, alice)?.is_empty());
        let neighbors = related_state(&conn, StateTable::People, alice)?;
        assert_eq!(neighbors.iter().map(|row| row.id).collect::<Vec<_>>(), vec![carol]);
        assert_eq!(neighbors[0].shared_sources, 1);
        assert_eq!(neighbors[0].confidence, crate::entity_pages::LISTED_CONFIDENCE, "a listed neighbour reads as a fact");
        // A legacy/orphaned provenance row must not create an edge without retained evidence.
        conn.execute_batch("PRAGMA foreign_keys=OFF")?;
        conn.execute("DELETE FROM event_log WHERE id = ?1", [first])?;
        assert!(related_state(&conn, StateTable::People, alice)?.is_empty());
        Ok(())
    }

    #[test]
    fn an_open_loop_carries_its_counterpartys_email_when_there_is_one() -> Result<(), Box<dyn std::error::Error>> {
        // #764: a reply can only be drafted to an address, never to the loop's description.
        let mut conn = crate::open_in_memory()?;
        let e = seed_event(&conn, "loop-counterparty");
        let with_email = |name: &'static str, emails: Option<&'static str>| NewPerson {
            display_name: name,
            aliases_json: None,
            emails_json: emails,
            handles_json: None,
            relationship_summary: None,
            confidence: 0.9,
            now: 1,
        };
        let sarah = insert_person(&mut conn, &with_email("Sarah", Some(r#"["sarah@client.example","s@home.example"]"#)), &[Provenance::new(e)])?;
        let broken = insert_person(&mut conn, &with_email("Broken", Some("not json")), &[Provenance::new(e)])?;
        let nameless = insert_person(&mut conn, &with_email("NoMail", None), &[Provenance::new(e)])?;
        for (who, description) in [(Some(sarah), "Reply to Sarah"), (Some(broken), "Reply to Broken"), (Some(nameless), "Reply to NoMail"), (None, "Reply to someone")] {
            insert_open_loop(
                &mut conn,
                &NewOpenLoop {
                    kind: OpenLoopKind::ReplyNeeded,
                    description,
                    counterparty_id: who,
                    project_id: None,
                    opened_at: 1,
                    confidence: 0.8,
                    now: 1,
                },
                &[Provenance::new(e)],
            )?;
        }
        let loops = list_open_loops(&conn)?;
        let email_of = |description: &str| {
            loops.iter().find(|l| l.description == description).and_then(|l| l.counterparty_email.clone())
        };
        assert_eq!(email_of("Reply to Sarah").as_deref(), Some("sarah@client.example"));
        assert_eq!(email_of("Reply to Broken"), None, "malformed emails do not fail the list");
        assert_eq!(email_of("Reply to NoMail"), None);
        assert_eq!(email_of("Reply to someone"), None);
        assert_eq!(loops.len(), 4);
        Ok(())
    }

    fn seed_event(conn: &Connection, hash: &str) -> i64 {
        insert_event(
            conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "evidence",
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn person(name: &str, confidence: f64) -> NewPerson<'_> {
        NewPerson { display_name: name, confidence, now: 100, ..Default::default() }
    }

    #[test]
    fn entity_pages_bound_people_and_projects_without_losing_the_next_page() {
        let conn = crate::open_in_memory().expect("test");
        for i in 0..105 {
            conn.execute("INSERT INTO people(display_name, confidence, created_at, updated_at, last_evidence_at, user_pinned, wording_edited) VALUES (?1, 1, 1, 1, 1, 1, 1)", [format!("Teammate {i}")]).expect("test");
            conn.execute("INSERT INTO projects(name, status, confidence, created_at, updated_at, last_evidence_at, user_pinned, wording_edited) VALUES (?1, 'active', 1, 1, 1, 1, 1, 1)", [format!("Project {i}")]).expect("test");
        }
        let first = list_people_page(&conn, 50, 0).expect("test");
        let next = list_people_page(&conn, 50, 50).expect("test");
        assert_eq!((first.len(), next.len()), (50, 50));
        assert!(first.iter().all(|a| next.iter().all(|b| a.id != b.id)));
        assert_eq!(list_people_page(&conn, 50, 100).expect("test").len(), 5);
        assert_eq!(list_projects_page(&conn, 50, 0).expect("test").len(), 50);
        assert_eq!(list_projects_page(&conn, 50, 100).expect("test").len(), 5);
    }

    #[test]
    fn insert_person_with_provenance_succeeds() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let id = insert_person(&mut conn, &person("Alice", 0.9), &[Provenance::new(e)]).unwrap();
        assert_eq!(provenance_count(&conn, StateTable::People, id).unwrap(), 1);
    }

    #[test]
    fn evidence_sources_group_by_source_and_capture_app_most_cited_first() {
        let mut conn = crate::open_in_memory().unwrap();
        let event = |source: &str, app: Option<&str>, hash: &str| {
            insert_event(
                &conn,
                &NewEvent {
                    ts: 1, source, kind: "text", app_bundle_id: app, window_title: None, content: "evidence",
                    content_hash: hash, dwell_ms: 0, display_id: None, window_bounds: None,
                },
            )
            .unwrap()
        };
        let mail1 = event("gmail", None, "m1");
        let mail2 = event("gmail", None, "m2");
        let safari = event("capture", Some("com.apple.Safari"), "c1");
        let slack_app = event("capture", Some("com.tinyspeck.slackmacgap"), "c2");
        // A connector event's app does not split it: Mail is Mail whatever window showed it.
        let mail3 = event("gmail", Some("com.apple.mail"), "m3");
        let links: Vec<Provenance> = [mail1, mail2, safari, slack_app, mail3].iter().map(|e| Provenance::new(*e)).collect();
        let id = insert_person(&mut conn, &person("Dana", 0.9), &links).unwrap();

        let sources = evidence_sources(&conn, StateTable::People, id, 10).unwrap();
        assert_eq!(sources[0], EvidenceSource { source: "gmail".into(), app_bundle_id: None, count: 3 });
        assert_eq!(sources.len(), 3);
        assert!(sources[1..].iter().all(|s| s.source == "capture" && s.count == 1));
        assert_eq!(
            sources[1..].iter().map(|s| s.app_bundle_id.as_deref().unwrap_or("")).collect::<Vec<_>>(),
            vec!["com.apple.Safari", "com.tinyspeck.slackmacgap"]
        );
        // The limit keeps the most-cited; a row with no links has none.
        assert_eq!(evidence_sources(&conn, StateTable::People, id, 1).unwrap().len(), 1);
        assert!(evidence_sources(&conn, StateTable::Projects, id, 10).unwrap().is_empty());
    }

    #[test]
    fn empty_provenance_is_rejected_and_nothing_written() {
        let mut conn = crate::open_in_memory().unwrap();
        let err = insert_person(&mut conn, &person("Bob", 0.9), &[]);
        assert!(matches!(err, Err(MemoryError::EmptyProvenance)));
        let count: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "no person row may exist after a rejected insert");
    }

    #[test]
    fn multiple_provenance_events_all_recorded() {
        let mut conn = crate::open_in_memory().unwrap();
        let e1 = seed_event(&conn, "h1");
        let e2 = seed_event(&conn, "h2");
        let id = insert_person(
            &mut conn,
            &person("Carol", 0.8),
            &[Provenance::new(e1), Provenance { event_id: e2, weight: 0.5 }],
        )
        .unwrap();
        assert_eq!(provenance_count(&conn, StateTable::People, id).unwrap(), 2);
    }

    #[test]
    fn list_commitments_returns_rows_with_source_event_due_ordered() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        // two commitments, one due later, one due sooner
        insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "later",
                due_at: Some(500),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "sooner",
                due_at: Some(100),
                status: CommitmentStatus::Overdue,
                project_id: None,
                confidence: 0.7,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();

        conn.execute("UPDATE commitments SET last_evidence_at = 50 WHERE description = 'later'", []).unwrap();
        let rows = list_commitments(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].description, "later", "most recently seen first");
        conn.execute("UPDATE commitments SET horizon = 'same_day' WHERE description = 'sooner'", []).unwrap();
        let rows = list_commitments(&conn).unwrap();
        assert_eq!(rows[0].description, "sooner", "same-day items lead");
        assert_eq!(rows[0].first_event_id, Some(e));
        assert_eq!(rows[0].status, "overdue");
        conn.execute("UPDATE commitments SET aged_at = 60 WHERE description = 'later'", []).unwrap();
        assert_eq!(list_commitments(&conn).unwrap().len(), 1, "aged rows leave the list");
    }

    #[test]
    fn list_open_loops_is_same_day_first_then_most_recent() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let loop_of = |desc: &'static str| NewOpenLoop {
            kind: OpenLoopKind::ReplyNeeded,
            description: desc,
            counterparty_id: None,
            project_id: None,
            opened_at: 1,
            confidence: 0.9,
            now: 1,
        };
        let fresh = insert_open_loop(&mut conn, &loop_of("fresh"), &[Provenance::new(e)]).unwrap();
        let stale = insert_open_loop(&mut conn, &loop_of("stale"), &[Provenance::new(e)]).unwrap();
        conn.execute("UPDATE open_loops SET staleness_days = 9 WHERE id = ?1", [stale]).unwrap();
        conn.execute("UPDATE open_loops SET staleness_days = 1, last_evidence_at = 9 WHERE id = ?1", [fresh]).unwrap();

        let rows = list_open_loops(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].description, "fresh");
        conn.execute("UPDATE open_loops SET horizon = 'same_day' WHERE id = ?1", [stale]).unwrap();
        let rows = list_open_loops(&conn).unwrap();
        assert_eq!(rows[0].description, "stale");
        assert_eq!(rows[0].staleness_days, 9);
        assert_eq!(rows[0].first_event_id, Some(e));
        conn.execute("UPDATE open_loops SET merged_into = ?1 WHERE id = ?2", [stale, fresh]).unwrap();
        assert_eq!(list_open_loops(&conn).unwrap().len(), 1, "merged rows leave the list");
        assert_eq!(list_open_loops_opened_since(&conn, 0).unwrap().len(), 1, "…and the Wrap's loose ends");
        assert_eq!(list_open_loops_active_since(&conn, 0).unwrap().len(), 1, "…and its still-open list");
    }

    #[test]
    fn loose_ends_read_newest_opened_first() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        for (desc, opened_at) in [("older", 10), ("newer", 20)] {
            let new = NewOpenLoop {
                kind: OpenLoopKind::ReplyNeeded,
                description: desc,
                counterparty_id: None,
                project_id: None,
                opened_at,
                confidence: 0.9,
                now: opened_at,
            };
            insert_open_loop(&mut conn, &new, &[Provenance::new(e)]).unwrap();
        }
        let rows = list_open_loops_opened_since(&conn, 0).unwrap();
        assert_eq!(rows.iter().map(|r| r.description.as_str()).collect::<Vec<_>>(), ["newer", "older"]);
    }

    #[test]
    fn bad_confidence_rolls_back_the_whole_insert() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        // confidence 2.0 violates the schema CHECK — the transaction must roll back, leaving
        // neither the person row nor any provenance.
        let res = insert_person(&mut conn, &person("Dave", 2.0), &[Provenance::new(e)]);
        assert!(res.is_err());
        let people: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        let prov: i64 = conn.query_row("SELECT count(*) FROM state_provenance", [], |r| r.get(0)).unwrap();
        assert_eq!(people, 0);
        assert_eq!(prov, 0);
    }

    #[test]
    fn provenance_fk_requires_a_real_event() {
        let mut conn = crate::open_in_memory().unwrap();
        // event_id 999 does not exist — the FK on state_provenance.event_id must reject it and
        // roll the person insert back.
        let res = insert_person(&mut conn, &person("Eve", 0.9), &[Provenance::new(999)]);
        assert!(res.is_err());
        let people: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(people, 0);
    }

    #[test]
    fn insert_project_with_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let p = NewProject {
            name: "Roadmap",
            status: ProjectStatus::Active,
            summary: Some("Q3 planning"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 100,
        };
        let id = insert_project(&mut conn, &p, &[Provenance::new(e)]).unwrap();
        assert_eq!(provenance_count(&conn, StateTable::Projects, id).unwrap(), 1);
        let status: String = conn.query_row("SELECT status FROM projects WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn insert_commitment_links_counterparty_and_project() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let person_id = insert_person(&mut conn, &person("Frank", 0.9), &[Provenance::new(e)]).unwrap();
        let e2 = seed_event(&conn, "h2");
        let proj = NewProject {
            name: "P",
            status: ProjectStatus::Active,
            summary: None,
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 100,
        };
        let project_id = insert_project(&mut conn, &proj, &[Provenance::new(e2)]).unwrap();
        let e3 = seed_event(&conn, "h3");
        let c = NewCommitment {
            direction: CommitmentDirection::Mine,
            counterparty_id: Some(person_id),
            description: "Send the report by Friday",
            due_at: Some(1_700_000_000_000),
            status: CommitmentStatus::Open,
            project_id: Some(project_id),
            confidence: 0.85,
            now: 100,
        };
        let id = insert_commitment(&mut conn, &c, &[Provenance::new(e3)]).unwrap();
        assert_eq!(provenance_count(&conn, StateTable::Commitments, id).unwrap(), 1);
    }

    #[test]
    fn commitment_with_dangling_counterparty_is_rejected() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let c = NewCommitment {
            direction: CommitmentDirection::Theirs,
            counterparty_id: Some(999), // no such person
            description: "x",
            due_at: None,
            status: CommitmentStatus::Open,
            project_id: None,
            confidence: 0.8,
            now: 100,
        };
        let res = insert_commitment(&mut conn, &c, &[Provenance::new(e)]);
        assert!(res.is_err(), "FK to a missing person must reject the commitment");
        let n: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn insert_open_loop_defaults_to_open() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn, "h1");
        let l = NewOpenLoop {
            kind: OpenLoopKind::ReplyNeeded,
            description: "Reply to Alice",
            counterparty_id: None,
            project_id: None,
            opened_at: 50,
            confidence: 0.7,
            now: 100,
        };
        let id = insert_open_loop(&mut conn, &l, &[Provenance::new(e)]).unwrap();
        let (status, kind): (String, String) = conn
            .query_row("SELECT status, kind FROM open_loops WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(status, "open");
        assert_eq!(kind, "reply_needed");
        assert_eq!(provenance_count(&conn, StateTable::OpenLoops, id).unwrap(), 1);
    }
}

#[cfg(test)]
mod quote_match_regression {
    #[test]
    fn borrowed_match_preserves_normalized_substring_semantics() {
        let sources = ["", "aaa b", "send\n\tthe\u{2003}deck today", "before café\n東京 after", "x send the decky", "a a b"];
        let quotes = ["", " \n", "aa b", "send the deck", " the\tdeck ", "café 東京", "deck", "a b", "missing"];
        for source in sources {
            for quote in quotes {
                let normalized = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
                let expected = !quote.trim().is_empty() && normalized(source).contains(&normalized(quote));
                assert_eq!(super::source_contains_quote(source, quote), expected, "{source:?} / {quote:?}");
            }
        }
    }
}

#[cfg(test)]
mod first_source_tests {
    #[test]
    fn first_valid_source_stops_before_the_recapture_tail() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute_batch("INSERT INTO commitments(id,direction,description,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
            VALUES(1,'mine','Send deck','open',0.6,0.6,1,1,1);").unwrap();
        for id in 1..=500 {
            conn.execute("INSERT INTO event_log(id,ts,source,kind,content,content_hash,last_seen_at) VALUES(?1,1,'capture','text','Send the deck',?2,1)", rusqlite::params![id,format!("first-source-{id}")]).unwrap();
            conn.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight,quote) VALUES('commitments',1,?1,1,?2)", rusqlite::params![id,if id==1 {"removed source quote"} else {"Send the deck"}]).unwrap();
        }
        let work = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&work);
        conn.progress_handler(1, Some(move || { count.fetch_add(1, Ordering::Relaxed); false })).unwrap();
        assert_eq!(super::get_commitment(&conn,1).unwrap().unwrap().first_event_id,Some(2));
        conn.progress_handler(0, None::<fn()->bool>).unwrap();
        assert!(work.load(Ordering::Relaxed)<500, "reader scanned the recapture tail: {} VM steps",work.load(Ordering::Relaxed));
        let complete = super::supported_provenance(&conn,super::StateTable::Commitments,1,usize::MAX).unwrap().unwrap();
        assert_eq!(complete.len(),499,"full search evidence keeps every valid link");
    }
}

#[cfg(test)]
mod recapture_identity_tests {
    #[test]
    fn changing_chrome_preserves_one_message_but_not_different_or_ambiguous_senders() {
        let quote = "I'll send the final deck";
        let first = "Inbox\nDana: I'll send the final deck.\nold sidebar";
        let scrolled = "Other navigation\nDana: I'll\nsend the final deck.\nnew message and sidebar";
        assert!(super::same_capture_quote(first, scrolled, quote));
        assert!(!super::same_capture_quote(first, "Inbox\nSam: I'll send the final deck.", quote));
        assert!(!super::same_capture_quote(first, "Dana: I'll send the final deck.\nSam: I'll send the final deck.", quote));
    }

    #[test]
    fn attribution_skips_timestamps_and_tolerates_a_bare_delta() {
        let quote = "I'll send the final deck";
        let same = |a, b| super::same_capture_quote(a, b, quote);
        assert!(same("Dana · 2m\nI'll send the final deck.", "Dana · 4m\nI'll send the final deck."));
        assert!(same("Dana · just now\nI'll send the final deck.", "Dana · 1m ago\nI'll send the final deck."));
        assert!(same("Dana\n10:42 AM\nI'll send the final deck.", "Dana\n10:42 AM\nI'll send the final deck.\nSam\n10:44 AM\nThanks"));
        assert!(!same("Dana\n10:42 AM\nI'll send the final deck.", "Sam\n10:42 AM\nI'll send the final deck."));
        // An AX delta carries only the new line; the later checkpoint shows who said it.
        assert!(same("I'll send the final deck.", "Sam: sounds good\nDana: I'll send the final deck."));
        assert!(same("10:42 AM\nI'll send the final deck.", "Dana\n10:42 AM\nI'll send the final deck."));
    }
}
