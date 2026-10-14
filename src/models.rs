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

/// Where a written mail goes out: SMTP submission. An account without one
/// cannot send — the compose card says so instead of failing at a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    /// The login to submit with; `None` is the account's own
    /// credentials, which is what most providers expect on submission.
    pub login: Option<SmtpLogin>,
}

impl SmtpSettings {
    /// The submission ports — 465 implicit TLS, 587 STARTTLS, 25 plain — not
    /// the ports mail is *received* on.
    pub fn default_port(tls: TlsMode) -> u16 {
        match tls {
            TlsMode::Implicit => 465,
            TlsMode::StartTls => 587,
            TlsMode::Plain => 25,
        }
    }
}

/// Credentials for a submission server that is not the account itself: an
/// authorization code, or a smarthost with its own login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpLogin {
    pub username: String,
    pub password: String,
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
    /// The outgoing server, when the account has one. `None` is an
    /// account that reads mail only: nothing it writes can leave it, and the
    /// compose card's Send says why rather than failing at a socket.
    pub smtp: Option<SmtpSettings>,
}

impl Account {
    /// Whether this account can send mail at all.
    pub fn can_send(&self) -> bool {
        self.smtp.is_some()
    }

    /// The credentials to submit with: the outgoing server's own pair when it
    /// names one, the account's own otherwise. The pair may hold an empty
    /// username — a submission server that wants no login, like a local relay
    /// — which is for the caller to notice.
    pub fn smtp_login(&self) -> Option<(&str, &str)> {
        match &self.smtp {
            Some(SmtpSettings {
                login: Some(login), ..
            }) => Some((login.username.as_str(), login.password.as_str())),
            Some(_) => Some((self.username.as_str(), self.password.as_str())),
            None => None,
        }
    }
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

/// One file attached to a mail.
///
/// Only what a card can be drawn from is kept: the name, the type and the
/// decoded size. The bytes themselves stay on the server — what is stored here
/// is the parsed mail, and Export fetches the original whole (see AGENTS.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// File name as the sender wrote it (RFC 2231 is decoded by mail-parser).
    pub name: String,
    /// MIME type, e.g. `application/pdf`. Empty for mail cached before the
    /// database recorded it (schema v6).
    pub content_type: String,
    /// Decoded size in bytes, 0 when unknown — the same "cached before v6"
    /// case, since nothing can derive it from a name.
    pub size: u64,
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
    /// The mail's own `Message-ID`, angle brackets off. Empty for mail cached
    /// before schema v7 — the header was not stored then — and for the rare
    /// mail that carries none.
    pub message_id: String,
    /// The `In-Reply-To` ids, angle brackets off: what the mail answers when
    /// its client wrote nothing else. Usually one.
    pub in_reply_to: Vec<String>,
    /// The `References` chain, oldest first: the conversation as the sender's
    /// client kept it. `crate::threads` reads it from the end — the last id
    /// this mailbox has is the parent.
    pub references: Vec<String>,
    pub body_text: String,
    /// Raw HTML body part, when the message is HTML-based; rendered by
    /// `crate::html` instead of the plain text.
    pub body_html: Option<String>,
    /// One-line plain-text snippet for the message list.
    pub preview: String,
    /// The mail's attachments, in the order the parts appear in it. Draws
    /// one card each at the end of the body.
    pub attachments: Vec<Attachment>,
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

    #[test]
    fn submission_ports_are_the_ones_mail_clients_use() {
        assert_eq!(SmtpSettings::default_port(TlsMode::Implicit), 465);
        assert_eq!(SmtpSettings::default_port(TlsMode::StartTls), 587);
        assert_eq!(SmtpSettings::default_port(TlsMode::Plain), 25);
    }

    /// An account submits with its own credentials unless its outgoing server
    /// names a different pair; an account with no outgoing server at all has
    /// nothing to submit with, which is what leaves Send disabled.
    #[test]
    fn an_account_submits_with_its_own_credentials_unless_it_names_others() {
        let mut account = Account {
            id: "work".into(),
            name: "Work".into(),
            protocol: Protocol::Imap,
            host: "imap.example.com".into(),
            port: 993,
            username: "me@example.com".into(),
            password: "secret".into(),
            tls: TlsMode::Implicit,
            smtp: None,
        };
        assert!(!account.can_send());
        assert_eq!(account.smtp_login(), None);

        account.smtp = Some(SmtpSettings {
            host: "smtp.example.com".into(),
            port: 587,
            tls: TlsMode::StartTls,
            login: None,
        });
        assert!(account.can_send());
        assert_eq!(account.smtp_login(), Some(("me@example.com", "secret")));

        account.smtp = Some(SmtpSettings {
            host: "smtp.example.com".into(),
            port: 465,
            tls: TlsMode::Implicit,
            login: Some(SmtpLogin {
                username: "code".into(),
                password: "abcd efgh".into(),
            }),
        });
        assert_eq!(account.smtp_login(), Some(("code", "abcd efgh")));
    }
}
