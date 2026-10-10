//! Error type shared by the protocol clients, sync jobs and storage.

use std::fmt;

#[derive(Debug)]
pub enum MailError {
    Io(std::io::Error),
    Tls(rustls::Error),
    /// The server replied with something we could not accept (NO / BAD / -ERR).
    Protocol(String),
    /// Login or authentication was rejected.
    Auth(String),
    Json(serde_json::Error),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MailError::Io(e) => write!(f, "I/O error: {e}"),
            MailError::Tls(e) => write!(f, "TLS error: {e}"),
            MailError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            MailError::Auth(msg) => write!(f, "authentication failed: {msg}"),
            MailError::Json(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for MailError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MailError::Io(e) => Some(e),
            MailError::Tls(e) => Some(e),
            MailError::Json(e) => Some(e),
            MailError::Protocol(_) | MailError::Auth(_) => None,
        }
    }
}

impl From<std::io::Error> for MailError {
    fn from(e: std::io::Error) -> Self {
        MailError::Io(e)
    }
}

impl From<rustls::Error> for MailError {
    fn from(e: rustls::Error) -> Self {
        MailError::Tls(e)
    }
}

impl From<serde_json::Error> for MailError {
    fn from(e: serde_json::Error) -> Self {
        MailError::Json(e)
    }
}

pub type MailResult<T> = Result<T, MailError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_the_server_message() {
        let e = MailError::Auth("invalid credentials".into());
        assert!(e.to_string().contains("invalid credentials"));
    }
}
