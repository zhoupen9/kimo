//! Hand-rolled mail protocol clients.
//!
//! Jimao speaks only IMAP4rev1 and POP3, implemented directly on top of
//! `std::net::TcpStream` (+ rustls for TLS) so the whole client stays pure
//! Rust with a minimal dependency tree.

pub mod imap;
pub mod pop3;
pub mod stream;
