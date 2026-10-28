//! The abstract mail-provider interface: what a sync job asks a mailbox for,
//! without knowing which protocol answers.
//!
//! A provider is one open session with an account's mail server. `open`
//! connects, negotiates TLS and logs in, and answers a `Box<dyn MailProvider>`
//! — `imap::ImapProvider` or `pop3::Pop3Provider` — so `crate::sync` never
//! branches on `Account::protocol` to reach the wire. The adapters own
//! sockets, protocol parsing and `crate::mail::parse`; they deliberately touch
//! neither the database nor the event channel nor the filesystem. Everything
//! that has to be ordered against the store — which marks move before a batch
//! and which after it, when a row is deleted, what the window is told — stays
//! in `crate::sync`, where the transaction rules already live.
//!
//! The upper layer is `vendor`: Gmail, Outlook and the rest differ in
//! endpoints, sign-in requirements and which mailbox a sent copy belongs in,
//! and that knowledge belongs beside the interface rather than in the account
//! form or in a protocol adapter. See `docs/providers.md`.

use std::collections::HashSet;

use crate::error::{MailError, MailResult};
use crate::models::{Account, MailMessage, Protocol};

pub mod imap;
pub mod pop3;
pub mod vendor;

/// The one mailbox name the protocol fixes, whatever a server calls the rest.
pub const INBOX: &str = "INBOX";

/// How many of the newest messages a folder's *first* fetch takes. Later
/// fetches are incremental, so this is the size of the one-off backfill, not
/// of a sync; a `Load older mail` click asks for its own batch
/// (`crate::sync::OLDER_BATCH`).
pub const BACKFILL_LIMIT: u32 = 50;

/// What a protocol can actually do, so a caller can skip a round trip it would
/// be told is impossible rather than asking and mapping the refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The server lists folders (IMAP). POP3 has one drop and no `LIST`.
    pub folders: bool,
    /// `\Seen` and `\Flagged` live on the server and can be pushed (IMAP).
    pub server_flags: bool,
    /// A sent copy can be filed with `APPEND` (IMAP).
    pub append: bool,
    /// A fetch has to be told which UIDs the database already holds (POP3's
    /// UIDLs). IMAP's UID range makes the database's knowledge unnecessary.
    pub stored_uids: bool,
}

/// The capabilities of a read protocol. SMTP submission is not on this table:
/// it is not a mailbox and `crate::sync::send_message` reaches
/// `crate::protocol::smtp` directly, with the endpoint the vendor layer
/// supplies.
pub fn capabilities(protocol: Protocol) -> Capabilities {
    match protocol {
        Protocol::Imap => Capabilities {
            folders: true,
            server_flags: true,
            append: true,
            stored_uids: false,
        },
        Protocol::Pop3 => Capabilities {
            folders: false,
            server_flags: false,
            append: false,
            stored_uids: true,
        },
    }
}

/// What the store knows about a folder before a fetch — the marks described in
/// `docs/architecture.md`, handed in rather than read by the adapter.
#[derive(Clone, Debug, Default)]
pub struct FolderMark {
    /// IMAP's generation; `None` for POP3 and for a folder nobody fetched.
    pub uidvalidity: Option<u32>,
    /// IMAP: one past the highest stored UID, where a warm fetch resumes.
    pub uid_next: Option<u32>,
    /// IMAP: the lowest UID ever fetched — the floor `Load older mail` walks
    /// down. POP3: the lowest message number this drop has been fetched from.
    pub first_num: Option<u32>,
    /// POP3 only: the UIDLs the database already holds. `None` when the
    /// protocol does not need them (`Capabilities::stored_uids`).
    pub stored: Option<HashSet<String>>,
}

/// What a folder fetch found: a first fill/backfill or a warm resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchKind {
    /// The folder had no usable mark, so this is its backfill — mail the
    /// window is seeing for the first time, never "news" for a notification.
    Fill,
    /// The folder resumed at its marks: what came back arrived since the last
    /// fetch, which is what a notification is raised from.
    Resume,
}

/// One folder's fetch, before anything is stored or announced.
#[derive(Debug)]
pub struct FolderBatch {
    /// IMAP's generation for this fetch; `None` for POP3.
    pub uidvalidity: Option<u32>,
    /// The mail, already parsed.
    pub messages: Vec<MailMessage>,
    pub kind: FetchKind,
    /// The floor this fetch learned, when it set one (a backfill). `None`
    /// leaves the stored floor alone.
    pub first_num: Option<u32>,
    /// `Some(answer)` when the server was asked how far down it goes — the
    /// inner `None` is an empty mailbox, which is an answer too — and `None`
    /// when this fetch did not ask, so the stored mark must stand.
    pub oldest: Option<Option<u32>>,
    /// Whether the store writes the marks before the batch or after it. POP3
    /// writes them first: its floor bounds the next fetch and nothing rotates
    /// it. IMAP writes them after, because `db::save_messages` clears both
    /// marks when the UIDVALIDITY moved and the new ones have to survive it.
    pub marks_before_store: bool,
    /// The adapter stopped short of the range it was asked for and has more
    /// to give: a plugin whose one call has a byte budget pages a large
    /// folder this way. The built-in adapters always finish their range, so
    /// they leave this false.
    pub more: bool,
}

/// One `Load older mail` fetch, before anything is stored or announced.
#[derive(Debug)]
pub struct OlderBatch {
    pub uidvalidity: Option<u32>,
    pub messages: Vec<MailMessage>,
    /// The floor the click moved to, when it asked for a range.
    pub first_num: Option<u32>,
    /// The server's oldest message, when the fetch asked (IMAP always records
    /// it; POP3's is known from the first fetch and is not re-asked here).
    pub oldest: Option<Option<u32>>,
    /// Whether anything is left below the new floor.
    pub available: bool,
}

/// What became of a delete: the ids whose remote copies are gone, and the first
/// reason the rest did not go. A partial run is a real answer — one mail the
/// server refused must not hide the ones it accepted.
#[derive(Debug)]
pub struct DeleteOutcome {
    /// The uids (as stored) the server agreed to delete.
    pub gone: Vec<String>,
    pub error: Option<MailError>,
}

/// One account's mailbox, over whichever protocol it speaks.
///
/// Every method runs on a worker thread (`crate::sync`); nothing here is
/// async. `open` has already connected and logged in, and `logout` is the
/// caller's job on every path (`with` does it).
pub trait MailProvider {
    fn capabilities(&self) -> Capabilities;

    /// The folders the server lists, in wire form. POP3 answers the one drop.
    fn folders(&mut self) -> MailResult<Vec<String>>;

    /// Fetch a folder from the marks the store holds: a resume where they are
    /// usable, a backfill of the newest `BACKFILL_LIMIT` where they are not.
    fn fetch_folder(&mut self, folder: &str, mark: &FolderMark) -> MailResult<FolderBatch>;

    /// Fetch the batch below the floor the folder has been fetched down to.
    /// `batch` is how many messages the click asks for.
    fn fetch_older(
        &mut self,
        folder: &str,
        mark: &FolderMark,
        batch: u32,
    ) -> MailResult<OlderBatch>;

    /// One message's original RFC 5322 source. `MailError::Protocol` says the
    /// server no longer has it.
    fn body(&mut self, folder: &str, uid: &str) -> MailResult<Vec<u8>>;

    /// Push `\Seen` (IMAP). POP3 has no server-side flags and does nothing.
    fn set_seen(&mut self, folder: &str, uid: &str) -> MailResult<()>;

    /// Push or clear `\Flagged` (IMAP). POP3 does nothing.
    fn set_flagged(&mut self, folder: &str, uid: &str, flagged: bool) -> MailResult<()>;

    /// Delete the remote copies. A POP3 adapter ends its session with `QUIT`
    /// inside this call, because that is what commits the deletions; the
    /// caller writes the local rows only after it returns.
    fn delete(&mut self, folder: &str, uids: &[String]) -> DeleteOutcome;

    /// File a copy in `mailbox` (IMAP's `APPEND`). POP3 has nowhere to put
    /// one and says so.
    fn append(&mut self, mailbox: &str, raw: &[u8]) -> MailResult<()>;

    /// End the session politely. Called on success and failure alike.
    fn logout(&mut self);
}

/// Open one account's mailbox: connect, negotiate TLS, log in. A plugin
/// provider account (`provider` set) opens through its WASM module — the
/// built-in client below never sees it, which is the whole point: the module
/// owns the wire, the login and the vendor's semantics.
pub fn open(account: &Account) -> MailResult<Box<dyn MailProvider>> {
    if account.is_plugin() {
        return Ok(Box::new(crate::plugin::provider::open_account(account)?));
    }
    match account.protocol {
        Protocol::Imap => Ok(Box::new(imap::ImapProvider::open(account)?)),
        Protocol::Pop3 => Ok(Box::new(pop3::Pop3Provider::open(account)?)),
    }
}

/// Run `f` on a fresh session and end it afterwards, whether `f` succeeded or
/// not — the connection hygiene the old `with_imap` had, now for both
/// protocols and every caller.
pub fn with<T>(
    account: &Account,
    f: impl FnOnce(&mut dyn MailProvider) -> MailResult<T>,
) -> MailResult<T> {
    let mut provider = open(account)?;
    let result = f(provider.as_mut());
    provider.logout();
    result
}

/// The server does not answer for this message any more. There is no original
/// left to export, and the local copy is not a substitute for one.
pub(crate) fn mail_is_gone() -> MailError {
    MailError::Protocol("the server no longer has this mail".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability table is what lets sync skip a round trip: POP3 has no
    /// folders, no server flags and no `APPEND`, and its fetch is the one that
    /// needs the stored UIDLs.
    #[test]
    fn each_protocol_advertises_what_it_can_do() {
        let imap = capabilities(Protocol::Imap);
        assert!(imap.folders);
        assert!(imap.server_flags);
        assert!(imap.append);
        assert!(!imap.stored_uids);

        let pop3 = capabilities(Protocol::Pop3);
        assert!(!pop3.folders);
        assert!(!pop3.server_flags);
        assert!(!pop3.append);
        assert!(pop3.stored_uids);
    }
}
