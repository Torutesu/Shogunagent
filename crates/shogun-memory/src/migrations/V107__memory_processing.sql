-- Rollback: docs/migrations/V107-rollback.md
-- Content-free, per-event proof of processing. Never infer success for pre-migration data.
CREATE TABLE event_processing_receipts (
    event_id INTEGER PRIMARY KEY REFERENCES event_log(id) ON DELETE CASCADE,
    arrival_seq INTEGER NOT NULL,
    reviewed_at INTEGER NOT NULL,
    lane TEXT NOT NULL CHECK (lane IN ('batch', 'local'))
) STRICT;

CREATE TRIGGER event_processing_reset AFTER UPDATE OF content, content_hash, ts, source, namespace ON event_log
BEGIN
    DELETE FROM event_processing_receipts WHERE event_id = new.id;
    DELETE FROM event_processing_failures WHERE event_id = new.id;
END;

CREATE TABLE event_processing_failures (
    event_id INTEGER PRIMARY KEY REFERENCES event_log(id) ON DELETE CASCADE,
    failed_at INTEGER NOT NULL,
    code TEXT NOT NULL CHECK (code IN ('consolidation_transport', 'consolidation_output', 'consolidation_storage'))
) STRICT;

-- An operator-configured worker's last pass. No contents, identifiers, counts or credentials.
CREATE TABLE memory_processing_runtime (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tick_at INTEGER NOT NULL,
    interval_ms INTEGER NOT NULL CHECK (interval_ms >= 0),
    outcome TEXT NOT NULL
) STRICT;
