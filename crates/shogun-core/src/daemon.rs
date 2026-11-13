//! The daemon's DB handle (daemon wiring, feature `db`). One shared SQLite connection that every
//! writer and reader uses — the data-gravity point of the whole system (CLAUDE.md invariant 1: the
//! DB is owned by the Rust core). Cheap to clone (it's an `Arc`), so the capture thread, the LLM
//! egress sink, and the traceability viewer all hold the same handle.
//!
//! Every method here swallows storage errors into an `Option`/empty result rather than
//! propagating a panic: the capture daemon must never crash on a write hiccup (CLAUDE.md
//! crash-resilience). Durable-write concerns (WAL, transactions) live in [`shogun_memory`].
//!
//! Clocks are injected ([`Clock`]) so timestamps are deterministic under test; production passes a
//! real wall-clock closure.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension};
use shogun_fusion::assemble::ActionCandidate;
use shogun_fusion::block::{BlockRef, ContextBlock, SourceKind};
use shogun_fusion::brief::{
    assemble_brief, assemble_degraded, CalendarLine, CommitmentDue, MorningBrief, OpenLoopItem,
};
use shogun_fusion::budget::TokenEstimator;
use shogun_memory::event_log::{self, NewEvent};
use shogun_memory::lessons;
use shogun_memory::state::{self, NewCommitment, NewOpenLoop, NewPerson, NewProject, Provenance};
use shogun_memory::traceability::{Filter, TraceRow};
use shogun_memory::MemoryError;

use crate::capture::dedup::{decide_hash, Recent};
use crate::db_sink::DbTraceabilitySink;
use crate::dreamcycle::plan::{
    remaining, CycleKind, JobKind, JobRun, JobState, DEGRADED_SEQUENCE, FULL_SEQUENCE,
};
use crate::memory_health::{FaultClass, MemoryFault, MemoryResult};

mod about_profile;
mod ai_search;
mod ai_search_jobs;
pub use ai_search_jobs::ai_search_api_json;
pub use ai_search::{status_parts, AiSearchHitView, AiSearchView, DbKeywordSearcher, KEYWORD_PHASE_STATUS};
mod daily_activity_summary;
mod agenda;
pub use agenda::{AgendaEvent, AGENDA_DAYS, AGENDA_MAX};
mod briefs;
pub use about_profile::{DeclaredProfile, GeneratedAboutProfile};
mod brief_generation;
pub use brief_generation::{set_brief_charm, BriefGeneration};
mod wrap_generation;
pub use wrap_generation::WrapLines;
mod activity;
/// `memory.ingest` / `memory.recall` on the store (E4-T3).
mod agent_memory;
pub mod api_entitlements;
mod chat;
mod chat_grounding;
pub use chat_grounding::{ChatGrounding, ChatRetrieval, CHAT_EVIDENCE_CHARS, CHAT_EVIDENCE_HITS};
pub mod memory_tools;
mod clarify;
pub use clarify::{ClarifyAskError, ClarifyPoll};
mod context;
mod db;
mod dream_cycle;
pub(crate) use dream_cycle::{enter_dream_effect_fence, DreamRunLease};
mod ingestion;
mod knowledge_context;
mod lesson_store;
mod local_writes;
pub use local_writes::{ExportAsk, SOURCE_MEETING_RECAP};
mod mail_triage;
pub use mail_triage::MAIL_ARCHIVE_NEEDS_APPROVAL;
mod relay_attempts;
mod maintenance;
mod meetings;
mod export_v1;
mod governance;
mod payments;
mod processing;
mod memory_fs;
mod memory_import;
mod memory_notes;
mod chat_memory;
pub mod auto_memory;
pub use chat_memory::NoteRef;
pub use meetings::CurrentOccurrence;
mod recall;
pub use recall::{
    FrameRescan, FrameRescanError, RESCAN_ERR_FRAME_EXPIRED, RESCAN_ERR_FRAME_MISSING,
    RESCAN_ERR_MEMORY_UNAVAILABLE, RESCAN_ERR_OCR_FAILED, RESCAN_ERR_VISUAL_RECALL_OFF,
};
mod replies;
mod search;
mod source_filter;
mod search_index;
pub mod sessions;
mod skills;
mod translate_jobs;
mod voice_dictionary;
pub mod dictionary_autoadd;
mod rewrite_loop;
pub use rewrite_loop::{RewritePromptContext, WriterIdentity};
mod writing_voice;

#[cfg(test)]
mod tests;

pub use crate::context::{ContextPack, Evidence, ScreenFrameRef};
pub use ingestion::IngestSummary;
pub use search::{
    AnswerabilityReasonV1, AnswerabilityV1, MemorySourceFamilyV1, SearchDepthV1, SearchErrorV1,
    SearchLexicalScopeV1, SearchPolicyErrorV1, SearchRequestV1, SearchResponseV1, SearchSloClassV1,
    SearchTimeRangeV1,
};

pub use briefs::{
    TodayBrief, brief_input_window, displayed_brief_payload, local_date_days_before, local_date_string, local_day_bounds,
    local_hour_now, local_next_midnight, local_wrap_window, os_gmt_offset_secs,
};
pub use activity::DashboardUsage;
#[cfg(test)]
use context::{
    cmp_ranked_evidence, evidence_to_blocks, rank_context_hits, REPLY_TURNS, REPLY_TURN_CHARS,
};
use context::{extracts_untrusted_body, LESSON_TOP_K};
pub use context::{
    CompressBudget, GenerationContext, PayloadSource, ReferentOutcome, ReplyContext,
    ThreadCandidate,
};
pub use db::{BackupFailureClass, Clock, DaemonDb, Db, HotPhase, HotStatus, SharedConn};
pub(crate) use db::load_db_encryption_key;
pub use maintenance::{overdue_notifications, DeleteAllError, DeleteAllOutcome, LocalMaintenance};
pub use replies::{
    PreparedReplyContext, ReplyContextCache, ReplyContextCacheSnapshot, SyncInvalidator,
};

mod voice_dictionary_support {
    use shogun_memory::voice_terms::{self, VoiceTerm};

    pub(super) fn dictionary_entry(term: VoiceTerm) -> crate::voice_dictionary::DictionaryEntry {
        use crate::voice_dictionary::TermScope;

        let scope = match term.scope {
            voice_terms::VoiceTermScope::Global => TermScope::Global,
            voice_terms::VoiceTermScope::Bundle => {
                TermScope::Bundle(term.scope_ref.unwrap_or_default())
            }
            voice_terms::VoiceTermScope::Surface => {
                TermScope::Surface(term.scope_ref.unwrap_or_default())
            }
        };
        crate::voice_dictionary::VoiceDictionary::user_entry(
            term.canonical,
            term.aliases,
            term.locale,
            scope,
            term.priority,
            term.enabled,
        )
    }
}

mod context_assembly {
    use shogun_fusion::block::{BlockRef, ContextBlock, ScoreInputs, SourceKind, StateTable};
    use shogun_fusion::budget::TokenEstimator;

    use super::Evidence;

    /// 検索 evidence: relevance は呼び出し側の検索スコア由来なので EVIDENCE_RELEVANCE を上書きする。
    pub(super) const EVIDENCE_RELEVANCE: f64 = 0.7;
    const EVIDENCE_SCORE: ScoreInputs = ScoreInputs {
        relevance: EVIDENCE_RELEVANCE,
        freshness: 0.5,
        task_link: 0.0,
        confidence: 1.0,
    };
    /// retrieved evidence の属する session の保存済み要約。参照先＝関連度高、要約＝confidence 1.0。
    pub(super) const SESSION_SUMMARY_SCORE: ScoreInputs = ScoreInputs {
        relevance: 0.85,
        freshness: 0.7,
        task_link: 0.5,
        confidence: 1.0,
    };
    /// 解決済みスレッドの保存済み要約。session と対称、参照先＝関連度高、要約＝confidence 1.0。
    pub(super) const THREAD_SUMMARY_SCORE: ScoreInputs = ScoreInputs {
        relevance: 0.9,
        freshness: 0.7,
        task_link: 0.5,
        confidence: 1.0,
    };
    /// confidence ゲート済み state fact。現在の作業に紐づく前提でやや高め、confidence は High 相当 0.9。
    const FACT_SCORE: ScoreInputs = ScoreInputs {
        relevance: 0.6,
        freshness: 0.6,
        task_link: 0.6,
        confidence: 0.9,
    };

    // Screen relevance and the open-loop kind mapping for the action row live in
    // `shogun_fusion::candidates` (matched there against title, host, excerpt and selection, and
    // scored by `crates/fusion-eval`).

    /// Normalize retrieved evidence into compressed context blocks.
    pub(super) fn evidence_to_blocks(
        evidence: &[Evidence],
        relevance: f64,
        est: &dyn TokenEstimator,
    ) -> Vec<ContextBlock> {
        evidence
            .iter()
            .flat_map(|e| {
                let mut blocks = Vec::with_capacity(usize::from(is_calendar_source(&e.source)) + 1);
                if is_calendar_source(&e.source) {
                    blocks.push(ContextBlock::new(
                        BlockRef::Event(e.event_id),
                        SourceKind::Structured,
                        calendar_fact_text(e),
                        evidence_score_inputs(SourceKind::Structured, relevance),
                        est,
                    ));
                }
                blocks.push(ContextBlock::new(
                    BlockRef::Event(e.event_id),
                    SourceKind::Evidence,
                    e.excerpt.clone(),
                    evidence_score_inputs(SourceKind::Evidence, relevance),
                    est,
                ));
                blocks
            })
            .collect()
    }

    /// Query-time confidence: the source trust prior, not a flat `1.0`, on quoted hits.
    fn evidence_score_inputs(kind: SourceKind, relevance: f64) -> ScoreInputs {
        ScoreInputs {
            relevance,
            freshness: EVIDENCE_SCORE.freshness,
            task_link: EVIDENCE_SCORE.task_link,
            confidence: shogun_fusion::trust::query_time_confidence(kind),
        }
    }

    pub(super) fn is_calendar_source(source: &str) -> bool {
        matches!(
            shogun_mcp::scope::from_source(source),
            Some(shogun_mcp::scope::Service::GoogleCalendar)
        )
    }

    fn calendar_fact_text(evidence: &Evidence) -> String {
        let title = evidence
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or("Untitled event");
        format!(
            "Calendar event\nTitle: {title}\nTimestamp (unix ms): {}\nEvent ID: {}",
            evidence.ts, evidence.event_id
        )
    }

    /// Normalize confidence-gated state facts into compressed context blocks.
    pub(super) fn facts_to_blocks(
        facts: &[(String, StateTable, i64)],
        est: &dyn TokenEstimator,
    ) -> Vec<ContextBlock> {
        facts
            .iter()
            .map(|(f, table, id)| {
                ContextBlock::new(
                    BlockRef::State {
                        table: *table,
                        id: *id,
                    },
                    SourceKind::StateFact,
                    f.clone(),
                    FACT_SCORE,
                    est,
                )
            })
            .collect()
    }
}

mod dream_ledger {
    use crate::dreamcycle::plan::{JobKind, JobState};

    pub(super) fn job_kind_str(kind: JobKind) -> &'static str {
        match kind {
            JobKind::Consolidation => "consolidation",
            JobKind::Compression => "compression",
            JobKind::StateUpdate => "state_update",
            JobKind::ConfidenceRecalc => "confidence_recalc",
            JobKind::ColdDemotion => "cold_demotion",
            JobKind::MorningBrief => "morning_brief",
            JobKind::LessonDistillation => "lesson_distillation",
            JobKind::PatternDistillation => "pattern_distillation",
        }
    }

    pub(super) fn parse_job_kind(s: &str) -> Option<JobKind> {
        Some(match s {
            "consolidation" => JobKind::Consolidation,
            "compression" => JobKind::Compression,
            "state_update" => JobKind::StateUpdate,
            "confidence_recalc" => JobKind::ConfidenceRecalc,
            "cold_demotion" => JobKind::ColdDemotion,
            "morning_brief" => JobKind::MorningBrief,
            "lesson_distillation" => JobKind::LessonDistillation,
            "pattern_distillation" => JobKind::PatternDistillation,
            _ => return None,
        })
    }

    pub(super) fn job_state_str(state: JobState) -> &'static str {
        match state {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }

    pub(super) fn parse_job_state(s: &str) -> Option<JobState> {
        Some(match s {
            "pending" => JobState::Pending,
            "running" => JobState::Running,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            _ => return None,
        })
    }
}

use context_assembly::{EVIDENCE_RELEVANCE, SESSION_SUMMARY_SCORE, THREAD_SUMMARY_SCORE};
use shogun_memory::voice_terms::{self, NewVoiceTerm, VoiceTerm};
use voice_dictionary_support::dictionary_entry;

/// How many recent capture bodies the near-dup collapse (FR-CAP-03) compares against. Bounds the
/// per-capture comparison cost; window re-reads are near each other in the log, so a small window
/// catches them.
const RECENT_DEDUP_WINDOW: usize = 8;

/// `inline_memory` に渡す fact 上限。`assemble_context` と `assemble_context_compressed` の
/// 両パスで一致させ、fact の一貫性を保つ。
const FACT_LIMIT: usize = 8;
