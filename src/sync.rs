//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket.

use std::sync::mpsc::Sender;

use crate::error::MailResult;
use crate::mail;
use crate::models::{Account, MailMessage, Protocol};
use crate::protocol::imap::ImapClient;
use crate::protocol::pop3::Pop3Client;

/// How many of the newest messages to fetch per folder per sync.
pub const FETCH_LIMIT: u32 = 50;

pub enum Job {
    /// Folder list (IMAP) plus the INBOX contents; for POP3, the whole drop.
    Sync { account: Account },
    /// Fetch one specific IMAP folder.
    SyncFolder { account: Account, folder: String },
    /// Push `\Seen` back to the server (IMAP only).
    MarkSeen {
        account: Account,
        folder: String,
        uid: u32,
    },
}

pub enum Event {
    SyncStarted {
        account_id: String,
    },
    Folders {
        account_id: String,
        folders: Vec<String>,
    },
    Messages {
        account_id: String,
        folder: String,
        uidvalidity: Option<u32>,
        messages: Vec<MailMessage>,
    },
    SyncDone {
        account_id: String,
    },
    SyncFailed {
        account_id: String,
        error: String,
    },
}

pub fn spawn(job: Job, ctx: egui::Context, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let account_id = match &job {
            Job::Sync { account }
            | Job::SyncFolder { account, .. }
            | Job::MarkSeen { account, .. } => account.id.clone(),
        };
        let _ = tx.send(Event::SyncStarted {
            account_id: account_id.clone(),
        });
        let result = match job {
            Job::Sync { account } => match account.protocol {
                Protocol::Imap => sync_imap_inbox(&account, &tx),
                Protocol::Pop3 => sync_pop3(&account, &tx),
            },
            Job::SyncFolder { account, folder } => sync_imap_folder(&account, &folder, &tx),
            Job::MarkSeen {
                account,
                folder,
                uid,
            } => imap_mark_seen(&account, &folder, uid),
        };
        let _ = match result {
            Ok(()) => tx.send(Event::SyncDone { account_id }),
            Err(e) => tx.send(Event::SyncFailed {
                account_id,
                error: e.to_string(),
            }),
        };
        ctx.request_repaint();
    });
}

fn with_imap<T>(
    account: &Account,
    f: impl FnOnce(&mut ImapClient) -> MailResult<T>,
) -> MailResult<T> {
    let mut client = ImapClient::connect(account)?;
    let result = f(&mut client);
    client.logout();
    result
}

fn sync_imap_inbox(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        let folders = client
            .list_folders()
            .unwrap_or_else(|_| vec!["INBOX".to_string()]);
        let _ = tx.send(Event::Folders {
            account_id: account.id.clone(),
            folders,
        });
        fetch_folder_into(client, "INBOX", account, tx)
    })
}

fn sync_imap_folder(account: &Account, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        fetch_folder_into(client, folder, account, tx)
    })
}

fn fetch_folder_into(
    client: &mut ImapClient,
    folder: &str,
    account: &Account,
    tx: &Sender<Event>,
) -> MailResult<()> {
    let info = client.select(folder)?;
    let mut messages = Vec::new();
    if info.exists > 0 {
        let end = info.exists;
        let start = end.saturating_sub(FETCH_LIMIT - 1).max(1);
        for fetched in client.fetch_bodies(&format!("{start}:{end}"))? {
            let seen = fetched
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("\\Seen"));
            messages.push(mail::parse(&fetched.raw, &fetched.uid.to_string(), seen));
        }
    }
    let _ = tx.send(Event::Messages {
        account_id: account.id.clone(),
        folder: folder.to_string(),
        uidvalidity: info.uidvalidity,
        messages,
    });
    Ok(())
}

fn imap_mark_seen(account: &Account, folder: &str, uid: u32) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        client.select(folder)?;
        client.uid_store_flags_silent(uid, "+FLAGS.SILENT (\\Seen)")
    })
}

fn sync_pop3(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
    let mut client = Pop3Client::connect(account)?;
    let result = (|| -> MailResult<()> {
        client.login(&account.username, &account.password)?;
        let count = client.stat()?;
        let uidls = client.uidl()?;
        let start = count.saturating_sub(FETCH_LIMIT as usize) + 1;
        let mut messages = Vec::new();
        for (num, uid) in uidls {
            if num < start {
                continue;
            }
            let raw = client.retr(num)?;
            messages.push(mail::parse(&raw, &uid, false));
        }
        let _ = tx.send(Event::Messages {
            account_id: account.id.clone(),
            folder: "INBOX".to_string(),
            uidvalidity: None,
            messages,
        });
        Ok(())
    })();
    client.quit();
    result
}
