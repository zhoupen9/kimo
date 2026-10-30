//! The store's own error type.
//!
//! SQLite (the project's one C dependency) is only named here, so
//! `kimo-core`'s error stays free of `rusqlite`. The store converts its error
//! to [`MailError`] at the boundary, which is what lets the sync and UI code
//! keep speaking one error type while `cargo tree -p kimo-store` is the only
//! place the C dependency is visible.

use std::fmt;

use kimo_core::error::MailError;

#[derive(Debug)]
pub enum StoreError {
    /// SQLite failed.
    Sqlite(rusqlite::Error),
    /// A legacy JSON cache could not be read.
    Json(serde_json::Error),
    /// A cache file could not be read or written.
    Io(std::io::Error),
    /// The store failed without SQLite being at fault.
    Storage(String),
    /// An error that came from a lower Kimo crate.
    Mail(MailError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(error) => write!(f, "storage error: {error}"),
            StoreError::Json(error) => write!(f, "settings error: {error}"),
            StoreError::Io(error) => write!(f, "I/O error: {error}"),
            StoreError::Storage(message) => write!(f, "storage error: {message}"),
            StoreError::Mail(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Sqlite(error) => Some(error),
            StoreError::Json(error) => Some(error),
            StoreError::Io(error) => Some(error),
            StoreError::Mail(error) => Some(error),
            StoreError::Storage(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Sqlite(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        StoreError::Json(error)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        StoreError::Io(error)
    }
}

impl From<MailError> for StoreError {
    fn from(error: MailError) -> Self {
        StoreError::Mail(error)
    }
}

/// A store failure as the shared error the rest of Kimo speaks.
impl From<StoreError> for MailError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Mail(error) => error,
            StoreError::Sqlite(error) => MailError::Storage(format!("database: {error}")),
            StoreError::Json(error) => MailError::Json(error),
            StoreError::Io(error) => MailError::Io(error),
            StoreError::Storage(message) => MailError::Storage(message),
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;
