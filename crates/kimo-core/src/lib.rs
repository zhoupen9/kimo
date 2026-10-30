//! The types every other Kimo crate shares: accounts and mail, the error type
//! the wire and workers speak, and the XDG paths and UI settings.
//!
//! Nothing here touches the network, egui, or SQLite. In particular the
//! database lives in `kimo-store`, so this crate does not pull the project's
//! one C dependency into the protocol or the model.

pub mod error;
pub mod models;
pub mod storage;
