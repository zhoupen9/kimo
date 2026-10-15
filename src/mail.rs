//! Raw RFC 5322 bytes → `MailMessage` via `mail-parser`. HTML bodies are
//! kept as-is (`body_html`) and rendered by `crate::html` in the UI.

use std::collections::BTreeSet;

use mail_parser::{MessageParser, MimeHeaders};

use crate::models::MailMessage;

pub fn parse(raw: &[u8], uid: &str, seen: bool) -> MailMessage {
    let parsed = MessageParser::default().parse(raw);
    let (
        subject,
        from,
        to,
        date,
        timestamp,
        message_id,
        in_reply_to,
        references,
        body_text,
        body_html,
        attachments,
        inline_images,
    ) = match parsed {
        Some(m) => {
            let dt = m.date().copied();
            // The body accessors convert between flavors, so look at the
            // real part type to know whether this message is HTML-based.
            let body_is_html = m
                .html_body
                .first()
                .or_else(|| m.text_body.first())
                .and_then(|&i| m.parts.get(i as usize))
                .is_some_and(|p| matches!(p.body, mail_parser::PartType::Html(_)));
            (
                m.subject().unwrap_or_default().to_owned(),
                address_to_string(m.from()),
                address_to_string(m.to()),
                dt.map(|d| d.to_string()).unwrap_or_default(),
                dt.map(|d| d.to_timestamp()),
                m.message_id().unwrap_or_default().to_string(),
                message_ids(m.in_reply_to()),
                message_ids(m.references()),
                m.body_text(0).map(|c| c.into_owned()).unwrap_or_default(),
                if body_is_html {
                    m.body_html(0).map(|c| c.into_owned())
                } else {
                    None
                },
                collect_attachments(&m),
                collect_inline_images(&m),
            )
        }
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
            String::new(),
            Vec::new(),
            Vec::new(),
            String::from_utf8_lossy(raw).into_owned(),
            None,
            Vec::new(),
            Vec::new(),
        ),
    };

    let preview = {
        let source = if !body_text.trim().is_empty() {
            body_text.clone()
        } else if let Some(html) = &body_html {
            html_to_plain(html)
        } else {
            String::new()
        };
        let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
        collapsed.chars().take(PREVIEW_CHARS).collect()
    };

    MailMessage {
        uid: uid.to_string(),
        subject: if subject.trim().is_empty() {
            "(no subject)".to_string()
        } else {
            subject
        },
        from: if from.trim().is_empty() {
            "(unknown sender)".to_string()
        } else {
            from
        },
        to,
        date,
        timestamp,
        seen,
        message_id,
        in_reply_to,
        references,
        body_text,
        body_html,
        preview,
        attachments,
        inline_images,
        // A mail just parsed off the wire carries no labels: they are the
        // user's own marks, and they live in the database. The flag is the one
        // mark the wire *does* carry — `\Flagged` — and the caller that knows
        // the flags (`crate::sync`, off the FETCH) sets it; what is parsed here
        // is the message itself.
        flagged: false,
        labels: BTreeSet::new(),
    }
}

/// The ids of an `In-Reply-To` or `References` header, angle brackets off and
/// empties dropped — mail-parser has already taken the brackets off and answers
/// with `Text` for one id and `TextList` for several. These are the headers a
/// conversation is stitched from; a mail with none of them is a thread of its
/// own unless its subject says otherwise (`crate::threads`).
fn message_ids(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    let one = |text: &str| {
        let id = text.trim().trim_matches(['<', '>']).trim();
        (!id.is_empty()).then(|| id.to_string())
    };
    match value {
        mail_parser::HeaderValue::Text(text) => one(text).into_iter().collect(),
        mail_parser::HeaderValue::TextList(list) => list.iter().filter_map(|t| one(t)).collect(),
        _ => Vec::new(),
    }
}

/// The mail's attachments, in the order their parts appear in it.
///
/// A part with no file name is not one: without a name there is nothing a card
/// could be titled with, and a part the sender left unnamed is usually a
/// structure of its own (a nested message, an HTML body part) rather than a
/// file somebody meant to send. The size is the *decoded* one — `contents()`
/// is the part after its transfer encoding has been undone, which is the size
/// the file would have on disk, not the base64 that carried it.
fn collect_attachments(m: &mail_parser::Message<'_>) -> Vec<crate::models::Attachment> {
    m.attachments()
        .filter_map(|part| {
            let name = part.attachment_name()?.to_string();
            Some(crate::models::Attachment {
                name,
                content_type: part.content_type().map(mime_type).unwrap_or_default(),
                size: part.contents().len() as u64,
            })
        })
        .collect()
}

/// One attachment's bytes out of the mail's original source, decoded — what a
/// click on its card hands to the downloads directory.
///
/// `ordinal` is the position the attachment had when the mail was parsed and
/// stored (the database's `ordinal`), and `name` the name that was stored with
/// it. Both have to match the part this parse finds: the bytes of *some* part
/// are not the file the card promised, so a mail whose parts no longer line up
/// answers `None` and the caller reports it rather than writing the wrong file.
pub fn attachment_bytes(raw: &[u8], ordinal: usize, name: &str) -> Option<Vec<u8>> {
    let parsed = MessageParser::default().parse(raw)?;
    let part = parsed.attachments().nth(ordinal)?;
    if part.attachment_name()? != name {
        return None;
    }
    Some(part.contents().to_vec())
}

/// A part's MIME type as `type/subtype`. `ctype()` never fails — RFC 5322
/// mail may carry no `Content-Type` at all, which mail-parser answers with
/// `text/plain` — while a bare type with no subtype (a hand-written header)
/// keeps the type alone rather than inventing a `/`.
fn mime_type(content_type: &mail_parser::ContentType) -> String {
    match content_type.subtype() {
        Some(sub) => format!("{}/{}", content_type.ctype(), sub),
        None => content_type.ctype().to_string(),
    }
}

/// Attachment parts that are images with a Content-ID — the `cid:` targets of
/// HTML mail — with their raw bytes, which is how the database stores them.
fn collect_inline_images(m: &mail_parser::Message<'_>) -> Vec<crate::models::InlineImage> {
    m.attachments()
        .filter_map(|part| {
            let cid = part.content_id()?.trim_matches(['<', '>']).to_string();
            let content_type = part.content_type()?;
            if cid.is_empty() || !content_type.ctype().eq_ignore_ascii_case("image") {
                return None;
            }
            Some(crate::models::InlineImage {
                cid,
                content_type: mime_type(content_type),
                data: part.contents().to_vec(),
            })
        })
        .collect()
}

/// Preview length in characters.
const PREVIEW_CHARS: usize = 140;

/// Plain text out of an HTML body, for list previews: concatenates text
/// nodes (bounded), skipping style/script/head/title subtrees.
pub(crate) fn html_to_plain(html: &str) -> String {
    let frag = scraper::Html::parse_fragment(html);
    let mut text = String::new();
    for node in frag.tree.root().descendants() {
        if let scraper::Node::Text(t) = node.value() {
            let in_hidden = node.ancestors().any(|a| {
                matches!(a.value(), scraper::Node::Element(e) if matches!(e.name(), "style" | "script" | "head" | "title"))
            });
            if !in_hidden {
                text.push_str(&t.text);
                if text.len() > 2_000 {
                    break;
                }
            }
        }
    }
    text
}

fn address_to_string(addr: Option<&mail_parser::Address<'_>>) -> String {
    let fmt_addr = |a: &mail_parser::Addr<'_>| {
        let address = a.address.as_deref().unwrap_or("");
        match a.name.as_deref() {
            Some(name) if !name.is_empty() && !address.is_empty() => format!("{name} <{address}>"),
            Some(name) if !name.is_empty() => name.to_string(),
            _ => address.to_string(),
        }
    };
    match addr {
        Some(mail_parser::Address::List(list)) => {
            list.iter().map(fmt_addr).collect::<Vec<_>>().join(", ")
        }
        Some(mail_parser::Address::Group(groups)) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .map(fmt_addr)
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

/// A unix timestamp as an RFC 5322 `Date:` header value — `Fri, 09 Oct 2026
/// 00:00:00 +0000` — which is what a written mail carries.
///
/// Always UTC: Kimo has no zone database and no time crate, and a mail that
/// says an absolute time in UTC is one every reader shows correctly. A guessed
/// local offset is the one thing that would *not* be.
pub fn rfc5322_date(secs: i64) -> String {
    /// 1970-01-01 was a Thursday, so the rotation starts there.
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let month = MONTHS[(month - 1) as usize];
    format!(
        "{weekday}, {day:02} {month} {year:04} {:02}:{:02}:{:02} +0000",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days-since-epoch → (year, month, day), Howard Hinnant's civil_from_days.
///
/// The one piece of calendar arithmetic the app and the message builder share:
/// `app::format_timestamp` prints it for the reading pane,
/// `rfc5322_date` writes it into a header.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date a written mail carries: named, zero-padded, and in UTC —
    /// including the two cases a hand-rolled calendar gets wrong, the epoch and
    /// a leap day.
    #[test]
    fn a_date_header_is_utc_and_named() {
        assert_eq!(
            rfc5322_date(1_791_504_000),
            "Fri, 09 Oct 2026 00:00:00 +0000"
        );
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc5322_date(1_709_164_800),
            "Thu, 29 Feb 2024 00:00:00 +0000"
        );
        // Hours, minutes and seconds come out of the remainder, and a time
        // before the epoch still has a weekday.
        assert_eq!(
            rfc5322_date(1_791_504_000 + 3_661),
            "Fri, 09 Oct 2026 01:01:01 +0000"
        );
        assert_eq!(rfc5322_date(-1), "Wed, 31 Dec 1969 23:59:59 +0000");
    }

    /// The headers a conversation is stitched from: the mail's own id, what it
    /// answers, and the chain its client kept — angle brackets off, in the
    /// order they were written, with a header that says nothing coming back as
    /// nothing rather than as an empty id.
    #[test]
    fn threading_headers_are_read_into_ids() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: Re: Hello\r\n\
                    Message-ID: <reply@example.com>\r\n\
                    In-Reply-To: <root@example.com>\r\n\
                    References: <root@example.com> <first@example.com>\r\n\
                    \r\n\
                    A reply.\r\n";
        let msg = parse(raw, "2", false);
        assert_eq!(msg.message_id, "reply@example.com");
        assert_eq!(msg.in_reply_to, ["root@example.com"]);
        assert_eq!(msg.references, ["root@example.com", "first@example.com"]);

        // A mail that threads nothing: no headers, no ids.
        let plain = parse(
            b"From: a@b.c\r\nSubject: Alone\r\n\r\nText.\r\n",
            "3",
            false,
        );
        assert_eq!(plain.message_id, "");
        assert!(plain.in_reply_to.is_empty() && plain.references.is_empty());

        // A client that wrote several In-Reply-To ids keeps them all, and one
        // that wrote a broken header is not a mail with a nameless parent.
        let several = parse(
            b"Message-ID: <m@example.com>\r\nIn-Reply-To: <a@example.com> <b@example.com>\r\n\
              References: <>\r\n\r\nText.\r\n",
            "4",
            false,
        );
        assert_eq!(several.in_reply_to, ["a@example.com", "b@example.com"]);
        assert!(several.references.is_empty());
    }

    #[test]
    fn parses_a_minimal_message() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    To: Bob <bob@example.com>\r\n\
                    Subject: Hello\r\n\
                    \r\n\
                    Hi there!\r\n";
        let msg = parse(raw, "42", false);
        assert_eq!(msg.subject, "Hello");
        assert!(msg.from.contains("alice@example.com"));
        assert!(msg.body_text.contains("Hi there!"));
        assert!(msg.body_html.is_none());
        assert!(!msg.seen);
    }

    #[test]
    fn html_body_is_captured_for_rendering() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: Rendered\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>Hello <b>world</b>!</p>\r\n";
        let msg = parse(raw, "7", true);
        let html = msg.body_html.expect("html body");
        assert!(html.contains("<b>world</b>"));
        assert!(msg.seen);
    }

    #[test]
    fn plain_message_is_not_marked_html_based() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: Plain\r\n\
                    \r\n\
                    Just text.\r\n";
        let msg = parse(raw, "8", false);
        assert!(msg.body_html.is_none());
        assert_eq!(msg.body_text.trim(), "Just text.");
        assert_eq!(msg.preview, "Just text.");
    }

    #[test]
    fn html_only_messages_get_a_text_preview() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: Rendered\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <html><head><style>.x{color:red}</style></head>\
                    <body><p>Hello <b>preview</b> world</p></body></html>\r\n";
        let msg = parse(raw, "11", false);
        assert!(!msg.preview.is_empty());
        assert!(msg.preview.contains("Hello preview world"));
        assert!(
            !msg.preview.contains("color"),
            "style text must not leak into previews"
        );
    }

    #[test]
    fn inline_cid_images_are_captured_with_their_bytes() {
        // multipart/related: an HTML body plus a base64 image with a
        // Content-ID, exactly like Outlook inline logos.
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: Inline art\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/related; boundary=\"XYZ\"\r\n\
                    \r\n\
                    --XYZ\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>See <img src=\"cid:logo@example.com\"></p>\r\n\
                    --XYZ\r\n\
                    Content-Type: image/png; name=\"logo.png\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    Content-ID: <logo@example.com>\r\n\
                    Content-Disposition: inline; filename=\"logo.png\"\r\n\
                    \r\n\
                    aGVsbG8gaW5saW5lIQ==\r\n\
                    --XYZ--\r\n";
        let msg = parse(raw, "12", false);
        assert!(msg.body_html.is_some());
        assert!(msg.attachments.iter().any(|a| a.name.contains("logo.png")));
        let inline = msg.inline_images.first().expect("inline image captured");
        assert_eq!(inline.cid, "logo@example.com");
        assert_eq!(inline.content_type, "image/png");
        // The part's base64 ("aGVsbG8gaW5saW5lIQ==") is already decoded here.
        assert_eq!(inline.data, b"hello inline!");
        // The image is an attachment as well, and its card says what it is.
        let card = msg
            .attachments
            .first()
            .expect("the part is an attachment too");
        assert_eq!(card.content_type, "image/png");
        assert_eq!(card.size, 13, "the decoded size of \"hello inline!\"");
    }

    #[test]
    fn an_attachment_keeps_its_name_type_and_decoded_size() {
        // multipart/mixed as almost every attachment arrives in: the body, then
        // a base64 file part. "hello world" is eleven bytes once decoded, which
        // is the size the card shows — not the sixteen base64 carried.
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: With a file\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                    \r\n\
                    --B\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    See attached.\r\n\
                    --B\r\n\
                    Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                    \r\n\
                    aGVsbG8gd29ybGQ=\r\n\
                    --B--\r\n";
        let msg = parse(raw, "13", false);
        assert_eq!(msg.attachments.len(), 1);
        let attachment = &msg.attachments[0];
        assert_eq!(attachment.name, "report.pdf");
        assert_eq!(attachment.content_type, "application/pdf");
        assert_eq!(attachment.size, 11);
    }

    /// One part out of the original: the card's own ordinal and name are what
    /// says *which* part, and a mail whose parts are not what the card said —
    /// another ordinal, another name — answers nothing rather than some other
    /// file's bytes.
    #[test]
    fn one_attachment_comes_back_out_of_the_original() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    Subject: With two files\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                    \r\n\
                    --B\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Two of them.\r\n\
                    --B\r\n\
                    Content-Type: text/plain; name=\"notes.txt\"\r\n\
                    Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
                    \r\n\
                    the first file\r\n\
                    --B\r\n\
                    Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                    \r\n\
                    aGVsbG8gd29ybGQ=\r\n\
                    --B--\r\n";
        assert_eq!(
            attachment_bytes(raw, 1, "report.pdf").expect("the second part"),
            b"hello world",
            "the base64 is decoded on the way out"
        );
        assert_eq!(
            attachment_bytes(raw, 0, "notes.txt").expect("the first part"),
            b"the first file"
        );
        // The name the card showed is part of the question: the same ordinal
        // under another name is not the file that was clicked.
        assert_eq!(attachment_bytes(raw, 1, "notes.txt"), None);
        assert_eq!(attachment_bytes(raw, 9, "report.pdf"), None);
    }

    #[test]
    fn gb2312_subject_and_body_decode_to_chinese() {
        // Subject encoded-word: "8月项目周报" (GBK, base64).
        // Body: "本月项目进展：一切正常。" as raw GBK bytes.
        let raw = b"From: liuyq@nroad.com.cn\r\n\
                    Subject: =?gb2312?B?ONTCz+7Ev9bcsag=?=\r\n\
                    Content-Type: text/plain; charset=gb2312\r\n\
                    \r\n\
                    \xb1\xbe\xd4\xc2\xcf\xee\xc4\xbf\xbd\xf8\xd5\xb9\xa3\xba\xd2\xbb\xc7\xd0\xd5\xfd\xb3\xa3\xa1\xa3\r\n";
        let msg = parse(raw, "9", false);
        assert_eq!(msg.subject, "8\u{6708}\u{9879}\u{76ee}\u{5468}\u{62a5}"); // 8月项目周报
        assert!(
            msg.body_text.contains("\u{9879}\u{76ee}"),
            "body should contain 项目"
        );
    }

    #[test]
    fn gbk_body_without_encoded_subject_still_decodes() {
        let raw = b"From: a@b.c\r\n\
                    Subject: plain ascii subject\r\n\
                    Content-Type: text/plain; charset=gbk\r\n\
                    Mime-Version: 1.0\r\n\
                    \r\n\
                    \xc4\xe3\xba\xc3\xa3\xac\xca\xc0\xbd\xe7\r\n";
        let msg = parse(raw, "10", false);
        assert!(
            msg.body_text.contains("\u{4f60}\u{597d}"),
            "body should contain 你好"
        );
    }
}
