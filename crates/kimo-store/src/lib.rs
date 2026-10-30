//! The mail database: schema and migrations, accounts, folders, cached mail,
//! labels and the FTS5 search that runs over them.
//!
//! This is the only crate that names `rusqlite` (the project's one C
//! dependency) and the only writer of the store. Worker threads own every
//! write; the UI reads through the functions here.

pub mod db;
pub mod error;
pub mod search;

pub use error::{StoreError, StoreResult};
