//! Daily brief and local calendar-window assembly.

use super::*;

impl Db {
    /// Open loops as Fusion/Brief input (stalest first; the Brief caps the count). Closed loops
    /// are excluded so resolving one from the panel removes it everywhere (memory, counts, Brief).
    pub fn open_loops(&self) -> Vec<OpenLoopItem> {
        self.try_open_loops().unwrap_or_default()
    }

    pub fn try_open_loops(&self) -> Result<Vec<OpenLoopItem>, String> {
        let rows = self
            .try_open_loop_rows()
            .map_err(|_| "dream open-loop read failed".to_string())?;
        let now = self.now_ms();
        Ok(rows
            .into_iter()
            .filter(|r| r.status != "closed" && !r.is_deferred(now))
            .map(|r| OpenLoopItem {
                // #926: a local mail guess is worded `possibly` however likely it is.
                confidence: shogun_fusion::confidence::wording_confidence(r.confidence, r.inferred),
                description: r.description,
                staleness_days: u32::try_from(r.staleness_days).unwrap_or(0),
                provenance_event_id: r.first_event_id,
            })
            .collect())
    }

    /// Assemble a full Morning Brief from DB state plus the supplied calendar / prose / suggestions
    /// (§6.8). The section rules + confidence gate are Fusion's; the daemon supplies the state.
    pub fn morning_brief(
        &self,
        calendar: Vec<CalendarLine>,
        what_happened: Vec<String>,
        suggested: Vec<ActionCandidate>,
        now_ms: i64,
    ) -> MorningBrief {
        assemble_brief(
            calendar,
            &self.commitments_due(now_ms),
            &self.open_loops(),
            what_happened,
            suggested,
        )
    }

    /// The local-only degraded Brief (FR-MB-04): calendar + overdue commitments from the DB, no
    /// prose — used when the Batch-API generation is unavailable.
    pub fn local_morning_brief(&self, calendar: Vec<CalendarLine>, now_ms: i64) -> MorningBrief {
        assemble_degraded(calendar, &self.commitments_due(now_ms))
    }

    /// Build the same local document used by the nightly job at first morning activity.
    ///
    /// A local row that still holds captured screen text in "What happened" (the legacy
    /// extractive layout) is rebuilt once.
    /// Callers poll this, so any other existing row is left alone.
    pub fn ensure_local_brief(&self, now_ms: i64) -> Result<(), String> {
        let date = local_date_string(now_ms);
        if let Some(row) = self.brief_for(&date) {
            if row.generated || !legacy_local_brief(&row.payload) { return Ok(()) }
        }
        use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
        let day = local_day_bounds(now_ms);
        DbDreamRunner::new(self, &LocalRuleClassifier, &LocalExtractiveSummarizer, now_ms)
            .local_brief(day.yesterday_start_ms, now_ms)
    }

    /// Why today's brief has no model-written section, as one state word the Today cards render
    /// (`complete` once it has one). Shared by every surface that shows the brief so they cannot
    /// disagree. Cheap enough to poll: no content is materialised for the `no_input` probe.
    pub fn brief_generation_state(&self, date: &str, now_ms: i64) -> Result<String, String> {
        if self.brief_for(date).is_some_and(|row| row.generated) {
            return Ok("complete".into());
        }
        let today = local_date_string(now_ms);
        Ok(match self.brief_generation(date)? {
            Some(run) if run.state == "processing" && now_ms.saturating_sub(run.updated_at) >= 10 * 60 * 1000 => "delayed".into(),
            Some(run) => run.display_state(),
            None if date != today => "expired".into(),
            None if !self.has_batch_eligible_events(local_day_bounds(now_ms).yesterday_start_ms, now_ms) => "no_input".into(),
            None => "not_run".into(),
        })
    }

    /// Assemble the Evening Wrap (§6.17, FR-EB-01/02): the day's outcome, what's still open,
    /// tomorrow's first items, and today's loose ends — **local aggregation only**, no LLM call
    /// and no egress. The caller supplies the window boundaries (`day_start_ms` = local midnight,
    /// `tomorrow_end_ms` = end of tomorrow) because timezone math belongs to the shell, and the
    /// tomorrow calendar lines because calendar data flows in from the connector lane.
    pub fn evening_wrap(
        &self,
        calendar_tomorrow: Vec<CalendarLine>,
        day_start_ms: i64,
        now_ms: i64,
        tomorrow_end_ms: i64,
    ) -> shogun_fusion::wrap::EveningWrap {
        use shogun_fusion::wrap::{assemble_wrap, WrapOutcome};

        // One lock for every day-window read: five separate `lock()` calls would let the day's
        // counts and the day's lists come from different instants, and the Wrap would then show
        // "3 done" beside a still-open list that already dropped one of them.
        let (outcome, active_commitments, active_loops, opened_today, all_commitments) = self
            .read_conn("state.evening_wrap_window", |c| {
                let decisions = shogun_memory::lessons::decision_counts_since(c, day_start_ms)
                    .unwrap_or((0, 0));
                // Mail triage counts for the day (issue #319 FR-TG-29). A failure here is a
                // zeroed line, not a missing Wrap: the evening summary is the one screen that
                // must never be an error page.
                let mail = shogun_memory::mail_triage::counts(c, day_start_ms).unwrap_or_default();
                let outcome = WrapOutcome {
                    commitments_done: u32::try_from(
                        state::count_commitments_done_since(c, day_start_ms).unwrap_or(0),
                    )
                    .unwrap_or(0),
                    loops_closed: u32::try_from(
                        state::count_open_loops_closed_since(c, day_start_ms).unwrap_or(0),
                    )
                    .unwrap_or(0),
                    actions_decided: u32::try_from(decisions.0).unwrap_or(0),
                    actions_adopted: u32::try_from(decisions.1).unwrap_or(0),
                    // Read under the same lock as the rest of the window, for the reason above:
                    // a separate `mail_triage_counts()` call would take its own lock and could
                    // report a triage that landed after the commitment counts were taken.
                    mail_triaged: u32::try_from(mail.triaged).unwrap_or(0),
                    mail_auto_archived: u32::try_from(mail.auto_archived).unwrap_or(0),
                    mail_needs_reply: u32::try_from(mail.needs_reply_open).unwrap_or(0),
                };
                (
                    outcome,
                    state::list_commitments_active_since(c, day_start_ms).unwrap_or_default(),
                    state::list_open_loops_active_since(c, day_start_ms).unwrap_or_default(),
                    state::list_open_loops_opened_since(c, day_start_ms).unwrap_or_default(),
                    state::list_commitments(c).unwrap_or_default(),
                )
            })
            .unwrap_or_default();

        let to_due = |r: state::CommitmentRow| CommitmentDue {
            overdue: r.status == "overdue" || r.due_at.is_some_and(|d| d < now_ms),
            description: r.description,
            due_at_ms: r.due_at,
            days_until_due: r.due_at.map(|d| CommitmentDue::days_until(d, now_ms)),
            confidence: r.confidence,
            provenance_event_id: r.first_event_id,
        };
        let to_loop = |r: state::OpenLoopRow| OpenLoopItem {
            confidence: shogun_fusion::confidence::wording_confidence(r.confidence, r.inferred),
            description: r.description,
            staleness_days: u32::try_from(r.staleness_days).unwrap_or(0),
            provenance_event_id: r.first_event_id,
        };

        // Snoozed rows stay out of every list, as on the Brief.
        let open = |r: &state::CommitmentRow| r.status != "done" && r.status != "cancelled" && !r.is_deferred(now_ms);

        // Tomorrow-due: unresolved commitments with a due time after now and inside tomorrow.
        let tomorrow_commitments: Vec<CommitmentDue> = all_commitments
            .iter()
            .filter(|r| open(r) && r.due_at.is_some_and(|d| d > now_ms && d <= tomorrow_end_ms))
            .cloned()
            .map(to_due)
            .collect();

        // Overdue work stays on the list even on a day nobody touched it (two at most).
        let active_ids: Vec<i64> = active_commitments.iter().map(|r| r.id).collect();
        let overdue_untouched = all_commitments
            .iter()
            .filter(|r| open(r) && !active_ids.contains(&r.id))
            .filter(|r| r.status == "overdue" || r.due_at.is_some_and(|d| d < now_ms))
            .take(2)
            .cloned();
        let still_open_commitments: Vec<CommitmentDue> = active_commitments
            .iter()
            .filter(|r| open(r))
            .cloned()
            .chain(overdue_untouched)
            .map(to_due)
            .collect();

        assemble_wrap(
            outcome,
            &still_open_commitments,
            &active_loops.into_iter().filter(|r| !r.is_deferred(now_ms)).map(to_loop).collect::<Vec<_>>(),
            calendar_tomorrow,
            &tomorrow_commitments,
            &opened_today.into_iter().filter(|r| !r.is_deferred(now_ms)).map(to_loop).collect::<Vec<_>>(),
        )
    }

    /// Persist the nightly Morning Brief for `date` (the Dream Cycle's MorningBrief job
    /// writes it, the morning display reads it). Upsert on the day key — idempotent under a
    /// crash-resume re-run (FR-DC-04). Returns `None` on a lock/write failure so the job can
    /// report the night as failed and stay resumable.
    pub fn save_brief(&self, date: &str, payload_json: &str, generated: bool) -> Option<bool> {
        self.try_save_brief(date, payload_json, generated).ok()
    }

    pub fn try_save_brief(
        &self,
        date: &str,
        payload_json: &str,
        generated: bool,
    ) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn_reported("briefs.upsert_brief", |conn| {
            shogun_memory::briefs::upsert_brief(conn, date, payload_json, generated, now)
                .map_err(|_| "dream brief write failed".to_string())
        })
    }

    /// The persisted brief for `date` (`None` when the nightly job hasn't written one — the caller
    /// falls back to [`Self::local_morning_brief`], FR-MB-04).
    pub fn brief_for(&self, date: &str) -> Option<shogun_memory::briefs::StoredBrief> {
        self.with_conn("briefs.get", |c| shogun_memory::briefs::get_brief(c, date))
            .ok()
            .flatten()
    }

    /// What the dashboard's Today shows under the brief, assembled on every read.
    ///
    /// Only the saved brief's prose ("What happened"), meeting recaps and suggestion labels come
    /// from today's `briefs` row: they were written once, by the night's run. "Commitments due"
    /// and "Open loops" are state, and a saved copy of them goes stale the moment the user closes,
    /// edits or defers one — so they are assembled now from the state tables through the same
    /// Fusion rules the brief uses (Low band dropped, Medium marked `possibly`, duplicates
    /// collapsed, five each). A missing or unreadable row leaves the prose empty, never an error.
    pub fn today_brief(&self, now_ms: i64) -> TodayBrief {
        let date = local_date_string(now_ms);
        let saved = self
            .brief_for(&date)
            .and_then(|row| displayed_brief_payload(&row).map(|payload| (payload, row.generated)));
        let live = assemble_brief(
            Vec::new(),
            &self.commitments_due(now_ms),
            &self.open_loops(),
            Vec::new(),
            Vec::new(),
        );
        let (what_happened, meetings, suggested_actions, generated, saved) = match saved {
            Some((payload, generated)) => (
                payload.what_happened,
                payload.meetings,
                payload.suggested_actions.into_iter().map(|a| a.label).collect(),
                generated,
                true,
            ),
            None => (Vec::new(), Vec::new(), Vec::new(), false, false),
        };
        TodayBrief {
            what_happened,
            meetings,
            suggested_actions,
            commitments_due: live.commitments_due,
            open_loops: live.open_loops,
            saved,
            generated,
        }
    }

    /// Where one event came from: its `source` plus, for captured events, the app it was captured
    /// in. This is the daily-summary card's deep-link data (issue #10): the chip label comes from
    /// the source, and a capture event's `app_bundle_id` is the app the chip re-opens. Metadata
    /// only — the event's content never rides along.
    pub fn event_source(&self, event_id: i64) -> Option<(String, Option<String>)> {
        self.with_conn("events.source", |c| {
            c.query_row(
                "SELECT source, app_bundle_id FROM event_log WHERE id = ?1",
                [event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .ok()
        .flatten()
    }
}

/// Today's brief as the dashboard shows it ([`Db::today_brief`]).
#[derive(Debug, Clone)]
pub struct TodayBrief {
    /// From the saved brief (empty without one).
    pub what_happened: Vec<String>,
    pub meetings: Vec<String>,
    pub suggested_actions: Vec<String>,
    /// From the state tables as they are now — never the saved snapshot.
    pub commitments_due: Vec<shogun_fusion::brief::BriefItem>,
    pub open_loops: Vec<shogun_fusion::brief::BriefItem>,
    /// A readable `briefs` row exists for today.
    pub saved: bool,
    /// That row was written by the model (`briefs.generated`).
    pub generated: bool,
}

/// Exact local midnight boundaries for the current and previous calendar day.
#[cfg(feature = "db")]
/// The event window the Morning Brief summarises for a nightly cycle created at `created_at_ms`
/// with input window `[cycle_from, cycle_to)`: from the start of yesterday (the same span the local
/// brief reads) or the cycle's own start if that is earlier, to the cycle's end. With hourly
/// derivation (#1066 B) the cycle's own window is usually just the last hour or empty.
pub fn brief_input_window(created_at_ms: i64, cycle_from: i64, cycle_to: i64) -> (i64, i64) {
    (
        cycle_from.min(local_day_bounds(created_at_ms).yesterday_start_ms),
        cycle_to,
    )
}

/// Midnight in the requested host-calendar day. Chrono keeps the server and desktop
/// on the same OS timezone as SQLite's `localtime` validation, including DST.
#[cfg(not(feature = "desktop"))]
fn day_start_in_zone<Tz: chrono::TimeZone>(now_ms: i64, days: i64, zone: &Tz) -> Option<i64> {
    let today = zone.timestamp_millis_opt(now_ms).single()?.date_naive();
    let date = today.checked_add_signed(chrono::Duration::try_days(days)?)?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    // Where DST starts at midnight (America/Santiago, Africa/Cairo) 00:00 does not exist and the
    // day begins at the first valid instant after the gap, as it does for SQLite's `localtime`.
    // Without this the caller fell back to UTC days for those dates.
    (0..=4 * 60)
        .find_map(|minute| zone.from_local_datetime(&(midnight + chrono::Duration::minutes(minute))).earliest())
        .map(|time| time.timestamp_millis())
}

#[cfg(all(test, not(feature = "desktop")))]
#[allow(clippy::unwrap_used)]
mod calendar_regression_tests {
    use super::*;
    use chrono::TimeZone;

    /// UTC-3 until 2026-09-06 00:00 local, then UTC-2: midnight that day does not exist, as on
    /// America/Santiago's spring-forward.
    #[derive(Clone, Copy, Debug)]
    struct GapAtMidnight;

    impl GapAtMidnight {
        fn switch_local() -> chrono::NaiveDateTime {
            chrono::NaiveDate::from_ymd_opt(2026, 9, 6).unwrap().and_hms_opt(0, 0, 0).unwrap()
        }
        fn before() -> chrono::FixedOffset {
            chrono::FixedOffset::west_opt(3 * 3600).unwrap()
        }
        fn after() -> chrono::FixedOffset {
            chrono::FixedOffset::west_opt(2 * 3600).unwrap()
        }
    }

    impl chrono::TimeZone for GapAtMidnight {
        type Offset = chrono::FixedOffset;
        fn from_offset(_: &chrono::FixedOffset) -> Self {
            GapAtMidnight
        }
        fn offset_from_local_date(&self, local: &chrono::NaiveDate) -> chrono::LocalResult<chrono::FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_local_datetime(&self, local: &chrono::NaiveDateTime) -> chrono::LocalResult<chrono::FixedOffset> {
            let switch = Self::switch_local();
            if *local < switch {
                chrono::LocalResult::Single(Self::before())
            } else if *local < switch + chrono::Duration::hours(1) {
                chrono::LocalResult::None
            } else {
                chrono::LocalResult::Single(Self::after())
            }
        }
        fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> chrono::FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> chrono::FixedOffset {
            // 2026-09-06 00:00 at UTC-3 is 03:00 UTC.
            if *utc < Self::switch_local() + chrono::Duration::hours(3) { Self::before() } else { Self::after() }
        }
    }

    #[test]
    fn a_day_whose_midnight_falls_in_a_dst_gap_starts_after_the_gap() {
        // 2026-09-06 12:00 local (UTC-2) is 14:00 UTC.
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 6, 14, 0, 0).unwrap().timestamp_millis();
        // The day starts at 01:00 UTC-2 = 03:00 UTC, not at a UTC midnight.
        assert_eq!(day_start_in_zone(now, 0, &GapAtMidnight), Some(chrono::Utc.with_ymd_and_hms(2026, 9, 6, 3, 0, 0).unwrap().timestamp_millis()));
        // The days on either side still start at their own midnights.
        assert_eq!(day_start_in_zone(now, -1, &GapAtMidnight), Some(chrono::Utc.with_ymd_and_hms(2026, 9, 5, 3, 0, 0).unwrap().timestamp_millis()));
        assert_eq!(day_start_in_zone(now, 1, &GapAtMidnight), Some(chrono::Utc.with_ymd_and_hms(2026, 9, 7, 2, 0, 0).unwrap().timestamp_millis()));
    }

    #[test]
    fn server_days_follow_non_utc_midnights_on_both_sides_of_utc() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 26, 1, 0, 0).unwrap().timestamp_millis();
        let east = chrono::FixedOffset::east_opt(19_800).unwrap();
        let west = chrono::FixedOffset::west_opt(28_800).unwrap();
        assert_eq!(day_start_in_zone(now, 0, &east), Some(chrono::Utc.with_ymd_and_hms(2026, 10, 25, 18, 30, 0).unwrap().timestamp_millis()));
        assert_eq!(day_start_in_zone(now, 0, &west), Some(chrono::Utc.with_ymd_and_hms(2026, 10, 25, 8, 0, 0).unwrap().timestamp_millis()));
    }

    #[test]
    fn server_day_window_matches_sqlites_canonical_local_day() {
        let now = 1_800_000_000_000;
        let bounds = local_day_bounds(now);
        let conn = shogun_memory::open_in_memory().unwrap();
        let dates: (String, String) = conn.query_row(
            "SELECT datetime(?1/1000,'unixepoch','localtime'),datetime(?2/1000,'unixepoch','localtime')",
            rusqlite::params![bounds.yesterday_start_ms, bounds.today_start_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(dates.0, format!("{} 00:00:00", local_date_days_before(now, 1)));
        assert_eq!(dates.1, format!("{} 00:00:00", local_date_string(now)));
    }
}

/// The local calendar day around `now_ms`. The Mac app's build (feature `desktop`) asks libc
/// (`localtime_r` / `mktime`, DST folded in); the server uses Chrono for the same host-local
/// calendar as SQLite. UTC hosts keep UTC days until principal-specific timezones land.
pub fn local_day_bounds(now_ms: i64) -> shogun_memory::search::LocalDayBounds {
    #[cfg(feature = "desktop")]
    {
        // SAFETY: `tm` is initialized by localtime_r. mktime accepts and normalizes the copied
        // local calendar values; `tm_isdst = -1` makes libc resolve the correct offset per date.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = (now_ms / 1000) as libc::time_t;
            if libc::localtime_r(&t, &mut tm).is_null() {
                return utc_day_bounds(now_ms);
            }
            tm.tm_hour = 0;
            tm.tm_min = 0;
            tm.tm_sec = 0;
            tm.tm_isdst = -1;
            let today = libc::mktime(&mut tm);
            if today == -1 {
                return utc_day_bounds(now_ms);
            }
            let mut yesterday_tm = tm;
            yesterday_tm.tm_mday -= 1;
            yesterday_tm.tm_isdst = -1;
            let yesterday = libc::mktime(&mut yesterday_tm);
            if yesterday == -1 {
                utc_day_bounds(now_ms)
            } else {
                shogun_memory::search::LocalDayBounds {
                    yesterday_start_ms: (yesterday as i64) * 1000,
                    today_start_ms: (today as i64) * 1000,
                }
            }
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        match (
            day_start_in_zone(now_ms, -1, &chrono::Local),
            day_start_in_zone(now_ms, 0, &chrono::Local),
        ) {
            (Some(yesterday_start_ms), Some(today_start_ms)) => {
                shogun_memory::search::LocalDayBounds { yesterday_start_ms, today_start_ms }
            }
            _ => utc_day_bounds(now_ms),
        }
    }
}

/// The Evening Wrap's window (§6.17): `(local midnight today, end of tomorrow)`.
///
/// Same libc path as [`local_day_bounds`] rather than `today_start + 2 × 24 h`. A day containing a
/// DST transition is 23 or 25 hours long, so fixed-length arithmetic would put "the end of
/// tomorrow" an hour off twice a year — and the Wrap's boundaries decide which commitments count
/// as tomorrow's, so an hour of slack at the edge silently moves items between sections.
#[cfg(feature = "db")]
pub fn local_wrap_window(now_ms: i64) -> (i64, i64) {
    let today_start = local_day_bounds(now_ms).today_start_ms;
    #[cfg(feature = "desktop")]
    {
        // SAFETY: `tm` is initialized by localtime_r and only read after it reports success.
        // mktime normalizes `tm_mday + 2` across month and year ends; `tm_isdst = -1` makes libc
        // resolve the offset in effect on *that* date, not today's.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = (today_start / 1000) as libc::time_t;
            if !libc::localtime_r(&t, &mut tm).is_null() {
                tm.tm_mday += 2;
                tm.tm_isdst = -1;
                let end = libc::mktime(&mut tm);
                if end != -1 {
                    return (today_start, (end as i64) * 1000);
                }
            }
        }
    }
    #[cfg(not(feature = "desktop"))]
    if let Some(end) = day_start_in_zone(now_ms, 2, &chrono::Local) {
        return (today_start, end);
    }
    (today_start, today_start + 2 * 24 * 60 * 60 * 1000)
}

/// The next local midnight after `today_start_ms`. A DST day is 23 or 25 hours, so the 24 h guess
/// is shifted by the change in UTC offset between the two midnights.
#[cfg(feature = "db")]
pub fn local_next_midnight(today_start_ms: i64) -> i64 {
    next_midnight_with(today_start_ms, os_gmt_offset_secs)
}

#[cfg(feature = "db")]
fn next_midnight_with(today_start_ms: i64, offset_secs: impl Fn(i64) -> i32) -> i64 {
    let guess = today_start_ms + 24 * 60 * 60 * 1000;
    guess + i64::from(offset_secs(today_start_ms) - offset_secs(guess)) * 1000
}

#[cfg(feature = "db")]
fn utc_day_bounds(now_ms: i64) -> shogun_memory::search::LocalDayBounds {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let today_start_ms = now_ms.div_euclid(DAY_MS) * DAY_MS;
    shogun_memory::search::LocalDayBounds {
        yesterday_start_ms: today_start_ms - DAY_MS,
        today_start_ms,
    }
}

/// The local calendar date (`YYYY-MM-DD`) an instant falls on — the `briefs` row key.
/// Mirrors [`local_day_bounds`]: the host calendar, with DST folded in.
#[cfg(feature = "db")]
/// Seconds east of UTC per the OS, DST folded in. The server uses the same host
/// timezone as SQLite rather than assuming every server runs in UTC.
pub fn os_gmt_offset_secs(now_ms: i64) -> i32 {
    #[cfg(feature = "desktop")]
    {
        // SAFETY: `tm` is only read after localtime_r reports success by returning non-null.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = now_ms.div_euclid(1000) as libc::time_t;
            if !libc::localtime_r(&t, &mut tm).is_null() {
                return tm.tm_gmtoff as i32;
            }
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        use chrono::TimeZone;
        if let Some(time) = chrono::Local.timestamp_millis_opt(now_ms).single() {
            return time.offset().local_minus_utc();
        }
    }
    0
}

/// The local hour (0..=23) for a wall-clock instant — the rhythm prior's band input.
pub fn local_hour_now(now_ms: i64) -> u32 {
    crate::dreamcycle::schedule::local_time(now_ms.div_euclid(1000), os_gmt_offset_secs(now_ms))
        .hour
}

pub fn local_date_string(now_ms: i64) -> String {
    let secs = now_ms.div_euclid(1000);
    #[cfg(feature = "desktop")]
    {
        // SAFETY: `tm` is only read after localtime_r reports success by returning non-null.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = secs as libc::time_t;
            if !libc::localtime_r(&t, &mut tm).is_null() {
                return format!(
                    "{:04}-{:02}-{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday
                );
            }
        }
    }
    let ymd = crate::dreamcycle::schedule::local_time(secs, os_gmt_offset_secs(now_ms)).yyyymmdd;
    format!(
        "{:04}-{:02}-{:02}",
        ymd / 10_000,
        (ymd / 100) % 100,
        ymd % 100
    )
}

/// The local calendar date a whole number of calendar days before `now_ms`. This uses the same
/// `localtime_r`/`mktime` path as [`local_day_bounds`], so a DST day is one date rather than an
/// assumed 24-hour elapsed interval. The server follows the same host-local calendar.
pub fn local_date_days_before(now_ms: i64, days: i32) -> String {
    #[cfg(feature = "desktop")]
    {
        // SAFETY: `tm` is initialized by localtime_r, and mktime normalizes a calendar-day
        // adjustment with DST inferred from the destination date.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = now_ms.div_euclid(1000) as libc::time_t;
            if !libc::localtime_r(&t, &mut tm).is_null() {
                tm.tm_mday -= days;
                tm.tm_isdst = -1;
                let shifted = libc::mktime(&mut tm);
                if shifted != -1 {
                    return local_date_string((shifted as i64) * 1000);
                }
            }
        }
    }
    #[cfg(not(feature = "desktop"))]
    if let Some(midnight) = day_start_in_zone(now_ms, -i64::from(days), &chrono::Local) {
        return local_date_string(midnight);
    }
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    local_date_string(now_ms.saturating_sub(i64::from(days) * DAY_MS))
}

fn normalized_state_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn display_state_lines(
    lines: Vec<shogun_memory::briefs::BriefLine>,
    limit: usize,
) -> Vec<shogun_memory::briefs::BriefLine> {
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::new();
    for line in lines {
        if seen.insert(normalized_state_text(&line.text)) {
            kept.push(line);
            if kept.len() == limit {
                break;
            }
        }
    }
    kept
}

/// A stored brief's payload as every surface renders it (`None` when it does not parse — the
/// caller falls back to the degraded assembly, FR-MB-04).
///
/// A local (`generated = 0`) row written before #717 still holds captured screen text in "What
/// happened" ("7 / 7 / 7 / 7", toolbar labels). Today's row is rebuilt once by
/// [`Db::ensure_local_brief`], but a past day's row is never rebuilt — its window is gone — so the
/// same legacy lines are dropped here, at read time, for every day. The stored row is left as it
/// is (nothing destructive); generated prose is retained. State lines are deduplicated and capped.
pub fn displayed_brief_payload(
    row: &shogun_memory::briefs::StoredBrief,
) -> Option<shogun_memory::briefs::BriefPayload> {
    let mut payload: shogun_memory::briefs::BriefPayload = serde_json::from_str(&row.payload).ok()?;
    if !row.generated {
        payload
            .what_happened
            .retain(|line| crate::dreamcycle::jobs::is_local_what_happened_line(line));
    }
    payload.commitments_due = display_state_lines(
        payload.commitments_due,
        shogun_fusion::brief::COMMITMENTS_MAX,
    );
    payload.open_loops =
        display_state_lines(payload.open_loops, shogun_fusion::brief::OPEN_LOOPS_MAX);
    Some(payload)
}

/// A local brief whose "What happened" still holds lines the current local builder never writes.
/// An unreadable payload is not rebuilt here; the display falls back on its own.
fn legacy_local_brief(payload: &str) -> bool {
    serde_json::from_str::<shogun_memory::briefs::BriefPayload>(payload).is_ok_and(|p| {
        p.what_happened
            .iter()
            .any(|line| !crate::dreamcycle::jobs::is_local_what_happened_line(line))
    })
}

#[cfg(test)]
mod display_tests {
    #[test]
    fn stored_state_dedupe_preserves_owners_and_meaningful_symbols() {
        let line = |text: &str, event| shogun_memory::briefs::BriefLine {
            text: text.into(),
            provenance_event_id: Some(event),
            evidence_kind: None,
            possibly: false,
        };
        let rows = vec![
            line("Maya said: migrate the C++ service", 1),
            line("Bob said: migrate the C++ service", 2),
            line("Maya said: migrate the C# service", 3),
            line("  Maya  said: migrate the C++ service  ", 4),
        ];

        let kept = super::display_state_lines(rows, 5);
        assert_eq!(kept.len(), 3);
    }
}

#[cfg(all(test, feature = "db"))]
mod next_midnight_tests {
    use super::next_midnight_with;

    #[test]
    fn a_dst_day_is_23_or_25_hours_and_a_normal_day_24() {
        const H: i64 = 3_600_000;
        let day = |from: i32, to: i32| next_midnight_with(0, |t| if t == 0 { from } else { to }) / H;
        assert_eq!(day(-18_000, -18_000), 24);
        assert_eq!(day(-18_000, -14_400), 23);
        assert_eq!(day(-14_400, -18_000), 25);
    }
}

#[cfg(test)]
mod today_brief_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use shogun_memory::state::{CommitmentDirection, CommitmentStatus};

    fn commitment(db: &Db, desc: &'static str, due_at: Option<i64>, confidence: f64) {
        let event = db
            .capture(&NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: desc,
                content_hash: desc,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap()
            .0;
        db.insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: desc,
                due_at,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence,
                now: 1,
            },
            &[Provenance::new(event)],
        )
        .unwrap();
    }

    #[test]
    fn todays_state_sections_are_live_and_only_prose_comes_from_the_saved_brief() {
        let now = 100 * 24 * 60 * 60 * 1000 + 12 * 60 * 60 * 1000;
        let db = Db::open_in_memory(Arc::new(move || now)).unwrap();
        let line = |text: &str| shogun_memory::briefs::BriefLine {
            text: text.into(),
            provenance_event_id: Some(1),
            evidence_kind: None,
            possibly: false,
        };
        // The night's snapshot: a commitment that has since been closed, written three times.
        let stale = shogun_memory::briefs::BriefPayload {
            date: local_date_string(now),
            today: Vec::new(),
            commitments_due: vec![
                line("closed yesterday"),
                line("closed yesterday"),
                line("closed yesterday"),
            ],
            open_loops: vec![line("resolved loop")],
            what_happened: vec!["Shipped the recap.".into()],
            meetings: vec!["Design review: agreed on the launch date.".into()],
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
        };
        db.try_save_brief(
            &local_date_string(now),
            &serde_json::to_string(&stale).unwrap(),
            true,
        )
        .unwrap();

        commitment(&db, "send the deck", Some(now + 3_600_000), 0.9);
        commitment(&db, "maybe call the vendor", Some(now + 7_200_000), 0.6);
        commitment(&db, "unreviewed candidate", Some(now + 60_000), 0.3);

        let today = db.today_brief(now);
        assert!(today.saved && today.generated);
        assert_eq!(today.what_happened, stale.what_happened);
        assert_eq!(today.meetings, stale.meetings);
        let due: Vec<(&str, bool)> = today
            .commitments_due
            .iter()
            .map(|i| (i.text.as_str(), i.possibly))
            .collect();
        assert_eq!(
            due,
            [("send the deck", false), ("maybe call the vendor", true)],
            "live rows only: the Low-band candidate and the stale snapshot are not shown"
        );
        assert!(today.open_loops.is_empty());

        // No saved row: the same live state, no prose.
        let fresh = Db::open_in_memory(Arc::new(move || now)).unwrap();
        commitment(&fresh, "send the deck", Some(now + 3_600_000), 0.9);
        let today = fresh.today_brief(now);
        assert!(!today.saved && today.what_happened.is_empty());
        assert_eq!(today.commitments_due.len(), 1);
    }
}
