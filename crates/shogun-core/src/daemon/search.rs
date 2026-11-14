//! Retrieval-facing daemon value types.

use super::*;

/// Maximum populated Cold partitions one complete explicit request may scan.
pub const MAX_COLD_SEARCH_PARTITIONS: usize = shogun_memory::search::DEFAULT_MAX_COLD_PARTITIONS;

pub use shogun_mcp::backend::{
    resolve_search_request, AnswerabilityReasonV1, AnswerabilityV1, MemorySourceFamilyV1,
    SearchDepthV1, SearchPolicyErrorV1, SearchRequestV1, SearchTimeRangeV1,
};

/// Lexical source set used by a completed search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLexicalScopeV1 {
    Warm,
    AllHistory,
}

/// Latency budget class. Default widening remains normal local search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSloClassV1 {
    Normal,
    ExplicitDeep,
}

/// Canonical search failure: policy rejection or existing content-free store fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum SearchErrorV1 {
    Policy(SearchPolicyErrorV1),
    Memory(crate::memory_health::MemoryFault),
}

impl SearchErrorV1 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy(error) => error.as_str(),
            Self::Memory(error) => error.as_str(),
        }
    }
}

impl From<crate::memory_health::MemoryFault> for SearchErrorV1 {
    fn from(error: crate::memory_health::MemoryFault) -> Self {
        Self::Memory(error)
    }
}

/// Ordered hits plus content-free policy execution metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResponseV1 {
    /// Legacy raw-event projection retained for existing in-process callers.
    pub hits: Vec<shogun_memory::search::SearchHit>,
    /// Full federated results used by the Memory API surfaces.
    pub federated_hits: Vec<shogun_memory::search::FederatedHit>,
    /// The newest event row behind each federated hit (`when` / `where` on API results).
    pub event_places: std::collections::BTreeMap<i64, shogun_memory::search::EventPlace>,
    /// The relevance floor's verdict (`shogun_memory::search::RELEVANCE_FLOOR_COVERAGE`). This
    /// path never empties `federated_hits` on it — suggestions stay available for the Notch and
    /// the context assembly — and expresses a failing verdict through `answerability` instead.
    pub relevance: shogun_memory::search::RelevanceFloor,
    pub answerability: AnswerabilityV1,
    pub requested_depth: SearchDepthV1,
    pub effective_depth: SearchDepthV1,
    pub requested_range: Option<SearchTimeRangeV1>,
    pub resolved_range: SearchTimeRangeV1,
    pub lexical_scope: SearchLexicalScopeV1,
    pub complete: bool,
    pub cold_partitions_visited: usize,
    pub cold_vectors_scanned: usize,
    pub slo_class: SearchSloClassV1,
    pub elapsed_ms: u64,
}

/// The answerability verdict for a search result. The hits are never emptied on the daemon's
/// path; a conservative verdict is expressed here. The relevance floor
/// (`shogun_memory::search::RELEVANCE_FLOOR_COVERAGE`, measured by every federated search)
/// only ever lowers the verdict: a top hit that would otherwise read as answerable but covers
/// too little of the question is reported as `LowQuality`, so the Memory API's `answerable:
/// false` follows from the floor without the candidates being withheld.
pub(crate) fn answerability(
    query: &str,
    hits: &[shogun_memory::search::FederatedHit],
    relevance: &shogun_memory::search::RelevanceFloor,
) -> AnswerabilityV1 {
    const MIN_MATCH_QUALITY: f64 = 0.4;
    const MAX_SUBSTRING_COVERAGE_GAP: f64 = 0.25;

    let reason = if query.trim().is_empty() {
        AnswerabilityReasonV1::EmptyQuery
    } else if hits.first().is_none_or(|hit| !hit.answer_eligible) {
        AnswerabilityReasonV1::NoEligibleMatch
    } else {
        let quality = hits[0].match_quality;
        if quality.anchor_count == 0 {
            AnswerabilityReasonV1::InsufficientSignal
        } else if quality.anchor_coverage < 1.0 {
            AnswerabilityReasonV1::MissingAnchor
        } else if quality.term_coverage < MIN_MATCH_QUALITY
            || quality.lexical_quality < MIN_MATCH_QUALITY
            || quality.lexical_quality - quality.term_coverage > MAX_SUBSTRING_COVERAGE_GAP
            || !relevance.passed
        {
            AnswerabilityReasonV1::LowQuality
        } else {
            AnswerabilityReasonV1::Answerable
        }
    };
    AnswerabilityV1 {
        answerable: reason == AnswerabilityReasonV1::Answerable,
        reason,
    }
}

impl Db {
    /// Hybrid/FTS search over the event log (`memory.search`). Empty on an empty query or failure.
    pub fn search(&self, query: &str, limit: usize) -> Vec<shogun_memory::search::SearchHit> {
        self.try_search(query, limit).unwrap_or_default()
    }

    /// Canonical memory-search policy resolver. Every caller supplies intent; this method alone
    /// resolves time, semantic depth, lexical widening, caps, completeness, and metadata.
    pub fn search_request(
        &self,
        request: &SearchRequestV1,
    ) -> Result<SearchResponseV1, SearchErrorV1> {
        let started = std::time::Instant::now();
        let now_ms = self.now_ms();
        let mut resolved =
            resolve_search_request(request, now_ms).map_err(SearchErrorV1::Policy)?;
        // A call for a connection without meeting notes and screen recall (owner decision
        // 2026-10-20, `shogun_mcp::source_policy`): those hits are left out below, so fetch the
        // largest page first and cut it back to the caller's limit after.
        let withheld = shogun_mcp::source_policy::current().is_withheld();
        let fetch_limit = if withheld {
            resolved.limit.max(shogun_mcp::backend::MAX_SEARCH_LIMIT)
        } else {
            resolved.limit
        };
        let query_plan = shogun_memory::search::plan_memory_query(
            &request.query,
            now_ms,
            local_day_bounds(now_ms),
        );
        // A relative date in the question (昨日, 先週, last month …) is the user's own range. It may
        // reach past the Warm window: lexical and derived lanes read any range cheaply, and the
        // semantic depth stays what the request resolved to.
        let mut question_range = false;
        if request.range.is_none() {
            if let (Some(from_ms), Some(to_ms)) = (query_plan.from_ms, query_plan.to_ms) {
                if from_ms < to_ms && to_ms <= now_ms {
                    resolved.resolved_range = SearchTimeRangeV1 { from_ms, to_ms };
                    question_range = true;
                }
            }
        }
        let slo_class = if resolved.explicit_deep {
            SearchSloClassV1::ExplicitDeep
        } else {
            SearchSloClassV1::Normal
        };

        if request.query.trim().is_empty() || resolved.limit == 0 {
            return Ok(SearchResponseV1 {
                hits: Vec::new(),
                federated_hits: Vec::new(),
                event_places: Default::default(),
                relevance: shogun_memory::search::RelevanceFloor::not_applicable(),
                answerability: answerability(
                    &request.query,
                    &[],
                    &shogun_memory::search::RelevanceFloor::not_applicable(),
                ),
                requested_depth: resolved.requested_depth,
                effective_depth: resolved.effective_depth,
                requested_range: resolved.requested_range,
                resolved_range: resolved.resolved_range,
                lexical_scope: SearchLexicalScopeV1::Warm,
                complete: true,
                cold_partitions_visited: 0,
                cold_vectors_scanned: 0,
                slo_class,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }

        // Model failure degrades honestly to lexical-only search, matching legacy behavior.
        let query_embedding = self.query_embedding(&request.query);
        let include_cold =
            resolved.effective_depth == SearchDepthV1::All && query_embedding.is_some();
        let range = shogun_memory::search::SearchRange {
            from_ms: resolved.resolved_range.from_ms,
            to_ms: resolved.resolved_range.to_ms,
        };
        let outcome = self
            .with_conn("memory.search", |conn| -> Result<_, rusqlite::Error> {
                if include_cold
                    && shogun_memory::search::cold_partition_count_in_range(conn, range)?
                        > MAX_COLD_SEARCH_PARTITIONS
                {
                    return Ok(Err(SearchPolicyErrorV1::ColdRangeTooBroad));
                }

                // Omitted bounds are the product lookup default: search canonical event text
                // across history while semantic and derived lanes stay Warm. Hit count cannot
                // decide relevance; a full page of weak recent matches must not hide an exact old
                // memory. Explicit ranges remain exact and never widen beyond caller intent.
                //
                // A range named in the question is intent too: 「昨日の会議」 must not answer
                // with last month's meeting, so it bounds the event lane like an explicit range.
                let widen_event_lexical = resolved.requested_range.is_none() && !question_range;
                let options = shogun_memory::search::FederatedSearchOptions {
                    since_ts: Some(resolved.resolved_range.from_ms),
                    to_ts: Some(resolved.resolved_range.to_ms),
                    depth: if include_cold {
                        shogun_memory::search::SearchDepth::All
                    } else {
                        shogun_memory::search::SearchDepth::WarmOnly
                    },
                    max_cold_partitions: MAX_COLD_SEARCH_PARTITIONS,
                    focus: shogun_memory::search::MemoryQueryFocus::Any,
                    source: match request.source_kind {
                        Some(source_kind) => {
                            shogun_memory::search::SourceConstraint::Require(match source_kind {
                                MemorySourceFamilyV1::Meeting => {
                                    shogun_memory::search::MemorySourceFamily::Meeting
                                }
                                MemorySourceFamilyV1::State => {
                                    shogun_memory::search::MemorySourceFamily::State
                                }
                                MemorySourceFamilyV1::Brief => {
                                    shogun_memory::search::MemorySourceFamily::Brief
                                }
                                MemorySourceFamilyV1::Person => {
                                    shogun_memory::search::MemorySourceFamily::Person
                                }
                                MemorySourceFamilyV1::Project => {
                                    shogun_memory::search::MemorySourceFamily::Project
                                }
                                MemorySourceFamilyV1::Thread => {
                                    shogun_memory::search::MemorySourceFamily::Thread
                                }
                                MemorySourceFamilyV1::Period => {
                                    shogun_memory::search::MemorySourceFamily::Period
                                }
                            })
                        }
                        None => query_plan.source,
                    },
                    anchors: query_plan.anchors.clone(),
                    widen_event_lexical,
                    ..shogun_memory::search::FederatedSearchOptions::default()
                };
                let mut federated = shogun_memory::search::search_federated(
                    conn,
                    &request.query,
                    query_embedding.as_deref(),
                    now_ms,
                    &options,
                    fetch_limit,
                )
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                let mut legacy_hits =
                    shogun_memory::search::hydrate(conn, &federated.raw_event_ranks)?;
                if withheld {
                    let ids: Vec<i64> = federated
                        .hits
                        .iter()
                        .flat_map(super::source_filter::hit_event_ids)
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    let sensitive = super::source_filter::sensitive_event_ids(conn, &ids)?;
                    federated.hits = super::source_filter::withhold_hits(
                        std::mem::take(&mut federated.hits),
                        &sensitive,
                    );
                    federated.hits.truncate(resolved.limit);
                    legacy_hits.retain(|hit| {
                        !shogun_mcp::source_policy::is_sensitive_source(&hit.source)
                    });
                }
                let primary_events = federated
                    .hits
                    .iter()
                    .flat_map(|hit| hit.event_ids(1))
                    .collect::<Vec<_>>();
                let places = shogun_memory::search::event_places(conn, &primary_events)?;
                Ok(Ok((federated, legacy_hits, places)))
            })
            .map_err(SearchErrorV1::Memory)?
            .map_err(SearchErrorV1::Policy)?;
        let (outcome, hits, event_places) = outcome;

        // The 500 ms search SLO is only real if the product path records it, so API/CLI/MCP
        // searches feed `Slo::Search` here rather than only the webview's `local_search` report.
        crate::metrics::record_global(
            crate::metrics::Slo::Search,
            started.elapsed().as_secs_f64() * 1000.0,
        );

        Ok(SearchResponseV1 {
            hits,
            answerability: answerability(&request.query, &outcome.hits, &outcome.relevance),
            federated_hits: outcome.hits,
            event_places,
            relevance: outcome.relevance,
            requested_depth: resolved.requested_depth,
            effective_depth: resolved.effective_depth,
            requested_range: resolved.requested_range,
            resolved_range: resolved.resolved_range,
            lexical_scope: if outcome.event_lexical_all_history {
                SearchLexicalScopeV1::AllHistory
            } else {
                SearchLexicalScopeV1::Warm
            },
            complete: true,
            cold_partitions_visited: outcome.cold.partitions_visited,
            cold_vectors_scanned: outcome.cold.rows_scanned,
            slo_class,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// [`Self::search`] with the failure kept (issue #121).
    ///
    /// This is the distinction the search box exists on: "nothing matched" is an answer the user
    /// can act on, and "the store did not answer" is not. Handing back an empty list for both
    /// tells them their memory is empty when it is merely unreachable.
    pub fn try_search(
        &self,
        query: &str,
        limit: usize,
    ) -> MemoryResult<Vec<shogun_memory::search::SearchHit>> {
        match self.search_request(&SearchRequestV1 {
            query: query.to_string(),
            limit,
            ..SearchRequestV1::default()
        }) {
            Ok(response) => Ok(response.hits),
            Err(SearchErrorV1::Memory(fault)) => Err(fault),
            // Canonical default request cannot violate policy; keep the legacy error type closed.
            Err(SearchErrorV1::Policy(_)) => Err(MemoryFault::Query),
        }
    }

    /// FTS search scoped to one event-log `source` (visual recall uses `screen_ocr`).
    pub fn search_source(
        &self,
        query: &str,
        source: &str,
        limit: usize,
    ) -> Vec<shogun_memory::search::SearchHit> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        self.with_conn("memory.search_source", |c| {
            let ids = shogun_memory::search::fts_search_source(c, query, source, limit)?;
            let ranked: Vec<(i64, f64)> = ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| (id, 1.0 / (i as f64 + 1.0)))
                .collect();
            shogun_memory::search::hydrate(c, &ranked)
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::search::{
        ComponentRanks, FederatedHit, FederatedSourceId, FederatedSourceKind, MatchQualitySummary,
        TierMetadata,
    };

    fn hit(match_quality: MatchQualitySummary, answer_eligible: bool) -> FederatedHit {
        FederatedHit {
            source_kind: FederatedSourceKind::Event,
            source_id: FederatedSourceId::Event(1),
            sources: Vec::new(),
            evidence_ids: Vec::new(),
            title: String::new(),
            excerpt: String::new(),
            confidence: 1.0,
            period_start_ms: None,
            period_end_ms: None,
            component_ranks: ComponentRanks::default(),
            fused_score: 1.0,
            tiers: TierMetadata::default(),
            answer_eligible,
            match_quality,
        }
    }

    fn passed_floor() -> shogun_memory::search::RelevanceFloor {
        shogun_memory::search::RelevanceFloor {
            passed: true,
            reason: shogun_memory::search::RelevanceFloorReason::Passed,
            coverage: Some(1.0),
            threshold: shogun_memory::search::RELEVANCE_FLOOR_COVERAGE,
        }
    }

    #[test]
    fn a_failing_relevance_floor_lowers_an_answerable_verdict_to_low_quality() {
        // The daemon never withholds the candidates on the floor; the Memory API's
        // `answerable: false` follows from this verdict instead.
        let quality = MatchQualitySummary {
            lexical_quality: 0.5,
            term_coverage: 0.5,
            anchor_coverage: 1.0,
            anchor_count: 1,
        };
        let below = shogun_memory::search::RelevanceFloor {
            passed: false,
            reason: shogun_memory::search::RelevanceFloorReason::BelowFloor,
            coverage: Some(0.25),
            threshold: shogun_memory::search::RELEVANCE_FLOOR_COVERAGE,
        };
        let lowered = answerability("Who owns Orchid rollout delivery?", &[hit(quality, true)], &below);
        assert_eq!(lowered.reason, AnswerabilityReasonV1::LowQuality);
        assert!(!lowered.answerable);
        let kept = answerability("Who owns Orchid rollout delivery?", &[hit(quality, true)], &passed_floor());
        assert_eq!(kept.reason, AnswerabilityReasonV1::Answerable);
        // Weaker reasons are not rewritten: an anchorless top hit stays "insufficient signal".
        let anchorless = MatchQualitySummary { anchor_count: 0, ..quality };
        let kept = answerability("what happened in the meeting", &[hit(anchorless, true)], &below);
        assert_eq!(kept.reason, AnswerabilityReasonV1::InsufficientSignal);
    }

    #[test]
    fn partial_natural_wording_is_answerable() {
        let result = answerability(
            "Who owns Orchid rollout delivery?",
            &[hit(
                MatchQualitySummary {
                    lexical_quality: 0.5,
                    term_coverage: 0.5,
                    anchor_coverage: 1.0,
                    anchor_count: 1,
                },
                true,
            )],
            &passed_floor(),
        );

        assert_eq!(result.reason, AnswerabilityReasonV1::Answerable);
    }

    #[test]
    fn substring_only_quality_boost_is_not_answerable() {
        let result = answerability(
            "Phoenix art",
            &[hit(
                MatchQualitySummary {
                    lexical_quality: 1.0,
                    term_coverage: 0.5,
                    anchor_coverage: 1.0,
                    anchor_count: 1,
                },
                true,
            )],
            &passed_floor(),
        );

        assert_eq!(result.reason, AnswerabilityReasonV1::LowQuality);
    }

    #[test]
    fn unresolved_evidence_stays_ineligible() {
        let result = answerability(
            "Orchid rollout",
            &[hit(
                MatchQualitySummary {
                    lexical_quality: 1.0,
                    term_coverage: 1.0,
                    anchor_coverage: 1.0,
                    anchor_count: 1,
                },
                false,
            )],
            &passed_floor(),
        );

        assert_eq!(result.reason, AnswerabilityReasonV1::NoEligibleMatch);
    }
}
