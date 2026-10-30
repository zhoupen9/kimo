//! WASM providers and extensions: the host boundary described in
//! `docs/mail-spec.md`. The ABI and loader are separate from the existing
//! protocol adapters so a standard IMAP/POP3/SMTP client remains a subset of
//! the same operation vocabulary.

// The ABI types live in their own crate so the WASM modules can share them.
pub use kimo_plugin_abi as abi;
pub mod auth;
pub mod discovery;
// The extension hooks (organizer.v1 / display.v1) have no UI worker yet;
// the plans and the two instance methods that validate them are the
// published half of that contract.
#[allow(dead_code)]
pub mod extension;
pub mod host;
pub mod net;
pub mod provider;
