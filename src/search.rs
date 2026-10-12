//! Search over the mail database: the index side here, the query side and the
//! UI that drives it in the next layer up.
//!
//! FTS5's `unicode61` tokenizer splits on punctuation and whitespace, which is
//! exactly wrong for Chinese, Japanese and Korean mail: a run of Han characters
//! has no spaces at all, so a whole sentence becomes one token and searching
//! for two of its characters finds nothing. The `messages.cjk` column
//! therefore holds a token stream the tokenizer *can* cut up: every wide
//! character on its own, plus every adjacent pair. That is the classic bigram
//! trick (Lucene's `CJKBigramFilter`), and it makes a two-character query
//! exact without a tokenizer plugin, without `unsafe`, and without the index
//! bloat of a trigram tokenizer.

use crate::models::MailMessage;

/// Writing systems whose text has no word spacing — Han, Hiragana, Katakana,
/// Hangul. Punctuation (、。「」) is deliberately outside these ranges: it still
/// separates runs.
fn is_wide(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF        // Hangul Jamo
        | 0x3040..=0x30FF      // Hiragana, Katakana (incl. the prolonged sound mark)
        | 0x3130..=0x318F      // Hangul compatibility Jamo
        | 0x3400..=0x4DBF      // CJK unified ideographs, extension A
        | 0x4E00..=0x9FFF      // CJK unified ideographs
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0x20000..=0x2FA1F    // extensions B…F
    )
}

/// The bigram stream for one text: every wide character on its own plus every
/// adjacent pair, in reading order. "发票号码" becomes
/// "发 票 发票 号 票号 码 号码" — the pairs keep their document order, which is
/// what makes a single-bigram phrase query ("发票") exact.
///
/// A pair never straddles a run boundary, so "发票，号码" has no 票号 in it —
/// which is what keeps the phrase search honest.
pub(crate) fn cjk_index(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if !is_wide(c) {
            previous = None;
            continue;
        }
        tokens.push(c.to_string());
        if let Some(previous) = previous {
            tokens.push(format!("{previous}{c}"));
        }
        previous = Some(c);
    }
    tokens.join(" ")
}

/// The `messages.cjk` column of one message: the bigram stream of every field
/// a CJK search should reach. Building it here rather than in SQL is the whole
/// reason the column is a stored column.
pub(crate) fn cjk_column(message: &MailMessage) -> String {
    cjk_index(&format!(
        "{} {} {} {}",
        message.subject, message.from, message.to, message.body_text
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_runs_become_unigrams_and_bigrams() {
        assert_eq!(cjk_index("发票号码"), "发 票 发票 号 票号 码 号码");
        // A single character is still searchable, on its own.
        assert_eq!(cjk_index("票"), "票");
        assert_eq!(cjk_index(""), "");
        assert_eq!(cjk_index("plain ascii"), "");
    }

    #[test]
    fn runs_never_share_a_bigram_across_punctuation() {
        // The comma separates the runs: no 票号 must be indexed here.
        assert_eq!(cjk_index("发票，号码"), "发 票 发票 号 码 号码");
        assert_eq!(cjk_index("发票 号码"), "发 票 发票 号 码 号码");
        assert_eq!(cjk_index("发票(号码)"), "发 票 发票 号 码 号码");
        // The pair that would span the boundary must not be indexed.
        assert!(!cjk_index("发票，号码").contains("票号"));
    }

    #[test]
    fn latin_and_digits_are_left_to_the_word_index() {
        assert_eq!(cjk_index("invoice 2026 发票"), "发 票 发票");
        assert_eq!(
            cjk_index("ＡＢＣ"),
            "",
            "full-width Latin is not a wide script"
        );
    }

    #[test]
    fn kana_and_hangul_get_the_same_treatment() {
        assert_eq!(cjk_index("ひらがな"), "ひ ら ひら が らが な がな");
        assert_eq!(cjk_index("한글"), "한 글 한글");
    }

    #[test]
    fn the_column_covers_subject_addresses_and_body() {
        let mut message = crate::models::MailMessage {
            uid: "1".to_string(),
            subject: "发票号码".to_string(),
            from: "财务 <caiwu@nroad.com.cn>".to_string(),
            to: "我 <me@example.com>".to_string(),
            date: String::new(),
            timestamp: None,
            seen: false,
            body_text: "请确认号码".to_string(),
            body_html: None,
            preview: String::new(),
            attachment_names: Vec::new(),
            inline_images: Vec::new(),
        };
        let column = cjk_column(&message);
        for bigram in ["发票", "票号", "号码", "确认", "请确"] {
            assert!(column.contains(bigram), "{bigram} missing from {column:?}");
        }
        message.body_text = String::new();
        assert!(
            cjk_column(&message).contains("发票"),
            "the subject alone is indexed"
        );
    }
}
