//! Hot layer — the last ~24h of events held in RAM (FR-MEM-01/02).
//!
//! The Hot layer is a *cache*, never the source of truth: every event is written to Warm
//! (the DB) first, then mirrored here for fast context assembly. It is bounded two ways:
//! a time window (default 24h) and a byte budget (default 200MB, FR-MEM-02). When the budget
//! is exceeded the oldest events are evicted (folded away); the count of evicted events is
//! kept so the caller can decide when to summarise. On process start the layer is rebuilt from
//! Warm ([`HotLayer::rebuild_from_warm_in_place`]) — Hot holds nothing that isn't already durable.

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::Connection;

pub const HOT_CONTENT_MAX_BYTES: usize = 64 * 1024;
pub const HOT_TAG_MAX_BYTES: usize = 4 * 1024;
pub const HOT_METADATA_MAX_BYTES: usize = 16 * 1024;
pub const HOT_PROJECTED_ROW_MAX_BYTES: usize = HOT_CONTENT_MAX_BYTES
    + 2 * HOT_TAG_MAX_BYTES
    + 3 * HOT_METADATA_MAX_BYTES;
pub const HOT_PROJECTED_ROW_STAGING_BYTES: usize = 2 * HOT_PROJECTED_ROW_MAX_BYTES + 4 * 1024;
pub const HOT_READ_OUTPUT_RESERVE_BYTES: usize = 32 * 1024 * 1024;
const HOT_SNAPSHOT_MAX_EVENTS: usize = 128;
const HOT_SNAPSHOT_MAX_SOURCES: usize = 8;
const HOT_SLOT_MAX_EVENTS: usize = 65_536;

/// One event mirrored in the Hot layer. Carries only what context assembly needs; the full row
/// stays in Warm.
#[derive(Clone, PartialEq, Eq)]
pub struct HotEvent {
    pub id: i64,
    pub ts: i64,
    pub last_seen_at: i64,
    pub source: Box<str>,
    pub kind: Box<str>,
    pub app_bundle_id: Option<Box<str>>,
    pub window_title: Option<Box<str>>,
    pub thread_key: Option<Box<str>>,
    pub content: Box<str>,
    pub content_len_chars: usize,
    pub content_truncated: bool,
    pub source_truncated: bool,
    pub kind_truncated: bool,
    pub app_bundle_id_truncated: bool,
    pub window_title_truncated: bool,
    pub thread_key_truncated: bool,
    pub dwell_ms: i64,
    pub display_id: Option<i64>,
}

impl HotEvent {
    /// Exact heap allocation owned by this entry's boxed strings. The fixed event allocation and
    /// pointer slot are accounted separately by [`HotLayer::recount_bytes`].
    fn approx_bytes(&self) -> usize {
        self.source.len()
            + self.kind.len()
            + self.app_bundle_id.as_ref().map_or(0, |value| value.len())
            + self.window_title.as_ref().map_or(0, |value| value.len())
            + self.thread_key.as_ref().map_or(0, |value| value.len())
            + self.content.len()
    }

    fn is_bounded(&self) -> bool {
        self.content.len() <= HOT_CONTENT_MAX_BYTES
            && self.source.len() <= HOT_TAG_MAX_BYTES
            && self.kind.len() <= HOT_TAG_MAX_BYTES
            && self.app_bundle_id.as_ref().is_none_or(|value| value.len() <= HOT_METADATA_MAX_BYTES)
            && self.window_title.as_ref().is_none_or(|value| value.len() <= HOT_METADATA_MAX_BYTES)
            && self.thread_key.as_ref().is_none_or(|value| value.len() <= HOT_METADATA_MAX_BYTES)
    }
}

fn bounded_head(content: &str, max_chars: usize, ellipsis: bool) -> String {
    let end = content.char_indices().nth(max_chars).map_or(content.len(), |(index, _)| index);
    let head = content[..end].trim();
    exact_join(head, if ellipsis && end < content.len() { "…" } else { "" })
}

fn exact_string(value: &str) -> String {
    exact_boxed_join(value, "").into_string()
}

fn exact_join(value: &str, suffix: &str) -> String {
    exact_boxed_join(value, suffix).into_string()
}

pub(crate) fn exact_boxed_str(value: &str) -> Box<str> {
    exact_boxed_join(value, "")
}

fn exact_boxed_join(value: &str, suffix: &str) -> Box<str> {
    let Some(len) = value.len().checked_add(suffix.len()) else { return Box::from("") };
    let mut joined = String::with_capacity(len);
    joined.push_str(value);
    joined.push_str(suffix);
    joined.into_boxed_str()
}

fn exact_empty_slots(len: usize) -> Box<[Option<Box<HotEvent>>]> {
    vec![None; len].into_boxed_slice()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotFold {
    pub from_seen_at: i64,
    pub to_seen_at: i64,
    pub first_event_id: i64,
    pub last_event_id: i64,
    pub event_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotBudgetSnapshot {
    pub resident_bytes: usize,
    pub staging_bytes: usize,
    pub peak_bytes: usize,
}

impl HotBudgetSnapshot {
    pub fn total_bytes(self) -> usize {
        self.resident_bytes.saturating_add(self.staging_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotPreviewCandidate {
    pub source: String,
    pub preview: crate::event_log::RecentEventPreview,
    pub lossy: bool,
}

pub struct HotContextCandidates {
    generation: u64,
    per_source_ids: Vec<(String, Vec<i64>)>,
    candidates: Vec<HotPreviewCandidate>,
    _lease: HotReadLease,
}

pub struct HotThreadCandidates {
    generation: u64,
    newest_first_ids: Vec<i64>,
    oldest_first: Vec<(i64, i64, String)>,
    lossy: bool,
    _lease: HotReadLease,
}

impl HotContextCandidates {
    pub fn generation(&self) -> u64 { self.generation }
    pub fn per_source_ids(&self) -> &[(String, Vec<i64>)] { &self.per_source_ids }
    pub fn candidates(&self) -> &[HotPreviewCandidate] { &self.candidates }

    /// Explicit ownership handoff after durable completeness validation. Candidate strings move;
    /// no snapshot-sized clone can coexist with the Hot DTO.
    pub fn into_candidates(self) -> Vec<HotPreviewCandidate> { self.candidates }
}

impl HotThreadCandidates {
    pub fn generation(&self) -> u64 { self.generation }
    pub fn newest_first_ids(&self) -> &[i64] { &self.newest_first_ids }
    pub fn oldest_first(&self) -> &[(i64, i64, String)] { &self.oldest_first }
    pub fn is_lossy(&self) -> bool { self.lossy }

    /// Explicit ownership handoff after durable thread-tail validation.
    pub fn into_oldest_first(self) -> Vec<(i64, i64, String)> { self.oldest_first }
}

struct HotReadLease {
    in_flight: Arc<AtomicBool>,
}

impl HotReadLease {
    fn acquire(in_flight: &Arc<AtomicBool>) -> Option<Self> {
        in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { in_flight: in_flight.clone() })
    }
}

impl Drop for HotReadLease {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

fn excerpt_capacity(excerpt_chars: usize) -> usize {
    HOT_CONTENT_MAX_BYTES
        .min(excerpt_chars.saturating_mul(4))
        .saturating_add(3)
}

fn context_snapshot_bytes(
    source_count: usize,
    per_source_limit: usize,
    excerpt_chars: usize,
) -> Option<usize> {
    let event_count = source_count.checked_mul(per_source_limit)?;
    let excerpt_bytes = excerpt_capacity(excerpt_chars);
    let per_event = size_of::<&HotEvent>()
        .checked_add(size_of::<HotPreviewCandidate>())?
        .checked_add(size_of::<(String, crate::event_log::RecentEventPreview)>())?
        .checked_add(HOT_TAG_MAX_BYTES)?
        .checked_add(2 * HOT_METADATA_MAX_BYTES)?
        .checked_add(excerpt_bytes)?
        // The merge retains one normalized key per candidate. Two String-sized slots cover the
        // HashSet load-factor workspace; lowercase expansion is bounded conservatively at 2x.
        .checked_add(2 * size_of::<String>())?
        .checked_add(2 * excerpt_bytes)?;
    let event_bytes = event_count.checked_mul(per_event)?;
    let per_source = size_of::<(String, Vec<i64>)>()
        .checked_add(HOT_TAG_MAX_BYTES)?
        .checked_add(per_source_limit.checked_mul(size_of::<i64>())?)?;
    // One normalization is transient: worst-case split words, joined text, and lowercase output.
    let word_slots = excerpt_bytes.saturating_add(1) / 2;
    let normalization_workspace = word_slots
        .checked_mul(2 * size_of::<&str>())?
        .checked_add(3 * excerpt_bytes)?;
    event_bytes
        .checked_add(source_count.checked_mul(per_source)?)?
        .checked_add(normalization_workspace)
}

fn thread_snapshot_bytes(limit: usize, excerpt_chars: usize) -> Option<usize> {
    let per_event = size_of::<&HotEvent>()
        .checked_add(size_of::<i64>())?
        .checked_add(size_of::<(i64, i64, String)>())?
        .checked_add(excerpt_capacity(excerpt_chars))?;
    limit.checked_mul(per_event)
}

/// Bounds for the Hot layer (FR-MEM-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotBounds {
    pub window_ms: i64,
    pub max_bytes: usize,
}

impl Default for HotBounds {
    fn default() -> Self {
        Self { window_ms: 24 * 60 * 60 * 1000, max_bytes: 200 * 1024 * 1024 }
    }
}

/// The in-RAM Hot layer.
pub struct HotLayer {
    bounds: HotBounds,
    buf: Box<[Option<Box<HotEvent>>]>,
    len: usize,
    bytes: usize,
    staging_bytes: usize,
    peak_bytes: usize,
    generation: u64,
    /// Total events evicted by the byte budget since construction (fold-to-summary signal).
    fold: Option<HotFold>,
    read_in_flight: Option<Arc<AtomicBool>>,
}

impl HotLayer {
    pub fn new(bounds: HotBounds) -> Self {
        let full_staging = HOT_PROJECTED_ROW_STAGING_BYTES
            .saturating_add(HOT_READ_OUTPUT_RESERVE_BYTES);
        let staging_bytes = full_staging.min(bounds.max_bytes);
        let available = bounds.max_bytes.saturating_sub(staging_bytes);
        let slot_cost = size_of::<Option<Box<HotEvent>>>()
            .saturating_add(size_of::<HotEvent>());
        let slot_count = (available / slot_cost).min(HOT_SLOT_MAX_EVENTS);
        let slots = exact_empty_slots(slot_count);
        let mut layer = Self {
            bounds,
            buf: slots,
            len: 0,
            bytes: 0,
            staging_bytes,
            peak_bytes: staging_bytes,
            generation: 0,
            fold: None,
            read_in_flight: (bounds.max_bytes >= full_staging)
                .then(|| Arc::new(AtomicBool::new(false))),
        };
        layer.recount_bytes();
        layer
    }

    pub fn with_defaults() -> Self {
        Self::new(HotBounds::default())
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn byte_size(&self) -> usize {
        self.bytes.saturating_add(self.staging_bytes)
    }

    pub fn budget_snapshot(&self) -> HotBudgetSnapshot {
        HotBudgetSnapshot {
            resident_bytes: self.bytes,
            staging_bytes: self.staging_bytes,
            peak_bytes: self.peak_bytes,
        }
    }

    pub fn evicted_count(&self) -> u64 {
        self.folded_count()
    }

    pub fn folded_count(&self) -> u64 {
        self.fold.map_or(0, |fold| fold.event_count)
    }

    pub fn folds(&self) -> std::option::Iter<'_, HotFold> {
        self.fold.iter()
    }

    /// Reconcile an event and evict/fold oldest entries until the strict byte budget holds.
    pub fn push(&mut self, event: HotEvent) {
        let now = event.last_seen_at;
        self.reconcile(event, now);
    }

    pub fn reconcile(&mut self, event: HotEvent, now: i64) -> bool {
        if !event.is_bounded() || event.source_truncated || event.kind_truncated {
            return false;
        }
        self.remove(event.id);
        self.evict_older_than(now);
        let cutoff = now.saturating_sub(self.bounds.window_ms.max(0));
        if event.last_seen_at < cutoff { return false; }
        if !self.make_room_for(&event) {
            return false;
        }
        let key = (event.last_seen_at, event.id);
        let index = self.find_insert_index(key);
        self.insert_boxed(index, Box::new(event));
        self.generation = self.generation.wrapping_add(1);
        self.recount_bytes();
        true
    }

    /// Drop events older than the time window relative to `now` (the 24h horizon). Returns the
    /// number dropped. Time-based eviction is not folded; folds describe byte pressure only.
    pub fn evict_older_than(&mut self, now: i64) -> usize {
        let cutoff = now.saturating_sub(self.bounds.window_ms.max(0));
        let mut dropped = 0;
        while let Some(front) = self.buf.first().and_then(Option::as_deref) {
            if front.last_seen_at < cutoff {
                if self.pop_front().is_some() {
                    dropped += 1;
                }
            } else {
                break;
            }
        }
        if dropped > 0 {
            self.generation = self.generation.wrapping_add(1);
        }
        self.recount_bytes();
        dropped
    }

    pub fn context_candidates(
        &mut self,
        now: i64,
        sources: &[&str],
        per_source_limit: usize,
        excerpt_chars: usize,
    ) -> Option<HotContextCandidates> {
        self.evict_older_than(now);
        let excerpt_chars = excerpt_chars.clamp(1, HOT_CONTENT_MAX_BYTES);
        let event_limit = sources.len().checked_mul(per_source_limit)?;
        if sources.len() > HOT_SNAPSHOT_MAX_SOURCES
            || sources.iter().any(|source| source.len() > HOT_TAG_MAX_BYTES)
            || event_limit > HOT_SNAPSHOT_MAX_EVENTS
            || !self.snapshot_request_fits(
                context_snapshot_bytes(sources.len(), per_source_limit, excerpt_chars)?,
            )
        {
            return None;
        }
        let lease = HotReadLease::acquire(self.read_in_flight.as_ref()?)?;
        let mut per_source_ids = Vec::with_capacity(sources.len());
        let mut candidates = Vec::with_capacity(event_limit);
        for source in sources {
            let mut events = Vec::with_capacity(per_source_limit);
            for event in self.events().filter(|event| {
                !event.source_truncated && event.source.as_ref() == *source
            }) {
                let index = events
                    .binary_search_by(|candidate: &&HotEvent| candidate.id.cmp(&event.id).reverse())
                    .unwrap_or_else(|index| index);
                if index < per_source_limit {
                    if events.len() == per_source_limit {
                        events.pop();
                    }
                    events.insert(index, event);
                }
            }
            per_source_ids.push((exact_string(source), events.iter().map(|event| event.id).collect()));
            for event in events {
                let excerpt = bounded_head(&event.content, excerpt_chars, false);
                let content_complete = !event.content_truncated
                    || event.content.chars().count() >= excerpt_chars;
                candidates.push(HotPreviewCandidate {
                    source: exact_string(source),
                    preview: crate::event_log::RecentEventPreview {
                        id: event.id,
                        ts: event.ts,
                        app_bundle_id: event.app_bundle_id.as_deref().map(exact_string),
                        window_title: event.window_title.as_deref().map(exact_string),
                        excerpt,
                        content_len: event.content_len_chars,
                        dwell_ms: event.dwell_ms,
                        display_id: event.display_id,
                    },
                    lossy: !content_complete
                        || event.app_bundle_id_truncated
                        || event.window_title_truncated,
                });
            }
        }
        Some(HotContextCandidates {
            generation: self.generation,
            per_source_ids,
            candidates,
            _lease: lease,
        })
    }

    pub fn thread_candidates(
        &mut self,
        now: i64,
        thread_key: &str,
        limit: usize,
        excerpt_chars: usize,
    ) -> Option<HotThreadCandidates> {
        self.evict_older_than(now);
        let excerpt_chars = excerpt_chars.clamp(1, HOT_CONTENT_MAX_BYTES);
        if thread_key.len() > HOT_METADATA_MAX_BYTES
            || limit > HOT_SNAPSHOT_MAX_EVENTS
            || !self.snapshot_request_fits(thread_snapshot_bytes(limit, excerpt_chars)?)
        {
            return None;
        }
        let lease = HotReadLease::acquire(self.read_in_flight.as_ref()?)?;
        let mut events = Vec::with_capacity(limit);
        for event in self.events().filter(|event| {
            !event.thread_key_truncated && event.thread_key.as_deref() == Some(thread_key)
        }) {
            let index = events
                .binary_search_by(|candidate: &&HotEvent| {
                    (candidate.ts, candidate.id).cmp(&(event.ts, event.id)).reverse()
                })
                .unwrap_or_else(|index| index);
            if index < limit {
                if events.len() == limit {
                    events.pop();
                }
                events.insert(index, event);
            }
        }
        let newest_first_ids = events.iter().map(|event| event.id).collect();
        let lossy = events.iter().any(|event| {
            event.content_truncated && event.content.chars().count() < excerpt_chars
        });
        let oldest_first = events.into_iter().rev()
            .map(|event| {
                (
                    event.id,
                    event.ts,
                    bounded_head(&event.content, excerpt_chars, true),
                )
            })
            .collect::<Vec<_>>();
        Some(HotThreadCandidates {
            generation: self.generation,
            newest_first_ids,
            oldest_first,
            lossy,
            _lease: lease,
        })
    }

    pub fn remove(&mut self, event_id: i64) -> bool {
        let Some(index) = self.events().position(|event| event.id == event_id) else { return false };
        self.remove_at(index);
        self.generation = self.generation.wrapping_add(1);
        self.recount_bytes();
        true
    }

    pub fn remove_since(&mut self, cutoff_ts: i64) -> usize {
        let before = self.len;
        let mut index = 0;
        while index < self.len {
            let remove = self.buf[index].as_deref().is_some_and(|event| event.ts >= cutoff_ts);
            if remove {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
        if self.len != before {
            self.generation = self.generation.wrapping_add(1);
        }
        self.recount_bytes();
        before.saturating_sub(self.len)
    }

    pub fn clear(&mut self) {
        for slot in &mut self.buf[..self.len] {
            *slot = None;
        }
        self.len = 0;
        self.fold = None;
        self.generation = self.generation.wrapping_add(1);
        self.recount_bytes();
    }

    /// Rebuild from Warm on startup (FR-MEM-02): load events within the time window, oldest
    /// first, applying the byte budget as they load. Replaces any current contents.
    pub fn rebuild_from_warm_in_place(
        &mut self,
        conn: &Connection,
        now: i64,
        mut should_stop: impl FnMut() -> bool,
    ) -> Result<bool, rusqlite::Error> {
        self.clear();
        if should_stop() {
            return Ok(false);
        }
        let cutoff = now.saturating_sub(self.bounds.window_ms.max(0));
        let complete = crate::event_log::visit_hot_events_since(conn, cutoff, |event| {
            if should_stop() {
                return false;
            }
            self.reconcile(event, now);
            true
        })?;
        if !complete {
            self.clear();
        }
        Ok(complete)
    }

    fn recount_bytes(&mut self) {
        self.bytes = self.buf.len().saturating_mul(size_of::<Option<Box<HotEvent>>>())
            .saturating_add(self.len.saturating_mul(size_of::<HotEvent>()))
            .saturating_add(self.events().map(HotEvent::approx_bytes).sum::<usize>())
            .saturating_add(self.fold.map_or(0, |_| size_of::<HotFold>()));
        self.update_peak();
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn make_room_for(&mut self, event: &HotEvent) -> bool {
        let added = size_of::<HotEvent>().saturating_add(event.approx_bytes());
        loop {
            if self.len < self.buf.len()
                && self.byte_size().saturating_add(added) <= self.bounds.max_bytes
            {
                return true;
            }
            if let Some(oldest) = self.pop_front() {
                self.add_fold(&oldest);
                self.recount_bytes();
            } else {
                return false;
            }
        }
    }

    fn events(&self) -> impl DoubleEndedIterator<Item = &HotEvent> {
        self.buf[..self.len].iter().filter_map(Option::as_deref)
    }

    fn find_insert_index(&self, key: (i64, i64)) -> usize {
        let mut left = 0;
        let mut right = self.len;
        while left < right {
            let middle = left + (right - left) / 2;
            let Some(candidate) = self.buf[middle].as_deref() else { return middle };
            if (candidate.last_seen_at, candidate.id) < key {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }

    fn insert_boxed(&mut self, index: usize, event: Box<HotEvent>) {
        for slot in (index..self.len).rev() {
            self.buf[slot + 1] = self.buf[slot].take();
        }
        self.buf[index] = Some(event);
        self.len += 1;
    }

    fn remove_at(&mut self, index: usize) -> Option<Box<HotEvent>> {
        let removed = self.buf.get_mut(index)?.take();
        for slot in index..self.len.saturating_sub(1) {
            self.buf[slot] = self.buf[slot + 1].take();
        }
        if self.len > 0 {
            self.len -= 1;
            self.buf[self.len] = None;
        }
        removed
    }

    fn pop_front(&mut self) -> Option<Box<HotEvent>> {
        self.remove_at(0)
    }

    fn update_peak(&mut self) {
        self.peak_bytes = self.peak_bytes.max(self.byte_size());
    }

    fn snapshot_request_fits(&self, bytes: usize) -> bool {
        let full_reserve = HOT_PROJECTED_ROW_STAGING_BYTES
            .saturating_add(HOT_READ_OUTPUT_RESERVE_BYTES);
        self.staging_bytes >= full_reserve && bytes <= HOT_READ_OUTPUT_RESERVE_BYTES
    }

    fn add_fold(&mut self, event: &HotEvent) {
        if let Some(fold) = self.fold.as_mut() {
            fold.from_seen_at = fold.from_seen_at.min(event.last_seen_at);
            fold.to_seen_at = fold.to_seen_at.max(event.last_seen_at);
            fold.first_event_id = fold.first_event_id.min(event.id);
            fold.last_event_id = fold.last_event_id.max(event.id);
            fold.event_count = fold.event_count.saturating_add(1);
        } else {
            self.fold = Some(HotFold {
                from_seen_at: event.last_seen_at,
                to_seen_at: event.last_seen_at,
                first_event_id: event.id,
                last_event_id: event.id,
                event_count: 1,
            });
        }
    }

    #[cfg(test)]
    fn recent(&mut self, now: i64, limit: usize) -> Vec<HotEvent> {
        self.evict_older_than(now);
        self.events().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert, insert_or_touch, NewEvent};

    const TEST_STAGING_BYTES: usize =
        HOT_PROJECTED_ROW_STAGING_BYTES + HOT_READ_OUTPUT_RESERVE_BYTES;

    fn ev(id: i64, ts: i64, content: &str) -> HotEvent {
        HotEvent {
            id, ts, last_seen_at: ts, source: "capture".into(), kind: "text".into(),
            app_bundle_id: None, window_title: None, thread_key: Some("capture:test".into()),
            content: content.into(), content_len_chars: content.chars().count(),
            content_truncated: false, source_truncated: false, kind_truncated: false,
            app_bundle_id_truncated: false, window_title_truncated: false,
            thread_key_truncated: false, dwell_ms: 0, display_id: None,
        }
    }

    #[test]
    fn recent_returns_newest_first() {
        let mut h = HotLayer::with_defaults();
        h.push(ev(1, 10, "a"));
        h.push(ev(2, 20, "b"));
        h.push(ev(3, 30, "c"));
        let r = h.recent(30, 2);
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 2]);
    }

    #[test]
    fn byte_budget_evicts_oldest_and_counts() {
        // Tiny budget so a few entries overflow.
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 2_500,
        };
        let mut h = HotLayer::new(bounds);
        for i in 0..100 {
            h.push(ev(i, i * 10, &"x".repeat(50))); // ~114 bytes each
        }
        assert!(h.byte_size() <= bounds.max_bytes);
        assert!(h.evicted_count() >= 1);
        // The newest survivor must still be present.
        assert_eq!(h.recent(990, 1)[0].id, 99);
    }

    #[test]
    fn oversized_single_event_never_exceeds_the_budget() {
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 70 * 1024,
        };
        let mut h = HotLayer::new(bounds);
        h.push(ev(1, 0, &"y".repeat(1000))); // far over budget
        assert!(h.byte_size() <= bounds.max_bytes);
    }

    #[test]
    fn zero_budget_retains_no_allocation() {
        let bounds = HotBounds { window_ms: i64::MAX, max_bytes: 0 };
        let mut h = HotLayer::new(bounds);
        h.push(ev(1, 0, "oversized for a zero budget"));
        assert_eq!(h.byte_size(), 0);
        assert!(h.recent(0, 1).is_empty());
    }

    #[test]
    fn time_window_eviction() {
        let bounds = HotBounds { window_ms: 100, max_bytes: usize::MAX };
        let mut h = HotLayer::new(bounds);
        h.reconcile(ev(1, 0, "old"), 50);
        h.reconcile(ev(2, 50, "mid"), 50);
        h.reconcile(ev(3, 200, "new"), 50);
        let dropped = h.evict_older_than(200); // cutoff = 100 → ids 1,2 drop
        assert_eq!(dropped, 2);
        assert_eq!(h.recent(200, 9).iter().map(|e| e.id).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn rebuild_loads_recent_from_warm() {
        let conn = crate::open_in_memory().unwrap();
        // Two events: one inside the 24h window, one far outside.
        let now = 100_000_000_000i64;
        for (ts, hash, content) in [(now - 1000, "h1", "inside"), (now - 48 * 3600 * 1000, "h2", "outside")] {
            insert(
                &conn,
                &NewEvent {
                    ts,
                    source: "capture",
                    kind: "text",
                    app_bundle_id: None,
                    window_title: None,
                    content,
                    content_hash: hash,
                    dwell_ms: 0,
                    display_id: None,
                    window_bounds: None,
                },
            )
            .unwrap();
        }
        let mut h = HotLayer::with_defaults();
        assert!(h.rebuild_from_warm_in_place(&conn, now, || false).unwrap());
        assert_eq!(h.len(), 1);
        assert_eq!(h.recent(now, 1)[0].content.as_ref(), "inside");
    }

    #[test]
    fn touch_reconciles_one_id_and_promotes_last_seen_recency() {
        let mut h = HotLayer::new(HotBounds {
            window_ms: 100,
            max_bytes: TEST_STAGING_BYTES + 4_096,
        });
        h.reconcile(ev(1, 0, "first"), 50);
        let mut touched = ev(1, 0, "first");
        touched.last_seen_at = 120;
        h.reconcile(touched, 120);
        assert_eq!(h.recent(120, 10).iter().map(|e| e.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn unprojected_oversized_event_is_omitted_without_crossing_budget() {
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 70 * 1_024,
        };
        let mut h = HotLayer::new(bounds);
        h.reconcile(ev(1, 0, &"界".repeat(30_000)), 0);
        assert!(h.byte_size() <= bounds.max_bytes);
        assert!(h.recent(0, 1).is_empty());
    }

    #[test]
    fn byte_overflow_creates_budgeted_deterministic_fold() {
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 1_200,
        };
        let mut h = HotLayer::new(bounds);
        for id in 1..=20 { h.reconcile(ev(id, id, &"x".repeat(100)), id); }
        assert!(h.byte_size() <= bounds.max_bytes);
        assert!(h.folded_count() > 0);
        assert_eq!(h.folds().len(), 1);
    }

    #[test]
    fn rebuild_uses_last_seen_and_persisted_sanitized_content() {
        let conn = crate::open_in_memory().unwrap();
        let raw = "visible\u{200b} AKIAIOSFODNN7EXAMPLE";
        let old = NewEvent {
            ts: 0, source: "capture", kind: "text", app_bundle_id: None, window_title: None,
            content: raw, content_hash: "same", dwell_ms: 0, display_id: None, window_bounds: None,
        };
        let (event_id, _) = insert_or_touch(&conn, &old).unwrap();
        insert_or_touch(&conn, &NewEvent { ts: 200, ..old }).unwrap();
        let mut hot = HotLayer::new(HotBounds {
            window_ms: 100,
            max_bytes: TEST_STAGING_BYTES + 8 * 1_024,
        });
        assert!(hot.rebuild_from_warm_in_place(&conn, 200, || false).unwrap());
        let event = hot.recent(200, 1).remove(0);
        assert_eq!(event.id, event_id);
        assert_eq!(event.ts, 0);
        assert_eq!(event.last_seen_at, 200);
        assert_ne!(event.content.as_ref(), raw);
    }

    #[test]
    fn hydration_bounds_every_text_field_before_hot_owns_it() {
        let conn = crate::open_in_memory().unwrap();
        let huge = "界".repeat(100_000);
        let event_id = crate::event_log::insert_with_thread(
            &conn,
            &NewEvent {
                ts: 1,
                source: &huge,
                kind: &huge,
                app_bundle_id: Some(&huge),
                window_title: Some(&huge),
                content: &huge,
                content_hash: "huge-fields",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
            Some(&huge),
        )
        .unwrap();
        let event = crate::event_log::hot_event_by_id(&conn, event_id).unwrap().unwrap();
        assert!(event.source.len() <= 4 * 1_024);
        assert!(event.kind.len() <= 4 * 1_024);
        assert!(event.app_bundle_id.as_deref().unwrap().len() <= 16 * 1_024);
        assert!(event.window_title.as_deref().unwrap().len() <= 16 * 1_024);
        assert!(event.thread_key.as_deref().unwrap().len() <= 16 * 1_024);
        assert!(event.content.len() <= HOT_CONTENT_MAX_BYTES);
        assert!(std::str::from_utf8(event.content.as_bytes()).is_ok());
        assert!(event.source_truncated);
        assert!(event.content_truncated);
    }

    #[test]
    fn recent_snapshots_copy_only_bounded_purpose_output() {
        let mut hot = HotLayer::with_defaults();
        hot.reconcile(ev(1, 1, &"x".repeat(32_000)), 1);
        let snapshot = hot.context_candidates(1, &["capture"], 1, 12).unwrap();
        assert_eq!(snapshot.candidates[0].preview.excerpt.len(), 12);
        assert_eq!(hot.buf[0].as_deref().unwrap().content.len(), 32_000);
        assert!(hot.budget_snapshot().peak_bytes <= HotBounds::default().max_bytes);
    }

    #[test]
    fn in_place_rebuild_clears_partial_state_and_never_crosses_budget() {
        let conn = crate::open_in_memory().unwrap();
        for id in 1..=3 {
            insert(&conn, &NewEvent {
                ts: id,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: &"x".repeat(8_000),
                content_hash: &format!("rebuild-{id}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            }).unwrap();
        }
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 40 * 1_024,
        };
        let mut hot = HotLayer::new(bounds);
        hot.reconcile(ev(99, 99, &"old".repeat(4_000)), 99);
        let address = &hot as *const HotLayer;
        let mut visited = 0;
        let complete = hot.rebuild_from_warm_in_place(&conn, 3, || {
            visited += 1;
            visited > 1
        }).unwrap();
        assert!(!complete);
        assert!(hot.is_empty());
        assert_eq!(address, &hot as *const HotLayer);
        assert!(hot.budget_snapshot().peak_bytes <= bounds.max_bytes);
    }

    #[test]
    fn fixed_slots_never_grow_and_peak_is_exact() {
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES + 16 * 1_024,
        };
        let mut hot = HotLayer::new(bounds);
        let slot_count = hot.buf.len();
        let mut observed_high_water = hot.byte_size();
        for id in 1..=200 {
            hot.reconcile(ev(id, id, "tiny"), id);
            observed_high_water = observed_high_water.max(hot.byte_size());
        }
        let budget = hot.budget_snapshot();
        assert_eq!(hot.buf.len(), slot_count);
        assert_eq!(budget.peak_bytes, observed_high_water);
        assert!(budget.peak_bytes <= bounds.max_bytes);
    }

    #[test]
    fn snapshot_lease_bounds_concurrency_and_releases_on_drop() {
        let mut hot = HotLayer::with_defaults();
        hot.reconcile(ev(1, 1, "one"), 1);
        let first = hot.context_candidates(1, &["capture"], 1, 12).unwrap();
        assert!(hot.context_candidates(1, &["capture"], 1, 12).is_none());
        drop(first);
        assert!(hot.context_candidates(1, &["capture"], 1, 12).is_some());
        assert!(hot.context_candidates(1, &["capture"; 9], 1, 12).is_none());
        assert!(hot.context_candidates(1, &["capture"], 129, 12).is_none());
        assert!(hot.budget_snapshot().peak_bytes <= HotBounds::default().max_bytes);
    }

    #[test]
    fn maximum_legal_snapshot_and_row_overlap_fit_their_reserves() {
        assert_eq!(
            HOT_PROJECTED_ROW_STAGING_BYTES,
            2 * HOT_PROJECTED_ROW_MAX_BYTES + 4 * 1024,
        );
        let predicted = context_snapshot_bytes(
            HOT_SNAPSHOT_MAX_SOURCES,
            HOT_SNAPSHOT_MAX_EVENTS / HOT_SNAPSHOT_MAX_SOURCES,
            HOT_CONTENT_MAX_BYTES,
        ).unwrap();
        assert!(predicted > 16 * 1024 * 1024);
        assert!(predicted <= HOT_READ_OUTPUT_RESERVE_BYTES);

        let content = "x".repeat(HOT_CONTENT_MAX_BYTES);
        let metadata = "m".repeat(HOT_METADATA_MAX_BYTES);
        let sources = (0..HOT_SNAPSHOT_MAX_SOURCES)
            .map(|index| format!("source-{index}"))
            .collect::<Vec<_>>();
        let source_refs = sources.iter().map(String::as_str).collect::<Vec<_>>();
        let mut hot = HotLayer::with_defaults();
        let mut id = 0;
        for source in &sources {
            for _ in 0..HOT_SNAPSHOT_MAX_EVENTS / HOT_SNAPSHOT_MAX_SOURCES {
                id += 1;
                let mut event = ev(id, id, &content);
                event.source = source.clone().into_boxed_str();
                event.app_bundle_id = Some(metadata.clone().into_boxed_str());
                event.window_title = Some(metadata.clone().into_boxed_str());
                assert!(hot.reconcile(event, id));
            }
        }
        let snapshot = hot.context_candidates(
            id,
            &source_refs,
            HOT_SNAPSHOT_MAX_EVENTS / HOT_SNAPSHOT_MAX_SOURCES,
            HOT_CONTENT_MAX_BYTES,
        ).unwrap();
        assert_eq!(snapshot.candidates().len(), HOT_SNAPSHOT_MAX_EVENTS);
        assert!(hot.budget_snapshot().peak_bytes <= HotBounds::default().max_bytes);
    }

    #[test]
    fn event_that_cannot_fit_is_never_inserted_even_transiently() {
        let bounds = HotBounds {
            window_ms: i64::MAX,
            max_bytes: TEST_STAGING_BYTES,
        };
        let mut hot = HotLayer::new(bounds);
        let generation = hot.generation();
        hot.reconcile(ev(1, 1, "cannot fit"), 1);
        assert_eq!(hot.generation(), generation);
        assert!(hot.is_empty());
        assert!(hot.budget_snapshot().peak_bytes <= bounds.max_bytes);
    }

    #[test]
    fn out_of_order_historical_insert_is_not_resident_but_exact_cutoff_is() {
        let mut hot = HotLayer::new(HotBounds {
            window_ms: 100,
            max_bytes: TEST_STAGING_BYTES + 8 * 1_024,
        });
        hot.reconcile(ev(1, 99, "outside"), 200);
        hot.reconcile(ev(2, 100, "cutoff"), 200);
        assert_eq!(hot.recent(200, 10).iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
    }
}
