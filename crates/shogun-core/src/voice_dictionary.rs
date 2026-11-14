//! Conservative local vocabulary matching for voice dictation.
//!
//! This layer corrects only explicit aliases. It deliberately does not use fuzzy, phonetic, or
//! edit-distance matching: a missed correction is safer than changing a name.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

/// Deepgram Nova-3 accepts at most 100 keyterm prompts. Keep this bound before request assembly.
pub const MAX_KEYTERM_HINTS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 120;
/// Storage admits at most this many normalized words in one alias. Longer aliases are retained
/// for Settings round-trips but deliberately skip local matching: a false negative is safe and
/// keeps transcript-time work bounded.
const MAX_ALIAS_TOKENS: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermScope {
    Global,
    Bundle(String),
    Surface(String),
}

#[derive(Debug, Clone)]
pub struct DictionaryEntry {
    pub canonical: String,
    pub aliases: Vec<String>,
    pub locale: Option<String>,
    pub scope: TermScope,
    pub priority: i32,
    pub enabled: bool,
    pub user_confirmed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DictionaryContext {
    pub locale: Option<String>,
    pub bundle_id: Option<String>,
    pub surface: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedTerm {
    pub canonical: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryCorrection {
    pub text: String,
    pub protected_terms: Vec<String>,
    pub matches: Vec<MatchedTerm>,
}

#[derive(Debug, Clone, Default)]
pub struct VoiceDictionary {
    entries: Vec<DictionaryEntry>,
    /// Exact aliases by their normalized first word. This avoids re-normalizing and scanning every
    /// stored alias at every transcript word. Storage also applies term and alias caps.
    aliases_by_first_token: HashMap<String, Vec<IndexedAlias>>,
}

#[derive(Debug, Clone)]
struct IndexedAlias {
    entry_index: usize,
    tokens: Vec<String>,
}

impl VoiceDictionary {
    pub fn new(entries: Vec<DictionaryEntry>) -> Self {
        let aliases_by_first_token = build_alias_index(&entries);
        Self {
            entries,
            aliases_by_first_token,
        }
    }

    /// Canonical terms suitable for a bounded STT keyterm hint list.
    pub fn keyterms(&self) -> Vec<String> {
        self.keyterms_for(&DictionaryContext::default())
    }

    /// Canonical terms eligible in this dictation context, deduplicated and bounded for STT.
    pub fn keyterms_for(&self, context: &DictionaryContext) -> Vec<String> {
        let mut candidates = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.enabled && entry.user_confirmed && entry_matches_context(entry, context)
            })
            .filter_map(|entry| {
                let canonical = entry.1.canonical.trim();
                (canonical.chars().count() <= MAX_KEYTERM_CHARS
                    && !canonical.is_empty()
                    && !canonical.chars().any(char::is_control)
                    && !normalized_tokens(canonical).is_empty())
                .then(|| (entry.0, canonical.to_string(), normalized_tokens(canonical)))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(
            |(left_index, left, left_tokens), (right_index, right, right_tokens)| {
                let left_entry = &self.entries[*left_index];
                let right_entry = &self.entries[*right_index];
                context_rank(right_entry)
                    .cmp(&context_rank(left_entry))
                    .then_with(|| right_entry.priority.cmp(&left_entry.priority))
                    .then_with(|| left_tokens.cmp(right_tokens))
                    .then_with(|| left.cmp(right))
            },
        );

        let mut terms = Vec::with_capacity(MAX_KEYTERM_HINTS);
        let mut seen = std::collections::HashSet::new();
        for (_, canonical, normalized) in candidates {
            if seen.insert(normalized) {
                terms.push(canonical);
                if terms.len() == MAX_KEYTERM_HINTS {
                    break;
                }
            }
        }
        terms
    }

    /// Merge explicit user terms with built-ins. User terms are retained even when disabled so
    /// settings can round-trip them; eligibility is evaluated at matching and ASR time.
    pub fn with_user_entries(entries: Vec<DictionaryEntry>) -> Self {
        let mut all_entries = Self::with_defaults().entries;
        all_entries.extend(entries);
        Self::new(all_entries)
    }

    /// Build an exact-match user entry. The canonical spelling is included as an alias so an ASR
    /// result that already has the right word is protected from later cloud cleanup.
    pub fn user_entry(
        canonical: String,
        mut aliases: Vec<String>,
        locale: Option<String>,
        scope: TermScope,
        priority: i32,
        enabled: bool,
    ) -> DictionaryEntry {
        if !aliases
            .iter()
            .any(|alias| normalized_tokens(alias) == normalized_tokens(&canonical))
        {
            aliases.push(canonical.clone());
        }
        DictionaryEntry {
            canonical,
            aliases,
            locale,
            scope,
            priority,
            enabled,
            user_confirmed: true,
        }
    }
    /// Built-in aliases shipped with dictation cleanup.
    pub fn with_defaults() -> Self {
        let terms = [
            ("ShogunAI", &["shogun ai", "show gun ai"][..]),
            ("GPT-OSS", &["gpt oss", "g p t oss"][..]),
            ("Deepgram", &["deep gram"][..]),
            ("Nova-3", &["nova 3"][..]),
            ("Tauri", &["tarry"][..]),
            ("Rust", &["rust"][..]),
            ("Whisper", &["whisper"][..]),
            ("Groq", &["g rock"][..]),
        ];
        Self::new(
            terms
                .into_iter()
                .map(|(canonical, aliases)| DictionaryEntry {
                    canonical: canonical.to_string(),
                    aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
                    locale: None,
                    scope: TermScope::Global,
                    priority: 0,
                    enabled: true,
                    user_confirmed: true,
                })
                .collect(),
        )
    }

    pub fn correct(&self, input: &str, context: &DictionaryContext) -> DictionaryCorrection {
        let tokens = tokenize(input);
        let mut replacements = Vec::new();
        let mut index = 0;

        while index < tokens.len() {
            let candidates = self
                .aliases_by_first_token
                .get(&tokens[index].normalized)
                .into_iter()
                .flatten()
                .filter_map(|alias| {
                    let entry = &self.entries[alias.entry_index];
                    (entry.enabled
                        && entry.user_confirmed
                        && entry_matches_context(entry, context)
                        && index + alias.tokens.len() <= tokens.len()
                        && match_uses_safe_boundaries(input, &tokens, index, alias.tokens.len())
                        && tokens[index..index + alias.tokens.len()]
                            .iter()
                            .map(|token| token.normalized.as_str())
                            .eq(alias.tokens.iter().map(String::as_str)))
                    .then_some((entry, alias.tokens.len()))
                })
                .collect::<Vec<_>>();

            let Some((entry, alias_len)) = choose_candidate(candidates) else {
                index += 1;
                continue;
            };

            let start = tokens[index].start;
            let end = tokens[index + alias_len - 1].end;
            replacements.push((start, end, entry.canonical.clone()));
            index += alias_len;
        }

        let mut output = String::with_capacity(input.len());
        let mut cursor = 0;
        let mut matches = Vec::with_capacity(replacements.len());
        for (start, end, canonical) in replacements {
            output.push_str(&input[cursor..start]);
            let output_start = output.len();
            output.push_str(&canonical);
            matches.push(MatchedTerm {
                canonical,
                start: output_start,
                end: output.len(),
            });
            cursor = end;
        }
        output.push_str(&input[cursor..]);

        let mut protected_terms = Vec::new();
        for matched in &matches {
            if !protected_terms
                .iter()
                .any(|term| term == &matched.canonical)
            {
                protected_terms.push(matched.canonical.clone());
            }
        }
        DictionaryCorrection {
            text: output,
            protected_terms,
            matches,
        }
    }
}

#[derive(Debug, Clone)]
struct Token {
    start: usize,
    end: usize,
    normalized: String,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in input.char_indices() {
        if is_lookup_token_char(ch) {
            start.get_or_insert(index);
        } else if let Some(token_start) = start.take() {
            tokens.push(Token {
                start: token_start,
                end: index,
                normalized: normalize_token(&input[token_start..index]),
            });
        }
    }
    if let Some(token_start) = start {
        tokens.push(Token {
            start: token_start,
            end: input.len(),
            normalized: normalize_token(&input[token_start..]),
        });
    }
    tokens
}

fn is_lookup_token_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || matches!(
            ch as u32,
            0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
        )
}

fn normalize_token(value: &str) -> String {
    value.nfkc().flat_map(char::to_lowercase).collect()
}

fn normalized_tokens(input: &str) -> Vec<String> {
    tokenize(input)
        .into_iter()
        .map(|token| token.normalized)
        .collect()
}

fn build_alias_index(entries: &[DictionaryEntry]) -> HashMap<String, Vec<IndexedAlias>> {
    let mut index: HashMap<String, Vec<IndexedAlias>> = HashMap::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        for alias in &entry.aliases {
            // Exact corrections must not consume punctuation inside an email, URL, path, or code
            // token. Reject punctuation-bearing aliases rather than guessing how to preserve them.
            if !alias
                .chars()
                .all(|ch| is_lookup_token_char(ch) || ch.is_whitespace())
            {
                continue;
            }
            let tokens = normalized_tokens(alias);
            if tokens.is_empty() || tokens.len() > MAX_ALIAS_TOKENS {
                continue;
            }
            index
                .entry(tokens[0].clone())
                .or_default()
                .push(IndexedAlias {
                    entry_index,
                    tokens,
                });
        }
    }
    index
}

fn match_uses_safe_boundaries(input: &str, tokens: &[Token], start: usize, len: usize) -> bool {
    let matched = &tokens[start..start + len];
    if matched.windows(2).any(|pair| {
        !input[pair[0].end..pair[1].start]
            .chars()
            .all(char::is_whitespace)
    }) {
        return false;
    }

    let range_start = matched[0].start;
    let range_end = matched[matched.len() - 1].end;
    let token_start = input[..range_start]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let token_end = input[range_end..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(input.len(), |(index, _)| range_end + index);
    !input[token_start..token_end]
        .chars()
        .any(|ch| matches!(ch, '@' | '/' | ':' | '_'))
}

fn entry_matches_context(entry: &DictionaryEntry, context: &DictionaryContext) -> bool {
    if let Some(locale) = &entry.locale {
        if !locale_matches(context.locale.as_deref(), locale) {
            return false;
        }
    }
    match &entry.scope {
        TermScope::Global => true,
        TermScope::Bundle(bundle) => context.bundle_id.as_deref() == Some(bundle.as_str()),
        TermScope::Surface(surface) => context.surface.as_deref() == Some(surface.as_str()),
    }
}

fn locale_matches(context_locale: Option<&str>, entry_locale: &str) -> bool {
    normalize_locale(context_locale) == normalize_locale(Some(entry_locale))
}

fn normalize_locale(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let value = value.split(['.', '@']).next()?.replace('_', "-");
    if value.eq_ignore_ascii_case("c") || value.eq_ignore_ascii_case("posix") {
        return None;
    }
    let parts = value
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let language = parts.first()?;
    if !language.chars().all(|ch| ch.is_ascii_alphabetic()) || !(2..=3).contains(&language.len()) {
        return None;
    }
    let mut normalized = language.to_ascii_lowercase();
    for part in parts.iter().skip(1) {
        if part.len() == 2 && part.chars().all(|ch| ch.is_ascii_alphabetic()) {
            normalized.push('-');
            normalized.push_str(&part.to_ascii_uppercase());
            break;
        }
        if part.len() == 3 && part.chars().all(|ch| ch.is_ascii_digit()) {
            normalized.push('-');
            normalized.push_str(part);
            break;
        }
    }
    Some(normalized)
}

fn context_rank(entry: &DictionaryEntry) -> u8 {
    // A focused application is more specific than a generic dictation surface. This is the same
    // order used for correction and Deepgram keyterm selection: bundle > surface > global.
    match &entry.scope {
        TermScope::Global => 0,
        TermScope::Surface(_) => 1,
        TermScope::Bundle(_) => 2,
    }
}

fn choose_candidate(
    candidates: Vec<(&DictionaryEntry, usize)>,
) -> Option<(&DictionaryEntry, usize)> {
    let max_len = candidates.iter().map(|(_, alias_len)| *alias_len).max()?;
    let mut best: Option<(&DictionaryEntry, usize)> = None;
    let mut ambiguous = false;
    for candidate @ (entry, _) in candidates
        .into_iter()
        .filter(|(_, alias_len)| *alias_len == max_len)
    {
        match best {
            None => best = Some(candidate),
            Some((selected, _)) => {
                let selected_rank = (context_rank(selected), selected.priority);
                let candidate_rank = (context_rank(entry), entry.priority);
                if candidate_rank > selected_rank {
                    best = Some(candidate);
                    ambiguous = false;
                } else if candidate_rank == selected_rank && entry.canonical != selected.canonical {
                    ambiguous = true;
                }
            }
        }
    }
    (!ambiguous).then_some(best?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(canonical: &str, aliases: &[&str]) -> DictionaryEntry {
        DictionaryEntry {
            canonical: canonical.into(),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
            locale: None,
            scope: TermScope::Global,
            priority: 0,
            enabled: true,
            user_confirmed: true,
        }
    }

    #[test]
    fn corrects_unique_alias_and_returns_protected_term() {
        let result = VoiceDictionary::new(vec![entry("ShogunAI", &["shogun ai"])])
            .correct("build shogun ai today", &DictionaryContext::default());
        assert_eq!(result.text, "build ShogunAI today");
        assert_eq!(result.protected_terms, vec!["ShogunAI"]);
    }

    #[test]
    fn defaults_preserve_groq_alias_behavior() {
        let result =
            VoiceDictionary::with_defaults().correct("use g rock", &DictionaryContext::default());
        assert_eq!(result.text, "use Groq");
    }

    #[test]
    fn prefers_longest_alias() {
        let result = VoiceDictionary::new(vec![entry("GPT-OSS", &["gpt", "gpt oss"])])
            .correct("gpt oss model", &DictionaryContext::default());
        assert_eq!(result.text, "GPT-OSS model");
    }

    #[test]
    fn skips_ambiguous_aliases() {
        let first = entry("Alpha", &["alpha"]);
        let second = entry("Beta", &["alpha"]);
        let result = VoiceDictionary::new(vec![first, second])
            .correct("alpha", &DictionaryContext::default());
        assert_eq!(result.text, "alpha");
    }

    #[test]
    fn does_not_replace_inside_a_word() {
        let result = VoiceDictionary::new(vec![entry("Rust", &["rust"])])
            .correct("trust rust", &DictionaryContext::default());
        assert_eq!(result.text, "trust Rust");
    }

    #[test]
    fn does_not_rewrite_inside_email_url_or_code_tokens() {
        let dictionary = VoiceDictionary::new(vec![
            entry("Example", &["example com"]),
            entry("Token", &["token"]),
            entry("CPlusPlus", &["c++"]),
        ]);
        let result = dictionary.correct(
            "mail a@example.com, https://token.example, and C++",
            &DictionaryContext::default(),
        );
        assert_eq!(
            result.text,
            "mail a@example.com, https://token.example, and C++"
        );
        assert!(result.matches.is_empty());
    }

    #[test]
    fn prefers_bundle_scope_over_global_scope() {
        let mut global = entry("global", &["draft"]);
        let mut scoped = entry("scoped", &["draft"]);
        scoped.scope = TermScope::Bundle("com.example.app".into());
        global.priority = 100;
        let result = VoiceDictionary::new(vec![global, scoped]).correct(
            "draft",
            &DictionaryContext {
                bundle_id: Some("com.example.app".into()),
                ..Default::default()
            },
        );
        assert_eq!(result.text, "scoped");
    }

    #[test]
    fn prefers_app_bundle_over_generic_surface_for_same_alias() {
        let mut surface = entry("surface draft", &["draft"]);
        surface.scope = TermScope::Surface("voice_dictation".into());
        surface.priority = 100;
        let mut bundle = entry("bundle draft", &["draft"]);
        bundle.scope = TermScope::Bundle("com.example.app".into());
        let context = DictionaryContext {
            bundle_id: Some("com.example.app".into()),
            surface: Some("voice_dictation".into()),
            ..Default::default()
        };
        let dictionary = VoiceDictionary::new(vec![surface, bundle]);
        assert_eq!(dictionary.correct("draft", &context).text, "bundle draft");
        assert_eq!(
            dictionary
                .keyterms_for(&context)
                .first()
                .map(String::as_str),
            Some("bundle draft")
        );
    }

    #[test]
    fn keyterms_respect_context_deduplicate_and_stay_bounded() {
        let mut entries = vec![
            entry("ShogunAI", &["show gun ai"]),
            entry("shogunai", &["x"]),
        ];
        let mut scoped = entry("OnlyMail", &["only mail"]);
        scoped.scope = TermScope::Bundle("com.apple.Mail".into());
        entries.push(scoped);
        entries.extend((0..MAX_KEYTERM_HINTS).map(|n| entry(&format!("term-{n}"), &["x"])));
        let dictionary = VoiceDictionary::new(entries);
        let terms = dictionary.keyterms_for(&DictionaryContext::default());
        assert_eq!(terms.first().map(String::as_str), Some("ShogunAI"));
        assert!(!terms.iter().any(|term| term == "OnlyMail"));
        assert_eq!(terms.len(), MAX_KEYTERM_HINTS);
    }

    #[test]
    fn lookup_uses_nfkc_case_and_whitespace_normalization() {
        let dictionary = VoiceDictionary::new(vec![
            entry("Café", &["cafe\u{301}"]),
            entry("Figma", &["Ｆｉｇｍａ"]),
        ]);
        let result = dictionary.correct("CAFÉ\tfigma", &DictionaryContext::default());
        assert_eq!(result.text, "Café\tFigma");

        let terms = VoiceDictionary::new(vec![entry("Ｆｉｇｍａ", &["x"]), entry("Figma", &["y"])]);
        assert_eq!(
            terms.keyterms_for(&DictionaryContext::default()),
            vec!["Figma"]
        );
    }

    #[test]
    fn keyterms_rank_context_and_priority_before_insertion_order() {
        let mut entries = (0..MAX_KEYTERM_HINTS)
            .map(|index| entry(&format!("built-in-{index}"), &["x"]))
            .collect::<Vec<_>>();
        let mut priority = entry("PriorityTerm", &["priority term"]);
        priority.priority = 100;
        entries.push(priority);
        let dictionary = VoiceDictionary::new(entries);
        let terms = dictionary.keyterms_for(&DictionaryContext::default());
        assert_eq!(terms.first().map(String::as_str), Some("PriorityTerm"));
        assert_eq!(terms.len(), MAX_KEYTERM_HINTS);
    }

    #[test]
    fn locale_context_accepts_normalized_bcp47_values() {
        let mut localized = entry("Figma", &["fig ma"]);
        localized.locale = Some("en-US".into());
        let dictionary = VoiceDictionary::new(vec![localized]);
        let result = dictionary.correct(
            "fig ma",
            &DictionaryContext {
                locale: Some("en_US.UTF-8".into()),
                ..Default::default()
            },
        );
        assert_eq!(result.text, "Figma");
    }

    #[test]
    fn aliases_beyond_token_bound_are_not_indexed() {
        let alias = std::iter::repeat_n("word", MAX_ALIAS_TOKENS + 1)
            .collect::<Vec<_>>()
            .join(" ");
        let dictionary = VoiceDictionary::new(vec![entry("Unsafe", &[&alias])]);
        assert_eq!(
            dictionary
                .correct(&alias, &DictionaryContext::default())
                .text,
            alias
        );
    }

    #[test]
    fn user_entry_protects_a_correctly_transcribed_canonical_term() {
        let dictionary = VoiceDictionary::new(vec![VoiceDictionary::user_entry(
            "Figma".into(),
            vec!["fig ma".into()],
            None,
            TermScope::Global,
            0,
            true,
        )]);
        let result = dictionary.correct("open Figma", &DictionaryContext::default());
        assert_eq!(result.text, "open Figma");
        assert_eq!(result.protected_terms, vec!["Figma"]);
    }

    #[test]
    fn merged_user_entries_are_indexed_for_local_correction() {
        let dictionary = VoiceDictionary::with_user_entries(vec![VoiceDictionary::user_entry(
            "Figma".into(),
            vec!["fig ma".into()],
            None,
            TermScope::Global,
            0,
            true,
        )]);
        assert_eq!(
            dictionary
                .correct("open fig ma", &DictionaryContext::default())
                .text,
            "open Figma"
        );
    }
}
