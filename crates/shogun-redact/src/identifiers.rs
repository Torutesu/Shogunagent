//! National ID and payment-card masking for text that is about to **leave the device**.
//!
//! This is deliberately *not* part of [`crate::redact`], the DB-write path. The audit position
//! (`research/pii-benchmark/docs/FINDINGS.md`, PII-002) is that masking personal data on the way
//! into the encrypted store would corrupt the memory the product exists to build: `people.emails`
//! and `identity::resolve` need addresses and handles intact, and a name masked at write time can
//! never be recovered.
//!
//! National ID numbers and payment cards are the documented exception, and the reason is narrow:
//! **they have no legitimate product use.** Nothing in SHOGUN resolves a person by their card
//! number. So the argument that protects emails from masking does not reach them, and a
//! deterministic backstop at the third-party boundary costs nothing the product wants to keep.
//! Sentient OS ships the same idea ahead of its cloud calls; `research/COMPARISON.md` section 3.
//!
//! Where it applies: [`crate::redact_egress`], for every prompt builder that sends stored or
//! captured text to a model (Batch consolidation / brief / mail triage, chat and draft context,
//! tool results, meeting transcripts; the list is on that function), and [`crate::redact_log`],
//! because a log is diagnostic rather than memory and an ID number there is pure exposure.
//!
//! **Detection is boundary-gated, and each detector earns its false-positive budget differently:**
//!
//! - **Payment cards** are found unlabelled, because the Luhn checksum plus a 13–19 digit length
//!   is specific enough to stand alone. It is not free — roughly one in ten random 16-digit runs
//!   passes Luhn — so this detector is confined to egress and logs, where masking a stray order
//!   number is harmless and masking a real card is the whole point. Group separators (space,
//!   hyphen, dot, ideographic space) must be one kind throughout and every group at most six
//!   digits, which is how a card is printed and how a float timestamp (`1727712000000.123`) is not.
//! - **US SSNs** are found unlabelled, because `NNN-NN-NNNN` with the reserved ranges excluded is
//!   a distinctive shape rather than a digit run. Spaces or dots in place of the hyphens are the
//!   same shape, with one extra guard: a group that continues a column of numbers on either side
//!   (`2026 123 45 6789 10`) is a table row, not an SSN.
//! - **My Number and passport numbers are label-gated.** A bare 12-digit run is a timestamp far
//!   more often than it is a 個人番号, and passport formats vary so widely across issuers that an
//!   unlabelled detector would match ordinary alphanumeric identifiers. Requiring the label buys
//!   a near-zero false-positive rate for the case that actually appears in captured text, which is
//!   a form or a message naming the field. The printed `1234 5678 9012` grouping of a My Number is
//!   accepted after a label.
//!
//! **Boundaries are ASCII boundaries.** Japanese text does not put spaces around a number, so
//! `マイナンバー123456789012です` is the normal way to write one. A kana or kanji neighbour is
//! therefore a token boundary, not a continuation; only an ASCII letter or digit glues onto a
//! number (`x4111111111111111` stays an identifier). A Japanese label needs no boundary before it
//! at all (`申請者個人番号：…`), because it cannot be the tail of an ASCII word. Full-width digits
//! (U+FF10–FF19) and the ideographic space (U+3000) are folded to ASCII for the scan only; the
//! text handed back keeps whatever was not masked exactly as it was written.

use std::borrow::Cow;

use super::MASK;

/// Labels after which a value is a national ID or travel document. Matched case-insensitively for
/// the ASCII entries; the Japanese ones have no case to fold.
///
/// Order matters: longer labels come first so `passport number` is consumed whole rather than
/// leaving `number` stranded after a `passport` match.
const ID_LABELS: &[&str] = &[
    "social security number",
    "social_security_number",
    "individual number",
    "individual_number",
    "passport number",
    "passport_number",
    "passport",
    "mynumber",
    "my number",
    "my_number",
    "マイナンバー",
    "個人番号",
    "パスポート",
    "旅券番号",
    "旅券",
    "ssn",
];

/// Shortest labelled value worth masking. Below this it is a field placeholder, not a document.
const MIN_ID_LEN: usize = 6;
/// Longest run taken as a single labelled value.
const MAX_ID_LEN: usize = 20;
/// Longest digit group in a card printed with separators (`3782 822463 10005`).
const MAX_CARD_GROUP: usize = 6;

/// Is `c` allowed inside a national ID or passport value?
fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Characters that may sit between a label and its value (`passport: X1234567`, `旅券番号 は …`).
fn is_label_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ':' | '=' | '#' | '.' | '：' | '・' | 'は' | 'の' | '>')
}

/// Separators that may group the digits of a card or an SSN. The ideographic space has already
/// been folded to an ASCII space by the time these run.
fn is_group_separator(b: u8) -> bool {
    matches!(b, b' ' | b'-' | b'.')
}

/// The ASCII form of a character the scanner folds, or `None` when it is kept as is.
fn fold_char(c: char) -> Option<char> {
    match c {
        '\u{FF10}'..='\u{FF19}' => char::from_u32(u32::from(c) - 0xFF10 + u32::from('0')),
        '\u{3000}' => Some(' '),
        _ => None,
    }
}

/// `text` with full-width digits and the ideographic space folded to ASCII, plus the map from
/// each folded byte offset back to the original one (one extra entry for the end). `None` when
/// there was nothing to fold, so the common case allocates nothing.
struct Folded {
    text: String,
    map: Vec<usize>,
}

fn fold_width(text: &str) -> Option<Folded> {
    if !text.chars().any(|c| fold_char(c).is_some()) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    for (at, c) in text.char_indices() {
        let folded = fold_char(c).unwrap_or(c);
        for _ in 0..folded.len_utf8() {
            map.push(at);
        }
        out.push(folded);
    }
    map.push(text.len());
    Some(Folded { text: out, map })
}

/// Luhn checksum over already-extracted decimal digits.
fn luhn_ok(digits: &[u8]) -> bool {
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(*d);
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// Byte length of a Luhn-valid card number at the start of `s`, allowing the usual group
/// separators. `None` when this is not a card.
fn card_span(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut digits: Vec<u8> = Vec::with_capacity(19);
    let mut i = 0usize;
    let mut end_of_digits = 0usize;
    let mut separator: Option<u8> = None;
    let mut group = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            digits.push(c - b'0');
            group += 1;
            if digits.len() > 19 || (separator.is_some() && group > MAX_CARD_GROUP) {
                return None;
            }
            i += 1;
            end_of_digits = i;
        } else if is_group_separator(c)
            && !digits.is_empty()
            && i + 1 < b.len()
            && b[i + 1].is_ascii_digit()
        {
            // A separator only continues the number when a digit follows it, so "4111 " ends here
            // rather than swallowing the next word. One kind of separator throughout, and no
            // group longer than a printed card's: `1727712000000.123` is a timestamp with a
            // fraction, not a card.
            if separator.is_some_and(|prev| prev != c) || group > MAX_CARD_GROUP {
                return None;
            }
            separator = Some(c);
            group = 0;
            i += 1;
        } else {
            break;
        }
    }
    if !luhn_ok(&digits) {
        return None;
    }
    // An ASCII letter or digit straight after the digits means this was an identifier that
    // happens to be numeric, not a card printed in running text. Kana or kanji (`…1111です`) is
    // how Japanese ends a sentence, not a continuation.
    if s[end_of_digits..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(end_of_digits)
}

/// Byte length of a structurally valid US SSN at the start of `s`, given `before` (the text up to
/// `s`) for the column guard.
///
/// Excludes the ranges the SSA never issues, which is what separates an SSN from any other
/// `3-2-4` digit grouping (a phone extension, a part number). Hyphens, spaces or dots may group
/// the digits, one kind throughout.
fn ssn_span(s: &str, before: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 11 {
        return None;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let sep = b[3];
    let shaped = is_group_separator(sep)
        && digit(0)
        && digit(1)
        && digit(2)
        && digit(4)
        && digit(5)
        && b[6] == sep
        && digit(7)
        && digit(8)
        && digit(9)
        && digit(10);
    if !shaped {
        return None;
    }
    // Part of a longer run: not an SSN. For the hyphen form a trailing hyphen is enough to say so
    // (`123-45-6789-1`); a space or dot is only a continuation when a digit follows it, since a
    // sentence may end right after the number.
    match (b.get(11), b.get(12)) {
        (Some(next), _) if next.is_ascii_digit() => return None,
        (Some(&next), _) if sep == b'-' && next == b'-' => return None,
        (Some(&next), Some(after)) if next == sep && after.is_ascii_digit() => return None,
        _ => {}
    }
    // A space- or dot-grouped match that continues a run of numbers (`2026 123 45 6789`) is a
    // row of a table, not an SSN. The hyphen form never needs this: a hyphen before it would
    // already have failed the boundary.
    if sep != b'-' {
        let mut prior = before.chars().rev();
        if prior.next() == Some(char::from(sep)) && prior.next().is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    let area: u32 = s.get(0..3)?.parse().ok()?;
    let group: u32 = s.get(4..6)?.parse().ok()?;
    let serial: u32 = s.get(7..11)?.parse().ok()?;
    if area == 0 || area == 666 || area >= 900 || group == 0 || serial == 0 {
        return None;
    }
    Some(11)
}

/// Byte length of a My Number in its printed `1234 5678 9012` form at the start of `s` (space or
/// hyphen, one kind throughout), or `None`. Only consulted after a label.
fn grouped_my_number_span(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 14 {
        return None;
    }
    let sep = b[4];
    if !matches!(sep, b' ' | b'-') || b[9] != sep {
        return None;
    }
    let groups_are_digits = [0..4, 5..9, 10..14]
        .into_iter()
        .all(|range| b[range].iter().all(u8::is_ascii_digit));
    if !groups_are_digits {
        return None;
    }
    // Part of a longer run of digits: not the 12-digit document.
    match (b.get(14), b.get(15)) {
        (Some(next), _) if next.is_ascii_alphanumeric() => return None,
        (Some(&next), Some(after)) if next == sep && after.is_ascii_digit() => return None,
        _ => {}
    }
    Some(14)
}

/// A label plus the value that follows it.
struct LabelledId {
    /// Bytes copied through verbatim (the label and its separator).
    label_end: usize,
    /// Total bytes consumed, label and value together.
    total: usize,
}

/// Case-insensitive ASCII prefix test that also accepts the non-ASCII labels verbatim.
fn label_at(s: &str, label: &str) -> bool {
    if label.is_ascii() {
        s.len() >= label.len() && s.as_bytes()[..label.len()].eq_ignore_ascii_case(label.as_bytes())
    } else {
        s.starts_with(label)
    }
}

/// Match `passport: X1234567` / `マイナンバー 123456789012` at the start of `s`.
///
/// `at_boundary` says whether `s` opens a token. An ASCII label must (`lessn` is not `ssn`); a
/// Japanese label is never the tail of an ASCII word, so it matches regardless.
fn match_labelled_id(s: &str, at_boundary: bool) -> Option<LabelledId> {
    let label = ID_LABELS
        .iter()
        .find(|l| (at_boundary || !l.is_ascii()) && label_at(s, l))?;
    let mut cursor = label.len();

    // Separator run between label and value. Bounded so a label cannot reach across a sentence.
    const MAX_SEPARATORS: usize = 4;
    for (seen, c) in s[cursor..].chars().enumerate() {
        if seen >= MAX_SEPARATORS || !is_label_separator(c) {
            break;
        }
        cursor += c.len_utf8();
    }
    let label_end = cursor;

    if let Some(grouped) = grouped_my_number_span(&s[cursor..]) {
        return Some(LabelledId {
            label_end,
            total: label_end + grouped,
        });
    }

    let value_len = s[cursor..]
        .char_indices()
        .find(|(_, c)| !is_id_char(*c))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len() - cursor);
    if !(MIN_ID_LEN..=MAX_ID_LEN).contains(&value_len) {
        return None;
    }
    // A value of pure letters is a word ("passport application"), not a document number.
    if !s[cursor..cursor + value_len]
        .chars()
        .any(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(LabelledId {
        label_end,
        total: label_end + value_len,
    })
}

/// True when `text` (already folded) holds anything worth a full scan.
fn might_contain_identifier(text: &str) -> bool {
    if text.bytes().any(|b| b.is_ascii_digit()) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    ID_LABELS.iter().any(|l| lower.contains(l))
}

/// Mask national ID numbers and payment cards in `text`.
///
/// Returns the input unchanged, without allocating, when nothing matched. Unmasked text comes
/// back exactly as written: the width folding is for the scan only.
pub(crate) fn mask_high_risk_identifiers(text: &str) -> Cow<'_, str> {
    let folded = fold_width(text);
    let scan: &str = folded.as_ref().map_or(text, |f| f.text.as_str());
    if !might_contain_identifier(scan) {
        return Cow::Borrowed(text);
    }
    // Folded byte offset -> original byte offset. Identity when nothing was folded.
    let orig = |at: usize| folded.as_ref().map_or(at, |f| f.map[at]);

    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut masked_any = false;

    while i < scan.len() {
        let rest = &scan[i..];

        // A match must open a token. Without this, the tail of a longer identifier could be read
        // as a card in its own right. Only ASCII letters and digits glue onto a number: a kana or
        // kanji neighbour (`マイナンバー123456789012です`) is a boundary.
        let before = &scan[..i];
        let at_boundary = !before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());

        if let Some(found) = match_labelled_id(rest, at_boundary) {
            out.push_str(&text[orig(i)..orig(i + found.label_end)]);
            out.push_str(MASK);
            i += found.total;
            masked_any = true;
            continue;
        }
        if at_boundary {
            if let Some(len) = ssn_span(rest, before) {
                out.push_str(MASK);
                i += len;
                masked_any = true;
                continue;
            }
            if let Some(len) = card_span(rest) {
                out.push_str(MASK);
                i += len;
                masked_any = true;
                continue;
            }
        }

        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[orig(i)..orig(i + ch_len)]);
        i += ch_len;
    }

    if masked_any {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> String {
        mask_high_risk_identifiers(s).into_owned()
    }

    #[test]
    fn luhn_valid_cards_are_masked_in_every_common_grouping() {
        // Standard test numbers: Visa, Mastercard, Amex (15), Visa (13).
        assert_eq!(m("card 4111111111111111 ok"), "card [redacted] ok");
        assert_eq!(m("card 4111 1111 1111 1111 ok"), "card [redacted] ok");
        assert_eq!(m("card 4111-1111-1111-1111 ok"), "card [redacted] ok");
        assert_eq!(m("amex 378282246310005 end"), "amex [redacted] end");
        assert_eq!(m("amex 3782 822463 10005 end"), "amex [redacted] end");
        assert_eq!(m("visa13 4222222222222 end"), "visa13 [redacted] end");
    }

    #[test]
    fn cards_grouped_with_dots_or_ideographic_spaces_are_masked() {
        assert_eq!(m("card 4111.1111.1111.1111 ok"), "card [redacted] ok");
        assert_eq!(m("card 4111　1111　1111　1111 ok"), "card [redacted] ok");
    }

    #[test]
    fn a_digit_run_that_fails_luhn_is_left_alone() {
        // One digit changed from the Visa test number, so the checksum no longer closes.
        assert_eq!(m("ref 4111111111111112 ok"), "ref 4111111111111112 ok");
        // Too short and too long to be a card at all.
        assert_eq!(m("order 123456789012"), "order 123456789012");
        assert_eq!(m("id 41111111111111111111"), "id 41111111111111111111");
    }

    #[test]
    fn mixed_separators_and_long_groups_are_not_a_card() {
        // Luhn-valid digits, but no card is printed with a 13-digit group or a fraction.
        let float_ts = "measured 1727712000000.000 seconds";
        assert!(luhn_ok(&[1, 7, 2, 7, 7, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(m(float_ts), float_ts);
        let mixed = "ref 4111 1111-1111 1111 ok";
        assert_eq!(m(mixed), mixed);
    }

    #[test]
    fn timestamps_and_ordinary_numbers_survive() {
        assert_eq!(m("built at 1727712000000 utc"), "built at 1727712000000 utc");
        assert_eq!(m("port 8080, pid 4321"), "port 8080, pid 4321");
        assert_eq!(
            m("meeting at 14:30 on 2026-09-29"),
            "meeting at 14:30 on 2026-09-29"
        );
    }

    #[test]
    fn an_ssn_is_masked_but_reserved_ranges_are_not() {
        assert_eq!(m("ssn 123-45-6789 filed"), "ssn [redacted] filed");
        // 666 and 9xx are never issued; 00 group and 0000 serial are invalid.
        assert_eq!(m("code 666-45-6789 x"), "code 666-45-6789 x");
        assert_eq!(m("code 900-45-6789 x"), "code 900-45-6789 x");
        assert_eq!(m("code 123-00-6789 x"), "code 123-00-6789 x");
        assert_eq!(m("code 123-45-0000 x"), "code 123-45-0000 x");
    }

    #[test]
    fn an_ssn_grouped_with_spaces_or_dots_is_the_same_shape() {
        assert_eq!(m("ssn 123 45 6789 filed"), "ssn [redacted] filed");
        assert_eq!(m("payroll lists 123 45 6789 for Dana"), "payroll lists [redacted] for Dana");
        assert_eq!(m("payroll lists 123.45.6789 for Dana"), "payroll lists [redacted] for Dana");
        // The sentence may end right after it.
        assert_eq!(m("number is 123 45 6789"), "number is [redacted]");
        assert_eq!(m("number is 123 45 6789."), "number is [redacted].");
        // The same range checks as the hyphen form.
        assert_eq!(m("code 666 45 6789 x"), "code 666 45 6789 x");
        assert_eq!(m("code 123.00.6789 x"), "code 123.00.6789 x");
    }

    #[test]
    fn a_column_of_numbers_is_not_an_ssn() {
        // A grouped match that continues a run of numbers on either side is a table row.
        let after = "totals 123 45 6789 10 end";
        assert_eq!(m(after), after);
        let before = "totals 2026 123 45 6789 end";
        assert_eq!(m(before), before);
        let dotted = "v1.123.45.6789 build";
        assert_eq!(m(dotted), dotted);
        // Mixed separators are not one shape.
        let mixed = "code 123-45 6789 x";
        assert_eq!(m(mixed), mixed);
    }

    #[test]
    fn a_longer_digit_run_is_not_read_as_an_ssn() {
        assert_eq!(m("part 123-45-67891 x"), "part 123-45-67891 x");
        assert_eq!(m("part 123-45-6789-1 x"), "part 123-45-6789-1 x");
    }

    #[test]
    fn labelled_documents_are_masked_and_the_label_is_kept() {
        // The label survives so the trace still says what was removed.
        assert_eq!(m("passport: X1234567 issued"), "passport: [redacted] issued");
        assert_eq!(m("マイナンバー 123456789012"), "マイナンバー [redacted]");
        assert_eq!(m("個人番号：123456789012"), "個人番号：[redacted]");
        assert_eq!(m("旅券番号 TR9988776"), "旅券番号 [redacted]");
        assert_eq!(m("Individual Number: 123456789012"), "Individual Number: [redacted]");
        assert_eq!(m("individual_number=123456789012"), "individual_number=[redacted]");
    }

    #[test]
    fn the_printed_my_number_grouping_is_masked_after_a_label() {
        assert_eq!(m("マイナンバー 1234 5678 9012"), "マイナンバー [redacted]");
        assert_eq!(m("マイナンバー 1234 5678 9012 です"), "マイナンバー [redacted] です");
        assert_eq!(m("個人番号: 1234-5678-9012"), "個人番号: [redacted]");
        assert_eq!(m("My Number 1234 5678 9012 ok"), "My Number [redacted] ok");
        // Without the label the grouping is just three numbers.
        let bare = "rows 1234 5678 9012 end";
        assert_eq!(m(bare), bare);
        // Four groups are a longer run, not the 12-digit document.
        let longer = "マイナンバー 1234 5678 9012 3456";
        assert_eq!(m(longer), longer);
    }

    #[test]
    fn a_label_without_a_number_shaped_value_is_prose() {
        assert_eq!(
            m("passport application review"),
            "passport application review"
        );
        assert_eq!(m("passport photos"), "passport photos");
    }

    #[test]
    fn kana_and_kanji_neighbours_are_boundaries() {
        // Japanese does not space a number off from the words around it.
        assert_eq!(m("私のマイナンバー123456789012です"), "私のマイナンバー[redacted]です");
        assert_eq!(m("申請者個人番号：123456789012"), "申請者個人番号：[redacted]");
        assert_eq!(m("カード番号4111111111111111です"), "カード番号[redacted]です");
        assert_eq!(m("社会保障番号123-45-6789です"), "社会保障番号[redacted]です");
        assert_eq!(m("社会保障番号123 45 6789です"), "社会保障番号[redacted]です");
    }

    #[test]
    fn ascii_neighbours_still_glue_onto_a_number() {
        // The tail of a longer alphanumeric identifier is not a card, an SSN or a label.
        let card = "x4111111111111111 y";
        assert_eq!(m(card), card);
        let suffixed = "4111111111111111x y";
        assert_eq!(m(suffixed), suffixed);
        let ssn = "ref123-45-6789 y";
        assert_eq!(m(ssn), ssn);
        let label = "lessn 123456789012";
        assert_eq!(m(label), label);
        let inner = "ab4111111111111111cd";
        assert_eq!(m(inner), inner);
    }

    #[test]
    fn full_width_digits_are_scanned_as_ascii_and_masked() {
        assert_eq!(m("カード番号４１１１１１１１１１１１１１１１です"), "カード番号[redacted]です");
        assert_eq!(m("マイナンバー　１２３４５６７８９０１２"), "マイナンバー　[redacted]");
        assert_eq!(m("ssn １２３-４５-６７８９ filed"), "ssn [redacted] filed");
        // A mixed-width card still adds up to one number.
        assert_eq!(m("card 4111 １１１１ 1111 1111 ok"), "card [redacted] ok");
    }

    #[test]
    fn unmasked_full_width_text_comes_back_exactly_as_written() {
        // The folding is for the scan only: a phone number, a date and the ideographic space are
        // the user's text and leave untouched and unallocated.
        let phone = "電話　０３－１２３４－５６７８　まで";
        assert!(matches!(mask_high_risk_identifiers(phone), Cow::Borrowed(_)));
        assert_eq!(m(phone), phone);
        // And around a match, the unmasked part keeps its original width.
        assert_eq!(
            m("日付２０２６年 card 4111111111111111 金額１０００円"),
            "日付２０２６年 card [redacted] 金額１０００円"
        );
    }

    #[test]
    fn text_with_nothing_to_mask_is_returned_borrowed() {
        assert!(matches!(
            mask_high_risk_identifiers("no identifiers here at all"),
            Cow::Borrowed(_)
        ));
        // Digits alone trigger the scan but must not force an allocation when nothing matches.
        assert!(matches!(
            mask_high_risk_identifiers("port 8080"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn multibyte_text_around_a_match_is_preserved() {
        let got = m("請求書の card 4111111111111111 を確認してください");
        assert!(got.contains("請求書の"), "{got}");
        assert!(got.contains("を確認してください"), "{got}");
        assert!(!got.contains("4111"), "{got}");
    }

    #[test]
    fn several_identifiers_in_one_string_are_all_masked() {
        let got = m("card 4111111111111111 and ssn 123-45-6789 and passport: X1234567");
        assert_eq!(got.matches("[redacted]").count(), 3, "{got}");
        assert!(!got.contains("4111"), "{got}");
        assert!(!got.contains("6789"), "{got}");
        assert!(!got.contains("X1234567"), "{got}");
    }
}
