//! Raw RFC 5322 bytes → `MailMessage` via `mail-parser`. HTML bodies are
//! kept as-is (`body_html`) and rendered by `crate::html` in the UI.

use mail_parser::{MessageParser, MimeHeaders};

use crate::models::MailMessage;

pub fn parse(raw: &[u8], uid: &str, seen: bool) -> MailMessage {
    let parsed = MessageParser::default().parse(raw);
    let (subject, from, to, date, timestamp, body_text, body_html, attachments) = match parsed {
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
                m.body_text(0).map(|c| c.into_owned()).unwrap_or_default(),
                if body_is_html {
                    m.body_html(0).map(|c| c.into_owned())
                } else {
                    None
                },
                m.attachments()
                    .filter_map(|part| part.attachment_name().map(|n| n.to_string()))
                    .collect(),
            )
        }
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
            String::from_utf8_lossy(raw).into_owned(),
            None,
            Vec::new(),
        ),
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
        body_text,
        body_html,
        attachment_names: attachments,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
