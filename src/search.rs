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

use crate::error::MailResult;
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

/// How many hits one search returns; the UI says so when the cap is reached.
pub const SEARCH_LIMIT: usize = 200;

/// What a search covers: everything, one account, or the folder on screen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Scope {
    AllMail,
    Account(String),
    Folder { account_id: String, folder: String },
}

impl Scope {
    /// The scope's `WHERE` fragment and its parameters.
    fn filter(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        match self {
            Scope::AllMail => (String::new(), Vec::new()),
            Scope::Account(account_id) => (
                "m.account_id = ?".to_string(),
                vec![Value::Text(account_id.clone())],
            ),
            Scope::Folder { account_id, folder } => (
                "m.account_id = ? AND m.folder = ?".to_string(),
                vec![Value::Text(account_id.clone()), Value::Text(folder.clone())],
            ),
        }
    }
}

/// One search: the free text plus every filter the search bar offers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchQuery {
    /// Free text: words, `"quoted phrases"`, Chinese runs.
    pub text: String,
    pub scope: Scope,
    /// Substring filters, matched against the display form of the addresses
    /// (`LIKE %term%`) — "peng" finds `zhoupen9@sina.cn`, which the word
    /// index cannot.
    pub from: String,
    pub to: String,
    pub subject: String,
    /// `YYYY-MM-DD`, read as UTC midnight — the same day the list shows.
    pub after: String,
    pub before: String,
    pub unread_only: bool,
    pub has_attachment: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            scope: Scope::AllMail,
            from: String::new(),
            to: String::new(),
            subject: String::new(),
            after: String::new(),
            before: String::new(),
            unread_only: false,
            has_attachment: false,
        }
    }
}

impl SearchQuery {
    /// Whether anything is being asked for. Until it is, the middle column
    /// shows the selected folder as usual; a text box holding only punctuation
    /// asks for nothing.
    pub fn is_active(&self) -> bool {
        fts_expression(&self.text).is_some()
            || !self.from.trim().is_empty()
            || !self.to.trim().is_empty()
            || !self.subject.trim().is_empty()
            || !self.after.trim().is_empty()
            || !self.before.trim().is_empty()
            || self.unread_only
            || self.has_attachment
    }

    /// The date bounds the query filters on: `after` is that day's midnight,
    /// `before` the end of that day. A text field that is not a date is
    /// simply ignored (the UI says nothing about it either).
    fn bounds(&self) -> (Option<i64>, Option<i64>) {
        (
            parse_date(&self.after),
            parse_date(&self.before).map(|day| day + 86_400 - 1),
        )
    }
}

/// One result row: enough to draw the card and to open the mail, plus a window
/// of the body around the match.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub account_id: String,
    pub folder: String,
    pub uid: String,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub timestamp: Option<i64>,
    pub seen: bool,
    /// How many attachments the mail has (0 for none).
    pub attachments: usize,
    pub snippet: String,
}

/// `YYYY-MM-DD` → unix seconds at UTC midnight, the inverse of the civil-date
/// arithmetic the message list formats with.
pub fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    let (year, rest) = text.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    let year: i64 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    let day: u32 = day.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400)
}

/// (year, month, day) → days since the epoch (Howard Hinnant's days_from_civil).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A `LIKE '%term%'` pattern for a substring filter, with the wildcards
/// neutralized, or `None` for a blank field.
fn like_pattern(value: &str) -> Option<String> {
    let term = value.trim();
    if term.is_empty() {
        return None;
    }
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    Some(format!("%{escaped}%"))
}

/// The columns a word is matched in. FTS5 has no parentheses, so a column
/// *set* like this is how one clause can cover several columns at once.
const TEXT_COLUMNS: &str = "subject from_addr to_addr body_text";

/// One piece of the free text: a run of wide characters, or a run of
/// everything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// CJK — matched as bigrams in the `cjk` column.
    Wide(String),
    /// Anything else — matched as a word prefix in the text columns.
    Word(String),
}

impl Term {
    /// The term as the user typed it, lowercased.
    pub fn text(&self) -> &str {
        match self {
            Term::Wide(run) | Term::Word(run) => run,
        }
    }
}

/// The terms of a query: a `"quoted"` run is one term, everything else splits
/// on whitespace, and each unit splits again where the writing system changes
/// ("发票ABC" is one wide run and one word).
pub fn terms(text: &str) -> Vec<Term> {
    let mut out = Vec::new();
    let mut unit = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                if !quoted {
                    flush_unit(&mut unit, &mut out);
                }
            }
            c if c.is_whitespace() && !quoted => flush_unit(&mut unit, &mut out),
            c => unit.push(c),
        }
    }
    flush_unit(&mut unit, &mut out);
    out
}

fn flush_unit(unit: &mut String, out: &mut Vec<Term>) {
    if !unit.trim().is_empty() {
        split_runs(unit, out);
    }
    unit.clear();
}

/// Split one unit into runs of wide and non-wide characters, dropping
/// punctuation-only runs: a query of "---" asks for nothing.
fn split_runs(unit: &str, out: &mut Vec<Term>) {
    let mut run = String::new();
    let mut wide = None;
    for c in unit.chars() {
        let is_wide = is_wide(c);
        if wide == Some(is_wide) {
            run.push(c);
            continue;
        }
        if let Some(was_wide) = wide {
            push_run(was_wide, &run, out);
        }
        run.clear();
        wide = Some(is_wide);
        run.push(c);
    }
    if let Some(was_wide) = wide {
        push_run(was_wide, &run, out);
    }
}

fn push_run(wide: bool, run: &str, out: &mut Vec<Term>) {
    let trimmed = run.trim_matches(|c: char| !c.is_alphanumeric() && !is_wide(c));
    if trimmed.is_empty() {
        return;
    }
    let lowered = trimmed.to_lowercase();
    out.push(if wide {
        Term::Wide(lowered)
    } else {
        Term::Word(lowered)
    });
}

/// The searchable units of a wide run: the character itself when it stands
/// alone, otherwise every adjacent pair.
fn cjk_grams(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() < 2 {
        return chars.iter().map(|c| c.to_string()).collect();
    }
    chars.windows(2).map(|pair| pair.iter().collect()).collect()
}

/// A user word as an FTS5 phrase. The only character with a meaning inside the
/// quotes is the quote itself, and FTS5 escapes that by doubling it.
fn quote_word(word: &str) -> String {
    format!("\"{}\"", word.replace('"', "\"\""))
}

/// The FTS5 `MATCH` expression for the free text, or `None` when there is
/// nothing to match (empty, or punctuation only). Every word of the user's is
/// quoted, so query text can never be read as FTS5 syntax.
///
/// A word becomes a *prefix* phrase, so "invoice" also finds "invoices"; a wide
/// run becomes one phrase per bigram, AND-ed — a single phrase listing all of a
/// run's pairs would have to find them adjacent in the index, and they are not
/// (each character sits between two of its pairs).
pub fn fts_expression(text: &str) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    for term in terms(text) {
        match term {
            Term::Word(word) => {
                clauses.push(format!("{{{TEXT_COLUMNS}}}: {} *", quote_word(&word)));
            }
            Term::Wide(run) => {
                for gram in cjk_grams(&run) {
                    clauses.push(format!("{{cjk}}: {}", quote_word(&gram)));
                }
            }
        }
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

/// Run a search. The caller holds the database connection (see `db::db`) for
/// the duration; this function touches nothing else.
pub fn run(conn: &rusqlite::Connection, query: &SearchQuery) -> MailResult<Vec<SearchHit>> {
    use rusqlite::types::Value;

    // A query that asks for nothing returns nothing: the UI keeps the folder
    // view for these, and an unfiltered dump of the whole database is never
    // what a search bar means.
    if !query.is_active() {
        return Ok(Vec::new());
    }
    let expression = fts_expression(&query.text);
    let mut sql = String::from(
        "SELECT m.account_id, m.folder, m.uid, m.subject, m.from_addr, m.date_header,
                m.ts, m.seen, m.body_text, m.preview,
                (SELECT COUNT(*) FROM attachments a WHERE a.message_id = m.id)
           FROM messages m",
    );
    if expression.is_some() {
        // The FTS table is never aliased: MATCH needs its real name.
        sql.push_str(" JOIN messages_fts ON messages_fts.rowid = m.id");
    }

    let mut params: Vec<Value> = Vec::new();
    let mut filters: Vec<String> = Vec::new();
    let (scope, scope_params) = query.scope.filter();
    if !scope.is_empty() {
        filters.push(scope);
        params.extend(scope_params);
    }
    for (column, field) in [
        ("m.from_addr", &query.from),
        ("m.to_addr", &query.to),
        ("m.subject", &query.subject),
    ] {
        if let Some(pattern) = like_pattern(field) {
            filters.push(format!("{column} LIKE ? ESCAPE '\\'"));
            params.push(Value::Text(pattern));
        }
    }
    let (after, before) = query.bounds();
    if let Some(after) = after {
        filters.push("m.ts >= ?".to_string());
        params.push(Value::Integer(after));
    }
    if let Some(before) = before {
        filters.push("m.ts <= ?".to_string());
        params.push(Value::Integer(before));
    }
    if query.unread_only {
        filters.push("m.seen = 0".to_string());
    }
    if query.has_attachment {
        filters.push("EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id)".to_string());
    }
    if let Some(expression) = &expression {
        filters.push("messages_fts MATCH ?".to_string());
        params.push(Value::Text(expression.clone()));
    }
    if !filters.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&filters.join(" AND "));
    }
    if expression.is_some() {
        // Relevance first, newest first among equally relevant mail. Without a
        // text query there is no relevance to rank by.
        sql.push_str(" ORDER BY bm25(messages_fts), m.ts DESC, m.id DESC");
    } else {
        sql.push_str(" ORDER BY m.ts DESC, m.id DESC");
    }
    sql.push_str(&format!(" LIMIT {SEARCH_LIMIT}"));

    let terms = terms(&query.text);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, i64>(10)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (
            account_id,
            folder,
            uid,
            subject,
            from,
            date,
            timestamp,
            seen,
            body,
            preview,
            attachments,
        ) = row?;
        hits.push(SearchHit {
            account_id,
            folder,
            uid,
            subject,
            from,
            date,
            timestamp,
            seen: seen != 0,
            attachments: attachments.max(0) as usize,
            snippet: snippet(&terms, &body, &preview),
        });
    }
    Ok(hits)
}

/// A window of the body around the first matched term, or the message's own
/// preview when nothing matches (mail found through a filter, or a term that
/// only occurs in an address).
pub fn snippet(terms: &[Term], body: &str, preview: &str) -> String {
    /// Characters of context on each side of the match.
    const BESIDE: usize = 50;
    /// Longest snippet, in characters.
    const MAX: usize = 120;

    let flat = collapse(body);
    let length = flat.chars().count();
    for term in terms {
        let needle = term.text();
        if let Some((start, end)) = find_ci(&flat, needle) {
            let from = flat[..start].chars().count().saturating_sub(BESIDE);
            let to = (flat[..end].chars().count() + BESIDE).min(length);
            let mut out = String::new();
            if from > 0 {
                out.push('…');
            }
            out.extend(flat.chars().skip(from).take(to - from));
            if to < length {
                out.push('…');
            }
            return out.chars().take(MAX + 1).collect();
        }
    }
    let fallback = if preview.trim().is_empty() {
        flat
    } else {
        preview.trim().to_string()
    };
    fallback.chars().take(MAX).collect()
}

/// Body text on one line: mail is full of newlines and hard wrapping.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The byte range of the first case-insensitive occurrence of `needle` in
/// `haystack`. Neither side is lowercased into a copy, because a lowercased
/// copy can be longer than its input and its offsets would not belong to
/// `haystack` any more.
fn find_ci(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let mut expected = needle.chars();
    let head = expected.next()?;
    for (start, c) in haystack.char_indices() {
        if !same_char(c, head) {
            continue;
        }
        let mut end = start + c.len_utf8();
        let mut matched = true;
        for expected in needle.chars().skip(1) {
            let Some(next) = haystack[end..].chars().next() else {
                matched = false;
                break;
            };
            if !same_char(next, expected) {
                matched = false;
                break;
            }
            end += next.len_utf8();
        }
        if matched {
            return Some((start, end));
        }
    }
    None
}

fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
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

    // ----- end to end, against a real database -------------------------------

    use crate::db::test_support::{save, save_as, temp_db};

    fn message(uid: &str, subject: &str, body: &str) -> MailMessage {
        MailMessage {
            uid: uid.to_string(),
            subject: subject.to_string(),
            from: "Alice <alice@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "Mon, 1 Jan 2026 00:00:00 +0000".to_string(),
            timestamp: Some(1_767_225_600),
            seen: false,
            body_text: body.to_string(),
            body_html: None,
            preview: body.chars().take(20).collect(),
            attachment_names: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    /// Run a query through the real code path.
    fn search(conn: &rusqlite::Connection, query: SearchQuery) -> Vec<SearchHit> {
        run(conn, &query).expect("search")
    }

    fn uids(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|hit| hit.uid.clone()).collect()
    }

    /// The uids of a result, sorted: relevance order is not what a scope test
    /// is about.
    fn sorted_uids(hits: &[SearchHit]) -> Vec<String> {
        let mut uids = uids(hits);
        uids.sort();
        uids
    }

    #[test]
    fn search_reaches_subject_body_sender_and_chinese() {
        let (mut conn, dir) = temp_db("search-fields");
        let mut chinese = message("1", "发票号码确认", "本月的发票已经开好，请确认号码");
        chinese.from = "财务 <caiwu@nroad.com.cn>".to_string();
        let english = message("2", "Invoice for August", "Your invoices are ready");
        save(&mut conn, "INBOX", Some(1), &[chinese, english]).unwrap();

        assert_eq!(
            uids(&search(&conn, query("invoi"))),
            ["2"],
            "a subject word, by prefix"
        );
        assert_eq!(
            uids(&search(&conn, query("ready"))),
            ["2"],
            "a word in the body"
        );
        assert_eq!(
            uids(&search(&conn, query("caiwu"))),
            ["1"],
            "a word in the sender"
        );
        assert_eq!(
            uids(&search(&conn, query("发票"))),
            ["1"],
            "a two-character Chinese run"
        );
        assert_eq!(
            uids(&search(&conn, query("确认号码"))),
            ["1"],
            "a Chinese run, in pieces"
        );
        assert!(
            search(&conn, query("票发")).is_empty(),
            "the pairs are not reversed"
        );
        assert!(search(&conn, query("nothing here")).is_empty());
        assert!(
            search(&conn, query("---")).is_empty(),
            "punctuation asks for nothing"
        );
        assert!(!query("---").is_active());
        assert!(!SearchQuery::default().is_active());
        assert!(query("发票").is_active());
        let filtered = SearchQuery {
            unread_only: true,
            ..SearchQuery::default()
        };
        assert!(filtered.is_active(), "a filter alone is a search");
        fs_remove(&dir);
    }

    #[test]
    fn search_is_scoped_to_an_account_or_a_folder() {
        let (mut conn, dir) = temp_db("search-scope");
        save(
            &mut conn,
            "INBOX",
            Some(1),
            &[message("1", "Report", "a report")],
        )
        .unwrap();
        save(
            &mut conn,
            "Trash",
            Some(1),
            &[message("2", "Report", "another report")],
        )
        .unwrap();
        save_as(
            &mut conn,
            "other",
            "INBOX",
            Some(1),
            &[message("3", "Report", "someone else's report")],
        )
        .unwrap();

        let all_mail = query("report");
        assert_eq!(
            sorted_uids(&search(&conn, all_mail.clone())),
            ["1", "2", "3"],
            "every account"
        );
        let one_account = SearchQuery {
            scope: Scope::Account("work".to_string()),
            ..all_mail.clone()
        };
        assert_eq!(sorted_uids(&search(&conn, one_account)), ["1", "2"]);
        let one_folder = SearchQuery {
            scope: Scope::Folder {
                account_id: "work".to_string(),
                folder: "Trash".to_string(),
            },
            ..all_mail
        };
        assert_eq!(uids(&search(&conn, one_folder)), ["2"]);
        fs_remove(&dir);
    }

    /// The tests own a temp directory each; this keeps the cleanup in one line.
    fn fs_remove(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_filters_narrow_by_field_date_flag_and_attachment() {
        let (mut conn, dir) = temp_db("search-filters");
        let mut old = message("1", "Weekly report", "the oldest report");
        old.timestamp = Some(1_000);
        old.from = "Boss <boss@example.com>".to_string();
        let mut new = message("2", "Weekly report", "the newest report");
        new.timestamp = Some(200_000); // 1970-01-03, two days after the other one
        new.seen = true;
        new.attachment_names = vec!["report.pdf".to_string()];
        save(&mut conn, "INBOX", Some(1), &[old, new]).unwrap();

        let by_sender = SearchQuery {
            from: "boss".into(),
            ..query("report")
        };
        assert_eq!(uids(&search(&conn, by_sender)), ["1"], "sender substring");
        let by_subject = SearchQuery {
            subject: "weekly".into(),
            ..query("report")
        };
        assert_eq!(sorted_uids(&search(&conn, by_subject)), ["1", "2"]);
        let unread = SearchQuery {
            unread_only: true,
            ..query("report")
        };
        assert_eq!(uids(&search(&conn, unread)), ["1"]);
        let with_parts = SearchQuery {
            has_attachment: true,
            ..query("report")
        };
        assert_eq!(uids(&search(&conn, with_parts)), ["2"]);
        let first_day = SearchQuery {
            after: "1970-01-01".into(),
            before: "1970-01-01".into(),
            ..query("report")
        };
        assert_eq!(
            uids(&search(&conn, first_day)),
            ["1"],
            "the window keeps the older mail"
        );
        let third_day = SearchQuery {
            after: "1970-01-03".into(),
            ..query("report")
        };
        assert_eq!(uids(&search(&conn, third_day)), ["2"], "and the newer one");

        // A filter with no text is a search too, and then the date orders it.
        let recent = SearchQuery {
            subject: "report".into(),
            ..SearchQuery::default()
        };
        assert_eq!(
            uids(&search(&conn, recent)),
            ["2", "1"],
            "newest first without a query"
        );
        fs_remove(&dir);
    }

    #[test]
    fn the_better_match_comes_first() {
        let (mut conn, dir) = temp_db("search-rank");
        save(
            &mut conn,
            "INBOX",
            Some(1),
            &[
                message("once", "Report", "one report here"),
                message("thrice", "Report", "report report report everywhere"),
            ],
        )
        .unwrap();
        assert_eq!(uids(&search(&conn, query("report"))), ["thrice", "once"]);
        fs_remove(&dir);
    }

    #[test]
    fn a_search_stops_at_the_limit() {
        let (mut conn, dir) = temp_db("search-limit");
        let batch: Vec<MailMessage> = (0..SEARCH_LIMIT + 5)
            .map(|index| message(&index.to_string(), "bulk", "a message"))
            .collect();
        save(&mut conn, "INBOX", Some(1), &batch).unwrap();
        assert_eq!(search(&conn, query("bulk")).len(), SEARCH_LIMIT);
        fs_remove(&dir);
    }

    #[test]
    fn quotes_and_wildcards_in_the_query_are_literal() {
        let (mut conn, dir) = temp_db("search-quotes");
        save(
            &mut conn,
            "INBOX",
            Some(1),
            &[message("1", "He said \"hello\" loudly", "the body")],
        )
        .unwrap();
        assert_eq!(
            uids(&search(&conn, query("\"hello\""))),
            ["1"],
            "quotes group one term"
        );
        assert_eq!(uids(&search(&conn, query("hello"))), ["1"]);

        let wild = SearchQuery {
            from: "100%_x".into(),
            ..SearchQuery::default()
        };
        assert!(
            search(&conn, wild).is_empty(),
            "a percent is a literal, not a wildcard"
        );
        fs_remove(&dir);
    }

    #[test]
    fn query_terms_split_words_from_wide_runs() {
        assert_eq!(
            terms("发票ABC"),
            vec![Term::Wide("发票".into()), Term::Word("abc".into())]
        );
        assert_eq!(
            terms("Invoice 2026"),
            vec![Term::Word("invoice".into()), Term::Word("2026".into())]
        );
        // Quotes group one term, spaces and all.
        assert_eq!(terms("\"two words\""), vec![Term::Word("two words".into())]);
        // Punctuation-only input asks for nothing.
        assert_eq!(terms("--- ..."), Vec::<Term>::new());
        assert_eq!(terms(""), Vec::<Term>::new());
    }

    #[test]
    fn words_become_prefix_phrases_and_wide_runs_become_bigrams() {
        assert_eq!(
            fts_expression("invoi"),
            Some("{subject from_addr to_addr body_text}: \"invoi\" *".to_string())
        );
        assert_eq!(fts_expression("发票"), Some("{cjk}: \"发票\"".to_string()));
        assert_eq!(
            fts_expression("发票号码"),
            Some("{cjk}: \"发票\" AND {cjk}: \"票号\" AND {cjk}: \"号码\"".to_string())
        );
        // One character is a unigram, because a run of one has no pair.
        assert_eq!(fts_expression("票"), Some("{cjk}: \"票\"".to_string()));
        assert_eq!(fts_expression("---"), None);
        assert_eq!(fts_expression("   "), None);
    }

    #[test]
    fn query_text_cannot_become_fts_syntax() {
        // FTS5 operators are just words to us, and every word is quoted.
        let expression = fts_expression("NEAR( OR body:x").expect("terms");
        assert!(!expression.contains("NEAR( OR"));
        assert!(expression.contains("\"near\" *"));
        // A quote inside a word cannot close the phrase it is wrapped in.
        let expression = fts_expression("in\"voice").expect("terms");
        assert_eq!(
            expression.matches('"').count(),
            2,
            "one phrase, one pair of quotes"
        );
    }

    #[test]
    fn date_fields_parse_to_the_day_bounds() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-01-01"), Some(1_767_225_600));
        assert_eq!(parse_date(" 2026-01-01 "), Some(1_767_225_600));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-01-32"), None);
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(parse_date(""), None);

        let mut query = SearchQuery::default();
        assert_eq!(query.bounds(), (None, None));
        query.after = "2026-01-01".into();
        query.before = "2026-01-02".into();
        assert_eq!(
            query.bounds(),
            (Some(1_767_225_600), Some(1_767_225_600 + 2 * 86_400 - 1)),
            "the before bound is the end of the day it names"
        );
    }

    #[test]
    fn substring_filters_escape_the_like_wildcards() {
        assert_eq!(like_pattern("  peng  "), Some("%peng%".to_string()));
        assert_eq!(like_pattern("   "), None);
        assert_eq!(
            like_pattern("50%_x\\y"),
            Some("%50\\%\\_x\\\\y%".to_string())
        );
    }

    #[test]
    fn snippets_show_the_match_with_context() {
        let body = format!("{}发票号码确认{}", "开头".repeat(60), "结尾".repeat(60));
        let window = snippet(&terms("发票"), &body, "preview");
        assert!(window.contains("发票号码"), "{window}");
        assert!(window.starts_with('…') && window.ends_with('…'), "{window}");
        assert!(
            window.chars().count() <= 121,
            "{} chars",
            window.chars().count()
        );

        // No match anywhere: the message's own preview is the best we have.
        assert_eq!(
            snippet(&terms("nothing"), "a body", "the preview"),
            "the preview"
        );
        assert_eq!(snippet(&[], "a body", "the preview"), "the preview");
        // Case does not matter, and neither does hard wrapping in the body.
        assert_eq!(
            snippet(&terms("HELLO"), "say hello\nthere", "p"),
            "say hello there"
        );
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
