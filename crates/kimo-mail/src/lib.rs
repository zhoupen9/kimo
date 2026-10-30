//! Mail as data: RFC 5322 in and out, conversations, and the folder kinds a
//! mailbox name maps to. No sockets, no egui, no database.

pub mod folders;
pub mod mail;
pub mod outgoing;
pub mod threads;
