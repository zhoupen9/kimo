//! Data models shared between storage, sync workers and the UI.

use serde::{Deserialize, Serialize};

/// The two (and only two) mail retrieval protocols Jimao speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
}

/// How TLS is negotiated for a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// TLS from the first byte (e.g. port 993/995).
    Implicit,
    /// Plain-text connect, then in-band upgrade (`STARTTLS` / `STLS`).
    StartTls,
    /// No TLS. Only sane on localhost or a trusted LAN.
    Plain,
}

/// A user-configured mail account.
#[derive(Clone, Debug, Serialize, Deserialize)]
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

/// A single message as stored locally. `uid` is the IMAP UID (within a
/// UIDVALIDITY generation) or the POP3 UIDL string.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default)]
    pub body_html: Option<String>,
    pub attachment_names: Vec<String>,
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
    fn protocol_serializes_lowercase() {
        let json = serde_json::to_string(&Protocol::Pop3).unwrap();
        assert_eq!(json, "\"pop3\"");
    }
}
