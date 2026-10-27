//! WASM providers and extensions: the host boundary described in
//! `docs/mail-spec.md`. The ABI and loader are separate from the existing
//! protocol adapters so a standard IMAP/POP3/SMTP client remains a subset of
//! the same operation vocabulary.

pub mod abi;
pub mod discovery;
pub mod extension;
#[allow(dead_code)] // The account form wires this adapter in the next step; tests exercise it now.
pub mod host;
#[allow(dead_code)] // The account form wires this adapter in the next step; tests exercise it now.
pub mod net;
#[allow(dead_code)] // The account form wires this adapter in the next step; tests exercise it now.
pub mod provider;
