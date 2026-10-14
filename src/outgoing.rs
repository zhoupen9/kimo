//! The mail being sent: the compose card's draft as a worker needs it, and the
//! RFC 5322 bytes it becomes.
//!
//! `mail.rs` is the other direction — raw bytes into a `MailMessage` — and the
//! two share the calendar arithmetic (`mail::civil_from_days`). What is here is
//! the half the app could get wrong without noticing, because these headers are
//! *built* rather than parsed: every field has to be checked for what a header
//! cannot carry (a CR or an LF), and the body has to survive every server
//! between here and the recipient.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;

/// A mail being sent: what the compose card holds, as the worker needs it.
/// Everything in it is already parsed — the addresses have been through
/// `parse_addresses` — and `render` is what turns it into bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The account's display name. Empty is a `From` with no name on it.
    pub from_name: String,
    /// The `From` address: the account's username, which the card refuses to
    /// send from when it is not one.
    pub from_addr: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Blind copies. They go in the SMTP envelope and nowhere else: no `Bcc:`
    /// header, so nobody who receives the mail can see them.
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Outgoing {
    /// Every address the envelope gets, in the order the fields name them.
    pub fn recipients(&self) -> Vec<&str> {
        self.to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .map(String::as_str)
            .collect()
    }

    /// The entries that must not be sent: anything that is not an address. The
    /// card refuses to send while this is not empty, and the worker checks it
    /// again rather than trusting the card.
    pub fn invalid(&self) -> Vec<&str> {
        self.to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .filter(|addr| !is_address(addr))
            .map(String::as_str)
            .collect()
    }
}

/// The addresses in one To / Cc / Bcc field, in the order they were typed.
///
/// Both separators are in use — a comma is the RFC's, a semicolon is what
/// Outlook's users type — so both split. A `Name <addr>` entry keeps the
/// address and drops the display name: the address is what a header carries,
/// and the name is in the reader's address book anyway. A CR or an LF is
/// replaced rather than kept, so nothing here can end a header line; whether
/// what is left is an address at all is `is_address`'s question.
pub fn parse_addresses(text: &str) -> Vec<String> {
    text.split([',', ';'])
        .map(|entry| {
            let entry = entry.trim();
            // A display name may itself contain angle brackets, so the address
            // is what the *last* pair encloses.
            match (entry.rfind('<'), entry.rfind('>')) {
                (Some(open), Some(close)) if close > open => entry[open + 1..close].trim(),
                _ => entry,
            }
        })
        .map(|entry| entry.replace(['\r', '\n'], " "))
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Whether one entry can be an address: a local part, one `@`, a domain, and
/// nothing else. A display name that never came in angle brackets, a list that
/// was not split, a CR or LF, a space — all of them are refused here, which is
/// what keeps a typed field from becoming a header of its own.
///
/// Deliberately structural rather than exhaustive: a domain with no dot
/// (`root@localhost`) is legal and a local relay needs it, so the test is
/// "could a server read this as an address", not "would a public provider take
/// it".
pub fn is_address(addr: &str) -> bool {
    if addr.is_empty() || addr.len() > 320 {
        return false;
    }
    if addr
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == ',')
    {
        return false;
    }
    let mut parts = addr.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// The mail as RFC 5322 bytes, CRLF line endings and all, ready for SMTP DATA.
///
/// `now` is the `Date:` header's instant in unix seconds, and it feeds the
/// `Message-ID` too: nothing here reads the clock, so a test can pin the whole
/// message.
pub fn render(mail: &Outgoing, now: i64) -> Vec<u8> {
    let mut out = String::new();
    push_header(&mut out, "From", &from_header(mail));
    push_header(&mut out, "To", &mail.to.join(", "));
    if !mail.cc.is_empty() {
        push_header(&mut out, "Cc", &mail.cc.join(", "));
    }
    push_header(&mut out, "Subject", &header_text(&mail.subject));
    push_header(&mut out, "Date", &crate::mail::rfc5322_date(now));
    push_header(&mut out, "Message-ID", &message_id(mail, now));
    push_header(&mut out, "MIME-Version", "1.0");
    push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
    push_header(&mut out, "Content-Transfer-Encoding", "base64");
    // The blank line between the headers and the body, which is the only line
    // in the message that means something by being empty.
    out.push_str("\r\n");
    out.push_str(&body_base64(&mail.body));
    out.into_bytes()
}

/// `Name <addr>`, with the name an encoded word when it cannot be a plain
/// phrase. No name at all is just the address, which is what every reader
/// shows anyway.
fn from_header(mail: &Outgoing) -> String {
    let name = mail.from_name.trim();
    if name.is_empty() {
        mail.from_addr.clone()
    } else {
        format!("{} <{}>", header_text(name), mail.from_addr)
    }
}

/// A header value that is safe to write: plain ASCII is written as it is, and
/// anything else becomes RFC 2047 encoded words. A control character counts as
/// "anything else" — it has no business in a header, and encoding it is the one
/// way to keep it from being one.
fn header_text(text: &str) -> String {
    if text.is_ascii() && !text.chars().any(|c| c.is_control()) {
        return text.to_string();
    }
    encoded_words(text)
}

/// The text as `=?UTF-8?B?…?=` words a decoder joins back into it.
///
/// Each word holds at most `ENCODED_WORD_BYTES` of UTF-8 — the RFC caps a word
/// at 75 characters, and base64 grows 3 bytes into 4 — and they are joined by
/// a fold, which is the one place a decoder drops the CRLF and the space
/// instead of reading them.
fn encoded_words(text: &str) -> String {
    /// 42 bytes of UTF-8 are 56 characters of base64, so `=?UTF-8?B?` (10) +
    /// 56 + `?=` (2) is a 68-character word: inside the 75-character cap with
    /// room for the 9 columns of `Subject: ` before the first one.
    const ENCODED_WORD_BYTES: usize = 42;
    let mut words = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(ENCODED_WORD_BYTES);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        // A single character longer than a whole word: the cap loses to
        // writing the text the user typed.
        if end == 0 {
            end = rest.len();
        }
        let (chunk, tail) = rest.split_at(end);
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes());
        words.push(format!("=?UTF-8?B?{encoded}?="));
        rest = tail;
    }
    words.join("\r\n ")
}

/// One header line, `Name: value`, folded at its spaces when it is long
/// enough to need it — a header line is meant to stay inside the 78 columns
/// every reader assumes.
///
/// A value that already carries folds (a run of encoded words) is written as
/// it is: folding *inside* one would break the base64 it holds, and the words
/// were built to fit.
fn push_header(out: &mut String, name: &str, value: &str) {
    /// Where a line starts folding: 78 columns is the RFC's own suggestion,
    /// and the indent of a folded line is one space.
    const FOLD_AT: usize = 78;
    out.push_str(name);
    out.push_str(": ");
    if !value.contains("\r\n") {
        let mut column = name.len() + 2;
        for (index, word) in value.split(' ').enumerate() {
            if index > 0 {
                if column + 1 + word.len() > FOLD_AT {
                    out.push_str("\r\n ");
                    column = 1;
                } else {
                    out.push(' ');
                    column += 1;
                }
            }
            out.push_str(word);
            column += word.len();
        }
        out.push_str("\r\n");
        return;
    }
    out.push_str(value);
    out.push_str("\r\n");
}

/// The body as base64, 76 columns to a line (RFC 2045's limit), over a body
/// whose line endings are CRLF whatever the card's text box had.
///
/// Base64 is what makes the message 7-bit clean: no server on the way needs
/// `8BITMIME`, none of them re-wraps a line, and a line that happens to start
/// with a dot is no longer special — though the SMTP client stuffs it anyway,
/// because that is the transport's job and not the message's.
fn body_base64(body: &str) -> String {
    let normalized = crlf(body);
    let encoded = base64::engine::general_purpose::STANDARD.encode(normalized.as_bytes());
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 2);
    let bytes = encoded.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let end = (start + 76).min(bytes.len());
        // Base64 is ASCII, so a 76-column cut is always a character boundary.
        out.push_str(&encoded[start..end]);
        out.push_str("\r\n");
        start = end;
    }
    out
}

/// `text` with CRLF line endings: a text box hands over a lone LF, and a stray
/// CR is not something a message body may contain at all.
fn crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                out.push_str("\r\n");
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => out.push_str("\r\n"),
            other => out.push(other),
        }
    }
    out
}

/// A `Message-ID` without a random-number crate: a process-wide counter (two
/// mails written in the same second still differ), a digest of what the mail
/// is, and the sender's own domain, so the id looks like every other one on the
/// wire.
fn message_id(mail: &Outgoing, now: i64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(now.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(count.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(mail.from_addr.as_bytes());
    hasher.update(b"\0");
    hasher.update(mail.subject.as_bytes());
    for addr in mail.recipients() {
        hasher.update(addr.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.digest().to_string();
    let domain = mail
        .from_addr
        .split_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.is_empty())
        .unwrap_or("localhost");
    format!("<kimo.{count}.{}@{domain}>", &digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail() -> Outgoing {
        Outgoing {
            from_name: "Ada Lovelace".to_string(),
            from_addr: "ada@example.com".to_string(),
            to: vec!["bob@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Hello".to_string(),
            body: "Hi there!\n\nBye.\n".to_string(),
        }
    }

    /// The decoded body of a rendered message, for comparing with what was
    /// typed.
    fn decode(body: &str) -> String {
        let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .expect("the body is base64");
        String::from_utf8(bytes).expect("the body is utf-8")
    }

    #[test]
    fn an_address_is_a_local_part_a_domain_and_nothing_else() {
        assert!(is_address("ada@example.com"));
        assert!(is_address("ada+tag@sub.example.co.uk"));
        assert!(is_address("root@localhost"), "a local relay has no dot");
        assert!(!is_address(""));
        assert!(!is_address("ada"));
        assert!(!is_address("ada@"));
        assert!(!is_address("@example.com"));
        assert!(!is_address("ada@@example.com"), "one at sign, not two");
        assert!(
            !is_address("ada@example.com, bob@example.com"),
            "a list is not one"
        );
        assert!(
            !is_address("Ada Lovelace <ada@example.com>"),
            "parsed first"
        );
        assert!(!is_address(".ada@example.com"));
        assert!(!is_address("ada@example.com."));
        assert!(!is_address("ada@exa mple.com"));
        // Header injection: an LF cannot survive parsing, and what is left of
        // the entry is not an address.
        assert!(!is_address("a@b.c\nBcc: evil@example.com"));
        assert!(!is_address("a@b.c\r\nBcc: evil@example.com"));
    }

    #[test]
    fn a_field_splits_on_commas_and_semicolons_and_unwraps_names() {
        assert_eq!(parse_addresses("a@b.c"), ["a@b.c"]);
        assert_eq!(
            parse_addresses(" a@b.c , d@e.f ;g@h.i "),
            ["a@b.c", "d@e.f", "g@h.i"]
        );
        assert_eq!(
            parse_addresses("Ada <ada@example.com>, Bob <bob@example.com>"),
            ["ada@example.com", "bob@example.com"]
        );
        assert_eq!(parse_addresses(""), Vec::<String>::new());
        assert_eq!(parse_addresses(" , ; "), Vec::<String>::new());
        // A newline typed into a field cannot end up in a header line.
        let injected = parse_addresses("a@b.c\nBcc: evil@example.com");
        assert_eq!(injected.len(), 1);
        assert!(!injected[0].contains('\n'));
        assert!(!is_address(&injected[0]));
    }

    /// What Kimo writes, Kimo can read: the rendered message goes back through
    /// the parser with its headers and its body intact, non-ASCII and all.
    #[test]
    fn a_rendered_mail_parses_back_with_its_headers_and_body() {
        let mut mail = mail();
        mail.subject = "发票号码 12345".to_string();
        mail.body = "本月项目进展：一切正常。\n下一行。\n".to_string();
        mail.cc = vec!["carol@example.com".to_string()];
        mail.bcc = vec!["dave@example.com".to_string()];
        let raw = render(&mail, 1_791_504_000);
        let text = String::from_utf8(raw.clone()).expect("the message is 7-bit clean");
        assert!(text.contains("Subject: =?UTF-8?B?"), "an encoded subject");
        assert!(
            !text.replace("\r\n", "").contains(['\r', '\n']),
            "every line ending is CRLF"
        );
        assert!(!text.contains("Bcc:"), "blind copies are envelope only");

        let parsed = crate::mail::parse(&raw, "1", false);
        assert_eq!(parsed.subject, "发票号码 12345");
        assert_eq!(
            parsed.body_text.replace("\r\n", "\n").trim_end(),
            "本月项目进展：一切正常。\n下一行。"
        );
        assert!(parsed.from.contains("Ada Lovelace"));
        assert!(parsed.from.contains("ada@example.com"));
        assert!(parsed.to.contains("bob@example.com"));
        assert_eq!(parsed.timestamp, Some(1_791_504_000), "the Date header");
        assert_eq!(parsed.attachment_names, Vec::<String>::new());
    }

    #[test]
    fn an_ascii_mail_writes_the_headers_a_reader_expects() {
        let raw = render(&mail(), 1_791_504_000);
        let text = String::from_utf8(raw).unwrap();
        let (headers, body) = text
            .split_once("\r\n\r\n")
            .expect("headers and a blank line");
        assert!(headers.starts_with("From: Ada Lovelace <ada@example.com>\r\n"));
        assert!(headers.contains("\r\nTo: bob@example.com\r\n"));
        assert!(headers.contains("\r\nSubject: Hello\r\n"));
        assert!(headers.contains("\r\nDate: Fri, 09 Oct 2026 00:00:00 +0000\r\n"));
        assert!(headers.contains("\r\nMIME-Version: 1.0\r\n"));
        assert!(headers.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"));
        // The last header of the block: what follows it is the blank line the
        // split took away, not another one.
        assert!(
            headers.ends_with("Content-Transfer-Encoding: base64"),
            "{headers}"
        );
        assert!(!headers.contains("Cc:"), "no Cc when there is none");
        let id = headers
            .lines()
            .find(|line| line.starts_with("Message-ID: "))
            .expect("a Message-ID");
        assert!(id.starts_with("Message-ID: <kimo."), "{id}");
        assert!(id.ends_with("@example.com>"), "{id}");

        // The body is what was typed, base64-encoded, 76 columns to a line.
        assert_eq!(decode(body), "Hi there!\r\n\r\nBye.\r\n");
        for line in body.lines() {
            assert!(line.len() <= 76, "{} columns in {line}", line.len());
        }
    }

    /// A body is a text box's, not a mail's: a lone LF becomes CRLF, a lone CR
    /// too, and a line that starts with a dot is carried as it is (the SMTP
    /// client is what stuffs it).
    #[test]
    fn a_body_is_carried_whole_whatever_its_line_endings() {
        let mut mail = mail();
        mail.body = "first\n.second\r\nthird\rfourth".to_string();
        let raw = String::from_utf8(render(&mail, 0)).unwrap();
        let body = raw.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(decode(body), "first\r\n.second\r\nthird\r\nfourth");
    }

    #[test]
    fn a_long_subject_folds_without_losing_anything() {
        let mut mail = mail();
        mail.subject =
            "A subject long enough that it has to be folded somewhere sensible and must still read back exactly as typed"
                .to_string();
        let raw = render(&mail, 0);
        let text = String::from_utf8(raw.clone()).unwrap();
        for line in text.split("\r\n\r\n").next().unwrap().split("\r\n") {
            assert!(line.len() <= 78, "{} columns: {line}", line.len());
        }
        assert_eq!(crate::mail::parse(&raw, "1", false).subject, mail.subject);
    }

    /// A long non-ASCII subject becomes several encoded words rather than one
    /// over-long line, and the reader joins them back into the text that was
    /// typed.
    #[test]
    fn a_long_encoded_subject_folds_into_words_a_reader_joins() {
        let mut mail = mail();
        mail.subject = "日".repeat(60);
        let raw = render(&mail, 0);
        let text = String::from_utf8(raw.clone()).unwrap();
        for line in text.split("\r\n\r\n").next().unwrap().split("\r\n") {
            assert!(line.len() <= 78, "{} columns: {line}", line.len());
        }
        assert!(text.contains("=?UTF-8?B?"));
        assert_eq!(
            crate::mail::parse(&raw, "1", false).subject,
            "日".repeat(60)
        );
    }

    #[test]
    fn the_envelope_holds_every_recipient_and_the_headers_do_not() {
        let mut mail = mail();
        mail.cc = vec!["carol@example.com".to_string()];
        mail.bcc = vec!["dave@example.com".to_string()];
        assert_eq!(
            mail.recipients(),
            ["bob@example.com", "carol@example.com", "dave@example.com"]
        );
        assert!(mail.invalid().is_empty());
        let raw = String::from_utf8(render(&mail, 0)).unwrap();
        assert!(raw.contains("Cc: carol@example.com"));
        assert!(!raw.contains("dave@example.com"), "bcc is not a header");

        mail.to.push("not an address".into());
        assert_eq!(mail.invalid(), ["not an address"]);
    }

    /// Two mails written in the same second are still two mails: the counter
    /// in the id is what says so.
    #[test]
    fn two_mails_from_one_second_have_different_ids() {
        let first = message_id(&mail(), 1_791_504_000);
        let second = message_id(&mail(), 1_791_504_000);
        assert_ne!(first, second);
        assert!(first.ends_with("@example.com>"));
        assert!(first.starts_with("<kimo."));
        // An address with no domain to borrow still gets a usable one.
        let mut bare = mail();
        bare.from_addr = "ada".to_string();
        assert!(message_id(&bare, 0).ends_with("@localhost>"));
    }

    #[test]
    fn a_from_with_no_name_is_just_the_address() {
        let mut mail = mail();
        mail.from_name = "  ".to_string();
        let raw = String::from_utf8(render(&mail, 0)).unwrap();
        assert!(raw.starts_with("From: ada@example.com\r\n"));
    }
}
