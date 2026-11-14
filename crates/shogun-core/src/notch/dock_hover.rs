//! Dock tile hover judgement — the pointer arriving on the app's Dock icon.
//!
//! The Dock never tells an app that the pointer is over its tile. The macOS adapter asks the
//! Dock's own accessibility tree instead ("which tile is under the pointer?"), which costs a
//! synchronous call to another process, so this core decides *when* that question is worth
//! asking. Two facts keep it cheap:
//!
//! - The Dock's frame changes rarely (apps come and go, the Dock moves edge). It is asked for at
//!   most once per [`DockHover::FRAME_TTL_MS`], and only while the pointer is actually moving.
//! - The tile question is asked only while the pointer is inside that frame, at most once per
//!   [`DockHover::QUERY_MS`]. Everywhere else a sample is a rectangle test and nothing more.
//!
//! Pure and time-injected, tested here on Linux; the adapter (`dock_hover.rs` in the shell)
//! supplies the pointer samples from the existing event tap and answers the questions.

use crate::notch::geometry::{Point, Rect};

/// What the adapter should do with the sample it just fed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing: the pointer is away from the Dock and the frame is fresh enough.
    Ignore,
    /// Ask the Dock for its frame, then call [`DockHover::frame_refreshed`].
    RefreshFrame,
    /// Ask the Dock which tile is under the pointer, then call [`DockHover::observed`].
    Query,
    /// The pointer was over our tile and has now left the Dock altogether: end the hover.
    Left,
}

/// What an answer changed about the hover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The pointer arrived on our tile.
    Entered,
    /// The pointer is no longer on our tile.
    Left,
}

/// The judgement. One per process; the adapter owns it behind a lock.
#[derive(Clone, Debug)]
pub struct DockHover {
    frame: Option<Rect>,
    /// When the frame was last (re)read, or None before the first read.
    frame_at: Option<u64>,
    /// A refresh has been requested and has not come back yet — do not queue another.
    refresh_pending: bool,
    /// A tile query has been requested and has not come back yet.
    query_pending: bool,
    last_query_ms: Option<u64>,
    /// The last answer said the pointer was over our tile.
    over: bool,
}

impl Default for DockHover {
    fn default() -> Self {
        Self::new()
    }
}

impl DockHover {
    /// How long a Dock frame is trusted before the next moving sample re-reads it.
    pub const FRAME_TTL_MS: u64 = 3_000;
    /// Minimum spacing between two tile questions while the pointer is inside the Dock.
    pub const QUERY_MS: u64 = 80;

    pub const fn new() -> Self {
        Self {
            frame: None,
            frame_at: None,
            refresh_pending: false,
            query_pending: false,
            last_query_ms: None,
            over: false,
        }
    }

    /// The Dock frame currently trusted, if any.
    pub fn frame(&self) -> Option<Rect> {
        self.frame
    }

    /// Whether the last answer put the pointer over our tile.
    pub fn is_over(&self) -> bool {
        self.over
    }

    /// A pointer move at `p` (CG coordinates, top-left origin) at `now_ms`.
    pub fn sample(&mut self, p: Point, now_ms: u64) -> Step {
        let inside = self.frame.is_some_and(|f| f.contains(p));
        if self.over && !inside {
            // Leaving the Dock ends the hover without a question, and before anything else: the
            // heart the icon is holding must let go the moment the pointer is gone. The next
            // arrival is a fresh enter.
            self.over = false;
            return Step::Left;
        }
        let stale = match self.frame_at {
            None => true,
            Some(at) => now_ms.saturating_sub(at) >= Self::FRAME_TTL_MS,
        };
        if stale && !self.refresh_pending {
            self.refresh_pending = true;
            return Step::RefreshFrame;
        }
        if !inside {
            return Step::Ignore;
        }
        if self.query_pending {
            return Step::Ignore;
        }
        // `map_or(true, ..)` rather than `is_none_or`: the latter is stable only from 1.82 and
        // this workspace pins MSRV 1.80.
        let spaced = self
            .last_query_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= Self::QUERY_MS);
        if !spaced {
            return Step::Ignore;
        }
        self.query_pending = true;
        self.last_query_ms = Some(now_ms);
        Step::Query
    }

    /// The Dock's frame, or None when the Dock could not be read (hidden, restarting, no
    /// permission). A None is trusted for the same TTL so a Dock that is not there is not asked
    /// about on every move.
    ///
    /// Returns [`Change::Left`] when a Dock that went away takes a hover with it.
    pub fn frame_refreshed(&mut self, frame: Option<Rect>, now_ms: u64) -> Option<Change> {
        self.frame = frame;
        self.frame_at = Some(now_ms);
        self.refresh_pending = false;
        if frame.is_none() && self.over {
            self.over = false;
            return Some(Change::Left);
        }
        None
    }

    /// A [`Step`] the adapter could not carry out (its worker queue was full). Clears the
    /// pending mark so the next move can ask again instead of waiting forever.
    pub fn cancel(&mut self, step: Step) {
        match step {
            Step::RefreshFrame => self.refresh_pending = false,
            Step::Query => self.query_pending = false,
            // A leave is already applied to this state; the adapter owes the icon the release.
            Step::Ignore | Step::Left => {}
        }
    }

    /// The answer to a [`Step::Query`]: `ours` says the tile under the pointer is this app's.
    /// Returns what changed: [`Change::Entered`] once per arrival, [`Change::Left`] once when the
    /// pointer slides off the tile (onto a neighbour, say), and nothing while it rests.
    pub fn observed(&mut self, ours: bool) -> Option<Change> {
        self.query_pending = false;
        let was = self.over;
        self.over = ours;
        match (was, ours) {
            (false, true) => Some(Change::Entered),
            (true, false) => Some(Change::Left),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dock() -> Rect {
        // A bottom Dock on a 1440×900 display, CG coordinates.
        Rect::new(300.0, 820.0, 840.0, 80.0)
    }

    fn inside() -> Point {
        Point::new(700.0, 860.0)
    }

    fn away() -> Point {
        Point::new(700.0, 400.0)
    }

    #[test]
    fn the_first_move_asks_for_the_frame_and_nothing_else_until_it_arrives() {
        let mut h = DockHover::new();
        assert_eq!(h.sample(inside(), 0), Step::RefreshFrame);
        // Every further move while the answer is outstanding is free.
        assert_eq!(h.sample(inside(), 10), Step::Ignore);
        assert_eq!(h.sample(away(), 20), Step::Ignore);
        h.frame_refreshed(Some(dock()), 30);
        assert_eq!(h.frame(), Some(dock()));
    }

    #[test]
    fn moves_away_from_the_dock_cost_nothing_while_the_frame_is_fresh() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        for t in (1..2_000).step_by(16) {
            assert_eq!(h.sample(away(), t), Step::Ignore, "at {t}ms");
        }
    }

    #[test]
    fn the_frame_is_reread_after_its_ttl_on_the_next_move() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        assert_eq!(h.sample(away(), DockHover::FRAME_TTL_MS - 1), Step::Ignore);
        assert_eq!(
            h.sample(away(), DockHover::FRAME_TTL_MS),
            Step::RefreshFrame
        );
        // Not twice.
        assert_eq!(h.sample(away(), DockHover::FRAME_TTL_MS + 5), Step::Ignore);
    }

    #[test]
    fn inside_the_dock_the_tile_is_asked_about_at_the_query_rate() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        assert_eq!(h.sample(inside(), 10), Step::Query);
        // Outstanding: no second question.
        assert_eq!(h.sample(inside(), 20), Step::Ignore);
        assert_eq!(h.observed(false), None);
        // Answered, but too soon.
        assert_eq!(h.sample(inside(), 50), Step::Ignore);
        assert_eq!(h.sample(inside(), 10 + DockHover::QUERY_MS), Step::Query);
    }

    #[test]
    fn arriving_on_our_tile_enters_once_and_sliding_off_it_leaves_once() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        assert_eq!(h.sample(inside(), 10), Step::Query);
        assert_eq!(h.observed(true), Some(Change::Entered), "first arrival");
        assert!(h.is_over());
        assert_eq!(h.sample(inside(), 200), Step::Query);
        assert_eq!(
            h.observed(true),
            None,
            "resting on the tile changes nothing"
        );
        // Sliding to a neighbouring tile, then back.
        assert_eq!(h.sample(inside(), 400), Step::Query);
        assert_eq!(h.observed(false), Some(Change::Left));
        assert!(!h.is_over());
        assert_eq!(h.sample(inside(), 500), Step::Query);
        assert_eq!(h.observed(false), None, "still on the neighbour");
        assert_eq!(h.sample(inside(), 600), Step::Query);
        assert_eq!(h.observed(true), Some(Change::Entered), "a re-arrival");
    }

    #[test]
    fn leaving_the_dock_frame_ends_the_hover_without_a_question() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        assert_eq!(h.sample(inside(), 10), Step::Query);
        assert_eq!(h.observed(true), Some(Change::Entered));
        // The leave is reported once, on the first move outside, even with a stale frame.
        assert_eq!(h.sample(away(), DockHover::FRAME_TTL_MS + 100), Step::Left);
        assert!(!h.is_over());
        assert_eq!(
            h.sample(away(), DockHover::FRAME_TTL_MS + 110),
            Step::RefreshFrame
        );
        h.frame_refreshed(Some(dock()), DockHover::FRAME_TTL_MS + 120);
        assert_eq!(
            h.sample(away(), DockHover::FRAME_TTL_MS + 130),
            Step::Ignore
        );
        assert_eq!(
            h.sample(inside(), DockHover::FRAME_TTL_MS + 200),
            Step::Query
        );
        assert_eq!(
            h.observed(true),
            Some(Change::Entered),
            "coming back from outside the Dock is an arrival"
        );
    }

    #[test]
    fn a_dock_that_goes_away_takes_the_hover_with_it() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        assert_eq!(h.sample(inside(), 10), Step::Query);
        assert_eq!(h.observed(true), Some(Change::Entered));
        assert_eq!(
            h.frame_refreshed(None, DockHover::FRAME_TTL_MS),
            Some(Change::Left)
        );
        assert!(!h.is_over());
        assert_eq!(h.frame_refreshed(None, 2 * DockHover::FRAME_TTL_MS), None);
    }

    #[test]
    fn a_dock_that_cannot_be_read_is_not_asked_again_before_the_ttl() {
        let mut h = DockHover::new();
        assert_eq!(h.sample(inside(), 0), Step::RefreshFrame);
        assert_eq!(h.frame_refreshed(None, 5), None);
        assert_eq!(h.sample(inside(), 100), Step::Ignore);
        assert_eq!(
            h.sample(inside(), 5 + DockHover::FRAME_TTL_MS),
            Step::RefreshFrame
        );
    }

    #[test]
    fn a_step_the_adapter_dropped_is_asked_for_again() {
        let mut h = DockHover::new();
        assert_eq!(h.sample(inside(), 0), Step::RefreshFrame);
        h.cancel(Step::RefreshFrame);
        assert_eq!(h.sample(inside(), 1), Step::RefreshFrame);
        h.frame_refreshed(Some(dock()), 2);
        assert_eq!(h.sample(inside(), 3), Step::Query);
        h.cancel(Step::Query);
        assert_eq!(h.sample(inside(), 3 + DockHover::QUERY_MS), Step::Query);
    }

    #[test]
    fn a_stale_frame_wins_over_a_query_and_the_query_resumes_after() {
        let mut h = DockHover::new();
        h.frame_refreshed(Some(dock()), 0);
        let t = DockHover::FRAME_TTL_MS;
        assert_eq!(h.sample(inside(), t), Step::RefreshFrame);
        h.frame_refreshed(Some(dock()), t + 1);
        assert_eq!(h.sample(inside(), t + 2), Step::Query);
    }
}
