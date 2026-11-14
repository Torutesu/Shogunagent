//! When two sightings are the same memory.
//!
//! A memory — a commitment, an open loop, a person, a project — reaches the log many times: the
//! same window is re-read as messages arrive, a reply quotes the original, a message list previews
//! it, a connector syncs what is already on screen. Each sighting is extracted again, and whether it
//! links to the existing row or becomes a twin is decided by the predicates in [`crate::state`],
//! [`crate::extract::persist_candidates_deduped`] and [`crate::knowledge::persist`].
//!
//! Comparing **volatile surface text for exact equality** — the raw window title (which carries
//! unread counters), the attribution line (which carries "Today" / "Yesterday"), the quote (which
//! a reply prefixes with `>` and a list preview truncates with `…`), and, for entities, the context
//! span the extractor happened to pick — is precise but badly fragmented: `dedup-bench` measured
//! 2.18 rows per real memory with zero wrong merges.
//!
//! This module holds the normalisations that make those comparisons stable, as pure functions, so
//! the identity rule is defined once and tested directly. Each one strips only what is volatile and
//! keeps what distinguishes: a counter is removed from a title but `Q3` and `PR #812` are not; a day
//! word is removed from an attribution but the sender's name is not. The precision scenarios in
//! `dedup-bench` (different senders, different objects, recurrences, namesakes) are the contract
//! that the stripping never goes further than that.

use rusqlite::{params, Connection, OptionalExtension};

use crate::knowledge::Kind;
use crate::state::StateTable;

/// Words that, after a number, make it a counter rather than part of the title.
const COUNTER_WORDS: &[&str] = &[
    "unread",
    "new",
    "message",
    "messages",
    "notification",
    "notifications",
    "item",
    "items",
    "mention",
    "mentions",
];

fn is_count(token: &str) -> bool {
    let t = token.trim_end_matches(['+', ',', ';']);
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == ',') && t.chars().any(|c| c.is_ascii_digit())
}

fn is_bracketed_count(token: &str) -> bool {
    let t = token.trim_end_matches([',', ';']);
    let inner = t
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .or_else(|| t.strip_prefix('[').and_then(|r| r.strip_suffix(']')));
    inner.is_some_and(is_count)
}

fn word_of(token: &str) -> String {
    token
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// A window title with its counters and unsaved markers removed: `(3) Inbox` and `(4) Inbox` are
/// the same window, as are `Inbox – 1,204 messages, 3 unread` and `Inbox – 1,205 messages`.
///
/// Only *counters* are removed — a number followed by a counter word, or a bracketed number that
/// opens the title, where browsers and chat apps put an unread badge. A number that is part of the
/// name stays: `Q3 board deck`, `PR #812` and `Invoice 1043` remain distinct windows, because the
/// same reviewer writing the same sentence in two pull requests is two memories. So does a
/// bracketed number anywhere else: `Chapter (3) - Draft` and `Untitled (2)` name documents, and a
/// trailing `Inbox (3)` cannot be told apart from them.
///
/// Deliberately narrower than [`crate::thread::normalise_window_title`], which also drops the whole
/// trailing ` - App` segment. That is right for grouping a referring question onto a thread; for
/// identity it would fuse `Inbox — Work` with `Inbox — Personal`.
pub fn stable_title(title: &str) -> String {
    let tokens: Vec<&str> = title.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let token = tokens[i];
        if i == 0 && is_bracketed_count(token) {
            i += 1;
            continue;
        }
        if is_count(token) {
            if let Some(next) = tokens.get(i + 1) {
                let next_word = word_of(next);
                if COUNTER_WORDS.contains(&next_word.as_str()) {
                    i += 2;
                    // "3 new items" / "2 new messages": the noun after "new" is part of the counter.
                    if next_word == "new" {
                        if let Some(after) = tokens.get(i) {
                            if COUNTER_WORDS.contains(&word_of(after).as_str()) {
                                i += 1;
                            }
                        }
                    }
                    continue;
                }
            }
        }
        kept.push(token);
        i += 1;
    }
    // Removing a counter that sat between two separators ("Acme - 3 new items - Slack") leaves
    // them adjacent; collapse the run so the title reads as if the counter had never been there.
    let mut cleaned: Vec<&str> = Vec::with_capacity(kept.len());
    for token in kept {
        if is_separator(token) && cleaned.last().is_none_or(|prev| is_separator(prev)) {
            continue;
        }
        cleaned.push(token);
    }
    while cleaned.last().is_some_and(|t| is_separator(t)) {
        cleaned.pop();
    }
    let joined = cleaned.join(" ");
    joined
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '•' | '*' | '●' | '-' | '–' | '—' | '|' | '·' | ',' | ':'))
        .to_lowercase()
}

fn is_separator(token: &str) -> bool {
    matches!(token, "-" | "–" | "—" | "|" | "·" | "•")
}

/// A capture's thread key when something assigned it explicitly, `None` when it is just the key
/// its own window title would derive.
///
/// The derived key embeds [`crate::thread::normalise_window_title`], which keeps a counter in the
/// middle of a title (`general - acme - 3 new items`), so comparing derived keys would undo the
/// counter tolerance of [`stable_title`]. An explicit key is different in kind: something resolved
/// this view to a specific conversation, and two views resolved to different conversations are
/// different conversations even under one title (`knowledge.rs`: "explicit identities must win").
pub fn explicit_thread(
    source: &str,
    app: Option<&str>,
    title: Option<&str>,
    thread_key: Option<&str>,
) -> Option<String> {
    let key = thread_key.filter(|k| !k.is_empty())?;
    let derived = crate::thread::thread_key(source, None, app, title);
    (derived.as_deref() != Some(key)).then(|| key.to_string())
}

/// Two capture titles name the same window.
pub fn same_window(previous: Option<&str>, current: Option<&str>) -> bool {
    match (previous, current) {
        (Some(a), Some(b)) => {
            let a = stable_title(a);
            !a.is_empty() && a == stable_title(b)
        }
        _ => false,
    }
}

/// Relative-day and weekday words. They sit in a message header next to the sender and change
/// overnight, so they cannot be part of who said it. Month names are deliberately absent: April,
/// May and June are people.
const DAY_WORDS: &[&str] = &[
    "today", "yesterday", "tomorrow", "at", "edited", "monday", "tuesday", "wednesday",
    "thursday", "friday", "saturday", "sunday", "mon", "tue", "tues", "wed", "thu", "thur",
    "thurs", "fri", "sat", "sun",
];

/// Clock tokens that change between captures of one message.
const TIME_WORDS: &[&str] = &["am", "pm", "ago", "just", "now"];

/// Is this attribution-line token part of the sender rather than of when it was sent?
pub fn attribution_token_is_stable(token: &str) -> bool {
    let word = token.trim_matches(|c: char| !c.is_alphanumeric());
    !word.is_empty()
        && !word.chars().any(char::is_numeric)
        && !TIME_WORDS.iter().any(|w| word.eq_ignore_ascii_case(w))
        && !DAY_WORDS.iter().any(|w| word.eq_ignore_ascii_case(w))
}

/// Strip leading reply-quote markers: `> > text` is `text`. A reply that quotes a promise is a
/// reference to that promise, not a second one.
pub fn strip_quote_markers(s: &str) -> &str {
    s.trim_start_matches(|c: char| c == '>' || c.is_whitespace())
}

/// Is the text at byte `start` of `source` on a reply-quoted line?
pub fn on_quoted_line(source: &str, start: usize) -> bool {
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..start].trim_start().starts_with('>')
}

/// Shortest overlap that identifies a truncated preview with its full sentence. Below this, the
/// shared text is a stock opening ("I'll send the") rather than a message.
pub const MIN_TRUNCATION_OVERLAP_CHARS: usize = 20;

fn normalize_text(s: &str) -> String {
    strip_quote_markers(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The text before a trailing ellipsis, when there is one.
fn truncated_core(s: &str) -> Option<String> {
    let t = s.trim_end();
    let core = t.strip_suffix('…').or_else(|| t.strip_suffix("..."))?;
    Some(normalize_text(core))
}

/// Does a suffix of `truncated`, starting on a word boundary, begin `full`?
fn tail_is_head(truncated: &str, full: &str) -> bool {
    let starts = std::iter::once(0).chain(
        truncated
            .char_indices()
            .filter(|(_, c)| *c == ' ')
            .map(|(i, _)| i + 1),
    );
    for start in starts {
        let Some(tail) = truncated.get(start..) else { continue };
        if tail.chars().count() < MIN_TRUNCATION_OVERLAP_CHARS {
            break;
        }
        if full.starts_with(tail) {
            return true;
        }
    }
    false
}

/// One quote is a truncated preview of the other: `… Hi Dana, I'll send the revised launch deck by
/// Fr…` previews `I'll send the revised launch deck by Friday`.
///
/// Exactly one side must be truncated. Two full quotes are compared by [`crate::state::quote_identity`];
/// two truncated ones cannot be shown to be the same sentence.
pub fn truncation_overlap(a: &str, b: &str) -> bool {
    match (truncated_core(a), truncated_core(b)) {
        (Some(core), None) => tail_is_head(&core, &normalize_text(b)),
        (None, Some(core)) => tail_is_head(&core, &normalize_text(a)),
        _ => false,
    }
}

/// Is this quote a truncated preview?
pub fn is_truncated(quote: &str) -> bool {
    truncated_core(quote).is_some()
}

/// May `candidate` join a row through a truncated preview, given the quotes the row already holds?
///
/// A preview is only a prefix: `I'll send the revised launch deck to…` previews `… to Dana` and
/// `… to legal for sign-off` alike. Once the row holds one full sentence, the preview has been
/// resolved to it; a different full sentence arriving through the same preview would fuse two
/// messages into one row. A preview joining a row is unaffected, and so is a full sentence the row
/// already holds.
pub fn preview_link_allowed<'a>(candidate: &str, held: impl IntoIterator<Item = &'a str>) -> bool {
    if is_truncated(candidate) {
        return true;
    }
    held.into_iter()
        .all(|q| is_truncated(q) || crate::state::same_quote_identity(q, candidate))
}

/// [`preview_link_allowed`] against the quotes retained on a state row's evidence. Read only once a
/// preview has already matched, which is rare.
pub(crate) fn preview_link_allowed_for_row(
    conn: &Connection,
    table: &str,
    id: i64,
    candidate: &str,
) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT quote FROM state_provenance WHERE state_table = ?1 AND state_id = ?2 AND quote IS NOT NULL",
    )?;
    let held = stmt
        .query_map(params![table, id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(preview_link_allowed(candidate, held.iter().map(String::as_str)))
}

/// Shortest word worth using as a [`quote_anchor`]; shorter ones ("the", "to") are everywhere.
const MIN_ANCHOR_CHARS: usize = 4;

/// A word every stored quote that can match `quote` exactly must contain, for a cheap SQL
/// prefilter.
///
/// Identity is decided in Rust, so the lookup query may only narrow candidates by conditions that
/// are *necessary*, never sufficient. This one is: [`crate::state::quote_identity`] ignores only
/// layout whitespace, reply markers, terminal punctuation and trailing mentions, so a stored quote
/// with the same identity contains every word of this one's identity. The anchor is therefore taken
/// from the identity, never from a stripped mention (`@alexandria` need not be in the stored
/// quote). The longest word is the most selective of them — the opening "I'll" is in nearly every
/// promise and would filter nothing.
///
/// A stored quote that is a *truncated preview* need not contain it (the word may sit past the
/// cut), so the caller must also admit stored quotes ending in an ellipsis; those are rare.
///
/// ASCII-lowercased on both sides because SQLite's `lower()` folds ASCII only; Unicode folding
/// here would make the prefilter reject a row the identity rule would accept.
///
/// `None` — no prefilter — when this quote is itself truncated (its overlap may start anywhere) or
/// has no word long enough to be selective.
pub fn quote_anchor(quote: &str) -> Option<String> {
    if is_truncated(quote) {
        return None;
    }
    let identity = crate::state::quote_identity(quote);
    let mut best: Option<&str> = None;
    for word in identity.split_whitespace() {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric());
        if core.chars().count() >= MIN_ANCHOR_CHARS
            && best.is_none_or(|b| core.chars().count() > b.chars().count())
        {
            best = Some(core);
        }
    }
    best.map(str::to_ascii_lowercase)
}

/// SQL admitting a stored quote that is a truncated preview — the case [`quote_anchor`] cannot
/// vouch for. `p` is the `state_provenance` alias.
pub(crate) const TRUNCATED_QUOTE_SQL: &str =
    "(rtrim(p.quote) LIKE '%…' OR rtrim(p.quote) LIKE '%...')";

/// Is `source` screen capture (which has no message identity of its own)?
pub fn is_screen_source(source: &str) -> bool {
    matches!(source, "capture" | "screen_ocr" | "screen_text")
}

/// Reply/forward prefixes a connector subject carries and a window title usually does not.
fn strip_reply_prefixes(subject: &str) -> &str {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(prefix) = ["re:", "fw:", "fwd:"].into_iter().find(|p| lower.starts_with(p)) else {
            return s;
        };
        s = s[prefix.len()..].trim_start();
    }
}

/// A title segment with bracketed annotations (`(Channel)`, `[External]`) removed.
fn bare_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut depth = 0usize;
    for c in segment.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Shortest subject that may identify a conversation on its own.
const MIN_SUBJECT_CHARS: usize = 3;

/// How a connector's own app looks on screen: the bundle ids of its desktop app, and the name its
/// web app puts after the subject in a browser tab title (`Launch deck - sam@acme.example - Gmail`).
///
/// A connector missing here is never linked across surfaces: nothing on screen can be shown to be
/// that service rather than any other page with a matching title.
fn connector_surface(source: &str) -> Option<(&'static [&'static str], &'static str)> {
    match source {
        "gmail" => Some((&[], "gmail")),
        "slack" => Some((&["com.tinyspeck.slackmacgap"], "slack")),
        _ => None,
    }
}

/// A screen capture shows the connector item's conversation. `screen` and `connector` are
/// `(source, app_bundle_id, window_title, thread_key)`; a connector item's title is its subject.
///
/// All of these must hold:
///
/// - the window is the connector's own app — its desktop app, or a title naming the service after
///   the subject. A subject is often a generic word; "Invoice - Globex Billing - Google Chrome" is
///   a billing page, not the Gmail thread "Invoice" from Acme;
/// - the connector's thread key is its own native conversation id, not one derived from the title,
///   and the screen was not resolved to a different conversation;
/// - one segment of the window title *is* the subject. Exact segment equality, not containment:
///   [`crate::thread::link_on_screen_to_thread`] accepts containment, which is right for guessing
///   which thread a question refers to — a wrong guess there is cheap — and wrong for identity: a
///   tab titled "NDA for Marcus" contains the subject "NDA" of a different thread, and the user may
///   well have sent both people the same promise.
///
/// Two threads in one service can still share a subject, so the caller also requires
/// [`screen_shows_message`], the identical sentence and an unresolved row. Under those guards this
/// is the rule connector threads already use (`RECAPTURE_CONTEXT_SQL`: same thread, same quote),
/// extended to the screen view of that thread.
pub fn screen_shows_thread(screen: Place<'_>, connector: Place<'_>) -> bool {
    let (screen_source, screen_app, screen_title, screen_thread) = screen;
    let (source, app, subject, thread) = connector;
    if !is_screen_source(screen_source) || is_screen_source(source) {
        return false;
    }
    let Some((bundles, service_name)) = connector_surface(source) else {
        return false;
    };
    let (Some(screen_title), Some(subject)) = (screen_title, subject) else {
        return false;
    };
    // The connector's own conversation id, namespaced by its service.
    let Some(key) = explicit_thread(source, app, Some(subject), thread) else {
        return false;
    };
    if !key.starts_with(&format!("{source}:")) {
        return false;
    }
    if explicit_thread(screen_source, screen_app, Some(screen_title), screen_thread)
        .is_some_and(|resolved| resolved != key)
    {
        return false;
    }
    let subject = bare_segment(strip_reply_prefixes(subject));
    if subject.chars().count() < MIN_SUBJECT_CHARS {
        return false;
    }
    let native_app = screen_app.is_some_and(|a| bundles.contains(&a));
    let screen = stable_title(screen_title);
    let segments: Vec<String> = title_segments(&screen).into_iter().map(bare_segment).collect();
    segments.iter().enumerate().any(|(i, segment)| {
        *segment == subject && (native_app || segments[i + 1..].iter().any(|s| s == service_name))
    })
}

/// The connector item's whole body is on screen, layout whitespace aside.
///
/// The subject names a conversation only loosely — two "Invoice" threads from two senders share it
/// — while the body carries the greeting, the signature and the rest of the message. A thread whose
/// synced body is not visible as one span (collapsed history, a truncated sync) is not linked:
/// a duplicate row is visible and fixable, a wrong merge silently loses a memory.
pub fn screen_shows_message(screen_content: &str, connector_content: &str) -> bool {
    let body = connector_content.trim();
    !body.is_empty() && crate::state::source_contains_quote(screen_content, body)
}

/// A window title split on the separators apps put between subject, account and app name.
fn title_segments(title: &str) -> Vec<&str> {
    let mut parts = vec![title];
    for sep in [" - ", " – ", " — ", " | "] {
        parts = parts.into_iter().flat_map(|p| p.split(sep)).collect();
    }
    parts
}

/// Kinds whose identity is their name.
///
/// Projects, organisations, tools and topics are proper nouns or subjects: two different
/// "Kestrel" projects in one person's working life are rare, and their context spans describe the
/// project rather than distinguish it. The About-You kinds are excluded because their context *is*
/// the fact ("Senior engineer at Acme"), and keying them by name would merge distinct facts.
fn name_is_identity(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Person | Kind::Project | Kind::Organization | Kind::Tool | Kind::Topic
    )
}

/// Normalised name, the identity of a [`name_is_identity`] entity: Unicode NFKC, diacritics folded
/// (Latin accents only: the kana voicing marks are not in the combining-diacritics block), lowercased,
/// with runs of whitespace collapsed. "Mikel", "mikel" and " MIKEL " are one name; so are "Élodie"
/// and "Elodie".
pub fn entity_name_key(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let composed: String = name.nfkc().collect();
    let folded: String = composed.nfd().filter(|c| !('\u{300}'..='\u{36f}').contains(c)).collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// SQL admitting every row whose `column` may have the [`entity_name_key`] bound at `param`. A
/// PREFILTER: callers compare [`entity_name_key`] of the stored name in Rust.
///
/// SQLite's `lower()` folds ASCII only and its `trim()` strips spaces only, so comparing
/// `lower(trim(column))` with a Rust-lowercased key split "Élodie Martin" from "élodie martin".
/// That comparison *is* the identity for a name of printable ASCII with single inner spaces, so it
/// keeps those rows cheap; any other name (`Élodie Martin`, `山田 太郎`, a double space or a tab) is
/// admitted unconditionally.
pub(crate) fn entity_name_prefilter_sql(column: &str, param: &str) -> String {
    format!(
        "(lower(trim({column})) = {param} OR {column} GLOB '*[^ -~]*' OR instr({column}, '  ') > 0)"
    )
}

/// `(source, app_bundle_id, window_title, thread_key)` of an event, borrowed.
pub type Place<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

/// Two screen captures show the same conversation: same window once counters are removed, and no
/// explicit thread identity that tells them apart.
pub fn same_screen_conversation(previous: Place<'_>, current: Place<'_>) -> bool {
    let (ps, pa, pt, pk) = previous;
    let (cs, ca, ct, ck) = current;
    is_screen_source(ps)
        && is_screen_source(cs)
        && pa.is_some_and(|a| !a.is_empty())
        && pa == ca
        && same_window(pt, ct)
        && explicit_thread(ps, pa, pt, pk) == explicit_thread(cs, ca, ct, ck)
}

/// Has the user said two of these rows are different entities (FR-ST-10)?
pub(crate) fn any_pair_blocked(conn: &Connection, table: StateTable, ids: &[i64]) -> Result<bool, rusqlite::Error> {
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            if crate::state_split::is_merge_blocked(conn, table, *a, *b)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The existing entity a name resolved to. `key` is its `extraction_key`; a row the user created
/// by hand may have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub id: i64,
    pub key: Option<String>,
}

fn state_table_of(kind: Kind) -> Option<StateTable> {
    match kind {
        Kind::Person => Some(StateTable::People),
        Kind::Project => Some(StateTable::Projects),
        _ => None,
    }
}

/// The people / projects row `id`, when the review model maps the mention `name` to it
/// (`existing_id`) and the two are lexically related ([`crate::entity_names::related`]). A wrong id
/// ("Dana Reyes" → the row "Toru") is ignored, so the name resolves on its own instead of renaming
/// someone else's row.
pub fn existing_row(conn: &Connection, kind: Kind, id: i64, name: &str) -> Result<Option<Resolved>, rusqlite::Error> {
    let Some(table) = state_table_of(kind) else { return Ok(None) };
    let name_col = if table == StateTable::People { "display_name" } else { "name" };
    let row: Option<(Resolved, String, Option<String>)> = conn
        .query_row(
            &format!("SELECT id, extraction_key, {name_col}, aliases FROM {} WHERE id = ?1", table.as_str()),
            [id],
            |r| Ok((Resolved { id: r.get(0)?, key: r.get(1)? }, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((resolved, stored, raw)) = row else { return Ok(None) };
    let known: std::collections::HashSet<String> = std::iter::once(stored)
        .chain(crate::entity_names::parse_aliases(raw.as_deref()))
        .map(|n| entity_name_key(&n))
        .filter(|k| !k.is_empty())
        .collect();
    let mention = std::iter::once(entity_name_key(name)).filter(|k| !k.is_empty()).collect();
    Ok(crate::entity_names::related(&known, &mention).then_some(resolved))
}

/// The existing entity this name (or one of the model's `aliases` for it) unambiguously names.
///
/// Identity is the normalised name ([`entity_name_key`]), scoped by kind, and a person or project
/// is also known by every alias it has accumulated: "mikel" and "Mikel" are one person, and so are
/// "Toru", "Toru Tano" and the handle "Torutesu" once one row lists the others. A one-word name
/// matches like any other (requiring a shared conversation would let the same first name pile up
/// as separate rows).
///
/// - The name itself decides first, against every row's name and aliases; the oldest row wins.
/// - Only when the name matches nothing do the model's aliases get a vote, and only against row
///   NAMES, and only when they agree on exactly one row.
/// - `None` (a new entity) when nothing matches, or when the user has split two of the matching
///   rows apart (`state_merge_blocks`): a name the user already called ambiguous is never resolved.
pub fn resolve_entity(
    conn: &Connection,
    kind: Kind,
    name: &str,
    aliases: &[String],
) -> Result<Option<Resolved>, rusqlite::Error> {
    if !name_is_identity(kind) {
        return Ok(None);
    }
    let name_key = entity_name_key(name);
    if name_key.is_empty() {
        return Ok(None);
    }
    let Some(table) = state_table_of(kind) else {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, extraction_key, name FROM knowledge_entities
              WHERE {} AND kind = ?2 AND extraction_key IS NOT NULL ORDER BY id",
            entity_name_prefilter_sql("name", "?1")
        ))?;
        let rows = stmt
            .query_map(params![name_key, kind.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        // The SQL only narrows; the name identity — Unicode case and inner whitespace — is decided here.
        return Ok(rows
            .into_iter()
            .find(|(_, _, stored)| entity_name_key(stored) == name_key)
            .map(|(id, key, _)| Resolved { id, key: Some(key) }));
    };
    let name_col = if table == StateTable::People { "display_name" } else { "name" };
    let mut stmt = conn.prepare(&format!(
        "SELECT id, extraction_key, {name_col}, aliases FROM {} ORDER BY id",
        table.as_str()
    ))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let known: Vec<(Resolved, String, Vec<String>)> = rows
        .into_iter()
        .map(|(id, key, stored, raw)| {
            let keys = crate::entity_names::parse_aliases(raw.as_deref()).iter().map(|a| entity_name_key(a)).collect();
            (Resolved { id, key }, entity_name_key(&stored), keys)
        })
        .collect();
    let mut matched: Vec<&Resolved> = known
        .iter()
        .filter(|(_, stored, keys)| *stored == name_key || keys.contains(&name_key))
        .map(|(r, _, _)| r)
        .collect();
    if matched.is_empty() {
        for alias in aliases {
            let alias_key = entity_name_key(alias);
            if alias_key.chars().count() < crate::entity_names::min_part_chars(&alias_key) {
                continue;
            }
            for (r, stored, _) in &known {
                if *stored == alias_key && !matched.iter().any(|m| m.id == r.id) {
                    matched.push(r);
                }
            }
        }
        if matched.len() != 1 {
            return Ok(None);
        }
    }
    if matched.len() > 1 {
        let ids: Vec<i64> = matched.iter().map(|r| r.id).collect();
        if any_pair_blocked(conn, table, &ids)? {
            return Ok(None);
        }
    }
    Ok(matched.first().copied().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_removed_from_titles() {
        assert_eq!(stable_title("(2) launch-plan | Acme | Slack"), stable_title("(5) launch-plan | Acme | Slack"));
        assert_eq!(stable_title("(99+) Inbox - sam@acme.example - Gmail"), "inbox - sam@acme.example - gmail");
        assert_eq!(
            stable_title("Inbox – 1,204 messages, 3 unread"),
            stable_title("Inbox – 1,205 messages")
        );
        assert_eq!(stable_title("general - Acme - 3 new items - Slack"), stable_title("general - Acme - Slack"));
        assert_eq!(stable_title("• draft.md"), "draft.md");
        assert_eq!(stable_title("(3) Inbox"), stable_title("(4) Inbox"));
    }

    #[test]
    fn only_a_leading_bracketed_number_is_a_counter() {
        // A bracketed number inside a title names the document: chapters, Finder's copies.
        assert_ne!(stable_title("Chapter (3) - Draft"), stable_title("Chapter (4) - Draft"));
        assert_ne!(stable_title("Untitled (2)"), stable_title("Untitled (3)"));
        // A trailing count is indistinguishable from those, so it is kept too.
        assert_eq!(stable_title("Inbox (3) - Outlook"), "inbox (3) - outlook");
    }

    #[test]
    fn numbers_that_name_the_window_are_kept() {
        assert_ne!(stable_title("Q3 board deck"), stable_title("Q4 board deck"));
        assert_ne!(stable_title("PR #812 - acme/api"), stable_title("PR #813 - acme/api"));
        assert_ne!(stable_title("Invoice 1043"), stable_title("Invoice 1044"));
        // A number followed by an ordinary word is not a counter.
        assert_ne!(stable_title("Room 3 booking"), stable_title("Room 4 booking"));
    }

    #[test]
    fn account_segments_are_kept() {
        // The thread normaliser drops the trailing segment; identity must not.
        assert_ne!(stable_title("Inbox — Work"), stable_title("Inbox — Personal"));
    }

    #[test]
    fn same_window_needs_a_title_on_both_sides() {
        assert!(same_window(Some("(1) a | Slack"), Some("(2) a | Slack")));
        assert!(!same_window(None, Some("a")));
        assert!(!same_window(Some(""), Some("")));
        assert!(!same_window(Some("(3)"), Some("(4)")), "a title that was only a counter names nothing");
    }

    #[test]
    fn day_words_are_not_part_of_the_sender() {
        for word in ["Today", "Yesterday", "at", "Mon", "Friday", "(edited)"] {
            assert!(!attribution_token_is_stable(word), "{word}");
        }
        for word in ["Sam", "Reed,", "Dana", "May", "June", "April"] {
            assert!(attribution_token_is_stable(word), "{word}");
        }
        assert!(!attribution_token_is_stable("4:02"));
        assert!(!attribution_token_is_stable("PM"));
    }

    #[test]
    fn quote_markers_are_stripped() {
        assert_eq!(strip_quote_markers("> I'll send it"), "I'll send it");
        assert_eq!(strip_quote_markers("> > I'll send it"), "I'll send it");
        assert_eq!(strip_quote_markers("I'll send it"), "I'll send it");
    }

    #[test]
    fn quoted_lines_are_recognised() {
        let src = "Perfect\nOn Tue, Sam wrote:\n> I'll send the deck\nThanks";
        let start = src.find("I'll").expect("present");
        assert!(on_quoted_line(src, start));
        let plain = "Sam\nI'll send the deck";
        assert!(!on_quoted_line(plain, plain.find("I'll").expect("present")));
    }

    #[test]
    fn a_truncated_preview_overlaps_its_sentence() {
        let preview = "Dana Reyes   Launch deck   Hi Dana, I'll send the revised launch deck by Fr…";
        let full = "I'll send the revised launch deck by Friday";
        assert!(truncation_overlap(preview, full));
        assert!(truncation_overlap(full, preview), "symmetric");
        assert!(truncation_overlap("I'll send the revised launch deck by...", full));
    }

    #[test]
    fn a_short_or_different_overlap_is_not_identity() {
        // Only the stock opening is shared.
        assert!(!truncation_overlap("Sam: I'll send the…", "I'll send the Q3 deck by Friday"));
        // The preview diverges from the sentence before it was cut.
        assert!(!truncation_overlap(
            "Hi Dana, I'll send the revised pricing deck by Fr…",
            "I'll send the revised launch deck by Friday"
        ));
        // Neither, or both, truncated: exact identity decides, not this.
        assert!(!truncation_overlap("I'll send the deck by Friday", "I'll send the deck by Friday"));
        assert!(!truncation_overlap("I'll send the revised launch deck…", "I'll send the revised launch…"));
    }

    #[test]
    fn the_anchor_is_the_most_selective_word() {
        assert_eq!(quote_anchor("I'll send the revised launch deck by Friday").as_deref(), Some("revised"));
        assert_eq!(quote_anchor("> I'll send the revised launch deck").as_deref(), Some("revised"));
        // ASCII-folded, to agree with SQLite's lower().
        assert_eq!(quote_anchor("Kestrel MIGRATION finished overnight").as_deref(), Some("migration"));
        // Trailing punctuation is not part of the word.
        assert_eq!(quote_anchor("I'll handle the onboarding.").as_deref(), Some("onboarding"));
        // Nothing selective: no prefilter rather than a useless one.
        assert_eq!(quote_anchor("ok so yes we do it"), None);
        // A truncated quote may overlap from anywhere, so it cannot anchor.
        assert_eq!(quote_anchor("Hi Dana, I'll send the revised launch deck by Fr…"), None);
    }

    #[test]
    fn the_anchor_is_contained_in_every_quote_identity_can_match() {
        let candidate = "I'll send the revised launch deck by Friday";
        let anchor = quote_anchor(candidate).expect("anchor");
        // Same sentence, different layout and a reply marker: the anchor alone admits these.
        for stored in [
            "I'll  send the revised launch deck by Friday.",
            "> I'll send the revised launch deck by Friday",
        ] {
            assert_eq!(crate::state::quote_identity(stored), crate::state::quote_identity(candidate));
            assert!(stored.to_ascii_lowercase().contains(&anchor), "{stored:?} lacks {anchor:?}");
        }
        // A trailing mention is not identity, so it can never be the anchor: the stored quote
        // without it must still pass the prefilter.
        let mentioned = "Ship the release @alexandria";
        assert_eq!(quote_anchor(mentioned).as_deref(), Some("release"), "the mention is not identity");
        assert!("Ship the release.".to_ascii_lowercase().contains("release"));
        let ja = "アプリ全体のバグは明日修正します @kaito";
        let ja_anchor = quote_anchor(ja).expect("anchor");
        for stored in ["アプリ全体のバグは明日修正します。", "アプリ全体のバグは明日修正します"] {
            assert_eq!(crate::state::quote_identity(stored), crate::state::quote_identity(ja));
            assert!(stored.to_ascii_lowercase().contains(&ja_anchor), "{stored:?} lacks {ja_anchor:?}");
        }
        // A preview cut before the anchor word still matches by truncation; the caller admits it
        // through TRUNCATED_QUOTE_SQL instead, so this must be a truncated quote.
        let cut_early = "Hi Dana, I'll send the rev…";
        let long_enough = "Dana Reyes   Launch deck   Hi Dana, I'll send the revised launch deck by Fr…";
        assert!(truncation_overlap(long_enough, candidate));
        assert!(is_truncated(long_enough) && is_truncated(cut_early));
    }

    const CHROME: Option<&str> = Some("com.google.Chrome");
    const SLACK_APP: Option<&str> = Some("com.tinyspeck.slackmacgap");

    fn on_screen<'a>(app: Option<&'a str>, title: &'a str) -> Place<'a> {
        ("capture", app, Some(title), None)
    }

    #[test]
    fn a_screen_title_resolves_to_its_thread() {
        let gmail = ("gmail", None, Some("Launch deck"), Some("gmail:thread-launch-deck"));
        assert!(screen_shows_thread(on_screen(CHROME, "Launch deck - sam@acme.example - Gmail"), gmail));
        // The latest message's subject carries "Re:"; the tab does not.
        assert!(screen_shows_thread(
            on_screen(CHROME, "(3) Launch deck - sam@acme.example - Gmail"),
            ("gmail", None, Some("Re: Launch deck"), Some("gmail:thread-launch-deck")),
        ));
        // The Slack desktop app, and Slack in a browser tab.
        let slack = ("slack", None, Some("launch-plan"), Some("slack:C04LAUNCH"));
        assert!(screen_shows_thread(on_screen(SLACK_APP, "launch-plan (Channel) - Acme - Slack"), slack));
        assert!(screen_shows_thread(on_screen(CHROME, "(2) launch-plan | Acme | Slack"), slack));
        assert!(!screen_shows_thread(on_screen(CHROME, "Pricing review - sam@acme.example - Gmail"), gmail));
        assert!(!screen_shows_thread(("capture", CHROME, None, None), gmail));
    }

    #[test]
    fn a_subject_on_another_site_is_not_the_thread() {
        // The Gmail thread "Invoice" with Acme, and a Globex billing page titled "Invoice".
        let acme = ("gmail", None, Some("Invoice"), Some("gmail:thread-acme-invoice"));
        assert!(!screen_shows_thread(on_screen(CHROME, "Invoice - Globex Billing - Google Chrome"), acme));
        assert!(!screen_shows_thread(on_screen(CHROME, "Invoice"), acme));
        // The service name must follow the subject, as the web app writes it.
        assert!(!screen_shows_thread(on_screen(CHROME, "Gmail - Invoice"), acme));
        // A connector with no recognisable surface is never linked.
        let notion = ("notion", None, Some("Launch plan"), Some("notion:page-1"));
        assert!(!screen_shows_thread(on_screen(CHROME, "Launch plan - Notion"), notion));
    }

    #[test]
    fn the_connector_key_is_compared_not_just_present() {
        let title = "Launch deck - sam@acme.example - Gmail";
        // A key derived from the subject is not a conversation id.
        let derived = crate::thread::thread_key("gmail", None, None, Some("Launch deck"));
        assert!(!screen_shows_thread(on_screen(CHROME, title), ("gmail", None, Some("Launch deck"), derived.as_deref())));
        // Another service's key.
        assert!(!screen_shows_thread(on_screen(CHROME, title), ("gmail", None, Some("Launch deck"), Some("slack:C04"))));
        // The screen was resolved to a different conversation.
        let connector = ("gmail", None, Some("Launch deck"), Some("gmail:thread-a"));
        assert!(!screen_shows_thread(("capture", CHROME, Some(title), Some("gmail:thread-b")), connector));
        assert!(screen_shows_thread(("capture", CHROME, Some(title), Some("gmail:thread-a")), connector));
        // Two connector items, or two screens, are not a cross-surface pair.
        assert!(!screen_shows_thread(("gmail", None, Some(title), Some("gmail:thread-a")), connector));
        assert!(!screen_shows_thread(on_screen(CHROME, title), on_screen(CHROME, "Launch deck")));
    }

    #[test]
    fn the_synced_body_must_be_on_screen() {
        let screen = "Launch deck\nSam Reed\nHi Dana,\nI'll send the deck by Friday.\nSam\nReply  Forward";
        assert!(screen_shows_message(screen, "Hi Dana,\nI'll send the deck by Friday.\nSam"));
        assert!(screen_shows_message(screen, "Hi Dana,  I'll send the deck\nby Friday.\nSam"), "layout whitespace");
        // The same sentence to someone else: same subject, different message.
        assert!(!screen_shows_message(screen, "Hi Marcus,\nI'll send the deck by Friday.\nSam"));
        assert!(!screen_shows_message(screen, "  "));
    }

    #[test]
    fn containment_is_not_identity() {
        // The resolver for referring questions would link these; identity must not.
        assert!(!screen_shows_thread(
            on_screen(CHROME, "NDA for Marcus - sam@acme.example - Gmail"),
            ("gmail", None, Some("NDA"), Some("gmail:thread-dana-nda")),
        ));
        // Too short to identify a conversation by itself.
        assert!(!screen_shows_thread(on_screen(CHROME, "Hi - Gmail"), ("gmail", None, Some("Hi"), Some("gmail:t"))));
    }

    #[test]
    fn one_preview_resolves_to_one_full_sentence() {
        let preview = "I'll send the revised launch deck to…";
        let dana = "I'll send the revised launch deck to Dana";
        let legal = "I'll send the revised launch deck to legal for sign-off";
        assert!(truncation_overlap(preview, dana) && truncation_overlap(preview, legal));
        // A row holding only the preview takes the first full sentence.
        assert!(preview_link_allowed(dana, [preview]));
        // Once it holds that sentence, a different one cannot reach it through the preview.
        assert!(!preview_link_allowed(legal, [preview, dana]));
        assert!(preview_link_allowed("> I'll send the revised launch deck to Dana.", [preview, dana]));
        // Another preview may still join the row.
        assert!(preview_link_allowed("…the revised launch deck to…", [preview, dana]));
    }

    #[test]
    fn a_derived_thread_key_is_not_an_explicit_identity() {
        let title = Some("general (Channel) - Acme - Slack");
        let app = Some("com.tinyspeck.slackmacgap");
        let derived = crate::thread::thread_key("capture", None, app, title);
        assert_eq!(explicit_thread("capture", app, title, derived.as_deref()), None);
        assert_eq!(
            explicit_thread("capture", app, title, Some("slack:C04LAUNCH")),
            Some("slack:C04LAUNCH".to_string())
        );
        assert_eq!(explicit_thread("capture", app, title, None), None);
    }

    #[test]
    fn same_screen_conversation_tolerates_counters_but_not_explicit_threads() {
        let app = Some("com.tinyspeck.slackmacgap");
        let a = Some("general - Acme - 3 new items - Slack");
        let b = Some("general - Acme - Slack");
        let ka = crate::thread::thread_key("capture", None, app, a);
        let kb = crate::thread::thread_key("capture", None, app, b);
        // The derived keys disagree (the counter sits mid-title); the conversation does not.
        assert_ne!(ka, kb);
        assert!(same_screen_conversation(("capture", app, a, ka.as_deref()), ("capture", app, b, kb.as_deref())));
        // Something resolved one view to another conversation: that wins over the title.
        assert!(!same_screen_conversation(("capture", app, b, kb.as_deref()), ("capture", app, b, Some("other-conversation"))));
        // Different apps never share a window.
        assert!(!same_screen_conversation(("capture", app, b, None), ("capture", Some("com.apple.mail"), b, None)));
        // A connector item is not a screen conversation.
        assert!(!same_screen_conversation(("capture", app, b, None), ("slack", app, b, None)));
    }

    #[test]
    fn the_name_prefilter_never_drops_a_row_the_identity_accepts() -> Result<(), rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE t (name TEXT)")?;
        let stored = ["Dana Reyes", " DANA REYES ", "Dana  Reyes", "Dana\tReyes", "Élodie Martin", "山田 太郎", "Sam Reed"];
        for name in stored {
            conn.execute("INSERT INTO t (name) VALUES (?1)", [name])?;
        }
        let sql = format!("SELECT name FROM t WHERE {}", entity_name_prefilter_sql("name", "?1"));
        for wanted in ["dana reyes", "ÉLODIE MARTIN", "山田 太郎", "Sam  Reed"] {
            let key = entity_name_key(wanted);
            let admitted: Vec<String> = conn
                .prepare(&sql)?
                .query_map([&key], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for name in stored.into_iter().filter(|n| entity_name_key(n) == key) {
                assert!(admitted.iter().any(|a| a == name), "{name:?} dropped for {wanted:?}");
            }
        }
        // A printable-ASCII name that differs is still filtered out in SQL.
        let admitted: Vec<String> = conn
            .prepare(&sql)?
            .query_map([entity_name_key("Dana Reyes")], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        assert!(!admitted.iter().any(|a| a == "Sam Reed"));
        Ok(())
    }

    #[test]
    fn about_you_kinds_keep_their_context_identity() {
        for kind in [Kind::SelfFact, Kind::Employment, Kind::Education, Kind::Activity] {
            assert!(!name_is_identity(kind), "{}", kind.as_str());
        }
    }
}
