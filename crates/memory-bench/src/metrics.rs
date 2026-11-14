//! How we measure.
//!
//! Percentiles come from [`spike_harness::stats::Percentiles`] rather than being recomputed here.
//! That crate is the designated carry-forward measurement asset and its nearest-rank
//! implementation is already unit-tested; a second percentile function in the tree would be a
//! second definition of p95, and the first time the two disagreed nobody would know which report
//! to believe.
//!
//! Individual samples are retained, not just their summary. A mean write latency of 2ms and a p99
//! of 400ms are the same mean, and only one of them is a product that feels broken.

use std::collections::HashMap;

use serde::Serialize;
use spike_harness::stats::Percentiles;

/// A latency sample set, kept in full so any percentile can be recomputed later.
#[derive(Debug, Default, Clone)]
pub struct LatencySeries {
    samples_ms: Vec<f64>,
}

impl LatencySeries {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_ns(&mut self, ns: u64) {
        self.samples_ms.push(ns as f64 / 1_000_000.0);
    }

    pub fn len(&self) -> usize {
        self.samples_ms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples_ms.is_empty()
    }

    pub fn samples_ms(&self) -> &[f64] {
        &self.samples_ms
    }

    /// `None` for an empty set — a percentile of nothing is not zero, and a report that printed
    /// `p95: 0.0ms` for a stage that never ran would read as a pass.
    pub fn percentiles(&self) -> Option<Percentiles> {
        Percentiles::of(&self.samples_ms)
    }

    pub fn mean_ms(&self) -> Option<f64> {
        if self.samples_ms.is_empty() {
            return None;
        }
        Some(self.samples_ms.iter().sum::<f64>() / self.samples_ms.len() as f64)
    }

    pub fn min_ms(&self) -> Option<f64> {
        self.samples_ms
            .iter()
            .copied()
            .fold(None, |acc: Option<f64>, x| {
                Some(match acc {
                    Some(a) => a.min(x),
                    None => x,
                })
            })
    }
}

/// Serializable latency summary. Mirrors [`Percentiles`] and adds the two fields it does not
/// carry, so a report never has to reach for the raw samples to say something ordinary.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LatencySummary {
    pub n: usize,
    pub min_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl LatencySummary {
    pub fn of(series: &LatencySeries) -> Option<Self> {
        let p = series.percentiles()?;
        Some(Self {
            n: p.n,
            min_ms: series.min_ms().unwrap_or(p.p50),
            mean_ms: series.mean_ms().unwrap_or(p.p50),
            p50_ms: p.p50,
            p95_ms: p.p95,
            p99_ms: p.p99,
            max_ms: p.max,
        })
    }
}

/// Ingest-side counters.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct WriteStats {
    /// Events handed to the backend.
    pub submitted: u64,
    /// Writes the backend absorbed into an existing row.
    pub deduplicated: u64,
    /// Merges that landed on a row carrying a *different* fact — information destroyed, not
    /// deduplicated (issue #221). The workload knows which fact each event carries, so the runner
    /// can check every reported merge against the row's owner. Always 0 for an honest backend;
    /// any non-zero value disqualifies the collapse rate as a score.
    pub wrong_merges: u64,
    /// Writes that returned an error.
    pub failed: u64,
    /// Rows in the log afterwards.
    pub rows_after: i64,
    /// Distinct facts the workload actually contained.
    pub unique_facts: u64,
    /// Repeat events the workload contained — what a perfect deduplicator would collapse.
    pub duplicate_events: u64,
}

impl WriteStats {
    /// Whether write-derived improvement scores are safe to compare.
    pub fn valid_for_improvement(&self) -> bool {
        self.wrong_merges == 0 && self.failed == 0
    }

    /// Rows held per distinct fact. 1.0 is perfect; 2.0 means the store is twice the size the
    /// information in it requires.
    ///
    /// This is the number that motivates the whole research direction, which is why it is derived
    /// from `rows_after` (what the database really holds) rather than from the number of writes
    /// we submitted.
    pub fn write_amplification(&self) -> Option<f64> {
        if self.unique_facts == 0 || !self.valid_for_improvement() {
            return None;
        }
        Some(self.rows_after as f64 / self.unique_facts as f64)
    }

    /// Of the repeats the workload contained, the share the backend recognised **correctly**.
    ///
    /// Only merges onto a row carrying the same fact count. A backend that combined two different
    /// memories would otherwise be rewarded for destroying information (issue #221): collapsing
    /// "Mom likes tea" into "Mom is allergic to tea" must never raise this number. Wrong merges
    /// disqualify the score entirely. Subtracting them is insufficient: destructive merging also
    /// shrinks `rows_after`, so publishing either write score would still reward data loss.
    ///
    /// `None` when the workload contained no repeats — in a clean corpus this metric has no
    /// denominator, and reporting 0% would suggest a failure where there was nothing to detect.
    pub fn duplicate_collapse_rate(&self) -> Option<f64> {
        if self.duplicate_events == 0 || !self.valid_for_improvement() {
            return None;
        }
        Some(self.deduplicated as f64 / self.duplicate_events as f64)
    }
}

/// Retrieval quality over the query set.
///
/// Recall and MRR are computed the same way [`shogun-memory`'s `retrieval_eval`] test computes
/// them, deliberately: two different definitions of recall@5 in one repository would make the
/// scale numbers and the quality numbers incomparable.
#[derive(Debug, Default, Clone)]
pub struct QualityAccumulator {
    /// 1-indexed rank of the first correct answer per query; `None` when none was returned.
    first_correct_rank: Vec<Option<usize>>,
    /// Queries where a superseded fact appeared anywhere in the returned window.
    stale_returned: u64,
    /// Queries where a superseded fact outranked every correct one — an actively wrong answer,
    /// not merely a stale row sitting further down the list.
    stale_outranked_current: u64,
    /// Queries whose expectations included at least one superseded fact (the denominator for the
    /// two counters above).
    temporal_queries: u64,
    failed: u64,
    /// 1-indexed rank by which *every* expected fact had been returned, per query that expected
    /// anything at all; `None` when at least one expected fact never came back.
    ///
    /// Separate from `first_correct_rank` because they answer different questions once a query
    /// needs more than one fact: recall asks whether *an* answer appeared, coverage asks whether
    /// the *whole* answer did. They are identical for single-fact queries, by construction.
    all_expected_rank: Vec<Option<usize>>,
    /// Queries needing more than one fact combined. Purely descriptive — it says whether coverage
    /// is measuring anything recall is not.
    multi_evidence_queries: u64,
    /// Queries that have no answer in the corpus at all.
    ///
    /// These are kept out of the recall denominator deliberately. An unanswerable question is not
    /// a retrieval miss, and counting it as one would report a system that abstains perfectly as
    /// having recall 0.
    abstention_queries: u64,
    /// Abstention queries that still came back with rows — evidence for an answer that does not
    /// exist.
    false_evidence: u64,
    /// Characters of event text returned per query (every query, answerable or not): the context
    /// a reader model would be handed per answer. Tokens are estimated at four characters each.
    evidence_chars: Vec<usize>,
}

impl QualityAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one query's outcome.
    ///
    /// `returned` is the ranked list of event ids; `fact_of` maps a row back to the fact it
    /// carries, which is how a duplicate row still counts as having found the answer.
    pub fn record(
        &mut self,
        returned: &[i64],
        fact_of: &HashMap<i64, String>,
        expected: &[String],
        superseded: &[String],
    ) {
        let rank_of = |set: &[String]| -> Option<usize> {
            returned.iter().enumerate().find_map(|(i, id)| {
                let fact = fact_of.get(id)?;
                set.iter().any(|e| e == fact).then_some(i + 1)
            })
        };

        // A question with no correct answer and nothing superseded is unanswerable. It gets its
        // own counters and never touches the recall denominator: abstaining correctly is a
        // success, and scoring it as a miss would make a perfectly-behaved system look broken.
        if expected.is_empty() && superseded.is_empty() {
            self.abstention_queries += 1;
            if !returned.is_empty() {
                self.false_evidence += 1;
            }
            return;
        }

        let correct = rank_of(expected);
        self.first_correct_rank.push(correct);

        if !expected.is_empty() {
            if expected.len() > 1 {
                self.multi_evidence_queries += 1;
            }
            // The rank by which the whole answer is in hand is the *worst* of the individual
            // ranks; if any piece never arrives, the question was never answerable from this list.
            let mut worst = 0usize;
            let mut complete = true;
            for want in expected {
                match returned.iter().enumerate().find_map(|(i, id)| {
                    let fact = fact_of.get(id)?;
                    (fact == want).then_some(i + 1)
                }) {
                    Some(rank) => worst = worst.max(rank),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            self.all_expected_rank.push(complete.then_some(worst));
        }

        if !superseded.is_empty() {
            self.temporal_queries += 1;
            if let Some(stale_rank) = rank_of(superseded) {
                self.stale_returned += 1;
                if correct.is_none_or(|c| stale_rank < c) {
                    self.stale_outranked_current += 1;
                }
            }
        }
    }

    /// Record how much event text one query returned, whatever its outcome.
    pub fn record_evidence_chars(&mut self, chars: usize) {
        self.evidence_chars.push(chars);
    }

    /// Mean characters of returned evidence per query; `None` before any query ran.
    pub fn evidence_chars_per_query(&self) -> Option<f64> {
        if self.evidence_chars.is_empty() {
            return None;
        }
        Some(self.evidence_chars.iter().sum::<usize>() as f64 / self.evidence_chars.len() as f64)
    }

    pub fn record_failure(&mut self) {
        self.failed += 1;
        // A failed query is a measured miss, not an absent sample. Omitting it from the denominator
        // would let a backend fail hard queries and inflate recall/MRR with only easy successes.
        self.first_correct_rank.push(None);
        self.all_expected_rank.push(None);
    }

    /// Share of queries whose *entire* expected answer appeared in the top `k`.
    pub fn coverage_at(&self, k: usize) -> Option<f64> {
        if self.all_expected_rank.is_empty() {
            return None;
        }
        let complete = self
            .all_expected_rank
            .iter()
            .filter(|r| matches!(r, Some(rank) if *rank <= k))
            .count();
        Some(complete as f64 / self.all_expected_rank.len() as f64)
    }

    pub fn queries(&self) -> usize {
        self.first_correct_rank.len()
    }

    /// Share of queries whose answer appeared in the top `k`.
    pub fn recall_at(&self, k: usize) -> Option<f64> {
        if self.first_correct_rank.is_empty() {
            return None;
        }
        let hits = self
            .first_correct_rank
            .iter()
            .filter(|r| matches!(r, Some(rank) if *rank <= k))
            .count();
        Some(hits as f64 / self.first_correct_rank.len() as f64)
    }

    /// Mean reciprocal rank — sensitive to how far down the answer sits, which recall@k is not.
    pub fn mrr(&self) -> Option<f64> {
        if self.first_correct_rank.is_empty() {
            return None;
        }
        let total: f64 = self
            .first_correct_rank
            .iter()
            .map(|r| r.map_or(0.0, |rank| 1.0 / rank as f64))
            .sum();
        Some(total / self.first_correct_rank.len() as f64)
    }

    /// Summarise, given the `k` results each query actually requested.
    ///
    /// A recall@k the run never measured is `None`, not a relabeled smaller recall: with `--k 1`
    /// the returned lists are one row long, so "recall@5" would be recall@1 wearing the wrong
    /// name (issue #221).
    pub fn summary(&self, retrieval_k: usize) -> QualitySummary {
        let recall_if_measured =
            |k: usize| -> Option<f64> { (retrieval_k >= k).then(|| self.recall_at(k)).flatten() };
        QualitySummary {
            queries: self.queries(),
            failed: self.failed,
            recall_at_1: recall_if_measured(1),
            recall_at_5: recall_if_measured(5),
            recall_at_10: recall_if_measured(10),
            mrr: self.mrr(),
            temporal_queries: self.temporal_queries,
            stale_returned: self.stale_returned,
            stale_outranked_current: self.stale_outranked_current,
            stale_rate: (self.temporal_queries > 0)
                .then(|| self.stale_returned as f64 / self.temporal_queries as f64),
            stale_outranked_rate: (self.temporal_queries > 0)
                .then(|| self.stale_outranked_current as f64 / self.temporal_queries as f64),
            abstention_queries: self.abstention_queries,
            false_evidence: self.false_evidence,
            false_evidence_rate: (self.abstention_queries > 0)
                .then(|| self.false_evidence as f64 / self.abstention_queries as f64),
            multi_evidence_queries: self.multi_evidence_queries,
            // Coverage is only reported when some query actually needed more than one fact.
            // On a single-fact workload it would equal recall exactly, and publishing the same
            // number under two names invites reading it as corroboration.
            coverage_at_5: (self.multi_evidence_queries > 0 && retrieval_k >= 5)
                .then(|| self.coverage_at(5))
                .flatten(),
            coverage_at_10: (self.multi_evidence_queries > 0 && retrieval_k >= 10)
                .then(|| self.coverage_at(10))
                .flatten(),
            evidence_chars_per_query: self.evidence_chars_per_query(),
            evidence_tokens_per_query: self
                .evidence_chars_per_query()
                .map(|chars| chars / CHARS_PER_TOKEN),
        }
    }
}

/// Rough token estimate for returned evidence: four characters per token, the usual English
/// figure. It is a size indicator for `benchmarks.json`, not a tokenizer.
pub const CHARS_PER_TOKEN: f64 = 4.0;

/// Serializable form of [`QualityAccumulator`]. Every rate is `Option` so a workload that cannot
/// express a metric reports `null` rather than a number that looks like a measurement.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct QualitySummary {
    pub queries: usize,
    pub failed: u64,
    pub recall_at_1: Option<f64>,
    pub recall_at_5: Option<f64>,
    pub recall_at_10: Option<f64>,
    pub mrr: Option<f64>,
    pub temporal_queries: u64,
    pub stale_returned: u64,
    pub stale_outranked_current: u64,
    pub stale_rate: Option<f64>,
    pub stale_outranked_rate: Option<f64>,
    /// Questions with no answer in the corpus. `0` on every workload but `abstention`.
    pub abstention_queries: u64,
    /// Of those, the ones that still got rows back.
    pub false_evidence: u64,
    /// `false_evidence / abstention_queries`. The share of unanswerable questions for which the
    /// retrieval path handed a downstream agent evidence anyway.
    pub false_evidence_rate: Option<f64>,
    /// Questions needing more than one fact combined.
    pub multi_evidence_queries: u64,
    /// Share of queries whose whole expected answer was inside the top 5. `None` unless some
    /// query needed more than one fact, where it would merely restate recall.
    pub coverage_at_5: Option<f64>,
    pub coverage_at_10: Option<f64>,
    /// Mean characters of event text returned per query — what a reader would be handed.
    pub evidence_chars_per_query: Option<f64>,
    /// The same at [`CHARS_PER_TOKEN`] characters per token.
    pub evidence_tokens_per_query: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact_map(pairs: &[(i64, &str)]) -> HashMap<i64, String> {
        pairs.iter().map(|(id, f)| (*id, f.to_string())).collect()
    }

    #[test]
    fn empty_series_reports_nothing_rather_than_zero() {
        let s = LatencySeries::new();
        assert!(s.percentiles().is_none());
        assert!(LatencySummary::of(&s).is_none());
    }

    #[test]
    fn latency_summary_matches_known_percentiles() {
        let mut s = LatencySeries::new();
        for i in 1..=100 {
            s.record_ns(i * 1_000_000); // 1ms .. 100ms
        }
        let sum = LatencySummary::of(&s).expect("summary");
        assert_eq!(sum.n, 100);
        assert_eq!(sum.p50_ms, 50.0);
        assert_eq!(sum.p95_ms, 95.0);
        assert_eq!(sum.p99_ms, 99.0);
        assert_eq!(sum.max_ms, 100.0);
        assert_eq!(sum.min_ms, 1.0);
        assert!((sum.mean_ms - 50.5).abs() < 1e-9);
    }

    #[test]
    fn write_amplification_is_rows_over_facts() {
        let w = WriteStats {
            rows_after: 1500,
            unique_facts: 1000,
            ..Default::default()
        };
        assert_eq!(w.write_amplification(), Some(1.5));
    }

    #[test]
    fn amplification_and_collapse_are_none_without_a_denominator() {
        let w = WriteStats::default();
        assert!(w.write_amplification().is_none());
        assert!(w.duplicate_collapse_rate().is_none());
    }

    #[test]
    fn wrong_merges_disqualify_all_write_improvement_scores() {
        // Issue #221: a backend that combines two *different* memories destroys information and
        // must not receive a score derived from the smaller, damaged database.
        let w = WriteStats {
            deduplicated: 150,
            wrong_merges: 100,
            rows_after: 200,
            unique_facts: 1_000,
            duplicate_events: 300,
            ..Default::default()
        };
        assert!(!w.valid_for_improvement());
        assert!(w.write_amplification().is_none());
        assert!(w.duplicate_collapse_rate().is_none());
    }

    #[test]
    fn failed_writes_disqualify_write_improvement_scores() {
        let w = WriteStats {
            failed: 1,
            rows_after: 299,
            unique_facts: 300,
            duplicate_events: 300,
            ..Default::default()
        };
        assert!(w.write_amplification().is_none());
        assert!(w.duplicate_collapse_rate().is_none());
    }

    #[test]
    fn recall_at_k_never_measured_is_null_not_a_relabel() {
        // Issue #221: with --k 1 the returned lists are one row long, so "recall@5" would just be
        // recall@1 wearing the wrong name. It must come back None.
        let mut q = QualityAccumulator::new();
        let mut fact_of = HashMap::new();
        fact_of.insert(1_i64, "f-a".to_string());
        q.record(&[1], &fact_of, &["f-a".to_string()], &[]);
        let s = q.summary(1);
        assert_eq!(s.recall_at_1, Some(1.0));
        assert!(s.recall_at_5.is_none(), "k=1 never measured the top five");
        assert!(s.recall_at_10.is_none(), "k=1 never measured the top ten");
        let s = q.summary(5);
        assert_eq!(s.recall_at_5, Some(1.0));
        assert!(s.recall_at_10.is_none());
    }

    #[test]
    fn collapse_rate_counts_recognised_repeats() {
        let w = WriteStats {
            deduplicated: 150,
            duplicate_events: 300,
            ..Default::default()
        };
        assert_eq!(w.duplicate_collapse_rate(), Some(0.5));
    }

    #[test]
    fn recall_and_mrr_track_the_rank_of_the_answer() {
        let facts = fact_map(&[(1, "a"), (2, "b"), (3, "c")]);
        let mut q = QualityAccumulator::new();
        // Answer at rank 1.
        q.record(&[1, 2, 3], &facts, &["a".into()], &[]);
        // Answer at rank 3.
        q.record(&[1, 2, 3], &facts, &["c".into()], &[]);
        assert_eq!(q.recall_at(1), Some(0.5));
        assert_eq!(q.recall_at(5), Some(1.0));
        // (1/1 + 1/3) / 2
        let mrr = q.mrr().expect("mrr");
        assert!((mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_missed_answer_scores_zero_not_none() {
        let facts = fact_map(&[(1, "a")]);
        let mut q = QualityAccumulator::new();
        q.record(&[1], &facts, &["missing".into()], &[]);
        assert_eq!(q.recall_at(10), Some(0.0));
        assert_eq!(q.mrr(), Some(0.0));
    }

    #[test]
    fn a_failed_query_stays_in_quality_denominators() {
        let facts = fact_map(&[(1, "a")]);
        let mut q = QualityAccumulator::new();
        q.record(&[1], &facts, &["a".into()], &[]);
        q.record_failure();
        let summary = q.summary(10);
        assert_eq!(summary.queries, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.recall_at_1, Some(0.5));
        assert_eq!(summary.mrr, Some(0.5));
    }

    #[test]
    fn stale_is_counted_separately_from_outranking() {
        let facts = fact_map(&[(1, "old"), (2, "new")]);
        let mut q = QualityAccumulator::new();
        // Current answer first, stale below it: returned, but not outranking.
        q.record(&[2, 1], &facts, &["new".into()], &["old".into()]);
        // Stale first: actively wrong.
        q.record(&[1, 2], &facts, &["new".into()], &["old".into()]);
        let s = q.summary(10);
        assert_eq!(s.temporal_queries, 2);
        assert_eq!(s.stale_returned, 2);
        assert_eq!(s.stale_outranked_current, 1);
        assert_eq!(s.stale_rate, Some(1.0));
        assert_eq!(s.stale_outranked_rate, Some(0.5));
    }

    #[test]
    fn stale_outranks_when_the_current_answer_is_absent_entirely() {
        let facts = fact_map(&[(1, "old")]);
        let mut q = QualityAccumulator::new();
        q.record(&[1], &facts, &["new".into()], &["old".into()]);
        let s = q.summary(10);
        assert_eq!(s.stale_outranked_current, 1);
    }

    #[test]
    fn a_clean_workload_reports_null_staleness_not_zero() {
        let facts = fact_map(&[(1, "a")]);
        let mut q = QualityAccumulator::new();
        q.record(&[1], &facts, &["a".into()], &[]);
        let s = q.summary(10);
        assert_eq!(s.temporal_queries, 0);
        assert!(s.stale_rate.is_none());
    }
}
