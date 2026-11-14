//! Script-aware lexical terms: one vocabulary for the index writer, the query side, the scorer and
//! excerpts (docs/search-eval.md).
//!
//! Two indexes serve lexical search. `event_fts` (V1) is a trigram index: it matches any substring
//! of three or more characters, which covers English words and longer Japanese compounds but can
//! never match a two-character word — and Japanese is full of them (会議, 予算, 田中). `event_grams_fts`
//! (V81) closes that gap: it indexes the two-character units a trigram index cannot, as whole
//! tokens:
//!
//! - every bigram inside a run of kanji, with **okurigana folded out** — a short hiragana gap
//!   between two kanji that is not a particle is dropped, so 打ち合わせ, 打合せ and 打ち合せ all
//!   yield the bigram 打合, and 申し込み / 申込 both yield 申込;
//! - every bigram inside a katakana or hangul run (バグ, メモ, 회의);
//! - two-character Latin tokens that carry a letter and are not English function words
//!   (q3, pr, ci, ux, v2).
//!
//! A query is split into [`Concept`]s at script boundaries. Hiragana particles and honorifics
//! (の, は, さん …) separate concepts and are never searched for, relative-date words (昨日, 先週,
//! yesterday …) become a time window elsewhere and are dropped here, and the whole query is
//! NFKC-normalised first so full-width and half-width forms meet. A concept matches a document
//! when its surface form occurs as a substring (trigram index), or when every one of its bigrams
//! is in the document's gram set (gram index).
//!
//! **The gram function is part of the stored format.** Rows already indexed keep the grams this
//! code produced when they were written; changing [`index_grams`] changes what new rows store and
//! what queries ask for. Any change must keep old rows matchable or ship a re-index.

use std::collections::{BTreeSet, HashSet};

use unicode_normalization::{is_nfkc_quick, IsNormalized, UnicodeNormalization};

/// Most distinct grams stored for one event. A 32 KiB window has far fewer distinct kanji pairs;
/// the cap only bounds a pathological paste.
const MAX_GRAMS_PER_DOCUMENT: usize = 8_192;
const TITLE_WEIGHT: f64 = 2.0;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Kanji,
    Hiragana,
    Katakana,
    Hangul,
    Other,
}

fn script(character: char) -> Script {
    match character as u32 {
        // 々 〆 〇, ヵ ヶ (counters written inside kanji compounds: 3ヶ月).
        0x3005..=0x3007 | 0x30F5 | 0x30F6 => Script::Kanji,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x3134F => Script::Kanji,
        0x3041..=0x3096 | 0x309D | 0x309E => Script::Hiragana,
        // ・ (30FB) and ゠ (30A0) are punctuation, not letters.
        0x30A1..=0x30FA | 0x30FC..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => Script::Katakana,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7A3 => Script::Hangul,
        _ if character.is_alphanumeric() => Script::Latin,
        _ => Script::Other,
    }
}

/// True for kana, kanji and hangul — the scripts written without spaces between words.
pub(crate) fn is_cjk(character: char) -> bool {
    matches!(
        script(character),
        Script::Kanji | Script::Hiragana | Script::Katakana | Script::Hangul
    )
}

/// NFKC (full-width → ASCII, half-width katakana → full-width) and lowercase.
pub(crate) fn normalize(text: &str) -> String {
    if is_nfkc_quick(text.chars()) == IsNormalized::Yes {
        text.chars().flat_map(char::to_lowercase).collect()
    } else {
        text.nfkc().flat_map(char::to_lowercase).collect()
    }
}

/// NFKC only, case kept — for terms shown back to callers.
fn nfkc(text: &str) -> String {
    if is_nfkc_quick(text.chars()) == IsNormalized::Yes {
        text.to_owned()
    } else {
        text.nfkc().collect()
    }
}

/// Hiragana that separate words rather than inflect them. A gap between two kanji that is one of
/// these splits the compound; any other gap of one or two characters is okurigana and is folded.
fn is_particle(gap: &str) -> bool {
    matches!(
        gap,
        "の" | "は"
            | "が"
            | "を"
            | "に"
            | "へ"
            | "と"
            | "で"
            | "や"
            | "も"
            | "か"
            | "ね"
            | "よ"
            | "な"
            | "から"
            | "まで"
            | "より"
            | "では"
            | "には"
            | "とは"
            | "での"
            | "への"
            | "との"
            | "にて"
            | "など"
            | "って"
            | "さん"
            | "さま"
            | "くん"
            | "です"
            | "ます"
            | "した"
            | "する"
            | "して"
            | "され"
            | "こと"
            | "もの"
            | "ため"
    )
}

/// English two-letter function words. Every other two-character Latin token is a candidate term
/// (Q3, PR, CI, UX, v2 …), which the trigram index alone could never match.
fn is_short_stopword(token: &str) -> bool {
    matches!(
        token,
        "of" | "to"
            | "in"
            | "is"
            | "it"
            | "on"
            | "at"
            | "by"
            | "be"
            | "as"
            | "an"
            | "or"
            | "we"
            | "he"
            | "if"
            | "my"
            | "me"
            | "do"
            | "no"
            | "so"
            | "up"
            | "us"
            | "go"
            | "am"
            | "oh"
            | "hi"
            | "ok"
            | "vs"
            | "re"
            | "fw"
            | "cc"
    )
}

/// Generic Japanese nouns a question wraps around its topic (「〜の内容」「関連情報」).
fn is_japanese_filler(term: &str) -> bool {
    matches!(term, "内容" | "情報" | "関連" | "詳細" | "件名" | "教え" | "知り")
}

/// One kanji compound with okurigana folded out, or one kana / hangul run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CjkUnit {
    /// What the gram index sees: kanji only for a compound, the run itself otherwise.
    key: String,
    /// The text as written, including folded okurigana and a short trailing inflection.
    surface: String,
    script: Script,
    folded: bool,
}

/// Contiguous same-script segments of already-normalised text. `Other` characters end a segment
/// and are not returned, but they still separate (a kanji compound never spans a space).
fn segments(text: &str) -> Vec<(Script, String)> {
    let mut out: Vec<(Script, String)> = Vec::new();
    let mut current: Option<(Script, String)> = None;
    for character in text.chars() {
        let class = script(character);
        match current.as_mut() {
            Some((script, buffer)) if *script == class => buffer.push(character),
            _ => {
                if let Some(done) = current.take() {
                    out.push(done);
                }
                current = Some((class, character.to_string()));
            }
        }
    }
    out.extend(current);
    out
}

/// Fold segments into kanji compounds (okurigana removed) and kana / hangul runs.
fn cjk_units(segments: &[(Script, String)]) -> Vec<CjkUnit> {
    let mut units = Vec::new();
    let mut compound: Option<CjkUnit> = None;
    let mut index = 0;
    while index < segments.len() {
        let (class, text) = &segments[index];
        match class {
            Script::Kanji => match compound.as_mut() {
                Some(unit) => {
                    unit.key.push_str(text);
                    unit.surface.push_str(text);
                }
                None => {
                    compound = Some(CjkUnit {
                        key: text.clone(),
                        surface: text.clone(),
                        script: Script::Kanji,
                        folded: false,
                    })
                }
            },
            Script::Hiragana => {
                let gap_chars = text.chars().count();
                let next_is_kanji =
                    matches!(segments.get(index + 1), Some((Script::Kanji, _)));
                match compound.as_mut() {
                    Some(unit) if next_is_kanji && gap_chars <= 2 && !is_particle(text) => {
                        // 打[ち]合 → 打合: okurigana inside a compound.
                        unit.surface.push_str(text);
                        unit.folded = true;
                    }
                    Some(unit) => {
                        // 見積[もり], 打合[せ]: a short trailing inflection belongs to the word as
                        // written; a particle does not.
                        if gap_chars <= 2 && !is_particle(text) {
                            unit.surface.push_str(text);
                        }
                        units.extend(compound.take());
                    }
                    None => units.push(CjkUnit {
                        key: text.clone(),
                        surface: text.clone(),
                        script: Script::Hiragana,
                        folded: false,
                    }),
                }
            }
            Script::Katakana | Script::Hangul => {
                units.extend(compound.take());
                units.push(CjkUnit {
                    key: text.clone(),
                    surface: text.clone(),
                    script: *class,
                    folded: false,
                });
            }
            Script::Latin | Script::Other => units.extend(compound.take()),
        }
        index += 1;
    }
    units.extend(compound);
    units
}

fn bigrams(text: &str) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    characters
        .windows(2)
        .map(|pair| pair.iter().collect::<String>())
        .collect()
}

fn has_letter(token: &str) -> bool {
    token.chars().any(char::is_alphabetic)
}

/// Every gram the V81 index stores for `text`, into `out`.
fn collect_grams(text: &str, out: &mut BTreeSet<String>) {
    let normalized = normalize(text);
    let segments = segments(&normalized);
    for (class, token) in &segments {
        if *class == Script::Latin
            && token.chars().count() == 2
            && has_letter(token)
            && !is_short_stopword(token)
        {
            out.insert(token.clone());
        }
    }
    for unit in cjk_units(&segments) {
        if unit.script == Script::Hiragana {
            continue;
        }
        for gram in bigrams(&unit.key) {
            if out.len() >= MAX_GRAMS_PER_DOCUMENT {
                return;
            }
            out.insert(gram);
        }
    }
}

/// The space-separated gram document V81 stores for one event (deduplicated, sorted).
pub(crate) fn index_grams(title: Option<&str>, content: &str) -> String {
    let mut grams = BTreeSet::new();
    if let Some(title) = title {
        collect_grams(title, &mut grams);
    }
    collect_grams(content, &mut grams);
    grams.into_iter().collect::<Vec<_>>().join(" ")
}

fn gram_set(text: &str) -> HashSet<String> {
    let mut grams = BTreeSet::new();
    collect_grams(text, &mut grams);
    grams.into_iter().collect()
}

/// Every gram the V81 index would store for a title and body (the set [`floor_units`] are
/// looked up in).
pub(crate) fn text_grams(title: &str, body: &str) -> HashSet<String> {
    let mut set = gram_set(title);
    set.extend(gram_set(body));
    set
}

/// The units the relevance floor counts a CJK concept as: the grams the V81 index holds for it.
///
/// A Japanese or Chinese question loses its particles and spaces to tokenisation, so one concept
/// can carry a whole clause (`的合同有错误吗`, `リリース`, `招聘会议`); counted as one word it lets a
/// project name typed beside it make up half the question. Counted in the index's own units a
/// 2-character word stays one unit, `請求書` is two (請求, 求書), `服务器故障` four, and a
/// katakana word its bigrams. Empty for a Latin concept, which is one unit as it stands.
pub(crate) fn floor_units(concept: &Concept) -> Vec<String> {
    if !concept.cjk {
        return Vec::new();
    }
    if !concept.grams.is_empty() {
        return concept.grams.clone();
    }
    concept.surface.as_deref().map(bigrams).unwrap_or_default()
}

/// One thing a question asks for. Matches a document when `surface` occurs in it, or when every
/// gram in `grams` is in the document's gram set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Concept {
    /// The term as typed (NFKC, case kept). What [`super::lexical_terms`] returns.
    pub display: String,
    /// Lowercased substring of ≥ 3 characters — matchable through the trigram index.
    pub surface: Option<String>,
    /// Lowercased two-character units — matchable through the gram index. All must be present.
    pub grams: Vec<String>,
    /// A two-character Latin token: matches on token boundaries only (`pr` is not in `price`).
    pub short_word: bool,
    /// Written in a CJK script (anchors and excerpts treat these as compounds).
    pub cjk: bool,
}

/// Relative-date words: they become a time window ([`super::query_time_window`]) and would only
/// make every match fail if searched for as text.
fn is_temporal_word(word: &str) -> bool {
    matches!(
        word,
        "yesterday" | "today" | "tonight" | "tomorrow" | "ago" | "earlier" | "recently"
    ) || is_latest_word(word)
}

/// Words that ask for the present state of something rather than for any mention of it
/// ("latest", "currently", "now"). They are not searched for as text — a row rarely says
/// "currently" — and they turn on newest-first ordering of equally matching rows
/// (`super::query_asks_for_latest`, E12-T1).
pub(crate) fn is_latest_word(word: &str) -> bool {
    matches!(word, "latest" | "newest" | "currently" | "nowadays" | "now")
}

/// Japanese counterparts of [`is_latest_word`]: stripped from the terms like the relative dates
/// (`最近` is in [`TEMPORAL_PHRASES_JA`] already) and read as a recency cue.
pub(crate) const LATEST_PHRASES_JA: &[&str] = &["最新", "現在", "直近", "最近", "今は", "今の", "いまは", "いまの"];

const TEMPORAL_PHRASES_JA: &[&str] = &[
    "一昨日", "おととい", "昨日", "きのう", "今日", "きょう", "本日", "今朝", "先々週", "先週",
    "今週", "来週", "先月", "今月", "来月", "今年", "去年", "昨年", "最近", "この前", "さっき",
    "最新", "現在", "直近",
];

/// Blank out Japanese relative-date phrases (`昨日`, `3日前`, `過去7日`) so they neither become
/// search terms nor glue neighbouring kanji into one compound.
fn strip_japanese_dates(text: &str) -> String {
    let mut out = text.to_owned();
    for phrase in TEMPORAL_PHRASES_JA {
        out = out.replace(phrase, " ");
    }
    // 「N日前」「N週間前」「Nか月前」「過去N日」 with ASCII digits (the text is already NFKC).
    let characters = out.chars().collect::<Vec<_>>();
    let mut kept = String::with_capacity(out.len());
    let mut index = 0;
    while index < characters.len() {
        let rest = &characters[index..];
        let starts_past = rest.starts_with(&['過', '去']);
        let digits_from = if starts_past { index + 2 } else { index };
        let digit_count = characters[digits_from.min(characters.len())..]
            .iter()
            .take_while(|character| character.is_ascii_digit() || is_kanji_numeral(**character))
            .count();
        if digit_count > 0 {
            let after = &characters[digits_from + digit_count..];
            let unit_len = ["日前", "週間前", "か月前", "ヶ月前", "日間", "日"]
                .iter()
                .find(|unit| {
                    let unit = unit.chars().collect::<Vec<_>>();
                    after.starts_with(&unit)
                        && (starts_past || !matches!(unit.as_slice(), ['日', '間'] | ['日']))
                })
                .map(|unit| unit.chars().count());
            if let Some(unit_len) = unit_len {
                kept.push(' ');
                index = digits_from + digit_count + unit_len;
                continue;
            }
        }
        kept.push(characters[index]);
        index += 1;
    }
    kept
}

pub(crate) fn is_kanji_numeral(character: char) -> bool {
    matches!(
        character,
        '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十'
    )
}

/// Split a question into concepts. Bounded by [`super::query::MAX_FTS_TERMS`].
pub(crate) fn query_concepts(query: &str) -> Vec<Concept> {
    let typed = strip_japanese_dates(&nfkc(query));
    let segments = segments(&typed);
    let mut concepts: Vec<Concept> = Vec::new();
    let push = |concepts: &mut Vec<Concept>, concept: Concept| {
        if concepts.len() < super::query::MAX_FTS_TERMS
            && !concepts
                .iter()
                .any(|existing| existing.surface == concept.surface && existing.grams == concept.grams)
        {
            concepts.push(concept);
        }
    };

    // Latin words, in order. Case is kept for display only.
    for (class, token) in &segments {
        if *class != Script::Latin {
            continue;
        }
        let lower = token.to_lowercase();
        let length = token.chars().count();
        if length >= 3 && !super::query::is_stopword(&lower) && !is_temporal_word(&lower) {
            push(
                &mut concepts,
                Concept {
                    display: token.clone(),
                    surface: Some(lower),
                    grams: Vec::new(),
                    short_word: false,
                    cjk: false,
                },
            );
        } else if length == 2 && has_letter(token) && !is_short_stopword(&lower) {
            push(
                &mut concepts,
                Concept {
                    display: token.clone(),
                    surface: None,
                    grams: vec![lower],
                    short_word: true,
                    cjk: false,
                },
            );
        }
    }

    let units = cjk_units(&segments);
    let mut cjk_found = false;
    for unit in &units {
        let key = unit.key.to_lowercase();
        let key_chars = key.chars().count();
        let surface = unit.surface.to_lowercase();
        let surface_chars = surface.chars().count();
        if unit.script == Script::Hiragana || key_chars < 2 || is_japanese_filler(&key) {
            continue;
        }
        cjk_found = true;
        let concept = match (unit.script, key_chars) {
            (Script::Kanji, 2) => Concept {
                display: unit.surface.clone(),
                surface: (surface_chars >= 3).then_some(surface),
                grams: vec![key],
                short_word: false,
                cjk: true,
            },
            (Script::Kanji, _) => Concept {
                display: unit.surface.clone(),
                surface: Some(if unit.folded { surface } else { key.clone() }),
                grams: bigrams(&key),
                short_word: false,
                cjk: true,
            },
            (_, 2) => Concept {
                display: unit.surface.clone(),
                surface: None,
                grams: vec![key],
                short_word: false,
                cjk: true,
            },
            _ => Concept {
                display: unit.surface.clone(),
                surface: Some(key),
                grams: Vec::new(),
                short_word: false,
                cjk: true,
            },
        };
        push(&mut concepts, concept);
    }

    // A question written only in hiragana still has to search for something.
    if concepts.is_empty() && !cjk_found {
        for unit in units {
            let lower = unit.key.to_lowercase();
            if unit.script == Script::Hiragana && lower.chars().count() >= 3 {
                push(
                    &mut concepts,
                    Concept {
                        display: unit.key.clone(),
                        surface: Some(lower),
                        grams: Vec::new(),
                        short_word: false,
                        cjk: true,
                    },
                );
            }
        }
    }
    concepts
}

/// One normalised document, ready for concept matching.
pub(crate) struct MatchText {
    title: String,
    body: String,
    grams: Option<HashSet<String>>,
    body_chars: usize,
}

impl MatchText {
    pub(crate) fn new(title: &str, body: &str) -> Self {
        let body = normalize(body);
        Self {
            title: normalize(title),
            body_chars: body.chars().count(),
            body,
            grams: None,
        }
    }

    fn grams(&mut self) -> &HashSet<String> {
        if self.grams.is_none() {
            let mut set = gram_set(&self.title);
            set.extend(gram_set(&self.body));
            self.grams = Some(set);
        }
        self.grams.get_or_insert_with(HashSet::new)
    }

    /// Weighted term frequency of `concept` (a title occurrence counts double).
    ///
    /// Down-weighting matches on short "chrome" lines (`Drafts 3`) was tried and measured: no
    /// change on the golden eval (BM25 length normalisation already demotes chrome-heavy
    /// windows), so it is not done here. Excerpts still avoid chrome (`excerpt.rs`).
    pub(crate) fn frequency(&mut self, concept: &Concept) -> f64 {
        let needle = match (&concept.surface, concept.grams.as_slice()) {
            (Some(surface), _) => surface.as_str(),
            (None, [gram]) => gram.as_str(),
            _ => "",
        };
        let mut frequency = 0.0;
        if !needle.is_empty() {
            frequency += TITLE_WEIGHT * occurrences(&self.title, needle, concept.short_word);
            frequency += occurrences(&self.body, needle, concept.short_word);
        }
        if frequency == 0.0
            && !concept.grams.is_empty()
            && !concept.short_word
            && concept.grams.iter().all(|gram| self.grams().contains(gram))
        {
            // Matched only through folded okurigana (打合せ ↔ 打ち合わせ) or a bag of bigrams.
            frequency = 1.0;
        }
        frequency
    }

    pub(crate) fn matches(&mut self, concept: &Concept) -> bool {
        self.frequency(concept) > 0.0
    }

    pub(crate) fn len_chars(&self) -> usize {
        self.body_chars + self.title.chars().count()
    }

    /// Normalised title + body, for verbatim checks.
    pub(crate) fn contains(&self, needle: &str) -> bool {
        self.title.contains(needle) || self.body.contains(needle)
    }
}

/// Occurrences of `needle` in `hay` (both normalised). A short word only counts on Latin token
/// boundaries.
fn occurrences(hay: &str, needle: &str, short_word: bool) -> f64 {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0.0;
    }
    let mut count = 0usize;
    for (at, _) in hay.match_indices(needle) {
        if short_word {
            let before = hay[..at].chars().next_back();
            let after = hay[at + needle.len()..].chars().next();
            let boundary =
                |character: Option<char>| character.is_none_or(|c| script(c) != Script::Latin);
            if !(boundary(before) && boundary(after)) {
                continue;
            }
        }
        count += 1;
    }
    count as f64
}

/// BM25 over concepts, computed in Rust so the trigram and gram indexes score on one scale.
pub(crate) struct Scorer {
    pub concepts: Vec<Concept>,
    idf: Vec<f64>,
    avg_len: f64,
    /// Multi-concept CJK runs as typed (予算の承認): a verbatim occurrence earns a bonus.
    phrases: Vec<String>,
}

/// Score and concept coverage for one document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LexicalScore {
    pub score: f64,
    pub coverage: f64,
}

impl Scorer {
    /// `document_frequency[i]` is the number of documents matching concept `i` among `total`.
    pub(crate) fn new(
        query: &str,
        concepts: Vec<Concept>,
        document_frequency: &[f64],
        total: f64,
        avg_len: f64,
    ) -> Self {
        let total = total.max(1.0);
        let idf = concepts
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let df = document_frequency.get(index).copied().unwrap_or(1.0).clamp(0.0, total);
                (1.0 + (total - df + 0.5) / (df + 0.5)).ln().max(0.01)
            })
            .collect();
        Self {
            phrases: verbatim_phrases(query, &concepts),
            concepts,
            idf,
            avg_len: avg_len.max(1.0),
        }
    }

    /// Multiply each concept's idf by `boosts[i]` (missing entries keep 1.0).
    pub(crate) fn with_boosts(mut self, boosts: &[f64]) -> Self {
        for (idf, boost) in self.idf.iter_mut().zip(boosts) {
            *idf *= boost.max(0.0);
        }
        self
    }

    pub(crate) fn score(&self, text: &mut MatchText) -> LexicalScore {
        if self.concepts.is_empty() {
            return LexicalScore { score: 0.0, coverage: 0.0 };
        }
        let length_norm = 1.0 - BM25_B + BM25_B * text.len_chars() as f64 / self.avg_len;
        let mut score = 0.0;
        let mut matched = 0usize;
        for (concept, idf) in self.concepts.iter().zip(&self.idf) {
            let frequency = text.frequency(concept);
            if frequency > 0.0 {
                matched += 1;
                score += idf * frequency * (BM25_K1 + 1.0) / (frequency + BM25_K1 * length_norm);
            }
        }
        let mean_idf = self.idf.iter().sum::<f64>() / self.idf.len() as f64;
        for phrase in &self.phrases {
            if text.contains(phrase) {
                score += 0.5 * mean_idf;
            }
        }
        LexicalScore {
            score,
            coverage: matched as f64 / self.concepts.len() as f64,
        }
    }
}

/// CJK runs that hold two or more concepts (予算の承認 → 予算, 承認), lowercased, ≥ 3 characters.
fn verbatim_phrases(query: &str, concepts: &[Concept]) -> Vec<String> {
    if concepts.iter().filter(|concept| concept.cjk).count() < 2 {
        return Vec::new();
    }
    let typed = normalize(&strip_japanese_dates(&nfkc(query)));
    let mut phrases = Vec::new();
    let mut current = String::new();
    for character in typed.chars().chain(std::iter::once(' ')) {
        if is_cjk(character) {
            current.push(character);
        } else {
            let trimmed = current.trim_end_matches(|c: char| script(c) == Script::Hiragana);
            let concepts_inside = concepts
                .iter()
                .filter(|concept| {
                    concept
                        .surface
                        .as_deref()
                        .or_else(|| concept.grams.first().map(String::as_str))
                        .is_some_and(|key| trimmed.contains(key))
                })
                .count();
            if trimmed.chars().count() >= 3 && concepts_inside >= 2 {
                phrases.push(trimmed.to_owned());
            }
            current.clear();
        }
    }
    phrases
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(query: &str) -> Vec<(Option<String>, Vec<String>)> {
        query_concepts(query)
            .into_iter()
            .map(|concept| (concept.surface, concept.grams))
            .collect()
    }

    fn s(value: &str) -> Option<String> {
        Some(value.to_owned())
    }

    fn g(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn two_character_words_become_gram_concepts() {
        assert_eq!(keys("会議"), vec![(None, g(&["会議"]))]);
        assert_eq!(keys("会議 予算"), vec![(None, g(&["会議"])), (None, g(&["予算"]))]);
    }

    #[test]
    fn particles_and_honorifics_split_and_are_dropped() {
        assert_eq!(keys("田中さんの件"), vec![(None, g(&["田中"]))]);
        assert_eq!(keys("予算の承認"), vec![(None, g(&["予算"])), (None, g(&["承認"]))]);
    }

    #[test]
    fn okurigana_variants_share_one_gram() {
        for query in ["打合せ", "打ち合わせ", "打ち合せ"] {
            let concepts = query_concepts(query);
            assert_eq!(concepts.len(), 1, "{query}");
            assert_eq!(concepts[0].grams, g(&["打合"]), "{query}");
        }
        assert_eq!(query_concepts("申し込み")[0].grams, g(&["申込"]));
        assert!(index_grams(None, "セミナーの申し込みを受け付けました").split(' ').any(|gram| gram == "申込"));
        assert!(index_grams(None, "明日の打ち合わせは本社で").split(' ').any(|gram| gram == "打合"));
    }

    #[test]
    fn particles_between_kanji_do_not_fold() {
        let grams = index_grams(None, "予算の承認");
        assert!(grams.split(' ').any(|gram| gram == "予算"));
        assert!(grams.split(' ').any(|gram| gram == "承認"));
        assert!(!grams.split(' ').any(|gram| gram == "算承"));
    }

    #[test]
    fn longer_compounds_keep_a_surface_and_a_gram_bag() {
        assert_eq!(keys("定例会議"), vec![(s("定例会議"), g(&["定例", "例会", "会議"]))]);
        assert_eq!(keys("リリースノート"), vec![(s("リリースノート"), Vec::new())]);
        assert_eq!(keys("バグ"), vec![(None, g(&["バグ"]))]);
    }

    #[test]
    fn short_latin_tokens_are_kept_except_function_words() {
        assert_eq!(
            keys("q3 revenue churn"),
            vec![(None, g(&["q3"])), (s("revenue"), Vec::new()), (s("churn"), Vec::new())]
        );
        assert_eq!(keys("PR is in CI"), vec![(None, g(&["pr"])), (None, g(&["ci"]))]);
        let grams = index_grams(Some("Q3 review"), "PR #1187 merged; CI green. It is in v2.");
        for expected in ["q3", "pr", "ci", "v2"] {
            assert!(grams.split(' ').any(|gram| gram == expected), "{expected}: {grams}");
        }
        assert!(!grams.split(' ').any(|gram| gram == "it" || gram == "is" || gram == "in"));
    }

    #[test]
    fn width_variants_meet_after_nfkc() {
        assert_eq!(keys("ｷｬﾝﾍﾟｰﾝ"), vec![(s("キャンペーン"), Vec::new())]);
        assert_eq!(keys("ＰＲ"), vec![(None, g(&["pr"]))]);
    }

    #[test]
    fn relative_dates_are_not_search_terms() {
        assert_eq!(keys("昨日の会議"), vec![(None, g(&["会議"]))]);
        assert_eq!(keys("2日前のメモ"), vec![(None, g(&["メモ"]))]);
        assert_eq!(keys("過去7日の障害"), vec![(None, g(&["障害"]))]);
        assert_eq!(keys("yesterday standup"), vec![(s("standup"), Vec::new())]);
    }

    #[test]
    fn mixed_scripts_split_at_boundaries() {
        assert_eq!(keys("Q3の売上"), vec![(None, g(&["q3"])), (None, g(&["売上"]))]);
        assert_eq!(keys("Slack連携"), vec![(s("slack"), Vec::new()), (None, g(&["連携"]))]);
    }

    #[test]
    fn short_word_matching_respects_latin_boundaries() {
        let concept = &query_concepts("PR")[0];
        assert!(MatchText::new("", "PR #12 merged").matches(concept));
        assert!(!MatchText::new("", "price list").matches(concept));
        assert!(MatchText::new("", "PRの確認").matches(concept));
    }

    #[test]
    fn folded_okurigana_matches_without_a_surface_hit() {
        let concept = &query_concepts("打合せ")[0];
        assert!(MatchText::new("", "明日の打ち合わせ").matches(concept));
        assert!(MatchText::new("打合せ：進捗", "").matches(concept));
        assert!(!MatchText::new("", "打つ").matches(concept));
    }

    #[test]
    fn a_title_occurrence_counts_double() {
        let concept = &query_concepts("draft")[0];
        assert_eq!(MatchText::new("Draft", "draft").frequency(concept), 3.0);
    }

    #[test]
    fn hiragana_only_question_still_searches() {
        assert_eq!(keys("おすすめ"), vec![(s("おすすめ"), Vec::new())]);
    }
}
