//! Local outcome evidence for commitments and open loops (plan E49).
//!
//! The on-device rules ([`crate::extract`]) only ever *open* low-confidence candidates. This module
//! reads what happens to them afterwards: a later message in the **same thread** that says the
//! thing happened ("Sent the Q3 budget to Oren."), was called off ("Never mind about the deck, it
//! is no longer needed.") or moved ("Update: the memo for Oren moves to Thursday.").
//!
//! Each such sentence that names an open row of the thread is recorded in `loop_updates` (V104)
//! as a typed update — `resolved`, `cancelled` or `redated` — with the event and the exact quote,
//! and then:
//!
//! - a **local candidate** (below the dashboard line, not the user's, never Batch-reviewed) is
//!   closed or re-dated in place, with the quote linked as provenance. Its confidence is never
//!   touched: local evidence may retire or move a local candidate, never lift it (owner decision
//!   2026-10-02, `LOCAL_RULE_MAX_CONFIDENCE`);
//! - a row the **loop consolidator** owns (≥ 0.5, or Batch-reviewed) or the **user** owns (pinned
//!   or edited) is left as it is. The update waits, unread, for the next consolidator call
//!   ([`crate::loops::consolidation_input`] shows it under "Outcomes noticed" with its source), and
//!   only a resolution or re-list that quotes it changes the row (owner decision 2026-10-14).
//!
//! The rules are deterministic and explainable, like the rest of the capture path: fixed cue
//! phrases, a question / future / negation veto, the same thread, and at least two shared content
//! words with the row (the artifact and the person), with a tie refused.
//!
//! E51 adds the passive voice ("the deck was handed to Oren", "has been rescheduled; the new date
//! is Friday"), Japanese sentences around English names and dates ("the Q3 budgetはOren Valeさんへ
//! 送付済みです", "the board memoの締切はSeptember 21に変更になりました"), and **outcomes posted on
//! another thread** (a new email, another channel). A threaded outcome that names no row of its
//! own thread may name a row of another thread, under a stricter rule than the thread's
//! ([`pick_cross_thread`]): at least [`MIN_SHARED_WORDS_CROSS_THREAD`] shared content words and at
//! least half of its own, among them a name (the counterparty or the project, written capitalised)
//! and a word that is not a name (the artifact), and no other open row anywhere within two shared
//! words of it. What it may then do is what an outcome on the row's own thread may do: close or
//! re-date a local candidate, never lift it; leave a consolidator-owned row as it is, with the
//! update unread for the consolidator. Screen text never takes this path (it has no thread, and an
//! app-scoped source is refused even if it had one), and meeting text never leaves a lead for a
//! consolidator-owned row, which the consolidator would never be shown.
//!
//! **Text without a thread** (screen capture, a note ingested with no `thread_key`) never changes
//! a row here, local candidate or not ([`reconcile_unthreaded_event`]). Nothing says which row it
//! is about; its time is when it was *seen*, not when it was said (an old "Sent …" scrolled into
//! view, a date read against the wrong day); and the same text comes back frame after frame. The
//! one thing it may do is leave a lead for the consolidator: a resolution or cancellation (never a
//! re-date) that names exactly one open row the consolidator owns — at least
//! [`MIN_SHARED_WORDS_UNTHREADED`] shared content words, at least half of its own, and no other
//! open row sharing [`MIN_SHARED_WORDS`] — is recorded unread and unapplied, once per row and
//! quote. The consolidator sees it with its source and changes the row only through a quote of its
//! own; a local candidate is left to its thread.
//!
//! The pass runs in the Dream Cycle's StateUpdate job and the hourly maintenance
//! ([`crate::recompute::recompute_overdue_and_staleness_detailed`]) over the events inserted since
//! its last run (`loop_update_scan`), so it is idempotent and bounded per run.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::StateTable;

/// How far back the first pass after the upgrade looks for outcomes.
pub const FIRST_SCAN_LOOKBACK_MS: i64 = 30 * 86_400_000;
/// Events one pass reads; the rest wait for the next pass.
pub const SCAN_BATCH: i64 = 5_000;
/// Content words an outcome must share with the row it closes.
pub const MIN_SHARED_WORDS: usize = 2;
/// Content words an outcome in text without a thread must share with the one row it names.
pub const MIN_SHARED_WORDS_UNTHREADED: usize = 3;
/// Content words an outcome on another thread must share with the row it names (E51).
pub const MIN_SHARED_WORDS_CROSS_THREAD: usize = 3;
/// How many more shared words the row an outcome on another thread names must have than any other
/// open row: closer than this, the sentence could be about either (E51).
pub const CROSS_THREAD_MARGIN: usize = 2;
/// The confidence below which a row is a local candidate the local path may change.
pub const DASHBOARD_LINE: f64 = 0.5;

/// What a later sentence says happened to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    /// It was done: sent, delivered, answered, decided.
    Resolved,
    /// It was called off.
    Cancelled,
    /// Its due date moved.
    Redated,
}

impl UpdateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateKind::Resolved => "resolved",
            UpdateKind::Cancelled => "cancelled",
            UpdateKind::Redated => "redated",
        }
    }
}

/// One outcome sentence: its kind, the exact span, and for a re-date the new due instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<'a> {
    pub kind: UpdateKind,
    pub quote: &'a str,
    pub due_at: Option<i64>,
}

/// Sentences that open with what was done ("Sent the plan to Oren.", "Done: …", "Decided: …").
const RESOLVED_OPENERS: &[&str] = &[
    "sent ", "done:", "done,", "done -", "delivered ", "replied ", "answered ", "finished ", "completed ",
    "decided:", "decided ", "settled ", "got ", "received ", "shipped ", "submitted ", "approved ", "resolved ",
    "handled ", "merged ", "closed out ",
];
/// The same, said about someone else or the thing itself ("Oren sent the plan over", "the plan
/// arrived from Oren", "review of the deck done and sent back").
const RESOLVED_ANYWHERE: &[&str] = &[
    " sent ", " arrived", " delivered", " is done", " done and ", "has been sent", " came through", " came in",
];
/// The thing was called off.
const CANCELLED: &[&str] = &[
    "never mind", "no longer needed", "not needed anymore", "no longer necessary", "dropping ", "called off",
    "cancelled", "canceled", "scrap ", "scratch ", "is off now", "not happening",
];
/// The due date moved; the new date follows the cue.
const REDATED: &[&str] = &[
    "moves to ", "moved to ", "now due ", "is now due ", "slips to ", "slipped to ", "pushed to ", "pushed back to ",
    "postponed to ", "postponed until ", "rescheduled to ", "rescheduled for ", "delayed to ", "delayed until ",
    "deferred to ", "new deadline is ", "new due date is ",
];
/// The passive of a resolution (E51): "the deck was handed to Oren", "the budget has been
/// shared". An auxiliary from [`RESOLVED_PASSIVE_AUX`] directly before a participle from
/// [`RESOLVED_PARTICIPLES`], as whole words.
const RESOLVED_PASSIVE_AUX: &[&str] = &["was ", "were ", "has been ", "have been ", "had been ", "is now ", "got "];
const RESOLVED_PARTICIPLES: &[&str] = &[
    "sent", "shared", "forwarded", "handed", "given", "passed", "submitted", "approved", "completed", "finished",
    "signed", "merged", "handled", "taken care of", "answered", "resolved", "decided", "uploaded", "published",
    "filed", "paid", "delivered", "returned", "reviewed", "settled", "fixed", "emailed", "posted", "signed off",
];
/// The passive of a cancellation (E51): "the offsite has been called off", "the brief was
/// shelved". "dropped off" is a delivery, not a cancellation, and is refused.
const CANCELLED_PASSIVE: &[&str] = &[
    "been dropped", "was dropped", "were dropped", "been scrapped", "was scrapped", "were scrapped",
    "been shelved", "was shelved", "were shelved", "been withdrawn", "was withdrawn", "been abandoned",
    "was abandoned", "been called off", "been cancelled", "been canceled",
];
/// Moves whose new date may come anywhere after the cue, not right behind it (E51): "the review
/// has been rescheduled; the new date is Friday", "the deadline was extended to September 30".
const REDATED_LOOSE: &[&str] = &[
    "rescheduled", "postponed", "pushed back", "delayed", "deferred", "extended to", "extended until",
    "extended till", "brought forward to", "moved up to", "moved forward to", "new date is", "new date:",
    "new deadline", "new due date",
];

/// Japanese outcomes (E51). A re-date names the new date just before the cue ("Mondayに変更",
/// "9月30日まで延期"); [`ja_redated_due`] reads it.
const JA_REDATED: &[&str] = &[
    "に変更", "へ変更", "に延期", "まで延期", "に延び", "まで延び", "に繰り下げ", "に繰り上げ", "に前倒し",
    "にずれ", "にずらし", "にリスケ", "変更になりました", "延期になりました",
];
/// Called off. [`JA_CANCELLED_NEGATIVE`] says it with a negative ending, which vetoes the others.
const JA_CANCELLED: &[&str] = &[
    "中止", "キャンセル", "不要になりました", "必要なくなりました", "取りやめ", "取り止め", "見送り",
    "なしになりました", "無しになりました", "白紙", "取り下げ",
];
const JA_CANCELLED_NEGATIVE: &[&str] = &["必要ありません", "要りません", "いりません", "不要です"];
/// Done: sent, delivered, answered, decided, received.
const JA_RESOLVED: &[&str] = &[
    "送付済み", "送信済み", "提出済み", "共有済み", "対応済み", "返信済み", "完了しました", "完了です", "送りました",
    "お送りしました", "送付しました", "送信しました", "提出しました", "共有しました", "対応しました",
    "返信しました", "回答しました", "お渡ししました", "渡しました", "決定しました", "決まりました",
    "承認しました", "承認されました", "マージしました", "マージされました", "届きました", "受け取りました",
    "受領しました", "終わりました", "済みです", "済ませました", "送られてきました",
];
/// Not stated as having happened: a condition, a maybe, a question, a plan under review.
const JA_NOT_STATED: &[&str] = &[
    "もし", "たら", "なら", "れば", "かもしれ", "かも", "でしょうか", "ますか", "ましたか", "ですか", "検討",
    "キャンセル待ち",
];
/// Not done: a negative, "not yet", a plan, or long ago.
const JA_NOT_DONE: &[&str] = &["ません", "まだ", "予定", "先月", "去年", "昨年", "以前", "前回"];

/// An outcome is stated, not planned, asked, denied or remembered from long ago.
const NOT_DONE: &[&str] = &[
    "will ", "'ll ", "going to", " not ", "n't", "if ", "unless ", "still ", " yet", "should ", "need to",
    "needs to", "please", "last month", "last year", "back in ", " ago", "reminder", "nudge", "chase",
];
/// A move or a cancellation is stated, not wondered about.
const NOT_STATED: &[&str] = &["if ", "unless ", " not ", "n't", "might ", "may ", "maybe ", "could "];

/// Words that say nothing about which item a sentence is about: function words, the cue verbs
/// themselves and every word a date is written with.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "about", "that", "this", "your", "you", "are", "was", "were", "will",
    "have", "has", "had", "all", "now", "any", "not", "its", "our", "their", "they", "them", "she", "him",
    "her", "his", "can", "could", "would", "should", "just", "still", "yet", "also", "been", "into", "onto",
    "over", "then", "than", "thanks", "thank", "update", "heads", "change", "moves", "moved", "slips",
    "slipped", "due", "done", "sent", "send", "sends", "got", "never", "mind", "need", "needed", "longer",
    "dropping", "decided", "decide", "settled", "settle", "whether", "ships", "ship", "goes", "out", "back",
    "please", "hi", "hello", "hey", "best", "cheers", "user", "promise", "sure", "it'll", "i'll", "i'm",
    "is", "it", "to", "of", "on", "in", "by", "be", "me", "my", "we", "us", "or", "an", "a", "i",
    // E51: the passive's and the re-date's own words.
    "shared", "handed", "given", "passed", "submitted", "approved", "completed", "finished", "delivered",
    "received", "rescheduled", "postponed", "delayed", "deferred", "extended", "cancelled", "canceled",
    "called", "off", "went", "new", "date", "deadline",
    "today", "tomorrow", "tonight", "week", "next", "monday", "tuesday", "wednesday", "thursday", "friday",
    "saturday", "sunday", "january", "february", "march", "april", "may", "june", "july", "august",
    "september", "october", "november", "december",
];

/// Sentence spans of `text`: split on newlines and `. ! ?`, trimmed, each an exact slice, with
/// whether it ended in a question mark.
fn spans(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '\n' | '.' | '!' | '?' | '。' | '！' | '？') {
            let s = text[start..i].trim();
            if !s.is_empty() {
                out.push((s, c == '?' || c == '？'));
            }
            start = i + c.len_utf8();
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push((tail, false));
    }
    out
}

/// The sentence without a leading bullet; and without a short label too ("user: ", "Oren Vale: ").
fn bodies(lower: &str) -> (&str, &str) {
    let bare = lower.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '*' | '•' | '·'));
    let labelled = match bare.split_once(": ") {
        Some((label, rest)) if label.split_whitespace().count() <= 3 => rest.trim_start(),
        _ => bare,
    };
    (bare, labelled)
}

fn any_in(haystack: &str, cues: &[&str]) -> bool {
    cues.iter().any(|c| haystack.contains(c))
}

/// The new due instant a re-date sentence names, read against when it was said: the words after
/// the cue, read as a deadline ("moves to Thursday" → "by Thursday"); for a loose cue
/// ([`REDATED_LOOSE`]) the first date anywhere after it ("rescheduled; the new date is Friday").
pub fn redated_due(lower: &str, said_at_ms: i64) -> Option<i64> {
    REDATED
        .iter()
        .find_map(|cue| {
            let at = lower.find(cue)? + cue.len();
            crate::due::due_at(&format!("by {}", &lower[at..]), said_at_ms)
        })
        .or_else(|| {
            REDATED_LOOSE.iter().find_map(|cue| {
                let at = lower.find(cue)? + cue.len();
                first_date_after(&lower[at..], said_at_ms)
            })
        })
}

/// The first date in `rest`, read as a deadline from each word on ("the new date is friday" →
/// "by friday").
fn first_date_after(rest: &str, said_at_ms: i64) -> Option<i64> {
    let starts = rest.char_indices().filter(|&(i, c)| {
        c.is_alphanumeric() && rest[..i].chars().next_back().is_none_or(|p| !p.is_alphanumeric())
    });
    for (i, _) in starts {
        if let Some(due) = crate::due::due_at(&format!("by {}", &rest[i..]), said_at_ms) {
            return Some(due);
        }
    }
    None
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x4E00..=0x9FFF | 0xFF66..=0xFF9F)
}

/// Whether `lower` holds an auxiliary + participle of a resolution as whole words.
fn passive_resolved(lower: &str) -> bool {
    RESOLVED_PASSIVE_AUX.iter().any(|aux| {
        RESOLVED_PARTICIPLES.iter().any(|pp| {
            let phrase = format!("{aux}{pp}");
            lower.match_indices(&phrase).any(|(at, _)| {
                let before = lower[..at].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
                let after = lower[at + phrase.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
                before && after
            })
        })
    })
}

/// The passive of a cancellation, "dropped off" (a delivery) refused.
fn passive_cancelled(lower: &str) -> bool {
    CANCELLED_PASSIVE.iter().any(|cue| {
        lower.match_indices(cue).any(|(at, _)| !lower[at + cue.len()..].starts_with(" off"))
    })
}

/// The new due instant a Japanese re-date names next to its cue at byte `cue_at`: the nearest date
/// before the cue (after a "から", the old date's end), else the first one after it.
fn ja_redated_due(lower: &str, cue_at: usize, cue_len: usize, said_at_ms: i64) -> Option<i64> {
    let before = &lower[..cue_at];
    let before = before.rfind("から").map_or(before, |i| &before[i + "から".len()..]);
    // The shortest suffix that holds a date is the date nearest the cue; a suffix starts at a word
    // boundary so "21" is not read out of "september 21".
    let mut found = None;
    for (i, _) in before.char_indices().rev().take(80) {
        let boundary = before[..i]
            .chars()
            .next_back()
            .is_none_or(|p| !(p.is_ascii_alphanumeric() || p == '/' || p == '-'));
        if !boundary {
            continue;
        }
        if let Some(due) = crate::due::due_at(&format!("{}までに", &before[i..]), said_at_ms) {
            found = Some((i, due));
            break;
        }
    }
    if let Some((i, due)) = found {
        // "来週の金曜に変更": the week word before the shortest date belongs to it.
        let head = &before[..i];
        for week in ["来週の", "来週"] {
            if head.ends_with(week) {
                let start = i - week.len();
                return crate::due::due_at(&format!("{}までに", &before[start..]), said_at_ms).or(Some(due));
            }
        }
        return Some(due);
    }
    let after = &lower[cue_at + cue_len..];
    first_date_after(after, said_at_ms)
}

/// The outcome a Japanese sentence states (E51), if any.
fn detect_ja(span: &str, said_at_ms: i64) -> Option<Outcome<'_>> {
    let lower = span.to_lowercase();
    if any_in(&lower, JA_NOT_STATED) {
        return None;
    }
    if let Some((at, cue)) = JA_REDATED.iter().find_map(|cue| lower.find(cue).map(|at| (at, *cue))) {
        if lower.contains("ません") {
            return None;
        }
        let due = ja_redated_due(&lower, at, cue.len(), said_at_ms)?;
        return Some(Outcome { kind: UpdateKind::Redated, quote: span, due_at: Some(due) });
    }
    if any_in(&lower, JA_CANCELLED_NEGATIVE) || (any_in(&lower, JA_CANCELLED) && !lower.contains("ません")) {
        return Some(Outcome { kind: UpdateKind::Cancelled, quote: span, due_at: None });
    }
    if any_in(&lower, JA_RESOLVED) && !any_in(&lower, JA_NOT_DONE) && !any_in(&format!(" {lower}"), NOT_DONE) {
        return Some(Outcome { kind: UpdateKind::Resolved, quote: span, due_at: None });
    }
    None
}

/// The outcome sentences of `text` (English), said at `said_at_ms`. A question, a plan, a
/// negation or a memory of long ago is not an outcome; a re-date needs a date the parser reads.
pub fn detect(text: &str, said_at_ms: i64) -> Vec<Outcome<'_>> {
    let mut out = Vec::new();
    for (span, question) in spans(text) {
        if question {
            continue;
        }
        if span.chars().any(is_cjk) {
            if let Some(outcome) = detect_ja(span, said_at_ms) {
                out.push(outcome);
                continue;
            }
        }
        let lower = format!(" {}", span.to_lowercase());
        let (bare, body) = bodies(lower.trim_start());
        let stated = !any_in(&lower, NOT_STATED);
        if stated && (any_in(&lower, REDATED) || any_in(&lower, REDATED_LOOSE)) {
            if let Some(due) = redated_due(&lower, said_at_ms) {
                out.push(Outcome { kind: UpdateKind::Redated, quote: span, due_at: Some(due) });
            }
            continue;
        }
        if stated && (any_in(&lower, CANCELLED) || passive_cancelled(&lower)) {
            out.push(Outcome { kind: UpdateKind::Cancelled, quote: span, due_at: None });
            continue;
        }
        let opener = |s: &str| RESOLVED_OPENERS.iter().any(|o| s.starts_with(o));
        if !any_in(&lower, NOT_DONE)
            && (opener(bare) || opener(body) || any_in(&lower, RESOLVED_ANYWHERE) || passive_resolved(&lower))
        {
            out.push(Outcome { kind: UpdateKind::Resolved, quote: span, due_at: None });
        }
    }
    out
}

/// The word tokens of `text`, as exact slices: runs of alphanumerics and apostrophes, also split
/// where Latin script meets Japanese ("budgetはfridayまでにoren" is budget / は / friday / までに /
/// oren), so an English name or artifact inside a Japanese sentence is a word of its own (E51).
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<(usize, bool)> = None;
    for (i, c) in text.char_indices() {
        let word = c.is_alphanumeric() || c == '\'';
        let cjk = is_cjk(c);
        match start {
            Some((s, script)) if !word || script != cjk => {
                out.push(&text[s..i]);
                start = word.then_some((i, cjk));
            }
            None if word => start = Some((i, cjk)),
            _ => {}
        }
    }
    if let Some((s, _)) = start {
        out.push(&text[s..]);
    }
    out
}

/// The content words of `text`: lower-cased alphanumeric tokens of three or more characters (two
/// when they hold a digit, "q3"), stop words and date words removed. Japanese runs are left out:
/// particles and verb endings say nothing about which item a sentence is about, and the names and
/// artifacts in a Japanese sentence about an English-named item are written in Latin script.
pub fn content_words(text: &str) -> HashSet<String> {
    tokens(text)
        .into_iter()
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.chars().any(is_cjk))
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 || (w.chars().count() == 2 && w.chars().any(|c| c.is_ascii_digit())))
        // A bare one- or two-digit number is a day of a date ("September 21"), not an item's name.
        .filter(|w| !(w.len() <= 2 && w.chars().all(|c| c.is_ascii_digit())))
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// The content words of `text` written as names: capitalised, and not the first word of the text
/// or of a speaker label's line ("Oren Vale: sent it" → vale … but Oren as the label is the
/// speaker, not the subject; "Sent the deck to Oren" → oren).
fn name_words(text: &str) -> HashSet<String> {
    let body = text.split_once(": ").map_or(text, |(label, rest)| {
        if label.split_whitespace().count() <= 3 { rest } else { text }
    });
    let content = content_words(body);
    tokens(body)
        .into_iter()
        .skip(1)
        .filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| content.contains(w))
        .collect()
}

/// An open row of the event's thread an outcome may be about.
#[derive(Debug, Clone)]
struct Target {
    table: StateTable,
    id: i64,
    description: String,
    due_at: Option<i64>,
    /// A local candidate: below the dashboard line, not the user's, never Batch-reviewed.
    local: bool,
}

/// Open rows with evidence in `thread_key` said before `before_ts` (not on `event_id` itself).
fn thread_targets(conn: &Connection, thread_key: &str, event_id: i64, before_ts: i64) -> Result<Vec<Target>, rusqlite::Error> {
    targets(conn, Some(thread_key), event_id, before_ts)
}

/// Open rows with evidence said before `before_ts` (not on `event_id` itself), in `thread_key`
/// when one is given, anywhere otherwise.
fn targets(conn: &Connection, thread_key: Option<&str>, event_id: i64, before_ts: i64) -> Result<Vec<Target>, rusqlite::Error> {
    let mut out = Vec::new();
    for (table, open, due) in [
        (StateTable::Commitments, "('open', 'overdue')", "s.due_at"),
        (StateTable::OpenLoops, "('open')", "NULL"),
    ] {
        let t = table.as_str();
        let sql = format!(
            "SELECT s.id, s.description, {due},
                    s.confidence < ?4 AND s.user_pinned = 0 AND s.user_edited_at IS NULL AND s.wording_edited = 0
                      AND s.horizon IS NULL
                      AND NOT EXISTS (SELECT 1 FROM state_provenance r WHERE r.state_table = '{t}'
                                       AND r.state_id = s.id AND r.batch_reviewed = 1)
               FROM {t} s
              WHERE s.status IN {open} AND s.aged_at IS NULL AND s.merged_into IS NULL
                AND EXISTS (SELECT 1 FROM state_provenance p JOIN event_log e ON e.id = p.event_id
                             WHERE p.state_table = '{t}' AND p.state_id = s.id
                               AND (?1 IS NULL OR e.thread_key = ?1) AND e.id != ?2 AND e.ts <= ?3)
              ORDER BY s.id"
        );
        let rows = conn
            .prepare(&sql)?
            .query_map(params![thread_key, event_id, before_ts, DASHBOARD_LINE], |r| {
                Ok(Target { table, id: r.get(0)?, description: r.get(1)?, due_at: r.get(2)?, local: r.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        out.extend(rows);
    }
    Ok(out)
}

/// The one row an outcome names: the most shared content words, at least [`MIN_SHARED_WORDS`] and
/// at least half of the outcome's own; a tie is refused (the sentence could be about either).
fn pick<'t>(targets: &'t [Target], outcome: &Outcome<'_>) -> Option<&'t Target> {
    let words = content_words(outcome.quote);
    if words.is_empty() {
        return None;
    }
    let mut best: Option<(&Target, usize)> = None;
    let mut tied = false;
    for target in targets {
        if outcome.kind == UpdateKind::Redated && target.table != StateTable::Commitments {
            continue;
        }
        let shared = content_words(&target.description).intersection(&words).count();
        if shared < MIN_SHARED_WORDS || 2 * shared < words.len() {
            continue;
        }
        match best {
            Some((_, n)) if n > shared => {}
            Some((_, n)) if n == shared => tied = true,
            _ => {
                best = Some((target, shared));
                tied = false;
            }
        }
    }
    if tied {
        return None;
    }
    best.map(|(t, _)| t)
}

/// The one open row anywhere an outcome posted on another thread names (E51): at least
/// [`MIN_SHARED_WORDS_CROSS_THREAD`] shared content words and at least half of the outcome's own;
/// among them a name (capitalised in the row or the outcome: the counterparty or the project) and
/// a word that is not one (the artifact); and every other open row at least
/// [`CROSS_THREAD_MARGIN`] shared words behind it. A re-date names commitments only, and a hedged
/// sentence or a retracted offer names nothing.
fn pick_cross_thread<'t>(targets: &'t [Target], outcome: &Outcome<'_>) -> Option<&'t Target> {
    // A retracted offer ("Scratch my offer on the schedule for Tamsin") withdraws the offer it
    // states itself; on another thread it is no evidence about an earlier promise.
    if crate::extract::hedged_or_retracted(outcome.quote) {
        return None;
    }
    let words = content_words(outcome.quote);
    if words.len() < MIN_SHARED_WORDS_CROSS_THREAD {
        return None;
    }
    let mut scored = targets
        .iter()
        .filter(|t| outcome.kind != UpdateKind::Redated || t.table == StateTable::Commitments)
        .map(|t| {
            let shared: HashSet<String> = content_words(&t.description).intersection(&words).cloned().collect();
            (t, shared)
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(_, shared)| std::cmp::Reverse(shared.len()));
    let (target, shared) = scored.first()?;
    let runner_up = scored.get(1).map_or(0, |(_, s)| s.len());
    if shared.len() < MIN_SHARED_WORDS_CROSS_THREAD
        || 2 * shared.len() < words.len()
        || shared.len() < runner_up + CROSS_THREAD_MARGIN
    {
        return None;
    }
    let names: HashSet<String> = name_words(outcome.quote).union(&name_words(&target.description)).cloned().collect();
    let named = shared.iter().any(|w| names.contains(w));
    let artifact = shared.iter().any(|w| !names.contains(w));
    (named && artifact).then_some(*target)
}

/// What one pass did, counts only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub scanned: usize,
    /// Updates recorded (applied or left for the consolidator).
    pub recorded: usize,
    /// Local candidates closed.
    pub closed: usize,
    /// Local candidates re-dated.
    pub redated: usize,
}

const SAME_DAY_MS: i64 = 12 * 3_600_000;

/// Record `outcome` from `event_id` against `target`, and apply it when the target is a local
/// candidate. Returns `(recorded, applied)`. A repeat of (row, event, kind) records nothing.
fn record(conn: &Connection, target: &Target, event_id: i64, outcome: &Outcome<'_>, now: i64) -> Result<(bool, bool), rusqlite::Error> {
    if outcome.kind == UpdateKind::Redated {
        // A "move" to the day it already holds is no news.
        if let (Some(old), Some(new)) = (target.due_at, outcome.due_at) {
            if (old - new).abs() < SAME_DAY_MS {
                return Ok((false, false));
            }
        }
    }
    let Some(quote) = crate::sanitize::persist_generated(outcome.quote) else { return Ok((false, false)) };
    let quote = quote.text.into_owned();
    let t = target.table.as_str();
    let added = conn.execute(
        "INSERT OR IGNORE INTO loop_updates (kind, state_table, state_id, event_id, quote, due_at, previous_due_at,
                                             applied, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8 WHERE EXISTS (SELECT 1 FROM event_log WHERE id = ?4)",
        params![outcome.kind.as_str(), t, target.id, event_id, quote, outcome.due_at, target.due_at, now],
    )?;
    if added == 0 || !target.local {
        return Ok((added > 0, false));
    }
    let ctx = crate::actor::current_write();
    let changed = match (outcome.kind, target.table) {
        (UpdateKind::Redated, StateTable::Commitments) => conn.execute(
            "UPDATE commitments SET due_at = ?2, updated_at = MAX(updated_at, ?3), actor_last = ?4,
                    status = CASE WHEN status = 'overdue' THEN 'open' ELSE status END
              WHERE id = ?1 AND status IN ('open', 'overdue')",
            params![target.id, outcome.due_at, now, ctx.actor_sql()],
        )?,
        (UpdateKind::Redated, _) => 0,
        (kind, table) => {
            let status = match (kind, table) {
                (UpdateKind::Resolved, StateTable::Commitments) => "done",
                (_, StateTable::Commitments) => "cancelled",
                _ => "closed",
            };
            conn.execute(
                &format!("UPDATE {t} SET status = ?2, updated_at = MAX(updated_at, ?3), actor_last = ?4 WHERE id = ?1"),
                params![target.id, status, now, ctx.actor_sql()],
            )?
        }
    };
    if changed == 0 {
        return Ok((true, false));
    }
    // The outcome's words are evidence of what happened to the row, never a Batch review.
    conn.execute(
        "INSERT INTO state_provenance (state_table, state_id, event_id, weight, quote, batch_reviewed)
         SELECT ?1, ?2, ?3, 1.0, ?4, 0 WHERE NOT EXISTS
           (SELECT 1 FROM state_provenance WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3)",
        params![t, target.id, event_id, quote],
    )?;
    conn.execute(
        "UPDATE loop_updates SET applied = 1 WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3 AND kind = ?4",
        params![t, target.id, event_id, outcome.kind.as_str()],
    )?;
    Ok((true, true))
}

/// Read the outcomes of one event (its content, time and thread) and record / apply them.
pub fn reconcile_event(conn: &Connection, event_id: i64, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let mut done = Reconciled { scanned: 1, ..Reconciled::default() };
    let row: Option<(String, i64, Option<String>, String)> = conn
        .query_row("SELECT content, ts, thread_key, source FROM event_log WHERE id = ?1", [event_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let Some((content, ts, thread, source)) = row else { return Ok(done) };
    // Screen text never names a row of another thread, whatever thread it was filed under.
    let cross_thread_allowed = !crate::event_log::APP_SCOPED_SOURCES.contains(&source.as_str());
    let batch_visible = !crate::event_log::BATCH_EXCLUDED_SOURCES.contains(&source.as_str());
    let Some(thread) = thread else { return reconcile_unthreaded_event(conn, event_id, &content, ts, now) };
    let outcomes = detect(&content, ts);
    if outcomes.is_empty() {
        return Ok(done);
    }
    for outcome in &outcomes {
        // Re-read per outcome: an earlier outcome of this event may have closed a row.
        let targets = thread_targets(conn, &thread, event_id, ts)?;
        let target = match pick(&targets, outcome) {
            Some(target) => target.clone(),
            None if cross_thread_allowed => {
                // E51: an outcome posted on another thread, under the stricter rule.
                let everywhere = self::targets(conn, None, event_id, ts)?;
                match pick_cross_thread(&everywhere, outcome) {
                    // A lead for the consolidator must be text it is shown.
                    Some(target) if target.local || batch_visible => target.clone(),
                    _ => continue,
                }
            }
            None => continue,
        };
        let target = &target;
        let (recorded, applied) = record(conn, target, event_id, outcome, now)?;
        done.recorded += usize::from(recorded);
        if applied {
            match outcome.kind {
                UpdateKind::Redated => done.redated += 1,
                _ => done.closed += 1,
            }
        }
        if applied {
            let _ = crate::search_documents::project_source_after_write(
                conn,
                &crate::state_edit::source_ref(target.table, target.id),
                now,
            );
        }
    }
    Ok(done)
}

/// The one open row anywhere a thread-less outcome names: at least
/// [`MIN_SHARED_WORDS_UNTHREADED`] shared content words and half of the outcome's own, and no other
/// open row sharing even [`MIN_SHARED_WORDS`] (two rows it could be about is one too many).
fn pick_unthreaded<'t>(targets: &'t [Target], outcome: &Outcome<'_>) -> Option<&'t Target> {
    let words = content_words(outcome.quote);
    let mut close = targets
        .iter()
        .map(|t| (t, content_words(&t.description).intersection(&words).count()))
        .filter(|(_, shared)| *shared >= MIN_SHARED_WORDS);
    let (target, shared) = close.next()?;
    if close.next().is_some() || shared < MIN_SHARED_WORDS_UNTHREADED || 2 * shared < words.len() {
        return None;
    }
    Some(target)
}

/// An event without a thread (see the module notes): never applied, never a re-date, never about a
/// local candidate; a resolution or cancellation that names exactly one consolidator-owned row is
/// recorded unread for the consolidator, once per (row, kind, quote) however often the text is
/// captured again. Sources the Batch lane may not see (meeting text) are skipped: the consolidator
/// would never be shown them.
fn reconcile_unthreaded_event(conn: &Connection, event_id: i64, content: &str, ts: i64, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let mut done = Reconciled { scanned: 1, ..Reconciled::default() };
    let outcomes: Vec<Outcome<'_>> = detect(content, ts).into_iter().filter(|o| o.kind != UpdateKind::Redated).collect();
    if outcomes.is_empty() {
        return Ok(done);
    }
    let source: Option<String> = conn.query_row("SELECT source FROM event_log WHERE id = ?1", [event_id], |r| r.get(0)).optional()?;
    if source.is_some_and(|s| crate::event_log::BATCH_EXCLUDED_SOURCES.contains(&s.as_str())) {
        return Ok(done);
    }
    for outcome in &outcomes {
        let targets = targets(conn, None, event_id, ts)?;
        let Some(target) = pick_unthreaded(&targets, outcome) else { continue };
        if target.local {
            continue; // a local candidate changes only on its own thread's words
        }
        let Some(quote) = crate::sanitize::persist_generated(outcome.quote) else { continue };
        let seen: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM loop_updates WHERE state_table = ?1 AND state_id = ?2 AND kind = ?3 AND quote = ?4)",
            params![target.table.as_str(), target.id, outcome.kind.as_str(), quote.text.as_ref()],
            |r| r.get(0),
        )?;
        if seen {
            continue; // the same words captured again
        }
        let (recorded, applied) = record(conn, target, event_id, outcome, now)?;
        debug_assert!(!applied, "a consolidator-owned row is never applied locally");
        done.recorded += usize::from(recorded);
    }
    Ok(done)
}

/// One pass over the events inserted since the last pass (at most [`SCAN_BATCH`]; threaded and
/// not, [`reconcile_event`]); the first pass after the upgrade starts [`FIRST_SCAN_LOOKBACK_MS`]
/// back. Idempotent.
pub fn reconcile(conn: &mut Connection, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let tx = conn.savepoint()?;
    let watermark: Option<i64> = tx
        .query_row("SELECT last_event_id FROM loop_update_scan WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    let from = match watermark {
        Some(id) => id,
        None => tx.query_row(
            "SELECT COALESCE((SELECT MIN(id) FROM event_log WHERE ts >= ?1) - 1, (SELECT MAX(id) FROM event_log), 0)",
            [now - FIRST_SCAN_LOOKBACK_MS],
            |r| r.get(0),
        )?,
    };
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM event_log WHERE id > ?1 ORDER BY id LIMIT ?2")?
        .query_map(params![from, SCAN_BATCH], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut total = Reconciled::default();
    for &id in &ids {
        let one = reconcile_event(&tx, id, now)?;
        total.scanned += one.scanned;
        total.recorded += one.recorded;
        total.closed += one.closed;
        total.redated += one.redated;
    }
    let last = ids.last().copied().unwrap_or(from);
    tx.execute(
        "INSERT INTO loop_update_scan (id, last_event_id) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET last_event_id = MAX(last_event_id, excluded.last_event_id)",
        [last],
    )?;
    tx.commit()?;
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::{
        insert_commitment_with_quote, insert_open_loop_with_quote, CommitmentDirection, CommitmentStatus, NewCommitment,
        NewOpenLoop, OpenLoopKind, Provenance,
    };

    const HOUR: i64 = 3_600_000;
    /// Monday 2026-09-07 10:00 UTC.
    const MON: i64 = 1_788_739_200_000 + 10 * HOUR;

    fn kinds(text: &str) -> Vec<(UpdateKind, &str)> {
        detect(text, MON).into_iter().map(|o| (o.kind, o.quote)).collect()
    }

    #[test]
    fn resolutions_cancellations_and_moves_are_read_with_their_exact_span() {
        for text in [
            "Sent the Q3 budget to Oren Vale.",
            "Done: the board memo is with Priya Nand now.",
            "Oren Vale sent the pricing deck over, thanks.",
            "Got the vendor contract from Kenji Arai.",
            "The latency report arrived from Dana Okafor, thanks.",
            "Replied to Mateo Ruiz about the customer FAQ.",
            "Finished the review of the design spec for Sven Morrow.",
            "Review of the test plan done and sent back to Noor Haddad.",
            "Decided: Heron ships with the rollout plan.",
            "Settled it, Kestrel goes out with the audit evidence.",
        ] {
            assert_eq!(kinds(text), vec![(UpdateKind::Resolved, text.trim_end_matches('.'))], "{text}");
        }
        for text in [
            "Never mind about the hiring scorecard for Felix Dorn, it is no longer needed.",
            "Dropping the partner brief for Amara Quist; the plan changed.",
        ] {
            assert_eq!(kinds(text)[0].0, UpdateKind::Cancelled, "{text}");
        }
        let moved = detect("Hi Oren,\n\nUpdate: the board memo for Oren Vale moves to Thursday.\n\nThanks", MON);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].kind, UpdateKind::Redated);
        assert_eq!(moved[0].quote, "Update: the board memo for Oren Vale moves to Thursday");
        // Thursday after Monday 2026-09-07 is 2026-09-10.
        let day = crate::due::civil_from_days(moved[0].due_at.unwrap().div_euclid(86_400_000));
        assert_eq!((day.month, day.day), (9, 10));
        for text in ["Heads up, the memo for Oren slips to September 21.", "Change of plan, the memo for Oren is now due Friday."] {
            assert_eq!(kinds(text)[0].0, UpdateKind::Redated, "{text}");
        }
    }

    #[test]
    fn plans_questions_denials_and_old_news_are_not_outcomes() {
        for text in [
            "I'll send the Q3 budget to Oren by Friday.",
            "Has Oren sent the deck yet?",
            "Oren hasn't sent the deck.",
            "Still waiting on Oren for the deck.",
            "Last month I sent the renewal quote to Lucia as promised.",
            "Back in August Felix got the training schedule from me, all settled.",
            "I sent a reminder to Oren about the deck.",
            "If the memo slips to Friday, tell me.",
            "Maybe the launch moves to Thursday.",
            "Waiting for Priya to send the API migration notes.",
            "lunch at noon?",
        ] {
            assert!(detect(text, MON).is_empty(), "{text}: {:?}", detect(text, MON));
        }
        // A move with no date the parser reads is not a re-date.
        assert!(detect("The memo moves to the next phase.", MON).is_empty());
    }

    #[test]
    fn content_words_drop_function_words_cues_and_dates() {
        let words = content_words("Update: the Q3 budget for Oren Vale moves to Thursday.");
        let mut sorted: Vec<_> = words.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["budget", "oren", "q3", "vale"]);
    }

    fn event(conn: &Connection, n: i64, ts: i64, thread: Option<&str>, content: &str) -> i64 {
        let id = crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "agent",
                kind: "email",
                app_bundle_id: None,
                window_title: Some("loop"),
                content,
                content_hash: &format!("h{n}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute("UPDATE event_log SET thread_key = ?2 WHERE id = ?1", params![id, thread]).unwrap();
        id
    }

    fn commitment(conn: &mut Connection, ev: i64, text: &str, due: Option<i64>, confidence: f64) -> i64 {
        insert_commitment_with_quote(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: text,
                due_at: due,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence,
                now: MON,
            },
            &[Provenance::new(ev)],
            Some(text),
        )
        .unwrap()
    }

    fn status(conn: &Connection, table: &str, id: i64) -> (String, f64, Option<i64>) {
        let due = if table == "commitments" { "due_at" } else { "NULL" };
        conn.query_row(&format!("SELECT status, confidence, {due} FROM {table} WHERE id = ?1"), [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
    }

    #[test]
    fn a_later_message_in_the_thread_closes_or_moves_a_local_candidate_without_lifting_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let due = crate::due::due_at(said, MON);
        let c = commitment(&mut conn, e1, said, due, 0.35);
        let promise = "Can you confirm the numbers in the customer FAQ?";
        let e2 = event(&conn, 2, MON + HOUR, Some("t2"), promise);
        let l = insert_open_loop_with_quote(
            &mut conn,
            &NewOpenLoop { kind: OpenLoopKind::ReplyNeeded, description: promise, counterparty_id: None, project_id: None, opened_at: MON, confidence: 0.3, now: MON },
            &[Provenance::new(e2)],
            Some(promise),
        )
        .unwrap();
        // Moved, then done; the loop is answered in its own thread.
        event(&conn, 3, MON + 2 * HOUR, Some("t1"), "Update: the Q3 budget for Oren Vale moves to Thursday.");
        let out = reconcile(&mut conn, MON + 3 * HOUR).unwrap();
        assert_eq!((out.recorded, out.redated, out.closed), (1, 1, 0), "{out:?}");
        let (st, conf, due) = status(&conn, "commitments", c);
        assert_eq!((st.as_str(), conf), ("open", 0.35), "a move never lifts confidence");
        let day = crate::due::civil_from_days(due.unwrap().div_euclid(86_400_000));
        assert_eq!((day.month, day.day), (9, 10));
        let e4 = event(&conn, 4, MON + 4 * HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 5, MON + 4 * HOUR, Some("t2"), "Replied to Oren about the customer FAQ.");
        let out = reconcile(&mut conn, MON + 5 * HOUR).unwrap();
        assert_eq!(out.closed, 2, "{out:?}");
        assert_eq!(status(&conn, "commitments", c).0, "done");
        assert_eq!(status(&conn, "commitments", c).1, 0.35);
        assert_eq!(status(&conn, "open_loops", l).0, "closed");
        // The outcome's quote is the row's provenance, never a Batch review.
        let (quote, reviewed): (String, i64) = conn
            .query_row(
                "SELECT quote, batch_reviewed FROM state_provenance WHERE state_table = 'commitments' AND state_id = ?1 AND event_id = ?2",
                params![c, e4],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((quote.as_str(), reviewed), ("Sent the Q3 budget to Oren Vale", 0));
        // Idempotent: a second pass over nothing new records nothing.
        assert_eq!(reconcile(&mut conn, MON + 6 * HOUR).unwrap(), Reconciled::default());
        let updates: i64 = conn.query_row("SELECT count(*) FROM loop_updates WHERE applied = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(updates, 3);
    }

    /// E51: before the cross-thread rule this test was "another thread changes nothing"; the
    /// outcome on t9 below is now refused because three open rows fit it equally (c, a and b), not
    /// because it is on another thread ([`an_outcome_on_another_thread_closes_the_one_row_it_clearly_names`]).
    #[test]
    fn another_thread_an_unrelated_sentence_or_a_tie_changes_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let c = commitment(&mut conn, e1, said, None, 0.35);
        event(&conn, 2, MON + HOUR, Some("t9"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 3, MON + HOUR, None, "Sent the Q3 budget to Oren Vale.");
        event(&conn, 4, MON + HOUR, Some("t1"), "Sent the pricing deck to Priya Nand.");
        // Two rows of one thread the same outcome fits equally: refused.
        let said2 = "I'll send the Q3 budget to Oren Vale by Monday.";
        let e5 = event(&conn, 5, MON, Some("t3"), said);
        let e6 = event(&conn, 6, MON, Some("t3"), said2);
        let a = commitment(&mut conn, e5, said, None, 0.35);
        let b = commitment(&mut conn, e6, said2, None, 0.35);
        event(&conn, 7, MON + HOUR, Some("t3"), "Sent the Q3 budget to Oren Vale.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!(out.closed, 0, "{out:?}");
        for id in [c, a, b] {
            assert_eq!(status(&conn, "commitments", id).0, "open");
        }
    }

    #[test]
    fn a_reviewed_or_pinned_row_is_left_to_the_consolidator_or_the_user() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let reviewed = commitment(&mut conn, e1, said, None, 0.9);
        let said2 = "I'll send the board memo to Priya Nand by Friday.";
        let e2 = event(&conn, 2, MON, Some("t2"), said2);
        let pinned = commitment(&mut conn, e2, said2, None, 0.35);
        conn.execute("UPDATE commitments SET user_pinned = 1 WHERE id = ?1", [pinned]).unwrap();
        event(&conn, 3, MON + HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 4, MON + HOUR, Some("t2"), "Sent the board memo to Priya Nand.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed), (2, 0), "{out:?}");
        assert_eq!(status(&conn, "commitments", reviewed).0, "open");
        assert_eq!(status(&conn, "commitments", pinned).0, "open");
        let unread: i64 = conn
            .query_row("SELECT count(*) FROM loop_updates WHERE applied = 0 AND read_at IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unread, 2, "kept for the consolidator");
    }

    #[test]
    fn the_first_pass_starts_a_bounded_window_back() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = MON - 40 * 86_400_000;
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, old, Some("t1"), said);
        let c = commitment(&mut conn, e1, said, None, 0.35);
        event(&conn, 2, old + HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        let out = reconcile(&mut conn, MON).unwrap();
        assert_eq!(out.closed, 0, "older than the first-pass window: {out:?}");
        assert_eq!(status(&conn, "commitments", c).0, "open");
    }

    fn unread(conn: &Connection) -> Vec<(String, i64, String)> {
        conn.prepare("SELECT kind, state_id, quote FROM loop_updates WHERE applied = 0 AND read_at IS NULL ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// E49: text without a thread (screen capture) never closes or re-dates a row. A resolution
    /// that names exactly one consolidator-owned row is left unread for the consolidator, once;
    /// a local candidate, a re-date, a weak or ambiguous match and meeting text leave nothing.
    #[test]
    fn text_without_a_thread_only_leaves_an_unambiguous_lead_for_the_consolidator() {
        let mut conn = crate::open_in_memory().unwrap();
        let local_said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), local_said);
        let local = commitment(&mut conn, e1, local_said, None, 0.35);
        let reviewed_said = "I'll send the board memo to Priya Nand by Friday.";
        let e2 = event(&conn, 2, MON, Some("t2"), reviewed_said);
        let reviewed = commitment(&mut conn, e2, reviewed_said, crate::due::due_at(reviewed_said, MON), 0.9);

        // A local candidate is not touched by text without a thread, however clear.
        event(&conn, 3, MON + HOUR, None, "Sent the Q3 budget to Oren Vale.");
        // A re-date read off a screen is never taken: the capture time is not when it was said.
        event(&conn, 4, MON + HOUR, None, "Update: the board memo for Priya Nand moves to Thursday.");
        // Too few shared words to name the row.
        event(&conn, 5, MON + HOUR, None, "Sent it to Priya.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed, out.redated), (0, 0, 0), "{out:?}");
        assert_eq!(status(&conn, "commitments", local).0, "open");
        assert!(unread(&conn).is_empty());

        // The resolution of the consolidator's row is left for it, unapplied; the row stays open.
        let done = "Sent the board memo to Priya Nand";
        event(&conn, 6, MON + 3 * HOUR, None, &format!("{done}. Next: lunch."));
        // The same words captured again in a later frame are not a second lead.
        event(&conn, 7, MON + 3 * HOUR + 60_000, None, &format!("{done}. Next: lunch at noon."));
        let out = reconcile(&mut conn, MON + 4 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed), (1, 0), "{out:?}");
        assert_eq!(unread(&conn), vec![("resolved".to_string(), reviewed, done.to_string())]);
        let (st, conf, _) = status(&conn, "commitments", reviewed);
        assert_eq!((st.as_str(), conf), ("open", 0.9), "only the consolidator closes it, through its own quote");
        let input = crate::loops::consolidation_input(&conn, MON + 4 * HOUR).unwrap().unwrap();
        assert!(input.text.contains(&format!("C{reviewed} | resolved |")), "{}", input.text);

        // Two open rows the words could be about: nothing.
        let other_said = "I'll send the board deck to Priya Nand by Monday.";
        let e8 = event(&conn, 8, MON + 5 * HOUR, Some("t3"), other_said);
        commitment(&mut conn, e8, other_said, None, 0.9);
        event(&conn, 9, MON + 6 * HOUR, None, "Sent the board stuff to Priya Nand.");
        let out = reconcile(&mut conn, MON + 7 * HOUR).unwrap();
        assert_eq!(out.recorded, 0, "{out:?}");
    }

    fn days(due: Option<i64>) -> Option<(u32, u32)> {
        due.map(|ms| {
            let d = crate::due::civil_from_days(ms.div_euclid(86_400_000));
            (d.month, d.day)
        })
    }

    /// E51: passive resolutions, cancellations and moves.
    #[test]
    fn passive_outcomes_are_read() {
        for text in [
            "The Q3 budget was handed to Oren Vale this morning.",
            "The vendor contract has been signed by Kenji Arai.",
            "The design spec review was submitted to Sven Morrow.",
            "The pricing deck got approved by Priya Nand.",
            "The onboarding guide has been taken care of for Felix Dorn.",
        ] {
            assert_eq!(kinds(text), vec![(UpdateKind::Resolved, text.trim_end_matches('.'))], "{text}");
        }
        for text in [
            "The offsite with Amara Quist has been called off.",
            "The partner brief was shelved after the reorg.",
            "The training schedule for Felix has been dropped.",
        ] {
            assert_eq!(kinds(text)[0].0, UpdateKind::Cancelled, "{text}");
        }
        // Monday 2026-09-07: Thursday is 09-10.
        let moved = detect("The design review with Sven has been rescheduled; the new date is Thursday.", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 10))));
        let moved = detect("The deadline for the board memo was extended to September 21.", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 21))));
        let moved = detect("The launch checklist was postponed, it is now planned for Friday.", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 11))));
    }

    #[test]
    fn passive_plans_denials_questions_and_deliveries_are_not_outcomes() {
        for text in [
            "The budget will be shared with Oren on Friday.",
            "The contract has not been signed yet.",
            "The contract hasn't been sent.",
            "Was the deck sent to Oren?",
            "If the memo is rescheduled to Friday, tell me.",
            "The review may be postponed to Friday.",
            "The launch was postponed until further notice.",
            "The deck was dropped off at reception.",
            "The report was sentenced to another review cycle.",
        ] {
            assert!(kinds(text).is_empty(), "{text}: {:?}", kinds(text));
        }
    }

    /// E51: Japanese sentences around English names and dates.
    #[test]
    fn japanese_outcomes_are_read_with_their_exact_span() {
        for text in [
            "the Q3 budgetはOren Valeさんへ送付済みです",
            "Kenjiさんからvendor contractが届きました",
            "design specのレビューを返信しました",
            "pricing deckはPriyaさんにお渡ししました",
        ] {
            assert_eq!(kinds(text), vec![(UpdateKind::Resolved, text)], "{text}");
        }
        for text in ["the offsite planは中止になりました", "pricing deckはもう必要ありません", "partner briefは見送りになりました"] {
            assert_eq!(kinds(text), vec![(UpdateKind::Cancelled, text)], "{text}");
        }
        let moved = detect("the board memoの締切はSeptember 21に変更になりました。", MON);
        assert_eq!(moved[0].quote, "the board memoの締切はSeptember 21に変更になりました");
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 21))));
        // From the old date to the new: the date next to the cue is the new one.
        let moved = detect("design reviewは金曜日から9月14日に延期になりました", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 14))));
        let moved = detect("launch checklistの期限はFridayからThursdayに変更です", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 10))));
        let moved = detect("Heronの定例は来週の金曜に変更になりました", MON);
        assert_eq!((moved[0].kind, days(moved[0].due_at)), (UpdateKind::Redated, Some((9, 18))));
    }

    #[test]
    fn japanese_plans_denials_questions_and_maybes_are_not_outcomes() {
        for text in [
            "the Q3 budgetはまだ送っていません",
            "予算が通ったらOrenさんに送付済みにします",
            "the offsiteは中止になるかもしれません",
            "the board memoは来週送付予定です",
            "締切は変更になりましたか",
            "キャンセル待ちです",
            "会議の日程が変更になりました",
            "先月vendor contractを送りました",
            "中止はしません",
        ] {
            assert!(kinds(text).is_empty(), "{text}: {:?}", kinds(text));
        }
    }

    #[test]
    fn content_words_split_latin_from_japanese() {
        let words = content_words("the Q3 budgetはFridayまでにOren Valeさんへお送りします");
        let mut sorted: Vec<_> = words.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["budget", "oren", "q3", "vale"]);
        let names = name_words("Sent the Q3 budget to Oren Vale.");
        assert!(names.contains("oren") && names.contains("vale") && names.contains("q3"), "{names:?}");
        assert!(!name_words("Oren Vale: sent the deck").contains("oren"), "the speaker label is not the subject");
    }

    fn open_loop(conn: &mut Connection, ev: i64, text: &str, confidence: f64) -> i64 {
        insert_open_loop_with_quote(
            conn,
            &NewOpenLoop { kind: OpenLoopKind::WaitingOnThem, description: text, counterparty_id: None, project_id: None, opened_at: MON, confidence, now: MON },
            &[Provenance::new(ev)],
            Some(text),
        )
        .unwrap()
    }

    /// E51: an outcome posted on another thread closes or re-dates the one local candidate it
    /// clearly names, never lifting it, and leaves a consolidator-owned row to the consolidator.
    #[test]
    fn an_outcome_on_another_thread_closes_the_one_row_it_clearly_names() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("mail-1"), said);
        let c = commitment(&mut conn, e1, said, crate::due::due_at(said, MON), 0.35);
        let memo = "I'll send the board memo to Priya Nand by Friday.";
        let e2 = event(&conn, 2, MON, Some("mail-2"), memo);
        let m = commitment(&mut conn, e2, memo, crate::due::due_at(memo, MON), 0.35);
        let waiting = "Waiting on Kenji Arai for the signed vendor contract.";
        let e3 = event(&conn, 3, MON, Some("chat-1"), waiting);
        let l = open_loop(&mut conn, e3, waiting, 0.35);
        let reviewed_said = "I'll send the pricing deck to Dana Okafor by Friday.";
        let e4 = event(&conn, 4, MON, Some("mail-3"), reviewed_said);
        let reviewed = commitment(&mut conn, e4, reviewed_said, None, 0.9);

        let e5 = event(&conn, 5, MON + HOUR, Some("mail-9"), "Hi all,\nThe Q3 budget was handed to Oren Vale this morning.\nThanks");
        event(&conn, 6, MON + HOUR, Some("chat-7"), "the board memoのPriya Nandさんへの締切はSeptember 21に変更になりました");
        // Without the counterparty the same move names no row: two content words, no name.
        event(&conn, 9, MON + HOUR, Some("chat-9"), "the board memoの締切はSeptember 22に変更になりました");
        event(&conn, 7, MON + HOUR, Some("chat-8"), "Kenji Arai sent the signed vendor contract over.");
        event(&conn, 8, MON + HOUR, Some("mail-10"), "The pricing deck has been shared with Dana Okafor.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed, out.redated), (4, 2, 1), "{out:?}");
        assert_eq!(status(&conn, "commitments", c), ("done".into(), 0.35, crate::due::due_at(said, MON)));
        let (st, conf, due) = status(&conn, "commitments", m);
        assert_eq!((st.as_str(), conf, days(due)), ("open", 0.35, Some((9, 21))));
        assert_eq!(status(&conn, "open_loops", l).0, "closed");
        let (st, conf, _) = status(&conn, "commitments", reviewed);
        assert_eq!((st.as_str(), conf), ("open", 0.9), "the consolidator's row is left to it");
        assert_eq!(unread(&conn).len(), 1);
        let quote: String = conn
            .query_row(
                "SELECT quote FROM state_provenance WHERE state_table = 'commitments' AND state_id = ?1 AND event_id = ?2",
                params![c, e5],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(quote, "The Q3 budget was handed to Oren Vale this morning");
    }

    /// E51 distractors: on another thread, an outcome that only shares a name, only shares the
    /// artifact, fits two rows nearly as well, or is screen or meeting text changes nothing.
    #[test]
    fn an_outcome_on_another_thread_that_is_ambiguous_or_thin_changes_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        let budget = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("mail-1"), budget);
        let b = commitment(&mut conn, e1, budget, None, 0.35);
        let forecast = "I'll send the Q3 forecast to Oren Vale by Monday.";
        let e2 = event(&conn, 2, MON, Some("mail-2"), forecast);
        let f = commitment(&mut conn, e2, forecast, None, 0.35);
        let unnamed = "i'll send the hiring scorecard draft numbers soon.";
        let e3 = event(&conn, 3, MON, Some("mail-3"), unnamed);
        let u = commitment(&mut conn, e3, unnamed, None, 0.35);
        let reviewed_said = "I'll send the board memo to Priya Nand by Friday.";
        let e4 = event(&conn, 4, MON, Some("mail-4"), reviewed_said);
        let reviewed = commitment(&mut conn, e4, reviewed_said, None, 0.9);
        let brief = "I'll send the partner brief to Amara Quist by Friday.";
        let e11 = event(&conn, 11, MON, Some("mail-5"), brief);
        let p = commitment(&mut conn, e11, brief, None, 0.35);

        // Fits the budget row and the forecast row by four words each: no margin.
        event(&conn, 5, MON + HOUR, Some("chat-1"), "Sent the Q3 budget forecast to Oren Vale.");
        // Only the counterparty.
        event(&conn, 6, MON + HOUR, Some("chat-2"), "Sent everything to Oren Vale.");
        // Only the artifact, no name.
        event(&conn, 7, MON + HOUR, Some("chat-3"), "sent the hiring scorecard draft numbers.");
        // A retracted offer of its own, about the same artifact and person.
        event(&conn, 10, MON + HOUR, Some("chat-4"), "Scratch my offer on the partner brief for Amara Quist, someone else has it.");
        // Screen text filed under a thread, and meeting text for the consolidator's row.
        crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: MON + HOUR,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.mail"),
                window_title: Some("Mail"),
                content: "Sent the Q3 budget to Oren Vale.",
                content_hash: "s1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute("UPDATE event_log SET thread_key = 'mail-77' WHERE content_hash = 's1'", []).unwrap();
        let meeting = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: MON + HOUR,
                source: "meeting",
                kind: "transcript",
                app_bundle_id: None,
                window_title: None,
                content: "The board memo was handed to Priya Nand.",
                content_hash: "m1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute("UPDATE event_log SET thread_key = 'meet-1' WHERE id = ?1", [meeting]).unwrap();
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed, out.redated), (0, 0, 0), "{out:?}");
        for id in [b, f, u, reviewed, p] {
            assert_eq!(status(&conn, "commitments", id).0, "open");
        }
    }

    /// Meeting text never reaches the Batch lane, so it leaves no lead for the consolidator.
    #[test]
    fn meeting_text_without_a_thread_leaves_no_lead() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the board memo to Priya Nand by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let reviewed = commitment(&mut conn, e1, said, None, 0.9);
        crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: MON + HOUR,
                source: "meeting",
                kind: "transcript",
                app_bundle_id: None,
                window_title: None,
                content: "Sent the board memo to Priya Nand.",
                content_hash: "m1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!(out.recorded, 0, "{out:?}");
        assert_eq!(status(&conn, "commitments", reviewed).0, "open");
    }
}
