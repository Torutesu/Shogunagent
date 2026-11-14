//! Screen ↔ state matching for the action row (FR-CF-01/03): does this person, project,
//! commitment or open loop appear on the focused screen?
//!
//! The direction matters. Asking "does any window-title word occur in the state text" would match
//! `gmail` / `inbox` / `chrome` against anything and never match Japanese (no spaces:
//! `田中さんから` is one "word"). So the state's own names and keywords are looked for **in** the
//! screen text, CJK-aware:
//!
//! - Latin needles match on word boundaries (`Ann` is not in `annual`), CJK needles as
//!   substrings (`田中` is in `田中さんから` and `田中様`), so honorific suffixes need no list on
//!   the screen side; names *stored* with one (`高橋様`) are stripped before matching.
//! - Generic app and work words (`gmail`, `inbox`, `reply`, `確認`, `対応` …) never count, and
//!   the browser / app name segment of a window title is dropped before anything is matched.
//!
//! Pure, allocation-light string work over bounded inputs (title + a ≤1.2k excerpt + a ≤2k
//! selection), so it fits the 150ms row budget with room to spare. No content leaves this module
//! and nothing here is logged.

use crate::surface::{is_browser, surface_of, Surface};

/// Han ideographs (CJK unified + extension A + compatibility).
fn is_han(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}' | '々')
}

fn is_hiragana(c: char) -> bool {
    matches!(c, '\u{3041}'..='\u{309F}')
}

fn is_katakana(c: char) -> bool {
    matches!(c, '\u{30A1}'..='\u{30FA}' | '\u{30FC}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}')
}

/// Hangul syllables (no word-boundary rule applies to them here either).
fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7AF}')
}

/// A character of a script written without spaces between words.
pub fn is_cjk(c: char) -> bool {
    is_han(c) || is_hiragana(c) || is_katakana(c) || is_hangul(c)
}

/// A character of a space-delimited word (Latin, Cyrillic, digits, …).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk(c)
}

/// Lowercase, fold full-width ASCII to half-width, treat the ideographic space and name dots
/// (`・`, `·`) as spaces, and collapse whitespace.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        let c = match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' | '・' | '·' | '･' => ' ',
            _ => c,
        };
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
            continue;
        }
        space = false;
        for l in c.to_lowercase() {
            out.push(l);
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Normalized screen text that needles are looked up in.
#[derive(Debug, Clone, Default)]
pub struct Haystack {
    text: String,
}

impl Haystack {
    pub fn new<'a>(parts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut text = String::new();
        for p in parts {
            let n = normalize(p);
            if n.is_empty() {
                continue;
            }
            if !text.is_empty() {
                // A hard separator so a needle cannot straddle two parts.
                text.push_str(" \u{1F} ");
            }
            text.push_str(&n);
        }
        Self { text }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether an already-normalized needle occurs. Latin edges must sit on word boundaries; a
    /// CJK edge needs none.
    pub fn contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let (Some(first), Some(last)) = (needle.chars().next(), needle.chars().last()) else {
            return false;
        };
        let mut from = 0;
        while let Some(at) = self.text[from..].find(needle) {
            let start = from + at;
            let end = start + needle.len();
            let before_ok =
                !is_word_char(first) || self.text[..start].chars().last().is_none_or(|c| !is_word_char(c));
            let after_ok =
                !is_word_char(last) || self.text[end..].chars().next().is_none_or(|c| !is_word_char(c));
            if before_ok && after_ok {
                return true;
            }
            // Advance one character past this occurrence's start.
            from = start + self.text[start..].chars().next().map_or(1, char::len_utf8);
        }
        false
    }

    /// [`Self::contains`] for a person-name needle. A short CJK name part (a family name such as
    /// `佐藤`) directly followed by another Han / katakana character is part of a longer word
    /// (`佐藤製薬`, a company) unless what follows is an honorific or title (`佐藤様`,
    /// `佐藤部長`). Kana particles and spaces after it (`佐藤さん`, `佐藤 花子`) are fine.
    pub fn contains_name(&self, needle: &str) -> bool {
        let short_cjk = needle.chars().all(is_cjk) && needle.chars().count() <= 3;
        if !short_cjk {
            return self.contains(needle);
        }
        let mut from = 0;
        while let Some(at) = self.text[from..].find(needle) {
            let start = from + at;
            let rest = &self.text[start + needle.len()..];
            let ok = match rest.chars().next() {
                Some(c) if is_han(c) || is_katakana(c) => HONORIFICS.iter().any(|h| rest.starts_with(h)),
                _ => true,
            };
            if ok {
                return true;
            }
            from = start + self.text[start..].chars().next().map_or(1, char::len_utf8);
        }
        false
    }
}

/// Work and filler words that say nothing about *which* thing is on screen.
const STOPWORDS: &[&str] = &[
    "about", "after", "again", "agenda", "also", "back", "been", "before", "being", "below", "between", "both",
    "call", "calls", "cannot", "chat", "check", "clean", "confirm", "could", "decide", "does", "done", "down",
    "draft", "during", "each", "email", "emails", "every", "file", "files", "finish", "follow", "follow-up",
    "from", "general", "have", "here", "into", "issue", "issues", "just", "last", "like", "list", "make",
    "mail", "meeting", "meetings", "message", "messages", "more", "most", "need", "needs", "next", "note",
    "notes", "only", "open", "other", "over", "owes", "page", "plan", "please", "post", "project",
    "projects", "pull", "release", "reply", "replies", "request", "review", "said", "same", "schedule",
    "send", "sent", "share", "should", "some", "submit", "such", "sync", "task", "tasks", "team", "than",
    "thank", "thanks", "that", "their", "them", "then", "there", "these", "they", "thing", "this",
    "those", "thread", "through", "today", "tomorrow", "update", "very", "waiting", "want", "week",
    "weekly", "what", "when", "where", "which", "while", "will", "with", "work", "would", "write", "your",
    "yours", "inbox", "new", "tab", "untitled", "monday", "tuesday", "wednesday", "thursday", "friday",
    "saturday", "sunday", "january", "february", "march", "april", "june", "july", "august",
    "september", "october", "november", "december",
];

/// App, product and site words: they appear in titles and chrome, never as a subject.
const APP_WORDS: &[&str] = &[
    "gmail", "google", "chrome", "safari", "firefox", "edge", "arc", "brave", "slack", "notion", "github",
    "gitlab", "linear", "jira", "confluence", "xcode", "visual", "studio", "code", "vscode", "cursor",
    "outlook", "calendar", "imessage", "finder", "terminal", "iterm", "zoom", "teams", "microsoft",
    "apple", "docs", "sheets", "slides", "drive", "figma", "discord", "whatsapp", "telegram", "line",
    "messenger", "obsidian", "pages", "word", "excel", "powerpoint", "keynote", "numbers", "dm",
];

/// Generic Japanese work words (whole CJK runs only).
const CJK_STOPWORDS: &[&str] = &[
    "確認", "対応", "連絡", "返信", "返事", "共有", "送付", "送信", "受信", "資料", "会議", "予定", "依頼", "件名",
    "提出", "作成", "今週", "来週", "先週", "今日", "明日", "本日", "昨日", "メール", "メッセージ", "チャット",
    "スレッド", "下書", "承認", "検討", "案内", "参加", "質問", "方法", "状況", "内容", "対応状況", "件", "様", "定例",
    "打合", "打合せ", "ミーティング", "カレンダー", "プロジェクト", "タスク", "レビュー", "フィードバック",
];

/// First/last-name tokens that are also everyday words (`Mark as read`, `May`, `Will`).
const NAME_STOPWORDS: &[&str] = &[
    "mark", "will", "may", "june", "april", "august", "bill", "grant", "chase", "frank", "rose", "summer",
    "art", "joy", "hope", "faith", "grace", "rich", "jack", "ray", "dawn", "eve", "lane", "page", "reed",
    "ward", "west", "young", "king", "white", "black", "green", "gray", "grey", "long", "hall", "wood",
    "stone", "bell", "cook", "park", "case", "hunter", "mason", "carter", "cash", "sky", "rain", "max",
    "sun", "star", "love", "dean", "glen", "miles", "rob", "sue", "pat", "don", "guy", "new", "read",
];

/// Honorifics and titles a stored Japanese name may carry.
const HONORIFICS: &[&str] = &[
    "さん", "さま", "様", "くん", "君", "ちゃん", "氏", "殿", "先生", "部長", "課長", "社長", "係長", "主任",
];

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token) || APP_WORDS.contains(&token)
}

/// A name with any trailing honorific removed (`高橋様` → `高橋`).
pub fn strip_honorific(name: &str) -> &str {
    let mut n = name.trim();
    loop {
        let before = n;
        for h in HONORIFICS {
            if let Some(rest) = n.strip_suffix(h) {
                if rest.trim().chars().count() >= 1 {
                    n = rest.trim_end();
                }
            }
        }
        if n == before {
            return n;
        }
    }
}

fn cjk_needle_ok(s: &str) -> bool {
    let n = s.chars().count();
    let han = s.chars().any(is_han);
    if han {
        n >= 2
    } else {
        n >= 3
    }
}

/// The strings that identify a person on screen: the full name (with and without the space for a
/// CJK name), its distinctive parts, aliases and email addresses. All normalized.
pub fn person_needles(name: &str, aliases: &[String], emails: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |s: String, out: &mut Vec<String>| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    for raw in std::iter::once(name).chain(aliases.iter().map(String::as_str)) {
        let full = normalize(strip_honorific(raw));
        if full.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = full.split(' ').filter(|t| !t.is_empty()).collect();
        let cjk = full.chars().any(is_cjk);
        if cjk {
            let joined: String = tokens.concat();
            if cjk_needle_ok(&joined) {
                push(joined.clone(), &mut out);
            }
            if tokens.len() > 1 {
                push(full.clone(), &mut out);
                for t in &tokens {
                    if t.chars().all(is_cjk) && cjk_needle_ok(t) {
                        push((*t).to_string(), &mut out);
                    }
                }
            } else if joined.chars().all(is_han) && (3..=4).contains(&joined.chars().count()) {
                // `田中太郎` written without a space: the family name is the usual reference.
                push(joined.chars().take(2).collect(), &mut out);
            }
        } else {
            if full.chars().count() >= 3 && !NAME_STOPWORDS.contains(&full.as_str()) && !is_stopword(&full) {
                push(full.clone(), &mut out);
            }
            if tokens.len() > 1 {
                for t in &tokens {
                    if t.chars().count() >= 3 && !NAME_STOPWORDS.contains(t) && !is_stopword(t) {
                        push((*t).to_string(), &mut out);
                    }
                }
            }
        }
    }
    for e in emails {
        let e = normalize(e);
        if e.contains('@') && e.len() >= 6 {
            push(e, &mut out);
        }
    }
    out
}

/// The strings that identify a project on screen: its full name, and the first word of a
/// multi-word Latin name when that word is distinctive (`Atlas migration` → `atlas`).
pub fn project_needles(name: &str) -> Vec<String> {
    let full = normalize(name);
    let mut out = Vec::new();
    if full.is_empty() {
        return out;
    }
    let tokens: Vec<&str> = full.split(' ').filter(|t| !t.is_empty()).collect();
    if full.chars().any(is_cjk) {
        if cjk_needle_ok(&full) && !CJK_STOPWORDS.contains(&full.as_str()) {
            out.push(full.clone());
        }
        return out;
    }
    let generic = tokens.iter().all(|t| is_stopword(t) || t.chars().count() < 3);
    if !generic && full.chars().count() >= 3 {
        out.push(full.clone());
    }
    if tokens.len() > 1 {
        if let Some(head) = tokens.first() {
            if head.chars().count() >= 5 && !is_stopword(head) {
                out.push((*head).to_string());
            }
        }
    }
    out
}

/// One keyword of a commitment / open-loop description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyword {
    pub text: String,
    /// One hit is enough: an identifier (`ENG-231`, `#412`), a proper noun inside the sentence
    /// (`Orion`), or a longer CJK term (`見積書`).
    pub distinctive: bool,
}

/// The keywords of a description. Latin words of four letters or more that are not generic,
/// identifiers with a digit, and CJK runs of Han / katakana (hiragana is where Japanese breaks
/// words, so it splits runs).
pub fn keywords(text: &str) -> Vec<Keyword> {
    let mut out: Vec<Keyword> = Vec::new();
    let push = |k: Keyword, out: &mut Vec<Keyword>| {
        if let Some(existing) = out.iter_mut().find(|e| e.text == k.text) {
            existing.distinctive |= k.distinctive;
        } else {
            out.push(k);
        }
    };
    // Latin / digit tokens, with their original casing for the proper-noun test.
    let mut first_word = true;
    for raw in text.split(|c: char| !(is_word_char(c) || c == '-' || c == '\'' || c == '’')) {
        let raw = raw.trim_matches(|c| c == '-' || c == '\'' || c == '’');
        let raw = raw.strip_suffix("'s").or_else(|| raw.strip_suffix("’s")).unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        let was_first = std::mem::replace(&mut first_word, false);
        let token = normalize(raw);
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let len = token.chars().count();
        if has_digit {
            if len >= 3 {
                push(Keyword { text: token, distinctive: true }, &mut out);
            }
            continue;
        }
        if len < 4 || is_stopword(&token) {
            continue;
        }
        let proper = !was_first && raw.chars().next().is_some_and(char::is_uppercase);
        push(Keyword { text: token, distinctive: proper }, &mut out);
    }
    // CJK runs (Han / katakana; hiragana and punctuation split them).
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<Keyword>| {
        if !run.is_empty() {
            let r = normalize(run);
            if cjk_needle_ok(&r) && !CJK_STOPWORDS.contains(&r.as_str()) {
                let distinctive = r.chars().count() >= 3;
                push(Keyword { text: r, distinctive }, out);
            }
            run.clear();
        }
    };
    for c in text.chars() {
        if is_han(c) || is_katakana(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

const TITLE_SEPARATORS: &[&str] = &[" - ", " — ", " – ", " | ", " · ", " • ", " ‹ ", " › "];

/// A title segment that names no subject: a mailbox view, an empty tab, an unread count.
fn generic_segment(seg: &str) -> bool {
    let n = normalize(seg);
    n.is_empty()
        || n.contains('@')
        || n.starts_with("inbox")
        || n.starts_with("new tab")
        || n.starts_with("untitled")
        || n.starts_with("受信トレイ")
        || n.starts_with("新しいタブ")
        || n.chars().all(|c| !c.is_alphanumeric() || c.is_ascii_digit())
        || n.split(' ').all(|t| APP_WORDS.contains(&t))
}

/// The parts of a window title that are about the content: account addresses, mailbox views and
/// the trailing app / site name (`… - Google Chrome`, `… — Visual Studio Code`, `… - Slack`)
/// are dropped.
pub fn title_segments(title: &str, bundle_id: &str, host: Option<&str>) -> Vec<String> {
    let mut segs: Vec<&str> = vec![title.trim()];
    for sep in TITLE_SEPARATORS {
        segs = segs.iter().flat_map(|s| s.split(sep)).collect();
    }
    let mut segs: Vec<&str> = segs.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect();
    // Browsers and web apps end titles with the site name.
    let web = is_browser(bundle_id) || host.is_some_and(|h| !h.trim().is_empty());
    if segs.len() > 1 && web {
        segs.pop();
    }
    segs.into_iter().filter(|s| !generic_segment(s)).map(str::to_string).collect()
}

const REPLY_PREFIXES: &[&str] = &["re:", "fwd:", "fw:", "aw:", "wg:", "返信:", "転送:"];

/// A short, content-only seed for the generic memory search: the user's selection when it is
/// short, otherwise the first content segment of the title without `Re:` / `Fwd:`. Empty when the
/// screen names nothing (the search then opens empty).
pub fn search_seed(title: &str, bundle_id: &str, host: Option<&str>, selection: Option<&str>) -> String {
    if let Some(sel) = selection.map(str::trim).filter(|s| !s.is_empty()) {
        if sel.chars().count() <= 60 && !sel.contains('\n') {
            return sel.to_string();
        }
    }
    let Some(seg) = title_segments(title, bundle_id, host).into_iter().next() else {
        return String::new();
    };
    let mut s = seg.trim();
    loop {
        let lower = s.to_lowercase();
        let Some(p) = REPLY_PREFIXES.iter().find(|p| lower.starts_with(*p)) else {
            break;
        };
        // Prefixes are ASCII or full CJK characters; slice on the original by char count.
        let skip = p.chars().count();
        let idx = s.char_indices().nth(skip).map_or(s.len(), |(i, _)| i);
        s = s[idx..].trim_start();
    }
    let s = s.trim();
    if s.chars().count() > 60 {
        s.chars().take(60).collect::<String>().trim_end().to_string()
    } else {
        s.to_string()
    }
}

/// The screen text the row matches against: the content segments of the title, then the excerpt
/// and the selection. The website host is not text and is not included.
pub fn screen_haystack(
    title: &str,
    bundle_id: &str,
    host: Option<&str>,
    excerpt: &str,
    selection: Option<&str>,
    salient: &[String],
) -> Haystack {
    let segs = title_segments(title, bundle_id, host);
    let mut parts: Vec<&str> = segs.iter().map(String::as_str).collect();
    parts.push(excerpt);
    if let Some(s) = selection {
        parts.push(s);
    }
    parts.extend(salient.iter().map(String::as_str));
    Haystack::new(parts)
}

/// Whether the surface is one where a reply to a person is the natural next step.
pub fn is_conversation_surface(bundle_id: &str, host: Option<&str>) -> bool {
    matches!(surface_of(bundle_id, host), Surface::Mail | Surface::Chat | Surface::Messaging)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hay(s: &str) -> Haystack {
        Haystack::new([s])
    }

    #[test]
    fn latin_needles_respect_word_boundaries_and_cjk_needles_do_not_need_them() {
        assert!(!hay("Annual planning").contains("ann"));
        assert!(hay("Ann Lee joined").contains("ann"));
        assert!(hay("#general").contains("general"));
        assert!(hay("田中さんから").contains("田中"));
        assert!(hay("田中様 いつも").contains("田中"));
        assert!(hay("ENG-231 flickers").contains("eng-231"));
        assert!(!hay("markets rallied").contains("market"));
    }

    #[test]
    fn a_cjk_family_name_inside_a_longer_word_is_not_that_person() {
        assert!(!hay("佐藤製薬が発表").contains_name("佐藤"));
        assert!(!hay("田中マートの特売").contains_name("田中"));
        for ok in ["佐藤様", "佐藤部長より", "佐藤さんから", "佐藤 花子", "担当: 佐藤"] {
            assert!(hay(ok).contains_name("佐藤"), "{ok}");
        }
        // Latin names are word-bounded.
        assert!(!hay("annual").contains_name("ann"));
    }

    #[test]
    fn normalization_folds_width_case_and_name_dots() {
        assert_eq!(normalize("ＡＢＣ　マイケル・ブラウン"), "abc マイケル ブラウン");
        assert_eq!(normalize("  Dana   Reyes "), "dana reyes");
    }

    #[test]
    fn person_needles_cover_cjk_family_names_honorifics_and_addresses() {
        let n = person_needles("田中 太郎", &[], &[]);
        assert!(n.contains(&"田中".to_string()) && n.contains(&"田中太郎".to_string()));
        assert_eq!(person_needles("高橋様", &[], &[]), vec!["高橋".to_string()]);
        assert!(person_needles("田中太郎", &[], &[]).contains(&"田中".to_string()));
        let m = person_needles("マイケル・ブラウン", &[], &[]);
        assert!(m.contains(&"マイケル".to_string()));
        let d = person_needles("Dana Reyes", &["dreyes".into()], &["Dana@Client.example".into()]);
        assert!(d.contains(&"dana reyes".to_string()));
        assert!(d.contains(&"dana".to_string()));
        assert!(d.contains(&"dreyes".to_string()));
        assert!(d.contains(&"dana@client.example".to_string()));
        // Everyday words are not names on their own.
        assert!(!person_needles("Mark Chen", &[], &[]).contains(&"mark".to_string()));
    }

    #[test]
    fn keywords_skip_generic_words_and_mark_identifiers_and_proper_nouns() {
        let k = keywords("Reply to Dana about the Q3 renewal terms");
        let texts: Vec<&str> = k.iter().map(|k| k.text.as_str()).collect();
        assert_eq!(texts, vec!["dana", "renewal", "terms"]);
        assert!(k[0].distinctive, "a capitalized word inside the sentence is a name");
        assert!(!k[1].distinctive);
        let id = keywords("Fix ENG-231 before the release cut");
        assert!(id.iter().any(|k| k.text == "eng-231" && k.distinctive));
        let ja = keywords("田中さんに見積書の件で返信する");
        let texts: Vec<&str> = ja.iter().map(|k| k.text.as_str()).collect();
        assert_eq!(texts, vec!["田中", "見積書"]);
        assert!(ja[1].distinctive);
    }

    #[test]
    fn titles_lose_app_names_accounts_and_mailbox_views() {
        assert_eq!(
            title_segments("Re: Q3 renewal terms - me@example.com - Gmail", "com.google.Chrome", Some("mail.google.com")),
            vec!["Re: Q3 renewal terms".to_string()]
        );
        assert!(title_segments("Inbox (12) - me@example.com - Gmail", "com.google.Chrome", None).is_empty());
        assert_eq!(
            title_segments("README.md — notes — Visual Studio Code", "com.microsoft.VSCode", None),
            vec!["README.md".to_string(), "notes".to_string()]
        );
        assert!(title_segments("New Tab - Google Chrome", "com.google.Chrome", None).is_empty());
        assert_eq!(search_seed("Re: Fwd: Q3 renewal terms - me@example.com - Gmail", "com.google.Chrome", None, None), "Q3 renewal terms");
        assert_eq!(search_seed("x", "com.apple.mail", None, Some("ENG-231")), "ENG-231");
    }
}
