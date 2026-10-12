//! Data models shared between storage, sync workers and the UI.
//!
//! Nothing here is serialised any more: accounts and mail live in the SQLite
//! database (see `crate::db`); the only JSON left in the project is
//! `settings.json` (`crate::storage::Settings`) and the one-shot legacy
//! import in `db.rs`.

/// The two (and only two) mail retrieval protocols Kimo speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Imap,
    Pop3,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Imap => "IMAP",
            Protocol::Pop3 => "POP3",
        }
    }

    pub fn default_port(self, tls: TlsMode) -> u16 {
        match (self, tls) {
            (Protocol::Imap, TlsMode::Implicit) => 993,
            (Protocol::Imap, _) => 143,
            (Protocol::Pop3, TlsMode::Implicit) => 995,
            (Protocol::Pop3, _) => 110,
        }
    }

    /// The value stored in `accounts.protocol`; the schema's CHECK constraint
    /// in `db.rs` uses exactly these two strings.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Imap => "imap",
            Protocol::Pop3 => "pop3",
        }
    }

    /// The inverse of [`Protocol::as_str`]. `None` for anything else, so a
    /// hand-edited database cannot quietly become the wrong protocol.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "imap" => Some(Protocol::Imap),
            "pop3" => Some(Protocol::Pop3),
            _ => None,
        }
    }
}

/// How TLS is negotiated for a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsMode {
    /// TLS from the first byte (e.g. port 993/995).
    Implicit,
    /// Plain-text connect, then in-band upgrade (`STARTTLS` / `STLS`).
    StartTls,
    /// No TLS. Only sane on localhost or a trusted LAN.
    Plain,
}

impl TlsMode {
    /// The value stored in `accounts.tls`.
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Implicit => "implicit",
            TlsMode::StartTls => "start_tls",
            TlsMode::Plain => "plain",
        }
    }

    /// The inverse of [`TlsMode::as_str`]; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "implicit" => Some(TlsMode::Implicit),
            "start_tls" => Some(TlsMode::StartTls),
            "plain" => Some(TlsMode::Plain),
            _ => None,
        }
    }
}

/// A user-configured mail account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub tls: TlsMode,
}

/// An inline image attachment, referenced from HTML mail via `cid:`.
#[derive(Clone, Debug)]
pub struct InlineImage {
    /// Content-ID without angle brackets, as used in `cid:` references.
    pub cid: String,
    /// MIME type, e.g. `image/png`.
    pub content_type: String,
    /// The image's raw bytes — a BLOB in `inline_images.data`.
    pub data: Vec<u8>,
}

/// A single message as stored locally. `uid` is the IMAP UID (within a
/// UIDVALIDITY generation) or the POP3 UIDL string.
#[derive(Clone, Debug)]
pub struct MailMessage {
    pub uid: String,
    pub subject: String,
    pub from: String,
    pub to: String,
    /// Raw `Date:` header value, kept for display fallback.
    pub date: String,
    /// Unix timestamp (UTC) when the date header could be parsed.
    pub timestamp: Option<i64>,
    pub seen: bool,
    pub body_text: String,
    /// Raw HTML body part, when the message is HTML-based; rendered by
    /// `crate::html` instead of the plain text.
    pub body_html: Option<String>,
    /// One-line plain-text snippet for the message list.
    pub preview: String,
    pub attachment_names: Vec<String>,
    /// Inline images (attachments with a Content-ID), rendered for `cid:`
    /// references in the HTML body.
    pub inline_images: Vec<InlineImage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_match_common_conventions() {
        assert_eq!(Protocol::Imap.default_port(TlsMode::Implicit), 993);
        assert_eq!(Protocol::Imap.default_port(TlsMode::StartTls), 143);
        assert_eq!(Protocol::Pop3.default_port(TlsMode::Implicit), 995);
        assert_eq!(Protocol::Pop3.default_port(TlsMode::Plain), 110);
    }

    #[test]
    fn protocol_and_tls_round_trip_through_their_database_strings() {
        for protocol in [Protocol::Imap, Protocol::Pop3] {
            assert_eq!(Protocol::parse(protocol.as_str()), Some(protocol));
        }
        for tls in [TlsMode::Implicit, TlsMode::StartTls, TlsMode::Plain] {
            assert_eq!(TlsMode::parse(tls.as_str()), Some(tls));
        }
        // These strings are the schema's CHECK constraint values.
        assert_eq!(Protocol::Imap.as_str(), "imap");
        assert_eq!(TlsMode::StartTls.as_str(), "start_tls");
        // A hand-edited row must not load as a different protocol.
        assert_eq!(Protocol::parse("smtp"), None);
        assert_eq!(Protocol::parse("IMAP"), None);
        assert_eq!(TlsMode::parse("starttls"), None);
    }
}
