//! Raw RFC 5322 bytes → `MailMessage` via `mail-parser`, with a crude
//! HTML-to-text fallback for messages that only have an HTML body.

use mail_parser::{MessageParser, MimeHeaders};

use crate::models::MailMessage;

pub fn parse(raw: &[u8], uid: &str, seen: bool) -> MailMessage {
    let parsed = MessageParser::default().parse(raw);
    let (subject, from, to, date, timestamp, body_text, attachments) = match parsed {
        Some(m) => {
            let dt = m.date().copied();
            (
                m.subject().unwrap_or_default().to_owned(),
                address_to_string(m.from()),
                address_to_string(m.to()),
                dt.map(|d| d.to_string()).unwrap_or_default(),
                dt.map(|d| d.to_timestamp()),
                m.body_text(0)
                    .map(|c| c.into_owned())
                    .or_else(|| m.body_html(0).map(|c| html_to_text(&c)))
                    .unwrap_or_else(|| "(message has no text body)".to_string()),
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

/// Very small HTML → text conversion for previewing HTML-only mail: drop
/// style/script blocks, convert obvious block tags to newlines, strip the
/// remaining tags and decode the handful of entities that matter.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut skip_until: Option<&'static str> = None;
    while let Some(c) = chars.next() {
        if let Some(end) = skip_until {
            let peek: String = chars
                .clone()
                .take(end.len())
                .collect::<String>()
                .to_lowercase();
            if peek.starts_with(end) {
                skip_until = None;
                for _ in 0..end.len() {
                    chars.next();
                }
                for t in chars.by_ref() {
                    if t == '>' {
                        break;
                    }
                }
            }
            continue;
        }
        if c != '<' {
            out.push(c);
            continue;
        }
        let tag: String = chars
            .clone()
            .take_while(|t| !matches!(t, '>' | ' ' | '\t' | '\n'))
            .collect();
        let tag = tag.to_lowercase();
        match tag.as_str() {
            "script" | "style" => {
                skip_until = Some(if tag == "script" {
                    "</script"
                } else {
                    "</style"
                })
            }
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                out.push('\n')
            }
            _ => {}
        }
        // Consume the rest of the tag.
        for t in chars.by_ref() {
            if t == '>' {
                break;
            }
        }
    }
    let mut decoded = decode_entities(&out);
    // Collapse runs of blank lines but keep paragraph breaks readable.
    while decoded.contains("\n\n\n") {
        decoded = decoded.replace("\n\n\n", "\n\n");
    }
    decoded.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
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
        assert!(!msg.seen);
    }

    #[test]
    fn html_fallback_strips_tags() {
        let text =
            html_to_text("<html><body><p>Hello<br>World</p><script>evil()</script></body></html>");
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("evil"));
        assert!(!text.contains('<'));
    }
}
