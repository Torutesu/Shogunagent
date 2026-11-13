//! Local data export and full deletion (FR-SET-07). The user owns their memory: they can export it
//! all as JSON and delete it all. Both operate entirely on-device — export is a local file, never a
//! network send (invariant 3), and deletion wipes user data while keeping the schema so the app
//! keeps working.

use rusqlite::Connection;
use serde_json::{json, Value};

/// Export all user data as a JSON string (FR-SET-07). Includes the event log and the four state
/// tables with their content — this is the user's own local export, so full content is included
/// (it never leaves the device by this path). Ordering is stable (by id) for reproducibility.
pub fn export_json(conn: &Connection) -> Result<String, rusqlite::Error> {
    let events = rows(conn, "SELECT id, ts, source, kind, app_bundle_id, window_title, content, dwell_ms FROM event_log ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "ts": r.get::<_, i64>(1)?,
            "source": r.get::<_, String>(2)?,
            "kind": r.get::<_, String>(3)?,
            "app_bundle_id": r.get::<_, Option<String>>(4)?,
            "window_title": r.get::<_, Option<String>>(5)?,
            "content": r.get::<_, String>(6)?,
            "dwell_ms": r.get::<_, i64>(7)?,
        }))
    })?;
    let people = rows(conn, "SELECT id, display_name, confidence, wording_edited FROM people ORDER BY id", |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "display_name": r.get::<_, String>(1)?, "confidence": r.get::<_, f64>(2)?, "wording_edited": r.get::<_, bool>(3)? }))
    })?;
    let projects = rows(conn, "SELECT id, name, status, confidence, wording_edited FROM projects ORDER BY id", |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "confidence": r.get::<_, f64>(3)?, "wording_edited": r.get::<_, bool>(4)? }))
    })?;
    let commitments = rows(conn, "SELECT id, description, due_at, status, confidence, wording_edited FROM commitments ORDER BY id", |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "description": r.get::<_, String>(1)?, "due_at": r.get::<_, Option<i64>>(2)?, "status": r.get::<_, String>(3)?, "confidence": r.get::<_, f64>(4)?, "wording_edited": r.get::<_, bool>(5)? }))
    })?;
    let open_loops = rows(conn, "SELECT id, kind, description, staleness_days, status, confidence, wording_edited FROM open_loops ORDER BY id", |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "description": r.get::<_, String>(2)?, "staleness_days": r.get::<_, i64>(3)?, "status": r.get::<_, String>(4)?, "confidence": r.get::<_, f64>(5)?, "wording_edited": r.get::<_, bool>(6)? }))
    })?;
    // The rest of what the app holds about the user: threads (titles + summaries), meeting
    // sessions, the user's own meeting notes, transcripts, recaps, the provenance links that let
    // them verify any state claim, and the traceability log (digests, plus the masked URL of a public web fetch). An
    // export that omitted these would not be "export it all" (FR-SET-07).
    let threads = rows(conn, "SELECT id, thread_key, title, summary, participants, first_activity_at, last_activity_at, event_count FROM threads ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "thread_key": r.get::<_, String>(1)?,
            "title": r.get::<_, Option<String>>(2)?, "summary": r.get::<_, Option<String>>(3)?,
            "participants": r.get::<_, Option<String>>(4)?,
            "first_activity_at": r.get::<_, i64>(5)?, "last_activity_at": r.get::<_, i64>(6)?,
            "event_count": r.get::<_, i64>(7)?,
        }))
    })?;
    let sessions = rows(conn, "SELECT id, kind, started_at, ended_at, title, participants, summary, decisions, in_person FROM sessions ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
            "started_at": r.get::<_, i64>(2)?, "ended_at": r.get::<_, Option<i64>>(3)?,
            "title": r.get::<_, Option<String>>(4)?, "participants": r.get::<_, Option<String>>(5)?,
            "summary": r.get::<_, Option<String>>(6)?, "decisions": r.get::<_, Option<String>>(7)?,
            "in_person": r.get::<_, bool>(8)?,
        }))
    })?;
    let session_notes = rows(conn, "SELECT id, session_id, body, updated_at FROM session_notes ORDER BY id", |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "session_id": r.get::<_, i64>(1)?, "body": r.get::<_, String>(2)?, "updated_at": r.get::<_, i64>(3)? }))
    })?;
    let transcript_segments = rows(conn, "SELECT id, session_id, ts, speaker, text, origin, confidence, original_text, edited_at, excluded FROM transcript_segments ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "session_id": r.get::<_, i64>(1)?, "ts": r.get::<_, i64>(2)?,
            "speaker": r.get::<_, Option<String>>(3)?, "text": r.get::<_, String>(4)?,
            "origin": r.get::<_, String>(5)?, "confidence": r.get::<_, Option<f64>>(6)?,
            "original_text": r.get::<_, Option<String>>(7)?, "edited_at": r.get::<_, Option<i64>>(8)?, "excluded": r.get::<_, bool>(9)?,
        }))
    })?;
    let meeting_recaps = rows(conn, "SELECT id, session_id, summary, decisions, next_actions, model, created_at FROM meeting_recaps ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "session_id": r.get::<_, i64>(1)?,
            "summary": r.get::<_, Option<String>>(2)?, "decisions": r.get::<_, Option<String>>(3)?,
            "next_actions": r.get::<_, Option<String>>(4)?, "model": r.get::<_, Option<String>>(5)?,
            "created_at": r.get::<_, i64>(6)?,
        }))
    })?;
    // Mail triage (V33, issue #319): estimates and metadata — never a body (bodies are event_log).
    let mail_triage = rows(conn, "SELECT thread_key, latest_event_id, label_key, bucket, attention, confidence, stage, reasons, bucket_lock, archive_status, archived_at_ms, handled_at_ms, classified_at_ms FROM mail_triage ORDER BY classified_at_ms", |r| {
        Ok(json!({
            "thread_key": r.get::<_, String>(0)?, "latest_event_id": r.get::<_, i64>(1)?,
            "label_key": r.get::<_, String>(2)?, "bucket": r.get::<_, String>(3)?,
            "attention": r.get::<_, String>(4)?, "confidence": r.get::<_, f64>(5)?,
            "stage": r.get::<_, String>(6)?, "reasons": r.get::<_, String>(7)?,
            "bucket_lock": r.get::<_, Option<String>>(8)?, "archive_status": r.get::<_, String>(9)?,
            "archived_at_ms": r.get::<_, Option<i64>>(10)?, "handled_at_ms": r.get::<_, Option<i64>>(11)?,
            "classified_at_ms": r.get::<_, i64>(12)?,
        }))
    })?;
    let mail_messages = rows(conn, "SELECT message_id, thread_key, event_id, from_addr, from_name, to_addrs, cc_addrs, is_from_me, gmail_label_ids, internal_date_ms FROM mail_messages ORDER BY internal_date_ms", |r| {
        Ok(json!({
            "message_id": r.get::<_, String>(0)?, "thread_key": r.get::<_, String>(1)?,
            "event_id": r.get::<_, i64>(2)?, "from_addr": r.get::<_, String>(3)?,
            "from_name": r.get::<_, Option<String>>(4)?, "to_addrs": r.get::<_, String>(5)?,
            "cc_addrs": r.get::<_, String>(6)?, "is_from_me": r.get::<_, i64>(7)? != 0,
            "gmail_label_ids": r.get::<_, String>(8)?, "internal_date_ms": r.get::<_, i64>(9)?,
        }))
    })?;
    let mail_labels = rows(conn, "SELECT key, name, kind, definition, default_bucket, auto_archive, sync_to_gmail, enabled FROM mail_labels ORDER BY sort_order", |r| {
        Ok(json!({
            "key": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
            "definition": r.get::<_, Option<String>>(3)?, "default_bucket": r.get::<_, String>(4)?,
            "auto_archive": r.get::<_, i64>(5)? != 0, "sync_to_gmail": r.get::<_, i64>(6)? != 0,
            "enabled": r.get::<_, i64>(7)? != 0,
        }))
    })?;
    let mail_sender_rules = rows(conn, "SELECT id, match_kind, match_value, label_key, bucket, source FROM mail_sender_rules ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "match_kind": r.get::<_, String>(1)?, "match_value": r.get::<_, String>(2)?,
            "label_key": r.get::<_, String>(3)?, "bucket": r.get::<_, Option<String>>(4)?, "source": r.get::<_, String>(5)?,
        }))
    })?;
    let mail_write_log = rows(conn, "SELECT id, ts_ms, thread_key, action_kind, approval, status, undone_at_ms FROM mail_write_log ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "ts_ms": r.get::<_, i64>(1)?, "thread_key": r.get::<_, String>(2)?,
            "action_kind": r.get::<_, String>(3)?, "approval": r.get::<_, String>(4)?,
            "status": r.get::<_, String>(5)?, "undone_at_ms": r.get::<_, Option<i64>>(6)?,
        }))
    })?;
    let session_speakers = rows(conn, "SELECT session_id, speaker_key, display_name, source, updated_at FROM session_speakers ORDER BY id", |r| {
        Ok(json!({
            "session_id": r.get::<_, i64>(0)?, "speaker_key": r.get::<_, String>(1)?,
            "display_name": r.get::<_, String>(2)?, "source": r.get::<_, String>(3)?,
            "updated_at": r.get::<_, i64>(4)?,
        }))
    })?;
    // Work Patterns (V27): the behavior profile and the presentation log are the user's data too
    // (metadata-only by construction, but "export it all" means all — FR-SET-07).
    let work_patterns = rows(conn, "SELECT id, kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active, last_seen_at FROM work_patterns ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "key": r.get::<_, String>(2)?,
            "scope": r.get::<_, String>(3)?, "scope_ref": r.get::<_, String>(4)?,
            "count_7d": r.get::<_, i64>(5)?, "count_30d": r.get::<_, i64>(6)?,
            "trend": r.get::<_, f64>(7)?, "confidence": r.get::<_, f64>(8)?,
            "active": r.get::<_, i64>(9)? != 0, "last_seen_at": r.get::<_, i64>(10)?,
        }))
    })?;
    let offer_impressions = rows(conn, "SELECT id, ts_ms, action_kind, surface, rank, context_app FROM offer_impressions ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "ts_ms": r.get::<_, i64>(1)?,
            "action_kind": r.get::<_, String>(2)?, "surface": r.get::<_, String>(3)?,
            "rank": r.get::<_, Option<i64>>(4)?, "context_app": r.get::<_, Option<String>>(5)?,
        }))
    })?;
    let state_provenance = rows(conn, "SELECT state_table, state_id, event_id, weight, quote, batch_reviewed FROM state_provenance ORDER BY state_table, state_id, event_id", |r| {
        Ok(json!({
            "state_table": r.get::<_, String>(0)?, "state_id": r.get::<_, i64>(1)?,
            "event_id": r.get::<_, i64>(2)?, "weight": r.get::<_, f64>(3)?,
            "quote": r.get::<_, Option<String>>(4)?,
            "batch_reviewed": r.get::<_, bool>(5)?,
        }))
    })?;
    let traceability = rows(conn, "SELECT id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party, kind, url FROM traceability_log ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "route": r.get::<_, String>(2)?,
            "purpose": r.get::<_, String>(3)?, "destination": r.get::<_, String>(4)?,
            "chunk_bytes": r.get::<_, i64>(5)?, "chunk_xxh64": r.get::<_, String>(6)?,
            "third_party": r.get::<_, i64>(7)? != 0, "kind": r.get::<_, String>(8)?,
            "url": r.get::<_, Option<String>>(9)?,
        }))
    })?;
    let chat_threads = rows(
        conn,
        "SELECT id, created_at_ms, updated_at_ms FROM chat_threads ORDER BY id",
        |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "created_at_ms": row.get::<_, i64>(1)?,
                "updated_at_ms": row.get::<_, i64>(2)?,
            }))
        },
    )?;
    let chat_messages = rows(
        conn,
        "SELECT thread_id, pos, role, text, citations
           FROM chat_messages
          ORDER BY thread_id, pos",
        |row| {
            Ok(json!({
                "thread_id": row.get::<_, i64>(0)?,
                "pos": row.get::<_, i64>(1)?,
                "role": row.get::<_, String>(2)?,
                "text": row.get::<_, String>(3)?,
                "citations": row.get::<_, Option<String>>(4)?,
            }))
        },
    )?;

    // Briefs (V15) and distilled lessons (V16) are the user's data too. Raw feedback_events /
    // lesson_provenance stay out by design — V16 marks them "local DB only; never exported".
    let briefs = rows(conn, "SELECT date, payload, generated, built_at FROM briefs ORDER BY date", |r| {
        Ok(json!({
            "date": r.get::<_, String>(0)?, "payload": r.get::<_, String>(1)?,
            "generated": r.get::<_, i64>(2)? != 0, "built_at": r.get::<_, i64>(3)?,
        }))
    })?;
    let lessons = rows(conn, "SELECT id, kind, scope, scope_ref, instruction, confidence, evidence_count, active, created_at, updated_at, last_evidence_at FROM lessons ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
            "scope": r.get::<_, String>(2)?, "scope_ref": r.get::<_, Option<String>>(3)?,
            "instruction": r.get::<_, String>(4)?, "confidence": r.get::<_, f64>(5)?,
            "evidence_count": r.get::<_, i64>(6)?, "active": r.get::<_, i64>(7)? != 0,
            "created_at": r.get::<_, i64>(8)?, "updated_at": r.get::<_, i64>(9)?,
            "last_evidence_at": r.get::<_, i64>(10)?,
        }))
    })?;
    let period_summaries = rows(conn, "SELECT id, level, period_start_ms, period_end_ms, title, content, generation_mode, generation_model, generated, content_digest, is_dirty, dirty_at_ms, generated_at_ms, created_at_ms, updated_at_ms FROM period_summaries ORDER BY period_start_ms, id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "level": r.get::<_, String>(1)?,
            "period_start_ms": r.get::<_, i64>(2)?, "period_end_ms": r.get::<_, i64>(3)?,
            "title": r.get::<_, String>(4)?, "content": r.get::<_, String>(5)?,
            "generation_mode": r.get::<_, String>(6)?, "generation_model": r.get::<_, Option<String>>(7)?,
            "generated": r.get::<_, i64>(8)? != 0, "content_digest": r.get::<_, String>(9)?,
            "is_dirty": r.get::<_, i64>(10)? != 0, "dirty_at_ms": r.get::<_, Option<i64>>(11)?,
            "generated_at_ms": r.get::<_, i64>(12)?, "created_at_ms": r.get::<_, i64>(13)?,
            "updated_at_ms": r.get::<_, i64>(14)?,
        }))
    })?;
    let period_summary_event_provenance = rows(conn, "SELECT summary_id, event_id, weight FROM period_summary_event_provenance ORDER BY summary_id, event_id", |r| {
        Ok(json!({"summary_id": r.get::<_, i64>(0)?, "event_id": r.get::<_, i64>(1)?, "weight": r.get::<_, f64>(2)?}))
    })?;
    let period_summary_child_provenance = rows(conn, "SELECT summary_id, child_summary_id, weight FROM period_summary_child_provenance ORDER BY summary_id, child_summary_id", |r| {
        Ok(json!({"summary_id": r.get::<_, i64>(0)?, "child_summary_id": r.get::<_, i64>(1)?, "weight": r.get::<_, f64>(2)?}))
    })?;
    let period_summary_shard_runs = rows(conn, "SELECT level, period_start_ms, period_end_ms, state, attempts, failure_class, updated_at_ms FROM period_summary_shard_runs ORDER BY level, period_start_ms", |r| {
        Ok(json!({"level": r.get::<_, String>(0)?, "period_start_ms": r.get::<_, i64>(1)?,
            "period_end_ms": r.get::<_, i64>(2)?, "state": r.get::<_, String>(3)?,
            "attempts": r.get::<_, i64>(4)?, "failure_class": r.get::<_, Option<String>>(5)?,
            "updated_at_ms": r.get::<_, i64>(6)?}))
    })?;
    let search_documents = rows(conn, "SELECT id, document_kind, source_table, source_id, title, body, period_start_ms, period_end_ms, confidence, generation_kind, generation_model, generated_at_ms, content_digest, source_updated_at_ms, is_dirty, dirty_at_ms, last_projected_at_ms, created_at_ms, updated_at_ms FROM search_documents ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "document_kind": r.get::<_, String>(1)?,
            "source_table": r.get::<_, String>(2)?, "source_id": r.get::<_, String>(3)?,
            "title": r.get::<_, String>(4)?, "body": r.get::<_, String>(5)?,
            "period_start_ms": r.get::<_, Option<i64>>(6)?, "period_end_ms": r.get::<_, Option<i64>>(7)?,
            "confidence": r.get::<_, f64>(8)?, "generation_kind": r.get::<_, String>(9)?,
            "generation_model": r.get::<_, Option<String>>(10)?, "generated_at_ms": r.get::<_, Option<i64>>(11)?,
            "content_digest": r.get::<_, String>(12)?, "source_updated_at_ms": r.get::<_, i64>(13)?,
            "is_dirty": r.get::<_, i64>(14)? != 0, "dirty_at_ms": r.get::<_, Option<i64>>(15)?,
            "last_projected_at_ms": r.get::<_, i64>(16)?, "created_at_ms": r.get::<_, i64>(17)?,
            "updated_at_ms": r.get::<_, i64>(18)?,
        }))
    })?;
    let search_document_provenance = rows(conn, "SELECT id, document_id, evidence_kind, event_id, session_id, transcript_segment_id, session_note_id, meeting_recap_id, brief_date, feedback_event_id, weight FROM search_document_provenance ORDER BY document_id, evidence_kind, id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "document_id": r.get::<_, i64>(1)?,
            "evidence_kind": r.get::<_, String>(2)?, "event_id": r.get::<_, Option<i64>>(3)?,
            "session_id": r.get::<_, Option<i64>>(4)?, "transcript_segment_id": r.get::<_, Option<i64>>(5)?,
            "session_note_id": r.get::<_, Option<i64>>(6)?, "meeting_recap_id": r.get::<_, Option<i64>>(7)?,
            "brief_date": r.get::<_, Option<String>>(8)?, "feedback_event_id": r.get::<_, Option<i64>>(9)?,
            "weight": r.get::<_, f64>(10)?,
        }))
    })?;
    let search_document_invalidations = rows(conn, "SELECT id, source_table, source_id, invalidation_kind, required_owner_updated_after_ms, invalidated_at_ms FROM search_document_invalidations ORDER BY id", |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "source_table": r.get::<_, String>(1)?,
            "source_id": r.get::<_, String>(2)?, "invalidation_kind": r.get::<_, String>(3)?,
            "required_owner_updated_after_ms": r.get::<_, Option<i64>>(4)?,
            "invalidated_at_ms": r.get::<_, i64>(5)?,
        }))
    })?;

    let writing_voice_examples = rows(conn, "SELECT app, text, ts_ms, person FROM writing_voice_examples ORDER BY id", |r| {
        Ok(json!({"app": r.get::<_, String>(0)?, "text": r.get::<_, String>(1)?, "ts_ms": r.get::<_, i64>(2)?, "person": r.get::<_, String>(3)?}))
    })?;
    let dictionary_autoadd = rows(conn, "SELECT original, corrected, observations, state, created_at_ms FROM dictionary_autoadd_corrections ORDER BY id", |r| {
        Ok(json!({ "original": r.get::<_, String>(0)?, "corrected": r.get::<_, String>(1)?, "observations": r.get::<_, i64>(2)?, "state": r.get::<_, String>(3)?, "created_at_ms": r.get::<_, i64>(4)? }))
    })?;
    let knowledge_entities = rows(conn, "SELECT id, kind, name, context, extraction_key, confidence, created_at, updated_at FROM knowledge_entities ORDER BY id", |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?, "context": r.get::<_, String>(3)?,
            "extraction_key": r.get::<_, String>(4)?, "confidence": r.get::<_, f64>(5)?, "created_at": r.get::<_, i64>(6)?, "updated_at": r.get::<_, i64>(7)?}))
    })?;
    let knowledge_provenance = rows(conn, "SELECT entity_id, event_id, quote FROM knowledge_provenance ORDER BY entity_id, event_id", |r| {
        Ok(json!({"entity_id": r.get::<_, i64>(0)?, "event_id": r.get::<_, i64>(1)?, "quote": r.get::<_, Option<String>>(2)?}))
    })?;
    // Imported memory (V52): the import records and the claims they stored (FR-SET-07).
    let memory_imports = rows(conn, "SELECT id, origin, provenance, via, generated_at, coverage_note, package_bytes, package_xxh64, item_count, created_at FROM memory_imports ORDER BY id", |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "origin": r.get::<_, String>(1)?, "provenance": r.get::<_, String>(2)?,
            "via": r.get::<_, String>(3)?, "generated_at": r.get::<_, Option<String>>(4)?, "coverage_note": r.get::<_, Option<String>>(5)?,
            "package_bytes": r.get::<_, i64>(6)?, "package_xxh64": r.get::<_, String>(7)?, "item_count": r.get::<_, i64>(8)?,
            "created_at": r.get::<_, i64>(9)?}))
    })?;
    let memory_import_items = rows(conn, "SELECT id, import_id, package_item, section, kind, name, target, summary, aliases, status, confidence, requested_confidence, evidence, linked_table, linked_id, created_at FROM memory_import_items ORDER BY id", |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "import_id": r.get::<_, i64>(1)?, "package_item": r.get::<_, String>(2)?,
            "section": r.get::<_, String>(3)?, "kind": r.get::<_, Option<String>>(4)?, "name": r.get::<_, String>(5)?,
            "target": r.get::<_, Option<String>>(6)?, "summary": r.get::<_, Option<String>>(7)?, "aliases": r.get::<_, Option<String>>(8)?,
            "status": r.get::<_, String>(9)?, "confidence": r.get::<_, f64>(10)?, "requested_confidence": r.get::<_, String>(11)?,
            "evidence": r.get::<_, Option<String>>(12)?, "linked_table": r.get::<_, Option<String>>(13)?,
            "linked_id": r.get::<_, Option<i64>>(14)?, "created_at": r.get::<_, i64>(15)?}))
    })?;
    let doc = json!({
        "user_memory_notes": rows(conn,"SELECT id,text,revision,forgotten,created_at,updated_at FROM user_memory_notes ORDER BY id",|r| Ok(json!({"id":r.get::<_,i64>(0)?,"text":r.get::<_,String>(1)?,"revision":r.get::<_,i64>(2)?,"forgotten":r.get::<_,bool>(3)?,"created_at":r.get::<_,i64>(4)?,"updated_at":r.get::<_,i64>(5)?})))?,
        "user_memory_note_versions": rows(conn,"SELECT note_id,revision,text,forgotten,changed_at FROM user_memory_note_versions ORDER BY note_id,revision",|r| Ok(json!({"note_id":r.get::<_,i64>(0)?,"revision":r.get::<_,i64>(1)?,"text":r.get::<_,String>(2)?,"forgotten":r.get::<_,bool>(3)?,"changed_at":r.get::<_,i64>(4)?})))?,
        "memory_imports": memory_imports,
        "memory_import_items": memory_import_items,
        "knowledge_entities": knowledge_entities,
        "knowledge_provenance": knowledge_provenance,
        "writing_voice_examples": writing_voice_examples,
        "rewrite_style_notes": crate::rewrite_loop::list_notes(conn)?,
        "dictionary_autoadd": dictionary_autoadd,
        "schema_version": crate::schema_version(conn)?,
        "event_log": events,
        "people": people,
        "projects": projects,
        "commitments": commitments,
        "open_loops": open_loops,
        "threads": threads,
        "sessions": sessions,
        "session_notes": session_notes,
        "transcript_segments": transcript_segments,
        "meeting_recaps": meeting_recaps,
        "session_speakers": session_speakers,
        "mail_triage": mail_triage,
        "mail_messages": mail_messages,
        "mail_labels": mail_labels,
        "mail_sender_rules": mail_sender_rules,
        "mail_write_log": mail_write_log,
        "work_patterns": work_patterns,
        "offer_impressions": offer_impressions,
        "state_provenance": state_provenance,
        "traceability_log": traceability,
        "chat_threads": chat_threads,
        "chat_messages": chat_messages,
        "briefs": briefs,
        "lessons": lessons,
        "period_summaries": period_summaries,
        "period_summary_event_provenance": period_summary_event_provenance,
        "period_summary_child_provenance": period_summary_child_provenance,
        "period_summary_shard_runs": period_summary_shard_runs,
        "search_documents": search_documents,
        "search_document_provenance": search_document_provenance,
        "search_document_invalidations": search_document_invalidations,
    });
    Ok(doc.to_string())
}

/// Run a query and collect each row into a JSON value.
fn rows(
    conn: &Connection,
    sql: &str,
    map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
) -> Result<Vec<Value>, rusqlite::Error> {
    let mut stmt = conn.prepare(sql)?;
    let out = stmt.query_map([], |r| map(r))?;
    out.collect()
}

/// What one bounded space-reclamation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Free-list pages before the pass.
    pub freelist_before: i64,
    /// Free-list pages after the pass.
    pub freelist_after: i64,
}

/// Give freed pages back to the filesystem, a bounded slice at a time, and let the FTS indexes
/// merge their segment tombstones.
///
/// Nothing else shrinks the file: frame expiry, `delete_since`, "delete all" and the FTS
/// delete+reinsert on content changes only move pages to SQLite's free list, so without this the
/// database sits at its high-water mark forever (2 GiB of expired visual-recall JPEG included).
/// `PRAGMA incremental_vacuum(N)` needs `auto_vacuum = INCREMENTAL`, which `apply_pragmas` sets
/// for databases created from now on; an older file converts on its next `VACUUM` (see
/// `Db::delete_all`). On a file in `auto_vacuum = NONE` the pragma is a harmless no-op. Must not
/// run inside an open transaction. `max_pages` bounds the work per call so an hourly pass stays
/// a few tens of ms, never a multi-second stall behind the connection lock.
pub fn reclaim_space(conn: &Connection, max_pages: i64) -> Result<ReclaimReport, rusqlite::Error> {
    // FTS5 external-content tables keep deleted rows as tombstones until segments merge; an
    // incremental 'merge' of a few segments per pass keeps the index from bloating without the
    // full 'optimize' rewrite. Merge first: it frees segment pages the vacuum below then returns.
    for fts in ["event_fts", "event_grams_fts", "search_documents_fts"] {
        let _ = conn.execute(&format!("INSERT INTO {fts}({fts}, rank) VALUES ('merge', 64)"), []);
    }
    let freelist_before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    // `incremental_vacuum` yields one (empty) result row per page it frees, so it has to be
    // stepped to exhaustion like a query — a single `execute` would return after the first page.
    {
        let mut stmt = conn.prepare(&format!("PRAGMA incremental_vacuum({})", max_pages.max(1)))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
    }
    let freelist_after: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    Ok(ReclaimReport { freelist_before, freelist_after })
}

/// Retention windows for the telemetry-like tables — the ones that grow with every LLM call or
/// context assembly and that nothing else ever trimmed.
///
/// `traceability_log` is the user's own record of what left the device, so it keeps a full
/// year; `llm_usage` the same, so a year of spend stays reviewable; `compression_metrics` is a
/// tuning signal and a month is plenty. `dream_batch_send_stats` (V54, one row per night) keeps a
/// year too: OPEN-B2 reads weeks of it, and a year of nights is a few hundred rows. `feedback_events` are deliberately not here: the lesson
/// watermark reads them and they hold the user's own corrections. `job_runs` is the Dream
/// Cycle's resume/high-water ledger and is left to it.
pub const TRACEABILITY_RETENTION_MS: i64 = 365 * 24 * 60 * 60 * 1000;
pub const LLM_USAGE_RETENTION_MS: i64 = 365 * 24 * 60 * 60 * 1000;
pub const COMPRESSION_METRICS_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const BATCH_SEND_STATS_RETENTION_MS: i64 = 365 * 24 * 60 * 60 * 1000;
/// Shared-service round trips (V67, issue #29 §4-6): the p95 `shogun metrics` shows reads the
/// last week, so a month is ample and the table stays a few thousand rows at most.
pub const SHARED_SERVICE_RTT_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What one telemetry-retention pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TelemetryPrune {
    pub traceability: usize,
    pub llm_usage: usize,
    pub compression_metrics: usize,
    pub batch_send_stats: usize,
    pub shared_service_rtt: usize,
}

/// Apply the retention windows above. Bounded per table per call so an hourly pass on a
/// year-old install never holds the connection for a bulk delete.
pub fn prune_telemetry(conn: &Connection, now_ms: i64, max_rows_per_table: usize) -> Result<TelemetryPrune, rusqlite::Error> {
    let limit = max_rows_per_table.max(1) as i64;
    let traceability = conn.execute(
        "DELETE FROM traceability_log WHERE id IN (
            SELECT id FROM traceability_log WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
        rusqlite::params![now_ms.saturating_sub(TRACEABILITY_RETENTION_MS), limit],
    )?;
    let llm_usage = conn.execute(
        "DELETE FROM llm_usage WHERE id IN (
            SELECT id FROM llm_usage WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
        rusqlite::params![now_ms.saturating_sub(LLM_USAGE_RETENTION_MS), limit],
    )?;
    let compression_metrics = conn.execute(
        "DELETE FROM compression_metrics WHERE id IN (
            SELECT id FROM compression_metrics WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
        rusqlite::params![now_ms.saturating_sub(COMPRESSION_METRICS_RETENTION_MS), limit],
    )?;
    let batch_send_stats = conn.execute(
        "DELETE FROM dream_batch_send_stats WHERE rowid IN (
            SELECT rowid FROM dream_batch_send_stats WHERE recorded_at < ?1 ORDER BY recorded_at LIMIT ?2)",
        rusqlite::params![now_ms.saturating_sub(BATCH_SEND_STATS_RETENTION_MS), limit],
    )?;
    let shared_service_rtt = conn.execute(
        "DELETE FROM shared_service_rtt WHERE id IN (
            SELECT id FROM shared_service_rtt WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
        rusqlite::params![now_ms.saturating_sub(SHARED_SERVICE_RTT_RETENTION_MS), limit],
    )?;
    Ok(TelemetryPrune {
        traceability,
        llm_usage,
        compression_metrics,
        batch_send_stats,
        shared_service_rtt,
    })
}

/// How many rows deletion removed, per table.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DeleteReport {
    pub events: usize,
    pub people: usize,
    pub projects: usize,
    pub commitments: usize,
    pub open_loops: usize,
    pub threads: usize,
    pub sessions: usize,
    pub session_notes: usize,
    pub transcript_segments: usize,
    pub meeting_recaps: usize,
    pub screen_frames: usize,
    pub traceability: usize,
    pub chat_threads: usize,
    pub chat_messages: usize,
    pub briefs: usize,
    pub feedback_events: usize,
    pub lessons: usize,
    pub search_documents: usize,
    pub search_document_provenance: usize,
    pub search_document_invalidations: usize,
    pub period_summaries: usize,
    pub period_summary_event_provenance: usize,
    pub period_summary_child_provenance: usize,
    pub period_summary_shard_runs: usize,
    /// Event rows dated before the cutoff that a time-range delete removed anyway (#900): the
    /// full-transcript and note rows `meeting_index` wrote for a meeting that reaches into the
    /// window, dated at the meeting's start. Not shown to the user: the daemon drops them from its
    /// in-memory recent events, which it otherwise clears only from the cutoff on. Always empty
    /// for "Delete everything", after which the daemon clears them whole.
    #[serde(skip)]
    pub meeting_rows_removed: Vec<i64>,
}

fn remove_autoadd_artifacts(tx: &rusqlite::Transaction<'_>, cutoff: i64) -> Result<(), rusqlite::Error> {
    tx.execute(
        "DELETE FROM voice_term_aliases WHERE EXISTS
         (SELECT 1 FROM dictionary_autoadd_artifacts a
          WHERE a.created_at_ms >= ?1 AND a.alias_id = voice_term_aliases.id
            AND a.term_id = voice_term_aliases.term_id)",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM voice_terms WHERE id IN
         (SELECT owner.term_id FROM dictionary_autoadd_artifacts owner
          WHERE owner.created_term = 1 AND EXISTS
            (SELECT 1 FROM dictionary_autoadd_artifacts doomed
             WHERE doomed.term_id = owner.term_id AND doomed.created_at_ms >= ?1))
         AND locale IS NULL AND scope = 'surface' AND scope_ref = 'voice_dictation'
         AND priority = 0 AND enabled = 1
         AND (SELECT count(*) FROM voice_term_aliases WHERE term_id = voice_terms.id) = 1
         AND EXISTS (SELECT 1 FROM dictionary_autoadd_artifacts a
           JOIN voice_term_aliases v ON v.id = a.canonical_alias_id AND v.term_id = a.term_id
           WHERE a.term_id = voice_terms.id AND a.created_term = 1 AND v.alias = voice_terms.canonical)",
        [cutoff],
    )?;
    // Keep text-free ownership of a surviving auto-created term. A later deletion of its other
    // learned aliases must still be able to remove the last auto-created canonical row.
    tx.execute(
        "UPDATE dictionary_autoadd_artifacts SET alias_id = NULL
         WHERE created_at_ms >= ?1 AND created_term = 1
           AND term_id IN (SELECT id FROM voice_terms)",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM dictionary_autoadd_artifacts WHERE created_at_ms >= ?1
         AND NOT (created_term = 1 AND term_id IN (SELECT id FROM voice_terms))",
        [cutoff],
    )?;
    tx.execute("DELETE FROM dictionary_autoadd_suppression WHERE created_at_ms >= ?1", [cutoff])?;
    Ok(())
}

/// Delete **all** user data (FR-SET-07), keeping the schema. Runs in a single transaction so a
/// failure leaves the database untouched. Child rows (provenance, commitments, open loops) go
/// before their parents to satisfy foreign keys; embeddings, the FTS mirror (via triggers), the
/// traceability log, and the Dream Cycle ledger are cleared too.
pub fn delete_all(conn: &mut Connection) -> Result<DeleteReport, rusqlite::Error> {
    let tx = conn.transaction()?;
    remove_autoadd_artifacts(&tx, i64::MIN)?;
    // Full reset intentionally terminates unfinished Dream work and removes its temporary input
    // pins before canonical events are deleted.
    tx.execute("DELETE FROM dream_cycles", [])?;
    tx.execute("DELETE FROM memory_processing_runtime", [])?;
    tx.execute("DELETE FROM about_profile_generation", [])?;
    tx.execute("DELETE FROM daily_activity_summary_jobs", [])?;
    tx.execute("DELETE FROM brief_generation", [])?;
    tx.execute("DELETE FROM wrap_generation", [])?;
    let period_summary_event_provenance =
        tx.execute("DELETE FROM period_summary_event_provenance", [])?;
    let period_summary_child_provenance =
        tx.execute("DELETE FROM period_summary_child_provenance", [])?;
    let period_summary_shard_runs = tx.execute("DELETE FROM period_summary_shard_runs", [])?;
    let search_document_provenance =
        tx.execute("DELETE FROM search_document_provenance", [])?;
    let search_documents = tx.execute("DELETE FROM search_documents", [])?;
    let period_summaries = tx.execute("DELETE FROM period_summaries", [])?;
    // The tenancy rows (V94–V97): the router's run ledger, the export deletion markers, the
    // principal's cursors and the typed relations are this person's data too. Relations reference
    // event_log, so they go before the events.
    tx.execute("DELETE FROM relations", [])?;
    tx.execute("DELETE FROM tool_runs", [])?;
    tx.execute("DELETE FROM export_tombstones", [])?;
    tx.execute("DELETE FROM principal_meta", [])?;
    // `memory.ingest` receipts (V99: who ingested what, by event id) and the agents' scratch
    // files (V100: their own working notes) are this person's data too.
    tx.execute("DELETE FROM ingest_receipts", [])?;
    tx.execute("DELETE FROM scratch_files", [])?;
    // The audit log (V101) records who wrote what and when: this person's activity, so it goes
    // too. The access grants (`scratch_acl`, `principal_grants`) are settings, kept like others.
    tx.execute("DELETE FROM audit_log", [])?;
    // The approval decisions (V103) are the same kind of activity record. The spent decision
    // nonces are kept: they hold no content, expire with their token, and wiping them would let
    // an unexpired approval link decide a second time.
    tx.execute("DELETE FROM approval_decisions", [])?;
    // Payment mandates and their spends (V105): this person's grants and payment activity. Spends
    // first (they reference mandates). Deleting a mandate only removes what an agent may pay.
    tx.execute("DELETE FROM payment_spends", [])?;
    tx.execute("DELETE FROM payment_mandates", [])?;
    // children first (FK order): provenance → commitments/open_loops → threads → people/projects.
    // `threads` holds titles, summaries and participants, so it is user data and must go too —
    // and it references projects, so it goes before them.
    tx.execute("DELETE FROM state_provenance", [])?;
    // Extracted knowledge (V37): entity names/contexts and the up-to-800-byte source quotes on
    // their provenance. Deleted explicitly rather than left to the event_log cascade and the
    // `knowledge_without_evidence` trigger, which never reach an entity with no provenance row.
    // The entity AD trigger clears `knowledge_entities_fts` (V39) row by row.
    tx.execute("DELETE FROM knowledge_provenance", [])?;
    tx.execute("DELETE FROM knowledge_entities", [])?;
    tx.execute("DELETE FROM state_disputes", [])?;
    // Split decisions (V50) name row ids that are about to be freed for reuse.
    tx.execute("DELETE FROM state_split_events", [])?;
    tx.execute("DELETE FROM state_merge_blocks", [])?;
    // Synced calendar events: titles, meeting links and attendee addresses are user data too.
    tx.execute("DELETE FROM calendar_occurrences", [])?;
    // Skill runs (V44): generated text that can quote the user's memory.
    tx.execute("DELETE FROM skill_runs", [])?;
    // Translation hand-offs (V66): minutes-old request / result text, never a history.
    tx.execute("DELETE FROM translate_jobs", [])?;
    // AI search hand-offs (V72): a question and its matched excerpts, minutes old.
    tx.execute("DELETE FROM ai_search_jobs", [])?;
    // Schedules (V45): the input text is the user's own words.
    tx.execute("DELETE FROM scheduled_jobs", [])?;
    // File exports (V48): folder paths and file names can carry meeting titles.
    tx.execute("DELETE FROM local_writes", [])?;
    // Imported memory (V52): claims about the user from another assistant or Computer History.
    tx.execute("DELETE FROM memory_import_items", [])?;
    tx.execute("DELETE FROM memory_imports", [])?;
    tx.execute("DELETE FROM user_memory_note_versions", [])?;
    tx.execute("DELETE FROM user_memory_notes", [])?;
    // Clarify questions (V53): agent-written questions and the user's own answers.
    tx.execute("DELETE FROM clarify_questions", [])?;
    // Review signals (V83): captured quotes; they also cascade with the event log.
    tx.execute("DELETE FROM loop_signal_reads", [])?;
    tx.execute("DELETE FROM loop_signals", [])?;
    // Outcome updates (V104): captured quotes too; the scan watermark goes with the event log.
    tx.execute("DELETE FROM loop_updates", [])?;
    tx.execute("DELETE FROM loop_update_scan", [])?;
    // Learned self names (V86): display names captured from chat windows, and their markers.
    tx.execute("DELETE FROM self_name_evidence", [])?;
    tx.execute("DELETE FROM memory_markers", [])?;
    let commitments = tx.execute("DELETE FROM commitments", [])?;
    let open_loops = tx.execute("DELETE FROM open_loops", [])?;
    let threads = tx.execute("DELETE FROM threads", [])?;
    let people = tx.execute("DELETE FROM people", [])?;
    let projects = tx.execute("DELETE FROM projects", [])?;
    // Visual-recall frames reference event_log (V12, no ON DELETE), so they must go before the
    // event log — with foreign_keys=ON a lingering frame FK-fails the event delete and rolls the
    // whole "delete everything" back for anyone who ever enabled Visual recall (FR-SET-07).
    let screen_frames = tx.execute("DELETE FROM screen_frames", [])?;
    // Mail triage (V33, issue #319): estimates, message metadata, corrections, the Batch ledger,
    // the write-back history, learned sender rules, cursors and identities are all user data.
    // The label registry stays (built-in seeds + the user's own custom definitions are settings).
    tx.execute("DELETE FROM mail_triage_feedback", [])?;
    tx.execute("DELETE FROM mail_triage_batch_items", [])?;
    tx.execute("DELETE FROM mail_write_log", [])?;
    tx.execute("DELETE FROM mail_triage", [])?;
    tx.execute("DELETE FROM mail_messages", [])?;
    tx.execute("DELETE FROM mail_sender_rules", [])?;
    tx.execute("DELETE FROM connector_sync_cursors", [])?;
    tx.execute("DELETE FROM mail_identities", [])?;
    // embeddings (Warm vec0 + Cold int8) then the event log (its AD trigger clears event_fts)
    tx.execute("DELETE FROM event_vec", [])?;
    tx.execute("DELETE FROM cold_embeddings", [])?;
    // V102: which model wrote each vector (ids only); the active space itself is a setting.
    tx.execute("DELETE FROM vector_models", [])?;
    // Dashboard usage is metadata-only, but it is still a record of user activity. Credits would
    // cascade with event_log; delete both tables explicitly so the full-wipe contract stays
    // obvious and content-free edit counts are removed too.
    tx.execute("DELETE FROM dashboard_activity_credits", [])?;
    tx.execute("DELETE FROM dashboard_edit_counts", [])?;
    let events = tx.execute("DELETE FROM event_log", [])?;
    // Everything that references sessions with no ON DELETE CASCADE must go before the sessions
    // themselves, or with foreign_keys=ON the delete FK-fails and rolls back. Meeting notes
    // (V8, the user's own words — the most personal rows here, FR-MT-10), transcript segments
    // (V9), and meeting recaps (V10) all reference sessions.
    let session_notes = tx.execute("DELETE FROM session_notes", [])?;
    // Transcripts (what was said, FR-MT-13) and recaps (what the model concluded) both hold
    // `NOT NULL REFERENCES sessions(id)` with no ON DELETE clause, so under foreign_keys=ON they
    // must go before sessions — forgetting either aborts the whole transaction and "delete
    // everything" deletes nothing (FR-SET-07).
    let transcript_segments = tx.execute("DELETE FROM transcript_segments", [])?;
    let meeting_recaps = tx.execute("DELETE FROM meeting_recaps", [])?;
    // Crash-recovery markers (V89) only point at sessions; explicit so delete_all owns them.
    tx.execute("DELETE FROM meeting_recovery", [])?;
    // Speaker names (V25) also hold `NOT NULL REFERENCES sessions(id)` with no cascade — the
    // E-01 lesson: forgetting one child table makes "delete everything" delete nothing.
    tx.execute("DELETE FROM session_speakers", [])?;
    // Sessions hold the meeting's title, summary and decisions — user data. event_log also
    // references sessions, and it was already cleared above, so sessions can go now (FR-SET-07,
    // FR-MT-05).
    let sessions = tx.execute("DELETE FROM sessions", [])?;
    let traceability = tx.execute("DELETE FROM traceability_log", [])?;
    // L5 learning data (V16) and persisted briefs (V15). feedback_events.before_text/after_text
    // hold the user's actual proposed and approved message bodies — the most personal rows here.
    // Provenance goes before both of its parents (FK order).
    tx.execute("DELETE FROM lesson_provenance", [])?;
    let lessons = tx.execute("DELETE FROM lessons", [])?;
    let feedback_events = tx.execute("DELETE FROM feedback_events", [])?;
    tx.execute("DELETE FROM writing_voice_examples", [])?;
    // Rewrite learning loop (V78): impressions first (FK), then notes and outcomes.
    tx.execute("DELETE FROM rewrite_note_impressions", [])?;
    tx.execute("DELETE FROM rewrite_style_notes", [])?;
    tx.execute("DELETE FROM rewrite_outcomes", [])?;
    tx.execute("DELETE FROM dictionary_autoadd_corrections", [])?;
    tx.execute("DELETE FROM dictionary_autoadd_suppression", [])?;
    tx.execute("UPDATE dictionary_autoadd_settings SET enabled = 0, consent_version = 0, generation = generation + 1 WHERE id = 1", [])?;
    tx.execute("DELETE FROM writing_voice_outputs", [])?;
    tx.execute("UPDATE writing_voice_settings SET enabled = 0, revision = revision + 1 WHERE id = 1", [])?;
    // feedback_events.id carries no AUTOINCREMENT, so ids restart at 1 after the wipe. The
    // distill watermark is monotonic (MAX on advance) and would sit above every future id,
    // silently ending lesson learning forever — rewind it with the data it indexed.
    tx.execute(
        "UPDATE lesson_distill_meta SET last_processed_feedback_id = 0 WHERE id = 1",
        [],
    )?;
    let briefs = tx.execute("DELETE FROM briefs", [])?;
    // Notch chats (V25). Children first; the singleton state FK becomes NULL when threads go.
    let chat_messages = tx.execute("DELETE FROM chat_messages", [])?;
    let chat_threads = tx.execute("DELETE FROM chat_threads", [])?;
    let search_document_invalidations =
        tx.execute("DELETE FROM search_document_invalidations", [])?;
    tx.execute("DELETE FROM job_runs", [])?;
    // Query-hash metrics carry no content, but they are still records of the user's activity.
    tx.execute("DELETE FROM compression_metrics", [])?;
    // Nightly Batch send counts (V54): no content, but a per-night activity record all the same.
    tx.execute("DELETE FROM dream_batch_send_stats", [])?;
    // Shared-service round trips (V67): timings only, but still a record of activity.
    tx.execute("DELETE FROM shared_service_rtt", [])?;
    // Work Patterns (V27): the behavior profile and the presentation log are activity records.
    tx.execute("DELETE FROM work_patterns", [])?;
    tx.execute("DELETE FROM offer_impressions", [])?;
    // Issue #780: the per-call model usage log (route, model, tokens, outcome) is a record of every assisted action; Batch relay attempts, Dream arrival
    // order and the suppression digests of deleted/edited records all describe data that no
    // longer exists.
    tx.execute("DELETE FROM llm_usage", [])?;
    tx.execute("DELETE FROM relay_attempts", [])?;
    tx.execute("DELETE FROM dream_event_arrivals", [])?;
    tx.execute("DELETE FROM state_suppressions", [])?;
    tx.execute("DELETE FROM state_suppressions_scoped", [])?;
    // The dictionary ownership key only keys digests of rows deleted above; a new one keeps any
    // digest that survives elsewhere (a backup, an export) from matching future rows.
    tx.execute("UPDATE dictionary_autoadd_secret SET key = randomblob(32) WHERE id = 1", [])?;
    tx.commit()?;

    Ok(DeleteReport {
        events,
        people,
        projects,
        commitments,
        open_loops,
        threads,
        sessions,
        session_notes,
        transcript_segments,
        meeting_recaps,
        screen_frames,
        traceability,
        chat_threads,
        chat_messages,
        briefs,
        feedback_events,
        lessons,
        search_documents,
        search_document_provenance,
        search_document_invalidations,
        period_summaries,
        period_summary_event_provenance,
        period_summary_child_provenance,
        period_summary_shard_runs,
        meeting_rows_removed: Vec::new(),
    })
}

/// A meeting or call still being recorded. A time-range delete never deletes one whole (#900):
/// the recorder keeps writing to it, so deleting the session under it made every later
/// transcript line, the close and the recap fail silently on the foreign key while the notch
/// still showed "recording". Stopping it instead would start its recap, which sends the
/// transcript to the user's AI provider. It is trimmed instead (see the meeting block in
/// [`delete_since`]).
const SESSION_RECORDING: &str = "ended_at IS NULL AND kind IN ('meeting', 'call')";

/// A session with any part at or after the cutoff `?1` (#900), which a time-range delete removes
/// whole: it started then, is still open, ended after it, or holds a transcript line or a note
/// saved then. Sessions are half-open, `[started_at, ended_at)`, so one that ended exactly at the
/// cutoff is wholly before it. The content checks catch a meeting closed by crash recovery, which
/// records `ended_at = started_at` (`daemon::meetings`) although its transcript ran on past the
/// cutoff. A meeting still being recorded ([`SESSION_RECORDING`]) is never matched.
///
/// Read once, before anything is deleted: the content checks look at the very rows the
/// deletion removes.
fn session_overlaps_window() -> String {
    format!(
        "NOT ({SESSION_RECORDING}) AND (
            started_at >= ?1
            OR ended_at IS NULL
            OR ended_at > ?1
            OR id IN (SELECT session_id FROM transcript_segments WHERE ts >= ?1)
            OR id IN (SELECT session_id FROM session_notes WHERE updated_at >= ?1))"
    )
}

/// The ids a one-parameter query returns, in its order.
fn ids_for(tx: &rusqlite::Transaction<'_>, sql: &str, cutoff_ts: i64) -> Result<Vec<i64>, rusqlite::Error> {
    let mut stmt = tx.prepare(sql)?;
    let ids = stmt
        .query_map([cutoff_ts], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Delete every user row whose occurrence time is at or after `cutoff_ts` (unix ms), and any state
/// row that loses ALL of its evidence as a result (design decision ③). Runs in a single
/// transaction. Derived summary text on a surviving state row may still reflect a deleted event
/// until the next Dream Cycle re-derivation — this is documented, not silently hidden.
pub fn delete_since(conn: &mut Connection, cutoff_ts: i64) -> Result<DeleteReport, rusqlite::Error> {
    let tx = conn.transaction()?;
    remove_autoadd_artifacts(&tx, cutoff_ts)?;
    let search_documents_before: usize = tx.query_row(
        "SELECT count(*) FROM search_documents",
        [],
        |row| row.get(0),
    )?;
    let search_document_provenance_before: usize = tx.query_row(
        "SELECT count(*) FROM search_document_provenance",
        [],
        |row| row.get(0),
    )?;

    // Meetings (#900): the sessions deleted whole and the ones still being recorded, read once
    // before anything is deleted (the overlap check looks at rows the deletion removes).
    let overlapping_sessions = ids_for(
        &tx,
        &format!("SELECT id FROM sessions WHERE {} ORDER BY id", session_overlaps_window()),
        cutoff_ts,
    )?;
    let recording_sessions: Vec<i64> = {
        let mut stmt = tx.prepare(&format!("SELECT id FROM sessions WHERE {SESSION_RECORDING} ORDER BY id"))?;
        let ids = stmt.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        ids
    };

    // Events in the window, plus the rows `meeting_index` wrote for a meeting deleted whole: the
    // full transcript and the note body, dated at the meeting's start (`source = 'meeting'`, the
    // same rows `session::delete_meeting` takes). Those carry what was said inside the window,
    // and the session delete below unlinks them, after which not even deleting the meeting
    // itself would reach them. Going through `delete_canonical_event_in` with the rest takes
    // their full-text and vector entries and their state provenance with them. A meeting still
    // being recorded has no such rows: it is indexed when it closes, from what is left.
    let mut meeting_rows_removed: Vec<i64> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id FROM event_log WHERE ts < ?1 AND source = ?2 AND session_id = ?3 ORDER BY id",
        )?;
        for session_id in &overlapping_sessions {
            let rows = stmt
                .query_map(
                    rusqlite::params![cutoff_ts, crate::meeting_index::SOURCE, session_id],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            meeting_rows_removed.extend(rows);
        }
    }
    let event_ids: Vec<i64> = {
        let mut ids = ids_for(&tx, "SELECT id FROM event_log WHERE ts >= ?1 ORDER BY id", cutoff_ts)?;
        // Disjoint by `ts`, but deduplicated anyway: an event deleted twice would be counted twice.
        ids.extend(&meeting_rows_removed);
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    // A ledger whose input window overlaps forgotten events cannot support a later retry. Delete
    // it before canonical event deletion; fully older windows remain available for their brief.
    tx.execute(
        "DELETE FROM brief_generation WHERE input_to_ts > ?1",
        [cutoff_ts],
    )?;
    tx.execute(
        "DELETE FROM wrap_generation WHERE input_to_ts > ?1",
        [cutoff_ts],
    )?;

    // Activity rows retain their local presentation date and latest UTC completion timestamp. A
    // timezone can change between recording and forgetting, so compare the latter directly to
    // the requested instant instead of today's local-date rendering. Deleting an overlapping
    // daily bucket is conservative, and catches late credits on an old deduplicated event.
    tx.execute(
        "DELETE FROM dashboard_activity_credits WHERE latest_activity_ms >= ?1",
        [cutoff_ts],
    )?;
    tx.execute(
        "DELETE FROM dashboard_edit_counts WHERE latest_activity_ms >= ?1",
        [cutoff_ts],
    )?;

    // Visual-recall frames in the window — and any frame referencing a doomed event (V12:
    // screen_frames.event_id → event_log with no cascade) — must go before the events, or the
    // event delete FK-fails and rolls the whole deletion back.
    let screen_frames = tx.execute(
        "DELETE FROM screen_frames WHERE created_at_ms >= ?1 OR event_id IN (SELECT id FROM event_log WHERE ts >= ?1)",
        [cutoff_ts],
    )?;
    // Mail triage (V33): a thread whose latest snapshot is in the window loses its estimate (the
    // provenance is gone — FR-TG-25); its messages cascade with the event. Corrections, ledger
    // and history rows are deleted by their own timestamps.
    tx.execute(
        "DELETE FROM mail_triage WHERE latest_event_id IN (SELECT id FROM event_log WHERE ts >= ?1) OR classified_at_ms >= ?1",
        [cutoff_ts],
    )?;
    tx.execute("DELETE FROM mail_messages WHERE internal_date_ms >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM mail_triage_feedback WHERE ts_ms >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM mail_triage_batch_items WHERE submitted_at_ms >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM mail_write_log WHERE ts_ms >= ?1", [cutoff_ts])?;
    // The sync watermark must not sit past deleted mail, or the next tick would skip it forever.
    tx.execute(
        "UPDATE connector_sync_cursors SET watermark_ms = min(watermark_ms, ?1), page_token = NULL WHERE watermark_ms >= ?1",
        [cutoff_ts],
    )?;

    // Brief request ledger (V44) first, as `delete_all` does: every deleted event fires the
    // `brief_generation_source_delete` trigger, which walks each surviving row's source manifest.
    // Rows whose window overlaps the cutoff are exactly the ones those events could match, so
    // dropping them here leaves the trigger nothing to scan per event. Rows whose window ended
    // before the cutoff cannot reference an event at or after it.
    tx.execute(
        "DELETE FROM brief_generation WHERE input_to_ts > ?1 OR created_at >= ?1 OR updated_at >= ?1 OR date IN (SELECT date FROM briefs WHERE built_at >= ?1)",
        [cutoff_ts],
    )?;

    // Shared ordering dirties surviving V21 owners and scrubs typed projection provenance before
    // canonical provenance, embeddings, and the event itself disappear.
    let period_summary_event_provenance_before: usize =
        tx.query_row("SELECT count(*) FROM period_summary_event_provenance", [], |row| row.get(0))?;
    let period_summary_child_provenance_before: usize =
        tx.query_row("SELECT count(*) FROM period_summary_child_provenance", [], |row| row.get(0))?;
    let mut events = 0;
    for event_id in &event_ids {
        events += usize::from(crate::search_documents::delete_canonical_event_in(
            &tx, *event_id,
        )?);
    }

    // Period summaries (V22) reaching past the cutoff were written from events that are now gone,
    // and hour text is quoted from them. The event-delete trigger only marks them dirty, and a
    // window left with no events is then skipped, so the text stayed (#766). Remove them (their
    // provenance cascades) and queue their windows again, so whatever survives in a window that
    // straddles the cutoff is summarised afresh.
    //
    // Their search projection goes first. `period_summaries::save` writes it directly, and no
    // foreign key or trigger ties it to its owner, so deleting the summary alone left its text
    // in `search_documents` and the full-text index, where search still found it. Deleting the
    // projection row takes its provenance and index entry with it.
    tx.execute(
        "DELETE FROM search_documents
          WHERE source_table = 'period_summaries'
            AND source_id IN (SELECT CAST(id AS TEXT) FROM period_summaries WHERE period_end_ms > ?1)",
        [cutoff_ts],
    )?;
    // A summary without a run row (written outside the scheduler) still gets its window queued.
    tx.execute(
        "INSERT OR IGNORE INTO period_summary_shard_runs
                (level, period_start_ms, period_end_ms, state, attempts, updated_at_ms)
         SELECT level, period_start_ms, period_end_ms, 'pending', 0,
                CAST(strftime('%s','now') AS INTEGER) * 1000
           FROM period_summaries WHERE period_end_ms > ?1",
        [cutoff_ts],
    )?;
    let period_summaries =
        tx.execute("DELETE FROM period_summaries WHERE period_end_ms > ?1", [cutoff_ts])?;
    tx.execute(
        "UPDATE period_summary_shard_runs SET state = 'pending', failure_class = NULL,
                updated_at_ms = CAST(strftime('%s','now') AS INTEGER) * 1000
          WHERE period_end_ms > ?1",
        [cutoff_ts],
    )?;
    // Threads (V5) are derived from events by `thread_key`, and titles surface in chat ("Which
    // one — …?"). A thread active in the window that has no events left goes; one that still has
    // older events is re-dated from them, and its model summary and participant list, which may
    // carry what was deleted, are cleared for the Dream Cycle to re-derive (#766).
    let threads = tx.execute(
        "DELETE FROM threads WHERE last_activity_at >= ?1
            AND NOT EXISTS (SELECT 1 FROM event_log e WHERE e.thread_key = threads.thread_key)",
        [cutoff_ts],
    )?;
    tx.execute(
        "UPDATE threads SET
                first_activity_at = (SELECT MIN(ts) FROM event_log e WHERE e.thread_key = threads.thread_key),
                last_activity_at = (SELECT MAX(ts) FROM event_log e WHERE e.thread_key = threads.thread_key),
                event_count = (SELECT count(*) FROM event_log e WHERE e.thread_key = threads.thread_key),
                summary = NULL,
                participants = NULL,
                updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
          WHERE last_activity_at >= ?1",
        [cutoff_ts],
    )?;

    // Meeting sessions that overlap the window, and everything that references them, must all go
    // before `DELETE FROM sessions` — with foreign_keys=ON a lingering child is a hard FK error
    // that rolls the whole deletion back. Children of sessions with NO ON DELETE CASCADE:
    // session_notes (V8), transcript_segments (V9), meeting_recaps (V10).
    //
    // A finished meeting overlaps the window when any of it falls at or after the cutoff
    // (`session_overlaps_window`). Such a meeting goes whole (#900), the part before the cutoff
    // included. Keying on the start alone kept a 9:00–11:00 meeting's last hour of transcript,
    // its notes and its recap after "Delete last hour" at 11:00. The recap and the notes cannot
    // be split at a time, so a partial delete would still leave text from the window behind: the
    // same conservative rule as the daily buckets above.
    //
    // A meeting still being recorded is not deleted whole (`SESSION_RECORDING`): the session row
    // stays so the recorder keeps working, and only what falls in the window goes. Its transcript
    // lines spoken at or after the cutoff are deleted. Its note is deleted if it was saved at or
    // after the cutoff: a note is one body typed over time and cannot be split at the cutoff, so
    // the delete errs toward removing it, the choice that keeps the user's privacy promise. It
    // has no recap and no indexed rows yet; both are built from what is left when it closes, so
    // the deleted lines cannot come back through them.
    //
    // The search projections (V21) of the meeting rows deleted here go first. Their delete
    // triggers only queue an invalidation, so the text stayed searchable until the projection
    // worker caught up; deleting the projection row takes its provenance and index entry with
    // it, as for the period summaries above.
    let mut session_notes = 0;
    for session_id in &recording_sessions {
        tx.execute(
            "DELETE FROM search_documents
              WHERE source_table = 'session_notes' AND source_id IN
                    (SELECT CAST(id AS TEXT) FROM session_notes WHERE session_id = ?1 AND updated_at >= ?2)",
            rusqlite::params![session_id, cutoff_ts],
        )?;
        session_notes += tx.execute(
            "DELETE FROM session_notes WHERE session_id = ?1 AND updated_at >= ?2",
            rusqlite::params![session_id, cutoff_ts],
        )?;
    }
    let mut transcript_segments = tx.execute(
        &format!(
            "DELETE FROM transcript_segments
              WHERE ts >= ?1 AND session_id IN (SELECT id FROM sessions WHERE {SESSION_RECORDING})"
        ),
        [cutoff_ts],
    )?;
    let (mut meeting_recaps, mut sessions) = (0, 0);
    for session_id in &overlapping_sessions {
        tx.execute(
            "DELETE FROM search_documents
              WHERE (source_table = 'sessions' AND source_id = CAST(?1 AS TEXT))
                 OR (source_table = 'session_notes' AND source_id IN
                      (SELECT CAST(id AS TEXT) FROM session_notes WHERE session_id = ?1))
                 OR (source_table = 'meeting_recaps' AND source_id IN
                      (SELECT CAST(id AS TEXT) FROM meeting_recaps WHERE session_id = ?1))",
            [session_id],
        )?;
        session_notes += tx.execute("DELETE FROM session_notes WHERE session_id = ?1", [session_id])?;
        transcript_segments +=
            tx.execute("DELETE FROM transcript_segments WHERE session_id = ?1", [session_id])?;
        meeting_recaps += tx.execute("DELETE FROM meeting_recaps WHERE session_id = ?1", [session_id])?;
        // Speaker names (V25): same FK shape as the three above.
        tx.execute("DELETE FROM session_speakers WHERE session_id = ?1", [session_id])?;
        // event_log.session_id (V7) also references sessions with no cascade. The meeting's own
        // spine rows went with the events above; any other event still pointing at it is kept
        // and unlinked, so the session delete cannot FK-fail.
        tx.execute("UPDATE event_log SET session_id = NULL WHERE session_id = ?1", [session_id])?;
        sessions += tx.execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
    }

    // Traceability rows for sends in the window.
    let traceability = tx.execute("DELETE FROM traceability_log WHERE ts >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM shared_service_rtt WHERE ts >= ?1", [cutoff_ts])?;
    // File-export records (V48) in the window; the files themselves are the user's and stay.
    tx.execute("DELETE FROM local_writes WHERE created_at >= ?1", [cutoff_ts])?;
    // Imports committed in the window (V52), items first. An import is one user action, so it
    // goes whole: its created_at is the moment of the commit.
    tx.execute(
        "DELETE FROM memory_import_items WHERE import_id IN (SELECT id FROM memory_imports WHERE created_at >= ?1)",
        [cutoff_ts],
    )?;
    tx.execute("DELETE FROM memory_imports WHERE created_at >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM user_memory_note_versions WHERE note_id IN (SELECT id FROM user_memory_notes WHERE updated_at >= ?1)",[cutoff_ts])?;
    tx.execute("DELETE FROM user_memory_notes WHERE updated_at >= ?1",[cutoff_ts])?;
    // Clarify questions asked in the window (V53), with their answers.
    tx.execute("DELETE FROM clarify_questions WHERE created_at >= ?1", [cutoff_ts])?;

    // The notch chat (V25): a question committed in the window quotes what it referenced, so
    // those transcript rows go with the events. A thread left empty carries no user data —
    // prune it, and chat_state's FK (ON DELETE SET NULL) clears a dangling active id itself.
    tx.execute("DELETE FROM chat_messages WHERE ts >= ?1", [cutoff_ts])?;
    tx.execute(
        "DELETE FROM chat_threads WHERE id NOT IN (SELECT DISTINCT thread_id FROM chat_messages)",
        [],
    )?;

    // L5 learning signals in the window (V16): provenance rows pointing at doomed feedback first
    // (FK), then the feedback rows; a lesson that loses ALL of its evidence goes with them — the
    // same orphan rule as the state tables. Briefs are keyed by build time (V15).
    tx.execute(
        "DELETE FROM lesson_provenance WHERE feedback_event_id IN (SELECT id FROM feedback_events WHERE ts_ms >= ?1)",
        [cutoff_ts],
    )?;
    let feedback_events = tx.execute("DELETE FROM feedback_events WHERE ts_ms >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM writing_voice_examples WHERE ts_ms >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM rewrite_outcomes WHERE created_at >= ?1", [cutoff_ts])?;
    tx.execute("DELETE FROM dictionary_autoadd_corrections WHERE created_at_ms >= ?1", [cutoff_ts])?;
    tx.execute("UPDATE dictionary_autoadd_settings SET generation = generation + 1 WHERE id = 1", [])?;
    tx.execute("DELETE FROM writing_voice_outputs WHERE ts_ms >= ?1", [cutoff_ts])?;
    tx.execute("UPDATE writing_voice_settings SET revision = revision + 1 WHERE id = 1", [])?;
    // The deleted rows are the newest (highest ids), and ids are reused after deletion (no
    // AUTOINCREMENT). Clamp the monotonic distill watermark down to the highest surviving id so
    // re-issued ids are not skipped as already-processed.
    tx.execute(
        "UPDATE lesson_distill_meta
             SET last_processed_feedback_id = MIN(
                 last_processed_feedback_id,
                 COALESCE((SELECT MAX(id) FROM feedback_events), 0))
           WHERE id = 1",
        [],
    )?;
    let lessons = tx.execute(
        "DELETE FROM lessons WHERE id NOT IN (SELECT lesson_id FROM lesson_provenance)",
        [],
    )?;
    // (The Brief request ledger for this window was cleared above, before the event loop.)
    let briefs = tx.execute("DELETE FROM briefs WHERE built_at >= ?1", [cutoff_ts])?;
    // Work Patterns (V27): impressions in the window go by timestamp. The derived profile's
    // counts may reflect just-deleted events, so they are zeroed — but the ROWS survive, because
    // `active = 0` is a user decision ("stop using this pattern") that a data deletion must not
    // silently undo. The next nightly distillation recomputes the numbers from what remains.
    tx.execute("DELETE FROM offer_impressions WHERE ts_ms >= ?1", [cutoff_ts])?;
    tx.execute(
        "UPDATE work_patterns SET count_7d = 0, count_30d = 0, trend = 0.0",
        [],
    )?;

    // Orphan sweep: any state row with no surviving provenance is removed (children first).
    let commitments = sweep_orphans(&tx, "commitments")?;
    let open_loops = sweep_orphans(&tx, "open_loops")?;
    // A SURVIVING commitment/open_loop/thread can still reference a person/project that just
    // lost its last provenance (their evidence lives in different events). Null those links
    // before the parent sweep: with foreign_keys=ON and no ON DELETE clause, the sweep would
    // otherwise FK-fail and roll back the entire deletion.
    tx.execute(
        "UPDATE commitments SET counterparty_id = NULL WHERE counterparty_id IN
             (SELECT id FROM people WHERE id NOT IN
                 (SELECT state_id FROM state_provenance WHERE state_table='people'))",
        [],
    )?;
    tx.execute(
        "UPDATE open_loops SET counterparty_id = NULL WHERE counterparty_id IN
             (SELECT id FROM people WHERE id NOT IN
                 (SELECT state_id FROM state_provenance WHERE state_table='people'))",
        [],
    )?;
    for table in ["commitments", "open_loops", "threads"] {
        tx.execute(
            &format!(
                "UPDATE {table} SET project_id = NULL WHERE project_id IN
                     (SELECT id FROM projects WHERE id NOT IN
                         (SELECT state_id FROM state_provenance WHERE state_table='projects'))"
            ),
            [],
        )?;
    }
    let people = sweep_orphans(&tx, "people")?;
    let projects = sweep_orphans(&tx, "projects")?;

    let search_documents_after: usize = tx.query_row(
        "SELECT count(*) FROM search_documents",
        [],
        |row| row.get(0),
    )?;
    let search_document_provenance_after: usize = tx.query_row(
        "SELECT count(*) FROM search_document_provenance",
        [],
        |row| row.get(0),
    )?;
    let period_summary_event_provenance_after: usize =
        tx.query_row("SELECT count(*) FROM period_summary_event_provenance", [], |row| row.get(0))?;
    let period_summary_child_provenance_after: usize =
        tx.query_row("SELECT count(*) FROM period_summary_child_provenance", [], |row| row.get(0))?;

    tx.commit()?;

    Ok(DeleteReport {
        events,
        people,
        projects,
        commitments,
        open_loops,
        // Threads left with no events; ones that kept older events are re-dated, not counted.
        threads,
        sessions,
        session_notes,
        transcript_segments,
        meeting_recaps,
        screen_frames,
        traceability,
        chat_threads: 0,
        chat_messages: 0,
        briefs,
        feedback_events,
        lessons,
        search_documents: search_documents_before.saturating_sub(search_documents_after),
        search_document_provenance: search_document_provenance_before
            .saturating_sub(search_document_provenance_after),
        search_document_invalidations: 0,
        period_summaries,
        period_summary_event_provenance: period_summary_event_provenance_before
            .saturating_sub(period_summary_event_provenance_after),
        period_summary_child_provenance: period_summary_child_provenance_before
            .saturating_sub(period_summary_child_provenance_after),
        // Queued again for regeneration, not removed.
        period_summary_shard_runs: 0,
        meeting_rows_removed,
    })
}

/// DELETE for one state table's rows that have no remaining provenance row. `None` for a table
/// this sweep does not know — a fifth state table added without touching here should be skipped,
/// not panic the maintenance thread mid-transaction (the caller treats it as "nothing to sweep").
fn orphan_sql(table: &str) -> Option<&'static str> {
    Some(match table {
        "commitments" => "DELETE FROM commitments WHERE id NOT IN (SELECT state_id FROM state_provenance WHERE state_table='commitments')",
        "open_loops" => "DELETE FROM open_loops WHERE id NOT IN (SELECT state_id FROM state_provenance WHERE state_table='open_loops')",
        "people" => "DELETE FROM people WHERE id NOT IN (SELECT state_id FROM state_provenance WHERE state_table='people')",
        "projects" => "DELETE FROM projects WHERE id NOT IN (SELECT state_id FROM state_provenance WHERE state_table='projects')",
        _ => return None,
    })
}

/// Run the orphan sweep for `table`, or 0 when the table is unknown to [`orphan_sql`].
fn sweep_orphans(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<usize, rusqlite::Error> {
    match orphan_sql(table) {
        Some(sql) => tx.execute(sql, []),
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};
    use crate::state::{insert_person, CommitmentDirection, CommitmentStatus, NewCommitment, NewPerson, Provenance};

    fn seed(conn: &mut Connection) {
        let e = insert_event(
            conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Mail"),
                window_title: Some("Inbox"),
                content: "Alice asked for the quarterly report",
                content_hash: "h1",
                dwell_ms: 5,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let alice = insert_person(conn, &NewPerson { display_name: "Alice", confidence: 0.9, now: 1, ..Default::default() }, &[Provenance::new(e)]).unwrap();
        insert_commitment(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: Some(alice),
                description: "send the report",
                due_at: Some(100),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.8,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
    }

    // shadow the crate import path for the test helper
    use crate::state::insert_commitment;

    #[test]
    fn export_includes_events_and_state_with_content() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        let json = export_json(&conn).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], crate::LATEST_SCHEMA_VERSION);
        assert_eq!(v["event_log"].as_array().unwrap().len(), 1);
        assert_eq!(v["event_log"][0]["content"], "Alice asked for the quarterly report");
        assert_eq!(v["people"][0]["display_name"], "Alice");
        assert_eq!(v["commitments"][0]["description"], "send the report");
    }

    #[test]
    fn export_includes_chat_threads_messages_and_citations() {
        let mut conn = crate::open_in_memory().unwrap();
        let store = crate::chat_thread::create(&mut conn, 100).unwrap();
        let thread_id = store.active_thread_id.unwrap();
        crate::chat_thread::append(
            &mut conn,
            thread_id,
            &crate::chat_thread::ChatMessage {
                role: "shogun".into(),
                text: "the deck is overdue".into(),
                citations: Some(r#"[{"event_id":1,"source":"gmail"}]"#.into()),
            },
            101,
        )
        .unwrap();

        let exported: Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();

        assert_eq!(exported["chat_threads"][0]["id"], thread_id);
        assert_eq!(exported["chat_messages"][0]["text"], "the deck is overdue");
        assert_eq!(
            exported["chat_messages"][0]["citations"],
            r#"[{"event_id":1,"source":"gmail"}]"#
        );
    }

    #[test]
    fn export_and_delete_all_include_search_projection_and_typed_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        crate::search_documents::rebuild_all(&mut conn, 10).unwrap();

        let exported: Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();
        assert!(!exported["search_documents"].as_array().unwrap().is_empty());
        assert!(!exported["search_document_provenance"].as_array().unwrap().is_empty());
        assert!(exported["search_document_invalidations"].is_array());
        assert!(exported["search_documents"][0].get("last_failure_class").is_none());

        let report = delete_all(&mut conn).unwrap();
        assert!(report.search_documents > 0);
        assert!(report.search_document_provenance > 0);
        assert!(report.search_document_invalidations > 0);
        for table in [
            "search_documents",
            "search_document_provenance",
            "search_document_invalidations",
            "search_documents_fts",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    #[test]
    fn delete_all_wipes_user_data_but_keeps_schema() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        let report = delete_all(&mut conn).unwrap();
        assert_eq!(report.events, 1);
        assert_eq!(report.people, 1);
        assert_eq!(report.commitments, 1);

        // every table is empty...
        for table in ["event_log", "people", "projects", "commitments", "open_loops", "threads", "state_provenance", "traceability_log"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
        // ...but the schema (and version) survives, so the app keeps working
        assert_eq!(crate::schema_version(&conn).unwrap(), Some(crate::LATEST_SCHEMA_VERSION));
        // a fresh insert still works
        seed(&mut conn);
        let n: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn delete_all_wipes_all_chat_threads_messages_and_active_state() {
        let mut conn = crate::open_in_memory().unwrap();
        let store = crate::chat_thread::create(&mut conn, 100).unwrap();
        crate::chat_thread::append(
            &mut conn,
            store.active_thread_id.unwrap(),
            &crate::chat_thread::ChatMessage {
                role: "me".into(),
                text: "what's overdue?".into(),
                citations: None,
            },
            101,
        )
        .unwrap();

        let report = delete_all(&mut conn).unwrap();

        assert_eq!(report.chat_threads, 1);
        assert_eq!(report.chat_messages, 1);
        assert!(crate::chat_thread::load(&conn).unwrap().threads.is_empty());
        assert_eq!(crate::chat_thread::load(&conn).unwrap().active_thread_id, None);
    }

    #[test]
    fn delete_all_wipes_meeting_sessions() {
        // A session carries the meeting's title, summary and decisions — user data by any
        // reading. "Delete everything" that leaves the record of who met about what would be a
        // privacy failure, not a missed table (FR-SET-07).
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some("Weekly sync"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.6,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::session::close(&conn, id, 2_000).unwrap();

        delete_all(&mut conn).unwrap();

        let n: i64 =
            conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "sessions must not survive delete_all");
    }

    #[test]
    fn delete_all_wipes_the_notes_the_user_typed_in_meetings() {
        // A meeting note is the user writing in their own words — the most personal row in the
        // database. It must not survive "delete everything", and because it references sessions,
        // forgetting it also breaks the delete outright under foreign_keys=ON (FR-SET-07).
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some("1:1"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.6,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::session_notes::save(&conn, id, "salary conversation", 1_200).unwrap();

        delete_all(&mut conn).unwrap();

        let n: i64 =
            conn.query_row("SELECT count(*) FROM session_notes", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "meeting notes must not survive delete_all");
    }

    #[test]
    fn delete_all_survives_a_meeting_with_transcript_and_recap() {
        // transcript_segments and meeting_recaps both hold NOT NULL FKs to sessions with no ON
        // DELETE (see the delete order in `delete_all`).
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some("Weekly sync"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.6,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::transcript_segments::append(
            &conn,
            &crate::transcript_segments::NewSegment {
                session_id: id,
                ts: 1_100,
                speaker: crate::transcript_segments::Speaker::Unknown,
                text: "we agreed on the renewal",
                confidence: 0.9,
            },
            1_100,
        )
        .unwrap();
        crate::meeting_recaps::save(&conn, id, "renewal agreed", "[]", "[]", None, "test", 1_300).unwrap();
        crate::session_speakers::assign(&conn, id, "other:0", "Tanaka", "user", 1_400).unwrap();
        crate::session::close(&conn, id, 2_000).unwrap();

        let report = delete_all(&mut conn).expect("delete_all must not trip the session FKs");
        assert_eq!(report.sessions, 1);
        assert_eq!(report.transcript_segments, 1);
        assert_eq!(report.meeting_recaps, 1);
        for table in [
            "sessions",
            "transcript_segments",
            "meeting_recaps",
            "session_notes",
            "session_speakers",
        ] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
    }

    #[test]
    fn delete_all_survives_a_visual_recall_frame() {
        // screen_frames.event_id → event_log with no ON DELETE (V12): see the delete order in
        // `delete_all`.
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        let event_id: i64 = conn.query_row("SELECT id FROM event_log LIMIT 1", [], |r| r.get(0)).unwrap();
        crate::screen_frames::insert(
            &conn,
            &crate::screen_frames::NewFrame {
                created_at_ms: 2,
                event_id: Some(event_id),
                accessibility_event_id: None,
                app_bundle_id: Some("com.apple.Mail"),
                window_title: Some("Inbox"),
                display_id: None,
                width: 100,
                height: 100,
                ocr_region: crate::screen_frames::OcrRegion::WholeFrame,
                jpeg: b"\xff\xd8fake",
            },
        )
        .unwrap();

        let report = delete_all(&mut conn).expect("delete_all must not trip the frame FK");
        assert_eq!(report.screen_frames, 1);
        for table in ["screen_frames", "event_log"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
    }

    #[test]
    fn delete_all_and_delete_since_cover_the_mail_triage_tables() {
        use crate::mail_triage::{self as mt, Attention, Bucket, MailMessage, Triage};
        let mut conn = crate::open_in_memory().unwrap();
        let mk = |c: &Connection, tid: &str, ts: i64| {
            let ev = NewEvent {
                ts,
                source: "gmail",
                kind: "email",
                app_bundle_id: None,
                window_title: Some("Offer"),
                content: &format!("shop {tid}"),
                content_hash: &crate::event_log::content_hash(&format!("shop {tid}")),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            };
            let id = crate::event_log::insert_with_thread(c, &ev, Some(tid)).unwrap();
            mt::upsert_messages(
                c,
                &[MailMessage {
                    message_id: format!("m-{tid}"),
                    thread_key: format!("gmail:{tid}"),
                    event_id: id,
                    from_addr: "deals@shop.example".into(),
                    from_name: None,
                    from_domain: "shop.example".into(),
                    to_addrs: vec![],
                    cc_addrs: vec![],
                    is_from_me: false,
                    in_reply_to: None,
                    gmail_label_ids: vec!["INBOX".into()],
                    has_list_unsubscribe: true,
                    has_ics: false,
                    internal_date_ms: ts,
                }],
                ts,
            )
            .unwrap();
            let t = Triage { label_key: "marketing".into(), bucket: Bucket::Other, attention: Attention::None, confidence: 0.9, reasons: vec![], bucket_lock: None };
            mt::upsert_local(c, &format!("gmail:{tid}"), id, &t, ts).unwrap();
        };
        mk(&conn, "old", 1_000);
        mk(&conn, "new", 5_000);
        mt::set_cursor(&conn, "gmail", &mt::SyncCursor { watermark_ms: 6_000, page_token: Some("p".into()) }, 6_000).unwrap();
        let exported = export_json(&conn).unwrap();
        let v: serde_json::Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(v["mail_triage"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["mail_messages"][0]["from_addr"], "deals@shop.example");
        assert!(v["mail_messages"][0].get("content").is_none(), "message rows carry metadata only");
        assert_eq!(v["mail_labels"].as_array().map(Vec::len), Some(9));

        delete_since(&mut conn, 3_000).unwrap();
        assert!(mt::get_triage(&conn, "gmail:new").unwrap().is_none());
        assert!(mt::get_triage(&conn, "gmail:old").unwrap().is_some());
        assert_eq!(mt::messages_for_thread(&conn, "gmail:new").unwrap().len(), 0);
        let cursor = mt::get_cursor(&conn, "gmail").unwrap().unwrap();
        assert_eq!((cursor.watermark_ms, cursor.page_token), (3_000, None));

        delete_all(&mut conn).unwrap();
        assert!(mt::get_triage(&conn, "gmail:old").unwrap().is_none());
        assert!(mt::identities(&conn).unwrap().is_empty());
        assert_eq!(mt::list_labels(&conn).unwrap().len(), 9, "the label registry is settings, not data");
    }

    #[test]
    fn export_delete_all_and_delete_since_cover_memory_imports() {
        use crate::memory_import::{commit, parse_request, Via};
        let mut conn = crate::open_in_memory().unwrap();
        let body = |name: &str| {
            format!(
                r#"{{"package":{{"format":"shogun-memory-import/v1","source":{{"platform":"other"}},
                   "open_loops":[{{"summary":"{name}","status":"open","confidence":"low","evidence":null}}]}},"select":"default"}}"#
            )
        };
        commit(&mut conn, &parse_request(&body("old loop")).unwrap(), Via::Ui, 1_000).unwrap();
        commit(&mut conn, &parse_request(&body("new loop")).unwrap(), Via::Ui, 5_000).unwrap();
        let v: serde_json::Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();
        assert_eq!(v["memory_imports"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["memory_import_items"][1]["name"], "new loop");
        assert_eq!(v["memory_imports"][0]["provenance"], "imported:other");

        delete_since(&mut conn, 3_000).unwrap();
        let names: Vec<String> = {
            let mut s = conn.prepare("SELECT name FROM memory_import_items").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert_eq!(names, ["old loop"]);
        delete_all(&mut conn).unwrap();
        let left: i64 = conn
            .query_row("SELECT (SELECT count(*) FROM memory_imports) + (SELECT count(*) FROM memory_import_items)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn delete_all_and_delete_since_cover_clarify_questions() {
        use crate::clarify_questions::{ask, record_answer};
        let mut conn = crate::open_in_memory().unwrap();
        let old = ask(&conn, "skill_run", 1, "Which Sam?", &[], true, 1_000, 900_000).unwrap();
        record_answer(&conn, old.id, "Sam Lee", None, "ui", 2_000).unwrap();
        ask(&conn, "skill_run", 2, "Which folder?", &[], true, 5_000, 900_000).unwrap();
        let count = |conn: &Connection| -> i64 {
            conn.query_row("SELECT count(*) FROM clarify_questions", [], |r| r.get(0)).unwrap()
        };
        delete_since(&mut conn, 3_000).unwrap();
        assert_eq!(count(&conn), 1, "only the question asked in the window goes");
        delete_all(&mut conn).unwrap();
        assert_eq!(count(&conn), 0);
    }

    #[test]
    fn delete_all_rewinds_the_lesson_distill_watermark() {
        // feedback ids restart after a wipe (no AUTOINCREMENT); a surviving watermark would sit
        // above every future id and lesson learning would silently never run again.
        let mut conn = crate::open_in_memory().unwrap();
        crate::lessons::set_distill_watermark(&conn, 100).unwrap();
        delete_all(&mut conn).unwrap();
        assert_eq!(crate::lessons::distill_watermark(&conn).unwrap(), 0);
    }

    #[test]
    fn delete_since_leaves_no_text_from_the_window_in_summaries_or_threads() {
        // #766: "Delete data › Last hour" removed the events, but the hour summary quoting them
        // and the thread titled after them stayed, and chat could still name the thread.
        const HOUR: i64 = 3_600_000;
        let mut conn = crate::open_in_memory().unwrap();
        let event = |conn: &Connection, ts: i64, content: &str, hash: &str, key: &str| {
            let id = insert_event(
                conn,
                &NewEvent {
                    ts,
                    source: "capture",
                    kind: "text",
                    app_bundle_id: None,
                    window_title: None,
                    content,
                    content_hash: hash,
                    dwell_ms: 5,
                    display_id: None,
                    window_bounds: None,
                },
            )
            .unwrap();
            conn.execute("UPDATE event_log SET thread_key = ?1 WHERE id = ?2", rusqlite::params![key, id]).unwrap();
            id
        };
        let lunch = event(&conn, 1_000, "Lunch moved to Friday.", "lunch", "capture:lunch");
        let plan_old = event(&conn, 1_500, "Planning notes for the launch.", "plan-old", "capture:plan");
        let offer = event(&conn, 2 * HOUR + 10, "Offer letter for Dana: base 185k.", "offer", "capture:offer");
        event(&conn, 2 * HOUR + 20, "Dana's direct line is 555-0100.", "plan-new", "capture:plan");
        for (key, title, first, last, summary) in [
            ("capture:lunch", "Lunch", 1_000, 1_000, None),
            ("capture:offer", "Offer letter — Dana — Gmail", 2 * HOUR + 10, 2 * HOUR + 10, None),
            ("capture:plan", "Launch plan", 1_500, 2 * HOUR + 20, Some("Dana's direct line is 555-0100")),
        ] {
            conn.execute(
                "INSERT INTO threads (thread_key, source, title, summary, participants, first_activity_at,
                                      last_activity_at, event_count, confidence, created_at, updated_at)
                 VALUES (?1, 'capture', ?2, ?3, '[1]', ?4, ?5, 2, 0.8, ?4, ?5)",
                rusqlite::params![key, title, summary, first, last],
            )
            .unwrap();
        }
        let hour_summary = |conn: &mut Connection, start: i64, content: &str, events: &[i64]| {
            crate::period_summaries::save(
                conn,
                &crate::period_summaries::NewPeriodSummary {
                    level: crate::period_summaries::PeriodLevel::Hour,
                    period_start_ms: start,
                    period_end_ms: start + HOUR,
                    title: "hour",
                    content,
                    generated: false,
                    generation_model: None,
                    event_ids: events,
                    child_summary_ids: &[],
                    now_ms: 3 * HOUR,
                },
            )
            .unwrap()
        };
        let first_hour =
            hour_summary(&mut conn, 0, "Lunch moved to Friday. Planning notes for the launch.", &[lunch, plan_old]);
        let third_hour = hour_summary(&mut conn, 2 * HOUR, "Offer letter for Dana: base 185k.", &[offer]);
        // A day summary citing both hours, written without a scheduler run row.
        crate::period_summaries::save(
            &mut conn,
            &crate::period_summaries::NewPeriodSummary {
                level: crate::period_summaries::PeriodLevel::Day,
                period_start_ms: 0,
                period_end_ms: 24 * HOUR,
                title: "day",
                content: "Lunch moved. Offer letter for Dana: base 185k.",
                generated: false,
                generation_model: None,
                event_ids: &[lunch, offer],
                child_summary_ids: &[first_hour, third_hour],
                now_ms: 3 * HOUR,
            },
        )
        .unwrap();
        let searchable = |conn: &Connection| -> (i64, i64) {
            conn.query_row(
                "SELECT (SELECT count(*) FROM search_documents
                          WHERE source_table = 'period_summaries' AND body LIKE '%Dana%'),
                        (SELECT count(*) FROM search_documents_fts
                          WHERE search_documents_fts MATCH 'Dana' AND document_kind = 'period_summary')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(searchable(&conn), (2, 2), "the hour and the day are searchable before");

        let report = delete_since(&mut conn, 2 * HOUR).unwrap();

        // The summaries' search projection and full-text entries go with them: deleted text that
        // search can still find is not deleted.
        assert_eq!(searchable(&conn), (0, 0));
        let orphaned: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_documents d
                  WHERE d.source_table = 'period_summaries'
                    AND NOT EXISTS (SELECT 1 FROM period_summaries p WHERE CAST(p.id AS TEXT) = d.source_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, 0, "no projection row outlives its summary");
        let day_queued: i64 = conn
            .query_row(
                "SELECT count(*) FROM period_summary_shard_runs
                  WHERE level = 'day' AND period_start_ms = 0 AND state = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(day_queued, 1, "a summary with no run row still has its window queued");

        let dana_text: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM period_summaries WHERE content LIKE '%Dana%')
                      + (SELECT count(*) FROM threads
                          WHERE title LIKE '%Dana%' OR COALESCE(summary, '') LIKE '%Dana%')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dana_text, 0, "no text from the deleted window survives");
        assert_eq!(report.period_summaries, 2, "the hour in the window and the day that cites it");
        assert_eq!(report.threads, 1, "the thread with no events left is gone");
        let kept: Vec<String> = conn
            .prepare("SELECT content FROM period_summaries ORDER BY period_start_ms")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(kept, vec!["Lunch moved to Friday. Planning notes for the launch.".to_string()]);
        let plan: (i64, i64, i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT first_activity_at, last_activity_at, event_count, summary, participants
                   FROM threads WHERE thread_key = 'capture:plan'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(plan, (1_500, 1_500, 1, None, None), "re-dated from what survives, derived text cleared");
        let lunch_last: i64 = conn
            .query_row("SELECT last_activity_at FROM threads WHERE thread_key = 'capture:lunch'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lunch_last, 1_000, "a thread outside the window is untouched");
    }

    #[test]
    fn delete_since_survives_a_surviving_commitment_referencing_an_orphaned_person() {
        // Alice's only evidence is a RECENT event; the commitment's evidence is an OLD event and
        // it references Alice. delete_since(recent) orphans Alice while the commitment survives —
        // the people sweep must not FK-fail on that link and roll back the entire deletion.
        let mut conn = crate::open_in_memory().unwrap();
        let old_event = insert_event(
            &conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "old evidence",
                content_hash: "old1",
                dwell_ms: 5,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let recent_event = insert_event(
            &conn,
            &NewEvent {
                ts: 1_000,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "recent evidence",
                content_hash: "new1",
                dwell_ms: 5,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let alice = insert_person(
            &mut conn,
            &NewPerson { display_name: "Alice", confidence: 0.9, now: 1_000, ..Default::default() },
            &[Provenance::new(recent_event)],
        )
        .unwrap();
        insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: Some(alice),
                description: "send Alice the report",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.8,
                now: 1,
            },
            &[Provenance::new(old_event)],
        )
        .unwrap();

        let report = delete_since(&mut conn, 500).expect("delete_since must not trip the people FK");
        assert_eq!(report.events, 1, "only the recent event goes");
        assert_eq!(report.people, 1, "Alice lost all her evidence");
        // The surviving commitment is kept, with its dangling link cleared.
        let (n, counterparty): (i64, Option<i64>) = conn
            .query_row("SELECT count(*), counterparty_id FROM commitments", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(counterparty, None);
    }

    #[test]
    fn delete_since_clamps_the_lesson_distill_watermark_to_surviving_ids() {
        use crate::lessons::{record_feedback, FeedbackKind, LessonScope, NewFeedback};
        let mut conn = crate::open_in_memory().unwrap();
        let old = record_feedback(
            &conn,
            FeedbackKind::Reject,
            LessonScope::Global,
            &NewFeedback { ts_ms: 10, ..Default::default() },
        )
        .unwrap();
        let newest = record_feedback(
            &conn,
            FeedbackKind::Reject,
            LessonScope::Global,
            &NewFeedback { ts_ms: 1_000, ..Default::default() },
        )
        .unwrap();
        crate::lessons::set_distill_watermark(&conn, newest).unwrap();

        delete_since(&mut conn, 500).unwrap();

        // The newest (highest-id) feedback died; its id will be reused. The watermark must fall
        // back to the highest surviving id so the reissued id is not skipped.
        assert_eq!(crate::lessons::distill_watermark(&conn).unwrap(), old);
    }

    #[test]
    fn export_covers_meetings_notes_transcripts_and_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        let id = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some("1:1"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.6,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::session_notes::save(&conn, id, "my own words", 1_200).unwrap();
        crate::transcript_segments::append(
            &conn,
            &crate::transcript_segments::NewSegment {
                session_id: id,
                ts: 1_100,
                speaker: crate::transcript_segments::Speaker::Unknown,
                text: "spoken words",
                confidence: 0.9,
            },
            1_100,
        )
        .unwrap();

        crate::session_speakers::assign(&conn, id, "other:0", "Tanaka", "user", 1_150).unwrap();

        let v: Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(v["session_notes"][0]["body"], "my own words");
        assert_eq!(v["transcript_segments"][0]["text"], "spoken words");
        assert_eq!(v["session_speakers"][0]["display_name"], "Tanaka");
        assert!(!v["state_provenance"].as_array().unwrap().is_empty(), "provenance links must export");
        assert!(v["traceability_log"].as_array().unwrap().is_empty());
    }

    #[test]
    fn work_patterns_and_impressions_are_exported_and_wiped() {
        let mut conn = crate::open_in_memory().unwrap();
        crate::work_patterns::record_impressions(&conn, 1_000, "notch", None, &[("open_app", Some(0))])
            .unwrap();
        conn.execute(
            "INSERT INTO work_patterns
               (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence, active,
                created_at, updated_at, last_seen_at)
             VALUES ('activity', 'translate_ja_en', 'global', '', 9, 20, 0.4, 0.6, 0, 1, 1, 1)",
            [],
        )
        .unwrap();

        let v: Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();
        assert_eq!(v["work_patterns"][0]["key"], "translate_ja_en");
        assert_eq!(v["offer_impressions"][0]["action_kind"], "open_app");

        // delete_since keeps the rows (active=0 is a user decision) but zeroes the counts.
        delete_since(&mut conn, 500).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM offer_impressions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        let (count, active): (i64, i64) = conn
            .query_row("SELECT count_7d, active FROM work_patterns", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(count, 0, "derived counts zeroed");
        assert_eq!(active, 0, "the user's OFF switch survives a data deletion");

        // delete_all removes everything, switches included.
        delete_all(&mut conn).unwrap();
        for table in ["work_patterns", "offer_impressions"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
    }

    #[test]
    fn export_after_delete_is_empty_collections() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        delete_all(&mut conn).unwrap();
        let v: Value = serde_json::from_str(&export_json(&conn).unwrap()).unwrap();
        assert!(v["event_log"].as_array().unwrap().is_empty());
        assert!(v["people"].as_array().unwrap().is_empty());
    }

    #[test]
    fn delete_since_removes_recent_events_and_keeps_older_ones() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = insert_event(&conn, &NewEvent { ts: 1_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: None, content: "old note", content_hash: "old",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        let recent = insert_event(&conn, &NewEvent { ts: 9_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: None, content: "recent note", content_hash: "new",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();

        let report = delete_since(&mut conn, 5_000).unwrap();
        assert_eq!(report.events, 1, "only the ts>=5000 event is deleted");

        let remaining: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM event_log ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(remaining, vec![old], "old survives, recent gone");
        let _ = recent;
    }

    #[test]
    fn delete_since_drops_orphaned_state_but_keeps_still_supported_state() {
        let mut conn = crate::open_in_memory().unwrap();
        // A person supported by BOTH an old and a recent event survives; a commitment supported
        // ONLY by the recent event is orphaned and removed.
        let old = insert_event(&conn, &NewEvent { ts: 1_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: None, content: "met Alice", content_hash: "e-old",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        let recent = insert_event(&conn, &NewEvent { ts: 9_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: None, content: "Alice asked X", content_hash: "e-new",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        let alice = insert_person(&mut conn, &NewPerson { display_name: "Alice", confidence: 0.9, now: 1, ..Default::default() },
            &[Provenance::new(old), Provenance::new(recent)]).unwrap();
        insert_commitment(&mut conn, &NewCommitment { direction: CommitmentDirection::Mine,
            counterparty_id: Some(alice), description: "do X", due_at: None,
            status: CommitmentStatus::Open, project_id: None, confidence: 0.8, now: 1 },
            &[Provenance::new(recent)]).unwrap();

        delete_since(&mut conn, 5_000).unwrap();

        let people: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        let commitments: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(people, 1, "Alice keeps her old evidence, survives");
        assert_eq!(commitments, 0, "commitment lost all evidence, removed");
        // provenance pointing at the deleted event is gone; the old one remains.
        let prov: i64 = conn.query_row("SELECT count(*) FROM state_provenance", [], |r| r.get(0)).unwrap();
        assert_eq!(prov, 1, "only the old-event provenance row survives");
    }

    #[test]
    fn delete_since_deletes_session_children_before_the_session_so_it_does_not_fk_fail() {
        // With foreign_keys=ON, a transcript segment or recap left behind when its session is
        // deleted is a hard FK error that rolls the whole deletion back. Any user who recorded a
        // meeting would then be unable to delete their recent data at all — the whole point.
        let mut conn = crate::open_in_memory().unwrap();
        let sid = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 9_000,
                title: Some("Recorded sync"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.6,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::transcript_segments::append(
            &conn,
            &crate::transcript_segments::NewSegment {
                session_id: sid,
                ts: 9_100,
                speaker: crate::transcript_segments::Speaker::Me,
                text: "hello team",
                confidence: 0.9,
            },
            9_100,
        )
        .unwrap();
        crate::meeting_recaps::save(&conn, sid, "we agreed X", "[]", "[]", None, "test-model", 9_200)
            .unwrap();
        // Finished: a meeting still being recorded is trimmed, not deleted whole (#900).
        crate::session::close(&conn, sid, 9_300).unwrap();

        let report = delete_since(&mut conn, 5_000);
        assert!(report.is_ok(), "delete must not FK-fail with a recorded meeting: {report:?}");

        for table in ["sessions", "transcript_segments", "meeting_recaps"] {
            let n: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} must be empty after delete_since");
        }
    }

    /// #900: a finished meeting is deleted when any of it falls in the window, not only when it
    /// started there, together with the full-transcript and note rows `meeting_index` wrote for
    /// it (dated at its start). One that ended before the cutoff, or exactly at it, is kept. A
    /// meeting still being recorded keeps its session and loses only what falls in the window
    /// (see `delete_since_trims_a_meeting_still_being_recorded`).
    #[test]
    fn delete_since_deletes_a_meeting_that_overlaps_the_window() {
        let mut conn = crate::open_in_memory().unwrap();
        // `word` is what was said: unique per meeting, so search can tell which survived.
        let meeting = |conn: &mut Connection, word: &str, started_at: i64, line_at: i64, ended_at: Option<i64>| -> i64 {
            let sid = crate::session::open(
                conn,
                &crate::session::NewSession {
                    kind: "meeting",
                    started_at,
                    title: Some("Weekly sync"),
                    app_bundle_id: Some("us.zoom.xos"),
                    calendar_occurrence_id: None,
                    confidence: 0.6,
                    provenance: "{}",
                },
            )
            .unwrap();
            let text = format!("we discussed {word} at length");
            crate::transcript_segments::append(
                conn,
                &crate::transcript_segments::NewSegment {
                    session_id: sid,
                    ts: line_at,
                    speaker: crate::transcript_segments::Speaker::Me,
                    text: &text,
                    confidence: 0.9,
                },
                line_at,
            )
            .unwrap();
            crate::session_notes::save(conn, sid, &format!("note about {word}"), started_at + 20).unwrap();
            if let Some(ended_at) = ended_at {
                crate::session::close(conn, sid, ended_at).unwrap();
                crate::meeting_recaps::save(conn, sid, "we agreed X", "[]", "[]", None, "test-model", ended_at)
                    .unwrap();
                crate::meeting_index::index_session(conn, sid).unwrap();
            }
            sid
        };
        let spine = |conn: &Connection, sid: i64| -> Vec<i64> {
            let mut stmt = conn
                .prepare("SELECT id FROM event_log WHERE source = ?1 AND session_id = ?2")
                .unwrap();
            stmt.query_map(rusqlite::params![crate::meeting_index::SOURCE, sid], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let found = |conn: &Connection, word: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM event_fts WHERE event_fts MATCH ?1",
                [format!("\"{word}\"")],
                |r| r.get(0),
            )
            .unwrap()
        };

        const CUTOFF: i64 = 10_000;
        let ran_across = meeting(&mut conn, "albatross", 9_000, 10_500, Some(11_000));
        // Closed by crash recovery: ended_at = started_at, but it was still talking after the cutoff.
        let crash_closed = meeting(&mut conn, "bandicoot", 8_500, 10_200, Some(8_500));
        let recording = meeting(&mut conn, "cassowary", 9_500, 10_100, None);
        let ended_before = meeting(&mut conn, "dugong", 6_000, 6_100, Some(7_000));
        let ended_at_cutoff = meeting(&mut conn, "echidna", 7_500, 7_600, Some(CUTOFF));
        let doomed_spine: Vec<i64> = [ran_across, crash_closed]
            .iter()
            .flat_map(|sid| spine(&conn, *sid))
            .collect();
        assert_eq!(doomed_spine.len(), 4, "a transcript and a note row per indexed meeting");
        assert_eq!(found(&conn, "albatross"), 2);
        crate::search_documents::rebuild_all(&mut conn, 10).unwrap();
        let projected = |conn: &Connection, sid: i64| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM search_documents
                  WHERE (source_table = 'sessions' AND source_id = CAST(?1 AS TEXT))
                     OR (source_table = 'session_notes' AND source_id IN
                          (SELECT CAST(id AS TEXT) FROM session_notes WHERE session_id = ?1))
                     OR (source_table = 'meeting_recaps' AND source_id IN
                          (SELECT CAST(id AS TEXT) FROM meeting_recaps WHERE session_id = ?1))",
                [sid],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(projected(&conn, ran_across) > 0, "the recap and the note are searchable first");

        let report = delete_since(&mut conn, CUTOFF).unwrap();
        assert_eq!(report.sessions, 2);
        // The daemon is told which rows dated before the cutoff went, for its recent events.
        let mut removed = report.meeting_rows_removed.clone();
        removed.sort_unstable();
        let mut doomed_sorted = doomed_spine.clone();
        doomed_sorted.sort_unstable();
        assert_eq!(removed, doomed_sorted);

        let left = |table: &str, column: &str| -> Vec<i64> {
            let mut stmt = conn
                .prepare(&format!("SELECT {column} FROM {table} ORDER BY {column}"))
                .unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        let kept = vec![recording, ended_before, ended_at_cutoff];
        assert_eq!(left("sessions", "id"), kept);
        // The recording meeting's only line was spoken in the window; its note was saved before.
        assert_eq!(left("transcript_segments", "session_id"), vec![ended_before, ended_at_cutoff]);
        assert_eq!(left("session_notes", "session_id"), kept);
        assert_eq!(left("meeting_recaps", "session_id"), vec![ended_before, ended_at_cutoff]);
        for gone in [ran_across, crash_closed] {
            let documents: i64 = conn
                .query_row(
                    "SELECT count(*) FROM search_documents
                      WHERE source_table = 'sessions' AND source_id = CAST(?1 AS TEXT)",
                    [gone],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(documents, 0, "meeting {gone} is still searchable by its summary");
        }
        let recap_documents: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_documents d
                  WHERE d.source_table IN ('meeting_recaps', 'session_notes')
                    AND NOT EXISTS (SELECT 1 FROM meeting_recaps r
                                     WHERE d.source_table = 'meeting_recaps' AND CAST(r.id AS TEXT) = d.source_id)
                    AND NOT EXISTS (SELECT 1 FROM session_notes n
                                     WHERE d.source_table = 'session_notes' AND CAST(n.id AS TEXT) = d.source_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recap_documents, 0, "a deleted recap or note is still searchable");

        // The full-transcript and note rows of the deleted meetings are gone, not just unlinked.
        for event_id in &doomed_spine {
            let n: i64 = conn
                .query_row("SELECT count(*) FROM event_log WHERE id = ?1", [event_id], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "meeting row {event_id} survived the delete");
        }
        assert_eq!(found(&conn, "albatross"), 0, "what was said in the window is not searchable");
        assert_eq!(found(&conn, "bandicoot"), 0);
        for gone in [ran_across, crash_closed] {
            let documents: i64 = conn
                .query_row(
                    "SELECT count(*) FROM search_document_provenance WHERE session_id = ?1",
                    [gone],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(documents, 0, "no search evidence still points at meeting {gone}");
        }
        // The kept meetings still have their own rows.
        assert_eq!(spine(&conn, ended_before).len(), 2);
        assert_eq!(found(&conn, "dugong"), 2);
    }

    /// #900, owner decision: a meeting still being recorded is not deleted whole, or the running
    /// recorder's next line, its close and its recap would fail on the foreign key while the notch
    /// still showed "recording". It keeps its session and loses only what falls in the window:
    /// the lines spoken then, and a note saved then (a note cannot be split at the cutoff, so it
    /// goes whole). Afterwards the recorder carries on, and the meeting is indexed at its close
    /// from what is left.
    #[test]
    fn delete_since_trims_a_meeting_still_being_recorded() {
        use crate::transcript_segments::{append, NewSegment, Speaker};
        const CUTOFF: i64 = 10_000;
        let mut conn = crate::open_in_memory().unwrap();
        let open = |conn: &Connection, started_at: i64| -> i64 {
            crate::session::open(
                conn,
                &crate::session::NewSession {
                    kind: "meeting",
                    started_at,
                    title: Some("Live sync"),
                    app_bundle_id: Some("us.zoom.xos"),
                    calendar_occurrence_id: None,
                    confidence: 0.6,
                    provenance: "{}",
                },
            )
            .unwrap()
        };
        let say = |conn: &Connection, sid: i64, ts: i64, word: &str| {
            let text = format!("we discussed {word} at length");
            append(
                conn,
                &NewSegment { session_id: sid, ts, speaker: Speaker::Me, text: &text, confidence: 0.9 },
                ts,
            )
            .unwrap();
        };
        let found = |conn: &Connection, word: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM event_fts WHERE event_fts MATCH ?1",
                [format!("\"{word}\"")],
                |r| r.get(0),
            )
            .unwrap()
        };
        let lines = |conn: &Connection, sid: i64| -> Vec<i64> {
            let mut stmt = conn
                .prepare("SELECT ts FROM transcript_segments WHERE session_id = ?1 ORDER BY ts")
                .unwrap();
            stmt.query_map([sid], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        let notes = |conn: &Connection, sid: i64| -> i64 {
            conn.query_row("SELECT count(*) FROM session_notes WHERE session_id = ?1", [sid], |r| r.get(0))
                .unwrap()
        };

        // Started before the cutoff, talking on into the window; the note was last saved in it.
        let edited_in_window = open(&conn, 9_000);
        say(&conn, edited_in_window, 9_100, "albatross");
        say(&conn, edited_in_window, 10_500, "aardwolf");
        crate::session_notes::save(&conn, edited_in_window, "note about albatross", 9_200).unwrap();
        crate::session_notes::save(&conn, edited_in_window, "note about aardwolf", 10_600).unwrap();
        // Started before the cutoff, its note last saved before it.
        let noted_before = open(&conn, 9_500);
        say(&conn, noted_before, 9_600, "bandicoot");
        say(&conn, noted_before, 10_100, "bonobo");
        crate::session_notes::save(&conn, noted_before, "note about bandicoot", 9_700).unwrap();
        // Started in the window and still recording: emptied, but its session stays too.
        let started_in_window = open(&conn, 10_200);
        say(&conn, started_in_window, 10_300, "cassowary");
        crate::session_notes::save(&conn, started_in_window, "note about cassowary", 10_400).unwrap();

        let report = delete_since(&mut conn, CUTOFF).unwrap();
        assert_eq!(report.sessions, 0, "no session still being recorded is deleted");
        assert_eq!(report.transcript_segments, 3);
        assert_eq!(report.session_notes, 2);
        assert!(report.meeting_rows_removed.is_empty(), "nothing is indexed while recording");
        let orphan_notes: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_documents d
                  WHERE d.source_table = 'session_notes'
                    AND NOT EXISTS (SELECT 1 FROM session_notes n WHERE CAST(n.id AS TEXT) = d.source_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_notes, 0, "a deleted note is still searchable");

        let open_sessions: i64 = conn
            .query_row("SELECT count(*) FROM sessions WHERE ended_at IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(open_sessions, 3, "every recording keeps its session");
        assert_eq!(lines(&conn, edited_in_window), vec![9_100]);
        assert_eq!(lines(&conn, noted_before), vec![9_600]);
        assert!(lines(&conn, started_in_window).is_empty());
        assert_eq!(notes(&conn, edited_in_window), 0, "a note saved in the window goes whole");
        assert_eq!(notes(&conn, noted_before), 1);
        assert_eq!(notes(&conn, started_in_window), 0);

        // The recorder carries on: the next line, the close and the recap all land.
        say(&conn, edited_in_window, 11_100, "anteater");
        crate::session::close(&conn, edited_in_window, 11_200).unwrap();
        crate::meeting_recaps::save(&conn, edited_in_window, "we agreed X", "[]", "[]", None, "test-model", 11_300)
            .unwrap();
        crate::meeting_index::index_session(&mut conn, edited_in_window).unwrap();
        assert_eq!(lines(&conn, edited_in_window), vec![9_100, 11_100]);
        assert_eq!(found(&conn, "albatross"), 1, "the line before the window is indexed at close");
        assert_eq!(found(&conn, "anteater"), 1, "and so is the line after the delete");
        assert_eq!(found(&conn, "aardwolf"), 0, "what was said in the window does not come back");
    }

    #[test]
    fn delete_since_deletes_the_event_exactly_at_the_cutoff() {
        // The docstring promises "at or after" (>=). An event whose ts equals the cutoff MUST go,
        // so a future refactor to `>` is caught here.
        let mut conn = crate::open_in_memory().unwrap();
        insert_event(&conn, &NewEvent { ts: 5_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: None, content: "boundary note", content_hash: "b",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        let report = delete_since(&mut conn, 5_000).unwrap();
        assert_eq!(report.events, 1, "ts == cutoff must be deleted (>=, not >)");
        let n: i64 = conn.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn delete_since_keeps_the_schema_and_fts_in_sync() {
        let mut conn = crate::open_in_memory().unwrap();
        insert_event(&conn, &NewEvent { ts: 9_000, source: "capture", kind: "text",
            app_bundle_id: None, window_title: Some("Inbox"), content: "secret meeting", content_hash: "h",
            dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        delete_since(&mut conn, 5_000).unwrap();
        // FTS mirror must not still return the deleted row.
        let hits: i64 = conn.query_row(
            "SELECT count(*) FROM event_fts WHERE event_fts MATCH 'secret'", [], |r| r.get(0)).unwrap();
        assert_eq!(hits, 0, "AD trigger cleared the FTS row");
    }

    #[test]
    fn reclaim_space_returns_freed_pages_and_is_safe_on_any_auto_vacuum_mode() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        // In-memory databases have no auto_vacuum; the pass must still succeed and report.
        let report = reclaim_space(&conn, 64).unwrap();
        assert!(report.freelist_after <= report.freelist_before);

        // A file database created by `open` is INCREMENTAL, so pages actually come back.
        let path = std::env::temp_dir().join(format!("shogun_reclaim_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = crate::open(&path).unwrap();
            let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
            assert_eq!(mode, 2, "new databases must be auto_vacuum = INCREMENTAL");
            for i in 0..400 {
                insert_event(
                    &conn,
                    &NewEvent {
                        ts: i,
                        source: "capture",
                        kind: "text",
                        app_bundle_id: None,
                        window_title: None,
                        content: &"x".repeat(4_000),
                        content_hash: &format!("h{i}"),
                        dwell_ms: 0,
                        display_id: None,
                        window_bounds: None,
                    },
                )
                .unwrap();
            }
            conn.execute("DELETE FROM event_log", []).unwrap();
            let before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap();
            assert!(before > 0, "a bulk delete must leave free pages to reclaim");
            let report = reclaim_space(&conn, 100_000).unwrap();
            assert!(report.freelist_before >= before, "{report:?}");
            assert_eq!(report.freelist_after, 0, "an unbounded pass returns every free page: {report:?}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn telemetry_retention_keeps_recent_rows_and_is_bounded() {
        let conn = crate::open_in_memory().unwrap();
        let day = 24 * 3_600_000i64;
        let now = 1_000 * day;
        for age_days in [1, 100, 400, 500] {
            conn.execute(
                "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party)
                 VALUES (?1, 'messages_api', 'agent', 'api.anthropic.com', 1, 'x', 0)",
                [now - age_days * day],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO compression_metrics (ts, query_hash, path, pre_tokens, post_tokens, compress_ms, assemble_ms)
                 VALUES (?1, 'q', 'raw', 1, 1, 0, 0)",
                [now - age_days * day],
            )
            .unwrap();
        }
        // Bounded: one row per table per pass.
        let first = prune_telemetry(&conn, now, 1).unwrap();
        assert_eq!((first.traceability, first.compression_metrics), (1, 1));
        let rest = prune_telemetry(&conn, now, 100).unwrap();
        assert_eq!(rest.traceability, 1, "400d and 500d are past a year; 1d and 100d stay");
        assert_eq!(rest.compression_metrics, 2, "100d, 400d, 500d are past a month; 1d stays");
        let left: i64 = conn.query_row("SELECT count(*) FROM traceability_log", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 2);
        let left: i64 = conn.query_row("SELECT count(*) FROM compression_metrics", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
        assert_eq!(prune_telemetry(&conn, now, 100).unwrap(), TelemetryPrune::default());
    }

    #[test]
    fn batch_send_stats_age_out_after_a_year_and_go_with_delete_all() {
        let mut conn = crate::open_in_memory().unwrap();
        let day = 24 * 3_600_000i64;
        let now = 1_000 * day;
        for (cycle, age_days) in [("recent", 7), ("old", 400)] {
            crate::batch_send_stats::upsert(
                &conn,
                &crate::batch_send_stats::SendStatsRow {
                    cycle_id: cycle.into(),
                    job: "consolidation".into(),
                    recorded_at: now - age_days * day,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert_eq!(prune_telemetry(&conn, now, 100).unwrap().batch_send_stats, 1);
        let left = crate::batch_send_stats::recent(&conn, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].cycle_id, "recent");
        delete_all(&mut conn).unwrap();
        assert!(crate::batch_send_stats::recent(&conn, 10).unwrap().is_empty());
    }

    #[test]
    fn shared_service_round_trips_age_out_after_a_month_and_go_with_delete_all() {
        use crate::traceability::Route;
        let mut conn = crate::open_in_memory().unwrap();
        let day = 24 * 3_600_000i64;
        let now = 1_000 * day;
        crate::shared_service_rtt::insert(&conn, now - 7 * day, Route::BatchRelay, 120, 200).unwrap();
        crate::shared_service_rtt::insert(&conn, now - 40 * day, Route::BatchRelay, 120, 200).unwrap();
        assert_eq!(prune_telemetry(&conn, now, 100).unwrap().shared_service_rtt, 1);
        assert_eq!(crate::shared_service_rtt::latencies_since(&conn, 0).unwrap()[0].count, 1);
        delete_all(&mut conn).unwrap();
        assert!(crate::shared_service_rtt::latencies_since(&conn, 0).unwrap().is_empty());
    }

    #[test]
    fn dashboard_activity_credits_and_edit_counts_follow_forget_and_wipe() -> Result<(), Box<dyn std::error::Error>> {
        // The retained event predates the cutoff while its daily credit completes later. The
        // stored presentation date deliberately predates a hypothetical new-zone cutoff date;
        // erasure must follow UTC completion, not either local-date rendering.
        let cutoff = 1_000_i64;
        let mut conn = crate::open_in_memory()?;
        let old_event = insert_event(
            &conn,
            &NewEvent {
                ts: 500,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: None,
                content: "old fixture",
                content_hash: "dashboard-activity-old",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )?;
        crate::activity::record_capture_credits(
            &conn,
            old_event,
            900,
            &[
                ("2026-09-07".to_string(), 1_000),
            ],
        )?;
        crate::activity::record_capture_credits(
            &conn,
            old_event,
            1_100,
            &[("2026-09-08".to_string(), 1_000)],
        )?;
        // An out-of-order touch must not lower the bucket's erase timestamp.
        crate::activity::record_capture_credits(
            &conn,
            old_event,
            1_050,
            &[("2026-09-08".to_string(), 1_000)],
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-07",
            crate::activity::EditKind::InlineEdit,
            900,
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-08",
            crate::activity::EditKind::ScribeRewrite,
            1_100,
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-08",
            crate::activity::EditKind::ScribeRewrite,
            1_050,
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-08",
            crate::activity::EditKind::InlineEdit,
            1_200,
        )?;
        let latest_capture: i64 = conn.query_row(
            "SELECT latest_activity_ms FROM dashboard_activity_credits WHERE local_date = '2026-09-08'",
            [],
            |row| row.get(0),
        )?;
        let latest_scribe: i64 = conn.query_row(
            "SELECT latest_activity_ms FROM dashboard_edit_counts
              WHERE local_date = '2026-09-08' AND kind = 'scribe_rewrite'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!((latest_capture, latest_scribe), (1_100, 1_100));

        delete_since(&mut conn, cutoff)?;

        let retained_credits: i64 = conn.query_row(
            "SELECT count(*) FROM dashboard_activity_credits", [], |row| row.get(0),
        )?;
        let retained_edits: i64 = conn.query_row(
            "SELECT count(*) FROM dashboard_edit_counts", [], |row| row.get(0),
        )?;
        assert_eq!(retained_credits, 1);
        assert_eq!(retained_edits, 1);
        assert_eq!(
            crate::activity::first_recorded_on(&conn, "2026-09-08")?,
            Some("2026-09-07".to_string())
        );

        delete_all(&mut conn)?;
        let activity_rows: i64 = conn.query_row(
            "SELECT (SELECT count(*) FROM dashboard_activity_credits) +
                    (SELECT count(*) FROM dashboard_edit_counts)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(activity_rows, 0);
        Ok(())
    }

    #[test]
    fn deletion_removes_brief_generation_ledgers_with_forgotten_events(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = crate::open_in_memory()?;
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1000, 'capture', 'text', 'brief source', 'brief-source', 1000)",
            [],
        )?;
        let event_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO brief_generation
                 (date, state, input_from_ts, input_to_ts, output_language, sources_json, created_at, updated_at)
             VALUES ('2026-08-09', 'pending', 0, 2000, 'en', ?1, 1000, 1000)",
            [serde_json::json!([[event_id, "brief-source-digest"]]).to_string()],
        )?;

        delete_since(&mut conn, 1000)?;
        let after_since: i64 =
            conn.query_row("SELECT count(*) FROM brief_generation", [], |row| {
                row.get(0)
            })?;
        assert_eq!(after_since, 0);

        conn.execute(
            "INSERT INTO brief_generation
                 (date, state, input_from_ts, input_to_ts, output_language, sources_json, created_at, updated_at)
             VALUES ('2026-08-10', 'pending', 2000, 3000, 'en', '[]', 2000, 2000)",
            [],
        )?;
        delete_all(&mut conn)?;
        let after_all: i64 =
            conn.query_row("SELECT count(*) FROM brief_generation", [], |row| {
                row.get(0)
            })?;
        assert_eq!(after_all, 0);
        Ok(())
    }

    /// Extracted knowledge (V37) and its source quotes, and the V39 trigram index over it, are
    /// user data. The wipe must not depend on the event cascade and the evidence trigger alone:
    /// an entity whose provenance is already gone is still text the user can search for.
    #[test]
    fn delete_all_leaves_no_searchable_extracted_knowledge_or_quotes() -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = crate::open_in_memory()?;
        let event = insert_event(
            &conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "Kestrelboard keeps the Zephyrine migration runbook",
                content_hash: "knowledge-wipe",
                dwell_ms: 5,
                display_id: None,
                window_bounds: None,
            },
        )?;
        conn.execute(
            "INSERT INTO knowledge_entities(kind,name,context,extraction_key,confidence,created_at,updated_at)
             VALUES('tool','Kestrelboard','keeps the Zephyrine migration runbook','wipe-linked',0.6,1,1)",
            [],
        )?;
        let linked = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO knowledge_provenance(entity_id,event_id,quote)
             VALUES(?1,?2,'Kestrelboard keeps the Zephyrine migration runbook')",
            rusqlite::params![linked, event],
        )?;
        // No provenance row: nothing cascades to it and the evidence trigger never fires for it.
        conn.execute(
            "INSERT INTO knowledge_entities(kind,name,context,extraction_key,confidence,created_at,updated_at)
             VALUES('topic','Quillhaven','Zephyrine planning notes','wipe-unlinked',0.6,1,1)",
            [],
        )?;

        let hits = |conn: &Connection, sql: &str, term: &str| -> Result<i64, rusqlite::Error> {
            conn.query_row(sql, [term], |row| row.get(0))
        };
        const KNOWLEDGE: &str =
            "SELECT count(*) FROM knowledge_entities_fts WHERE knowledge_entities_fts MATCH ?1";
        const EVENTS: &str = "SELECT count(*) FROM event_fts WHERE event_fts MATCH ?1";
        assert_eq!(hits(&conn, KNOWLEDGE, "Zephyrine")?, 2, "seeded rows are indexed");
        assert_eq!(hits(&conn, EVENTS, "Zephyrine")?, 1, "the quoted source is indexed");

        delete_all(&mut conn)?;

        for table in ["knowledge_entities", "knowledge_provenance", "knowledge_nodes", "knowledge_links"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
        // `knowledge_entities_fts` is external-content: `count(*)` reads the base table, so only a
        // MATCH (which reads the index) or the per-document shadow table can show residue.
        for term in ["Kestrelboard", "Quillhaven", "Zephyrine", "runbook"] {
            assert_eq!(hits(&conn, KNOWLEDGE, term)?, 0, "knowledge index still matches {term}");
            assert_eq!(hits(&conn, EVENTS, term)?, 0, "event index still matches {term}");
            let docs: i64 = conn.query_row(
                "SELECT count(*) FROM search_documents_fts WHERE search_documents_fts MATCH ?1",
                [term],
                |r| r.get(0),
            )?;
            assert_eq!(docs, 0, "search projection still matches {term}");
        }
        let indexed: i64 =
            conn.query_row("SELECT count(*) FROM knowledge_entities_fts_docsize", [], |r| r.get(0))?;
        assert_eq!(indexed, 0, "knowledge index keeps per-document entries");
        conn.execute(
            "INSERT INTO knowledge_entities_fts(knowledge_entities_fts) VALUES('integrity-check')",
            [],
        )?;
        Ok(())
    }

    #[test]
    fn deletion_removes_owned_autoadd_terms_but_keeps_manual_dictionary_edits() -> Result<(), Box<dyn std::error::Error>> {
        use crate::voice_terms::{self, NewVoiceTerm, VoiceTermScope};
        let mut conn = crate::open_in_memory()?;
        let input = |canonical: &str, aliases: Vec<String>| NewVoiceTerm {
            canonical: canonical.into(), aliases, locale: None,
            scope: Some(VoiceTermScope::Surface), scope_ref: Some("voice_dictation".into()),
            priority: None, enabled: true,
        };
        let manual = voice_terms::create_voice_term(&mut conn, &input("Jonathan", vec!["Johnny".into()]), 100)?;
        let learned = voice_terms::create_voice_term(&mut conn, &input("John", vec!["Jon".into()]), 200)?;
        let alias: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jon'", [learned.id], |r| r.get(0))?;
        let canonical: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'John'", [learned.id], |r| r.get(0))?;
        conn.execute(
            "INSERT INTO dictionary_autoadd_corrections
             (id, original, corrected, observations, last_observation_id, state, created_at_ms,
              term_id, alias_id, canonical_alias_id, term_updated_at_ms, created_term)
             VALUES (1, 'Jon', 'John', 2, 2, 'added', 200, ?1, ?2, ?3, 200, 1)",
            rusqlite::params![learned.id, alias, canonical],
        )?;
        conn.execute(
            "INSERT INTO dictionary_autoadd_artifacts
             (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
             VALUES (1, ?1, ?2, ?3, 1, 200)",
            rusqlite::params![learned.id, alias, canonical],
        )?;
        conn.execute("INSERT INTO dictionary_autoadd_suppression (pair_digest, created_at_ms, reason)
                      VALUES (?1, 100, 'undo'), (?2, 200, 'undo')",
            rusqlite::params![vec![1u8; 32], vec![2u8; 32]])?;
        delete_since(&mut conn, 150)?;
        assert_eq!(voice_terms::list_voice_terms(&conn)?.iter().map(|term| term.id).collect::<Vec<_>>(), vec![manual.id]);
        assert_eq!(conn.query_row("SELECT count(*) FROM dictionary_autoadd_suppression", [], |r| r.get::<_, i64>(0))?, 1);

        let layered = voice_terms::create_voice_term(&mut conn, &input("Joanna", vec!["Jo".into(), "Joa".into()]), 250)?;
        let first_alias: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jo'", [layered.id], |r| r.get(0))?;
        let later_alias: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Joa'", [layered.id], |r| r.get(0))?;
        let canonical: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Joanna'", [layered.id], |r| r.get(0))?;
        for (correction_id, alias_id, created_term, created_at) in [(4, first_alias, 1, 250), (5, later_alias, 0, 350)] {
            conn.execute(
                "INSERT INTO dictionary_autoadd_artifacts
                 (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![correction_id, layered.id, alias_id, canonical, created_term, created_at],
            )?;
        }
        delete_since(&mut conn, 300)?;
        assert_eq!(voice_terms::list_voice_terms(&conn)?.into_iter().find(|term| term.id == layered.id).ok_or("missing original term")?.aliases.len(), 2);
        delete_since(&mut conn, 200)?;
        assert!(!voice_terms::list_voice_terms(&conn)?.iter().any(|term| term.id == layered.id));

        let learned = voice_terms::create_voice_term(&mut conn, &input("Jane", vec!["Jan".into()]), 300)?;
        let alias: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jan'", [learned.id], |r| r.get(0))?;
        let canonical: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jane'", [learned.id], |r| r.get(0))?;
        conn.execute(
            "INSERT INTO dictionary_autoadd_artifacts
             (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
             VALUES (2, ?1, ?2, ?3, 1, 300)",
            rusqlite::params![learned.id, alias, canonical],
        )?;
        voice_terms::update_voice_term(&mut conn, learned.id, &input("Jane", vec!["Jan".into(), "My name".into()]), 400)?;
        let newest = voice_terms::create_voice_term(&mut conn, &input("Jade", vec!["Jad".into()]), 500)?;
        let alias: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jad'", [newest.id], |r| r.get(0))?;
        let canonical: i64 = conn.query_row("SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = 'Jade'", [newest.id], |r| r.get(0))?;
        conn.execute(
            "INSERT INTO dictionary_autoadd_artifacts
             (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
             VALUES (3, ?1, ?2, ?3, 1, 500)",
            rusqlite::params![newest.id, alias, canonical],
        )?;
        delete_all(&mut conn)?;
        let remaining = voice_terms::list_voice_terms(&conn)?;
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().any(|term| term.id == learned.id && term.aliases.contains(&"My name".into())));
        assert_eq!(conn.query_row("SELECT count(*) FROM dictionary_autoadd_suppression", [], |r| r.get::<_, i64>(0))?, 0);
        Ok(())
    }

    /// Issue #780: "Delete everything" must account for every table. Each one is wiped by
    /// `delete_all`, emptied by a cascade or trigger from a table it wipes, or a setting kept on
    /// purpose. A table added by a later migration fails here until someone decides which.
    #[test]
    fn delete_all_accounts_for_every_table() {
        // Settings and internal bookkeeping that hold no user content and must survive so the
        // app keeps working on the emptied database.
        const KEPT: &[&str] = &[
            "refinery_schema_history",
            // V101 access grants: who may read or write which namespace (settings, no content).
            "scratch_acl",
            "principal_grants",
            "chat_state",
            "dictionary_autoadd_secret",
            "dictionary_autoadd_settings",
            "lesson_distill_meta",
            "mail_labels",
            "mail_triage_settings",
            "search_document_backfill_state",
            // V81 back-fill cursor: a row id, no content.
            "event_grams_backfill",
            "writing_voice_settings",
            // V102: the store's active embedding model (a model id, no content).
            "embedding_space",
            // V103: spent decision-token nonces (replay protection until the token expires).
            "approval_spent_nonces",
        ];
        // Emptied by ON DELETE CASCADE from a wiped parent, or by a trigger on one.
        const CASCADED: &[&str] = &[
            "event_processing_receipts",
            "event_processing_failures",
            "dream_batch_items",
            "dream_brief_effects",
            "dream_cycle_inputs",
            "event_fts",
            // V81 triggers on event_log empty both when its rows go.
            "event_grams_fts",
            "event_grams_pending",
            "knowledge_entities_fts",
            // V87 triggers on screen_frames empty it when its rows go.
            "screen_frame_sizes",
            "search_document_embeddings",
            "search_document_vec",
            "search_documents_fts",
            "user_memory_notes_fts",
        ];
        let conn = crate::open_in_memory().unwrap();
        let source = include_str!("maintenance.rs");
        let body_of = |name: &str| {
            let start = source.find(&format!("fn {name}(")).unwrap();
            let rest = &source[start..];
            rest[..rest.find("\n}\n").unwrap()].to_string()
        };
        let wiped = body_of("delete_all") + &body_of("remove_autoadd_artifacts");
        let virtual_tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let unplaced: Vec<&String> = tables
            .iter()
            .filter(|t| {
                let shadow = virtual_tables.iter().any(|v| t.starts_with(&format!("{v}_")));
                let deleted = wiped.contains(&format!("DELETE FROM {t}\"")) || wiped.contains(&format!("DELETE FROM {t} "));
                !(shadow || deleted || KEPT.contains(&t.as_str()) || CASCADED.contains(&t.as_str()))
            })
            .collect();
        assert!(unplaced.is_empty(), "tables delete_all does not account for: {unplaced:?}");
    }

    #[test]
    fn delete_all_removes_usage_log_and_relay_records() {
        let mut conn = crate::open_in_memory().unwrap();
        seed(&mut conn);
        conn.execute_batch(
            "INSERT INTO relay_attempts (fingerprint, request_id) VALUES ('fp', 'req');
             INSERT INTO dream_event_arrivals (event_id) VALUES (1);
             INSERT INTO event_processing_receipts (event_id, arrival_seq, reviewed_at, lane)
                 SELECT id, 1, 100, 'batch' FROM event_log LIMIT 1;
             INSERT INTO event_processing_failures (event_id, failed_at, code)
                 SELECT id, 100, 'consolidation_output' FROM event_log LIMIT 1;
             INSERT INTO memory_processing_runtime VALUES (1, 100, 60000, 'review_complete');
             INSERT INTO state_suppressions (state_table, match_digest, reason, suppressed_at)
             VALUES ('commitments', 'legacy-digest', 'deleted', 100);
             INSERT INTO state_suppressions_scoped (state_table, namespace, match_digest, reason, suppressed_at)
             VALUES ('commitments', 'agent_scratch:alpha', 'scoped-digest', 'edited', 200);",
        )
        .unwrap();
        let secret_before: Vec<u8> = conn
            .query_row("SELECT key FROM dictionary_autoadd_secret WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        delete_all(&mut conn).unwrap();
        for table in ["llm_usage", "relay_attempts", "dream_event_arrivals", "state_suppressions", "state_suppressions_scoped",
            "event_processing_receipts", "event_processing_failures", "memory_processing_runtime"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} should be empty after delete_all");
        }
        let secret_after: Vec<u8> = conn
            .query_row("SELECT key FROM dictionary_autoadd_secret WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(secret_after.len(), 32);
        assert_ne!(secret_before, secret_after);
    }
}
