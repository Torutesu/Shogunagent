//! Where in the transcript a minutes item was said (docs/meeting-gap-analysis-2026-09.md D5).
//!
//! The model writes decisions and next actions as prose; the transcript keeps timestamps. This
//! module joins the two after the fact — no prompt change, no extra model call — by finding the
//! run of transcript lines that shares the most vocabulary with each item. The Recap shows that
//! line's clock next to the item and jumps to it on click.
//!
//! Pure text matching, deliberately conservative: an item that resembles nothing gets `None`
//! rather than a guess, because a wrong timestamp is worse than no timestamp.

use std::collections::HashSet;

/// How many consecutive transcript lines one item may span. ASR segments are short (a clause
/// or two), and a decision is usually stated across a few of them.
const WINDOW: usize = 3;

/// The share of an item's tokens that must appear in the window before it counts as an anchor.
const MIN_CONTAINMENT: f64 = 0.34;

/// English function words that would otherwise let "we will do the thing" match any line.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "to", "and", "of", "in", "on", "for", "it", "that", "this", "is", "are",
    "be", "was", "were", "with", "as", "at", "by", "or", "we", "our", "will", "would", "should",
    "can", "could", "have", "has", "had", "do", "does", "did", "not", "no", "so", "if", "but",
    "about", "from", "into", "than", "then", "there", "their", "they", "them", "you", "your", "i",
    "me", "my", "he", "she", "his", "her", "us", "all", "any", "some", "more", "also",
];

/// Tokenize for overlap: lowercased Latin/digit words (two characters or more, minus stop
/// words) plus character bigrams for CJK runs, which have no spaces to split on.
fn tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    let flush_word = |word: &mut String, out: &mut HashSet<String>| {
        if word.chars().count() >= 2 && !STOP_WORDS.contains(&word.as_str()) {
            out.insert(std::mem::take(word));
        } else {
            word.clear();
        }
    };
    for ch in text.chars() {
        if is_cjk(ch) {
            flush_word(&mut word, &mut out);
            if let Some(prev) = prev_cjk {
                out.insert(format!("{prev}{ch}"));
            }
            prev_cjk = Some(ch);
        } else if ch.is_alphanumeric() {
            prev_cjk = None;
            for lower in ch.to_lowercase() {
                word.push(lower);
            }
        } else {
            prev_cjk = None;
            flush_word(&mut word, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    out
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xFF66..=0xFF9F // Half-width Katakana
    )
}

/// The timestamp where `item` was said, or `None` when nothing resembles it. `lines` are
/// `(ts, text)` in time order. The best-matching window of consecutive lines wins; inside it,
/// the anchor is the line that shares the most words with the item (the window only lets a
/// statement that spilled across segments count as one). Ties go to the earliest, since a
/// decision is usually first stated before it is repeated.
pub fn anchor_ts(item: &str, lines: &[(i64, &str)]) -> Option<i64> {
    let needle = tokens(item);
    if needle.is_empty() || lines.is_empty() {
        return None;
    }
    let line_tokens: Vec<HashSet<String>> = lines.iter().map(|(_, text)| tokens(text)).collect();
    let per_line: Vec<usize> = line_tokens
        .iter()
        .map(|set| needle.iter().filter(|token| set.contains(*token)).count())
        .collect();
    let mut best: Option<(f64, i64)> = None;
    for start in 0..lines.len() {
        let end = (start + WINDOW).min(lines.len());
        let shared = needle
            .iter()
            .filter(|token| {
                line_tokens[start..end]
                    .iter()
                    .any(|set| set.contains(*token))
            })
            .count();
        if shared == 0 {
            continue;
        }
        let containment = shared as f64 / needle.len() as f64;
        // One shared token is a coincidence unless the item is tiny.
        if containment < MIN_CONTAINMENT || (shared < 2 && needle.len() > 2) {
            continue;
        }
        // The line inside the window that carries the most of the item, earliest on ties.
        let anchor = (start..end)
            .max_by(|a, b| per_line[*a].cmp(&per_line[*b]).then(b.cmp(a)))
            .map(|index| lines[index].0)
            .unwrap_or(lines[start].0);
        // MSRV 1.80: `Option::is_none_or` is 1.82.
        if best.is_none_or(|(score, _)| containment > score) {
            best = Some((containment, anchor));
        }
    }
    best.map(|(_, ts)| ts)
}

/// [`anchor_ts`] for each item, in order.
pub fn anchors(items: &[String], lines: &[(i64, &str)]) -> Vec<Option<i64>> {
    items.iter().map(|item| anchor_ts(item, lines)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<(i64, &'static str)> {
        vec![
            (1_000, "morning everyone let's get started"),
            (5_000, "so the vendor came back with a lower price"),
            (
                9_000,
                "I think we should renew the contract for another year",
            ),
            (12_000, "agreed, renew for a year and revisit pricing in Q3"),
            (
                20_000,
                "next, Tanaka can you email the vendor with the decision",
            ),
            (24_000, "sure I'll send it tomorrow"),
        ]
    }

    #[test]
    fn a_decision_anchors_to_the_line_where_it_was_said() {
        assert_eq!(
            anchor_ts("Renew the contract for another year", &lines()),
            Some(9_000)
        );
        assert_eq!(
            anchor_ts("Email the vendor with the decision", &lines()),
            Some(20_000)
        );
    }

    #[test]
    fn an_item_that_resembles_nothing_gets_no_anchor() {
        assert_eq!(anchor_ts("Hire two designers in Berlin", &lines()), None);
        assert_eq!(anchor_ts("", &lines()), None);
        assert_eq!(anchor_ts("Renew the contract", &[]), None);
    }

    #[test]
    fn function_words_alone_never_anchor() {
        // Every line has "the" / "we" / "it"; the item must share real vocabulary.
        assert_eq!(anchor_ts("We will do it with them", &lines()), None);
    }

    #[test]
    fn japanese_items_anchor_by_character_bigrams() {
        let lines = vec![
            (1_000, "では始めましょう"),
            (4_000, "見積もりの件ですが来週までに回答します"),
            (8_000, "契約は来年まで更新することで合意しました"),
        ];
        assert_eq!(anchor_ts("契約を来年まで更新する", &lines), Some(8_000));
        assert_eq!(anchor_ts("見積もりに来週回答", &lines), Some(4_000));
        assert_eq!(anchor_ts("採用計画の見直し", &lines), None);
    }

    #[test]
    fn a_repeated_decision_anchors_to_its_first_statement() {
        let lines = vec![
            (1_000, "we should ship the beta in March"),
            (30_000, "to recap: ship the beta in March"),
        ];
        assert_eq!(anchor_ts("Ship the beta in March", &lines), Some(1_000));
    }

    #[test]
    fn anchors_keeps_item_order() {
        let items = vec![
            "Renew the contract for another year".to_string(),
            "Nothing like this".to_string(),
        ];
        assert_eq!(anchors(&items, &lines()), vec![Some(9_000), None]);
    }
}
