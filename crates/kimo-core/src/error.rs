//! Error type shared by the protocol clients, sync jobs and storage.
//!
//! The database's own errors stay in `kimo-store`: naming a `rusqlite` error
//! here would make every crate that only needs the model depend on SQLite (the
//! project's one C dependency). The store converts its errors to [`MailError`]
//! at its boundary.

use std::fmt;

#[derive(Debug)]
pub enum MailError {
    Io(std::io::Error),
    /// The server replied with something we could not make sense of, or could
    /// not accept (POP3's -ERR, an unexpected line). IMAP's own NO / BAD are
    /// `Refused` instead: a caller may have a fallback for those.
    Protocol(String),
    /// The server understood a command and refused it (IMAP NO / BAD). Kept
    /// apart from `Protocol`, which also covers a server answering something
    /// we cannot make sense of: a caller with a fallback for a command the
    /// server does not implement (an extension only some servers have) must
    /// not take a broken connection for a refusal.
    Refused(String),
    /// Login or authentication was rejected.
    Auth(String),
    /// A settings file could not be read or written.
    Json(serde_json::Error),
    /// The storage layer failed without SQLite being at fault: the database
    /// could not be opened, an old cache file could not be read, …
    Storage(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MailError::Io(e) => write!(f, "I/O error: {e}"),
            MailError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            MailError::Refused(msg) => write!(f, "server refused: {msg}"),
            MailError::Auth(msg) => write!(f, "authentication failed: {msg}"),
            MailError::Json(e) => write!(f, "settings error: {e}"),
            MailError::Storage(message) => write!(f, "storage error: {message}"),
        }
    }
}

impl std::error::Error for MailError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MailError::Io(e) => Some(e),
            MailError::Json(e) => Some(e),
            MailError::Storage(_)
            | MailError::Protocol(_)
            | MailError::Refused(_)
            | MailError::Auth(_) => None,
        }
    }
}

impl From<std::io::Error> for MailError {
    fn from(e: std::io::Error) -> Self {
        MailError::Io(e)
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
