//! Hand-rolled mail protocol clients.
//!
//! Kimo speaks IMAP4rev1 and POP3 for reading, and SMTP submission for
//! sending, implemented directly on top of `std::net::TcpStream` (+ rustls for
//! TLS) so the whole client stays pure Rust with a minimal dependency tree.
//!
//! `ffdhe` is the one exception to "rustls does the TLS": rustls ships no
//! finite-field DH group, so servers that offer only `DHE-RSA-*` suites (Sina)
//! need the group, and the two suites on top of it, supplied from here.

pub mod ffdhe;
pub mod imap;
pub mod pop3;
pub mod smtp;
pub mod stream;
