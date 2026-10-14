//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket, and it
//! never writes to the database either: these jobs do that (see `crate::db`).
//! Besides mail sync, jobs also fetch remote HTML-mail resources (images,
//! stylesheets) over HTTPS.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::db;
use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::{Account, MailMessage, Protocol};
use crate::protocol::imap::ImapClient;
use crate::protocol::pop3::Pop3Client;
use crate::storage;

/// How many of the newest messages the *first* sync of a folder takes. Later
/// syncs are incremental — the folder's marks say what is already stored — so
/// this is the size of the one-off backfill, not of a sync.
pub const BACKFILL_LIMIT: u32 = 50;

/// The lowest 1-based position a first sync reaches: the newest
/// `BACKFILL_LIMIT` messages of `total`. Everything older waits for a
/// `Load older mail` click — see `OLDER_BATCH`.
fn backfill_start(total: usize) -> usize {
    total.saturating_sub(BACKFILL_LIMIT as usize) + 1
}

/// How many older messages one `Load older mail` click fetches. The same size
/// as a folder's first fetch, so the list grows in the steps it started in.
pub const OLDER_BATCH: u32 = 50;

/// The UID range one click takes out of an IMAP folder: the batch below the
/// floor the folder has been fetched down to, or the server's oldest message,
/// whichever is closer — and `None` when there is nothing below the floor at
/// all, either because the floor is the first UID there is or because the
/// server's oldest message is one we already hold.
fn older_uids(floor: u32, oldest: u32, batch: u32) -> Option<(u32, u32)> {
    if floor <= 1 || oldest >= floor {
        return None;
    }
    Some((oldest.max(floor.saturating_sub(batch)), floor - 1))
}

/// The same for a POP3 drop, whose numbering always starts at 1: the only
/// question is how far above it the floor is. A floor below one batch's worth
/// takes the whole of what is left, which is why the subtraction saturates —
/// the first click of a folder that was backfilled from number 21 asks for
/// 1..20, not for an underflow.
fn older_numbers(floor: u32, batch: u32) -> Option<(u32, u32)> {
    if floor <= 1 {
        return None;
    }
    Some((floor.saturating_sub(batch).max(1), floor - 1))
}

/// The UID a fetch resumes from, or `None` when the folder has to be fetched
/// afresh. Both sides have to agree on the UIDVALIDITY generation: a mark from
/// another one describes mail that is gone, and a server that reports none does
/// not promise its UIDs survive the session — such a folder keeps the old
/// behaviour of refetching the newest messages rather than risking a skip.
fn resume_uid(mark: &db::FolderSync, selected: Option<u32>) -> Option<u32> {
    match (mark.uidvalidity, selected) {
        (Some(stored), Some(selected)) if stored == selected => {
            mark.uid_next.filter(|uid| *uid > 0)
        }
        _ => None,
    }
}

/// Hard caps for remote mail resources (images / stylesheets).
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_STYLESHEET_BYTES: u64 = 512 * 1024;
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(20);

pub enum Job {
    /// Folder list (IMAP) plus new mail: INBOX, every folder that has been
    /// fetched before and, when it is given, `folder` — the one the window is
    /// looking at, which a manual sync has to include because a folder with no
    /// mark yet is never part of the sweep. For POP3, the new messages of the
    /// drop. `purge` drops the account's stored mail before fetching — for a
    /// new account, or one whose endpoint just moved to another mailbox.
    Sync {
        account: Account,
        purge: bool,
        /// A folder to fetch even though it has no mark; ignored when the
        /// server no longer lists it.
        folder: Option<String>,
    },
    /// Fetch one specific IMAP folder.
    SyncFolder { account: Account, folder: String },
    /// Fetch the mail below the floor one folder has been fetched down to —
    /// the `Load older mail` button at the bottom of the list. The folder is
    /// asked how far down it goes first, so a click that finds nothing is one
    /// that reports it rather than one that fails.
    LoadOlder { account: Account, folder: String },
    /// Push `\Seen` back to the server (IMAP only) and store the flag in the
    /// database (both protocols; POP3 has no server-side flags).
    MarkSeen {
        account: Account,
        folder: String,
        uid: String,
    },
    /// Delete one message. The server is told first and the database row goes
    /// only once it has agreed (see `delete_message`), so nothing is ever
    /// dropped here while the server still has it.
    DeleteMessage {
        account: Account,
        folder: String,
        uid: String,
    },
    /// Fetch one message's original RFC 5322 source and write it into `dir` as
    /// `<subject>.eml` — the title bar's Export. The server's copy is the
    /// message whole (every header, every body part, every attachment), which
    /// is why this is a round trip rather than a rebuild from the database:
    /// what is stored here is the parsed mail, with the attachments by name
    /// only.
    ExportMessage {
        account: Account,
        folder: String,
        uid: String,
        /// The directory the file goes in (`storage::downloads_dir`).
        dir: PathBuf,
        /// What the file is named after: the subject, which the worker turns
        /// into a file name (`export_stem`).
        subject: String,
    },
    /// Remove an account: its row, its folders and, through the cascades, every
    /// message cached for it.
    DeleteAccount { account_id: String },
    /// Download and decode a remote image referenced by HTML mail.
    FetchImage { url: String },
    /// Download a remote stylesheet referenced by HTML mail.
    FetchStylesheet { url: String },
    /// Rasterize an HTML body with Blitz (see `crate::blitz_render`).
    /// Results come back as `Event::HtmlRendered`.
    RenderHtml {
        /// Identifies the page (message + geometry), so a result that arrives
        /// after the user moved on can be dropped.
        key: String,
        request: Box<crate::blitz_render::PageRequest>,
    },
    /// Enumerate the installed font families for the Preferences pickers
    /// (see `crate::fonts`). Results come back as `Event::FontsScanned`.
    ScanFonts,
    /// Run a search against the mail database (see `crate::search`). Results
    /// come back as `Event::SearchResults`, tagged with `seq`.
    Search {
        seq: u64,
        query: crate::search::SearchQuery,
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
    /// Whether the server has older mail below what one folder holds — the
    /// answer the list's `Load older mail` button is drawn from. Sent by every
    /// job that asks: a sync probes as it fetches, and a click answers as it
    /// loads, so the button is never stale for longer than one round trip.
    OlderMail {
        account_id: String,
        folder: String,
        available: bool,
    },
    /// One message is out of the server and out of the database, so the UI
    /// can drop its in-memory copy of it.
    MessageDeleted {
        account_id: String,
        folder: String,
        uid: String,
    },
    /// One message was written out as a `.eml`, at this path. The path is the
    /// one thing about an export the UI cannot know by itself: the name it
    /// asked for may already have been taken, and the file is then numbered.
    MessageExported {
        path: PathBuf,
    },
    ImageLoaded {
        url: String,
        width: u32,
        height: u32,
        /// RGBA pixels, `width * height * 4` bytes.
        rgba: Vec<u8>,
    },
    ImageFailed {
        url: String,
    },
    StylesheetLoaded {
        url: String,
        text: String,
    },
    StylesheetFailed {
        url: String,
    },
    /// A Blitz-rendered HTML body, ready to be uploaded as textures.
    HtmlRendered {
        key: String,
        page: crate::blitz_render::Page,
    },
    HtmlRenderFailed {
        key: String,
        error: String,
    },
    /// The installed font families, sorted by name.
    FontsScanned(Vec<crate::fonts::FontFamily>),
    /// The hits of one search. `seq` identifies the request: the UI drops an
    /// answer for a query it has already replaced.
    SearchResults {
        seq: u64,
        hits: Vec<crate::search::SearchHit>,
    },
    SearchFailed {
        seq: u64,
        error: String,
    },
}

pub fn spawn(job: Job, ctx: egui::Context, tx: Sender<Event>) {
    std::thread::spawn(move || match job {
        Job::FetchImage { url } => {
            let event = match fetch_image(&url) {
                Ok((width, height, rgba)) => Event::ImageLoaded {
                    url,
                    width,
                    height,
                    rgba,
                },
                Err(_) => Event::ImageFailed { url },
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        }
        Job::FetchStylesheet { url } => {
            let event = match fetch_text(&url) {
                Ok(text) => Event::StylesheetLoaded { url, text },
                Err(_) => Event::StylesheetFailed { url },
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        }
        Job::RenderHtml { key, request } => {
            match crate::blitz_render::render(&request) {
                Ok(Some(page)) => {
                    let _ = tx.send(Event::HtmlRendered { key, page });
                }
                // Superseded by a newer render while this one was running:
                // stay silent. The key may already belong to that newer
                // render, so reporting a failure would mislabel it.
                Ok(None) => return,
                Err(error) => {
                    let _ = tx.send(Event::HtmlRenderFailed { key, error });
                }
            }
            ctx.request_repaint();
        }
        Job::ScanFonts => {
            let _ = tx.send(Event::FontsScanned(crate::fonts::scan()));
            ctx.request_repaint();
        }
        Job::Search { seq, query } => {
            let event = match db::db().and_then(|db| crate::search::run(db.conn(), &query)) {
                Ok(hits) => Event::SearchResults { seq, hits },
                Err(error) => Event::SearchFailed {
                    seq,
                    error: error.to_string(),
                },
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        }
        Job::MarkSeen {
            account,
            folder,
            uid,
        } => {
            // Not a sync: no spinner for a flag write, and the local half is
            // stored whether or not the server accepts the push.
            if let Err(error) = mark_seen(&account, &folder, &uid) {
                let _ = tx.send(Event::SyncFailed {
                    account_id: account.id.clone(),
                    error: error.to_string(),
                });
            }
            ctx.request_repaint();
        }
        Job::DeleteAccount { account_id } => {
            if let Err(error) = db::delete_account(&account_id) {
                let _ = tx.send(Event::SyncFailed {
                    account_id,
                    error: error.to_string(),
                });
            }
            ctx.request_repaint();
        }
        Job::LoadOlder { account, folder } => {
            // A round trip like any other: the account's spinner says the
            // server is being asked while it happens.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            match load_older(&account, &folder, &tx) {
                Ok(()) => {
                    let _ = tx.send(Event::SyncDone { account_id });
                }
                Err(error) => {
                    let _ = tx.send(Event::SyncFailed {
                        account_id,
                        error: format!("older mail: {error}"),
                    });
                }
            }
            ctx.request_repaint();
        }
        Job::DeleteMessage {
            account,
            folder,
            uid,
        } => {
            // Not a sync, but it is a round trip: the account's spinner is
            // what says the mail is on its way out while the server is asked.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            match delete_message(&account, &folder, &uid) {
                Ok(()) => {
                    // The row leaves the list only now, with the database
                    // already short of it.
                    let _ = tx.send(Event::MessageDeleted {
                        account_id: account_id.clone(),
                        folder,
                        uid,
                    });
                    let _ = tx.send(Event::SyncDone { account_id });
                }
                // The local copy is still there — that is the point of the
                // order above — and the account carries the reason.
                Err(error) => {
                    let _ = tx.send(Event::SyncFailed {
                        account_id,
                        error: format!("delete: {error}"),
                    });
                }
            }
            ctx.request_repaint();
        }
        Job::ExportMessage {
            account,
            folder,
            uid,
            dir,
            subject,
        } => {
            // A round trip like a delete's, and bracketed the same way: the
            // account's spinner is what says a mail is being fetched while the
            // server is asked.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            match export_message(&account, &folder, &uid, &dir, &subject) {
                Ok(path) => {
                    // The file is on disk before the app is told where: the
                    // event is what the notice above the message list is drawn
                    // from.
                    let _ = tx.send(Event::MessageExported { path });
                    let _ = tx.send(Event::SyncDone { account_id });
                }
                // Nothing was written — a write that failed halfway took its
                // partial file with it — so the account carries the reason,
                // like it does for every other job that failed.
                Err(error) => {
                    let _ = tx.send(Event::SyncFailed {
                        account_id,
                        error: format!("export: {error}"),
                    });
                }
            }
            ctx.request_repaint();
        }
        account_job => {
            let account_id = match &account_job {
                Job::Sync { account, .. } | Job::SyncFolder { account, .. } => account.id.clone(),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts
                | Job::Search { .. }
                | Job::MarkSeen { .. }
                | Job::DeleteMessage { .. }
                | Job::ExportMessage { .. }
                | Job::LoadOlder { .. }
                | Job::DeleteAccount { .. } => unreachable!(),
            };
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            let result = match account_job {
                Job::Sync {
                    account,
                    purge,
                    folder,
                } => {
                    // The account row comes first: the mail below refers to it,
                    // and a brand-new account has to be durable before its mail
                    // is. Purging, when the endpoint moved, is part of the same
                    // job so nothing can reorder those steps.
                    db::save_account(&account).and_then(|()| {
                        if purge {
                            db::purge_mail(&account.id)?;
                        }
                        match account.protocol {
                            Protocol::Imap => sync_imap(&account, folder.as_deref(), &tx),
                            Protocol::Pop3 => sync_pop3(&account, &tx),
                        }
                    })
                }
                Job::SyncFolder { account, folder } => sync_imap_folder(&account, &folder, &tx),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts
                | Job::Search { .. }
                | Job::MarkSeen { .. }
                | Job::DeleteMessage { .. }
                | Job::ExportMessage { .. }
                | Job::LoadOlder { .. }
                | Job::DeleteAccount { .. } => unreachable!(),
            };
            let _ = match result {
                Ok(()) => tx.send(Event::SyncDone { account_id }),
                Err(e) => tx.send(Event::SyncFailed {
                    account_id,
                    error: e.to_string(),
                }),
            };
            ctx.request_repaint();
        }
    });
}

fn remote_url(url: &str) -> MailResult<()> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(MailError::Protocol(format!("refusing non-http url: {url}")))
    }
}

/// GET a URL with the standard timeouts and a byte cap. Shared with
/// `blitz_render`, which fetches mail resources while rasterizing.
pub(crate) fn http_get(url: &str, max_bytes: u64) -> MailResult<Vec<u8>> {
    remote_url(url)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(FETCH_CONNECT_TIMEOUT)
        .timeout_read(FETCH_READ_TIMEOUT)
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|e| MailError::Protocol(format!("http: {e}")))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(MailError::Protocol("resource exceeds size limit".into()));
    }
    Ok(bytes)
}

/// Fetch (or take from the disk cache) and decode a remote image into RGBA.
fn fetch_image(url: &str) -> MailResult<(u32, u32, Vec<u8>)> {
    let bytes = match storage::load_resource(url) {
        Some(cached) => cached,
        None => {
            let bytes = http_get(url, MAX_IMAGE_BYTES)?;
            storage::save_resource(url, &bytes);
            bytes
        }
    };
    let decoded = image::load_from_memory(&bytes)
        .map_err(|e| MailError::Protocol(format!("image decode: {e}")))?;
    let rgba = decoded.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err(MailError::Protocol(format!(
            "unreasonable image dimensions {width}x{height}"
        )));
    }
    Ok((width, height, rgba.into_raw()))
}

/// Fetch (or take from the disk cache) a remote stylesheet's text.
fn fetch_text(url: &str) -> MailResult<String> {
    let bytes = match storage::load_resource(url) {
        Some(cached) => cached,
        None => {
            let bytes = http_get(url, MAX_STYLESHEET_BYTES)?;
            storage::save_resource(url, &bytes);
            bytes
        }
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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

/// The folders one sync of an account sweeps, in order: the folder the window
/// is looking at when it was asked for — the column says "Not fetched yet —
/// click Sync" for a folder with no mark at all, and such a folder is never
/// part of the sweep by itself — then INBOX, which is where mail arrives, then
/// everything that has been fetched before, so a warm sync catches up instead
/// of refetching. A folder the server no longer lists is skipped; the mail
/// cached for it stays.
fn sync_queue(open: Option<&str>, listed: &[String], warmed: &[String]) -> Vec<String> {
    let mut queue: Vec<String> = Vec::new();
    if let Some(open) = open.filter(|open| listed.iter().any(|known| known == open)) {
        queue.push(open.to_string());
    }
    if !queue
        .iter()
        .any(|queued| queued.eq_ignore_ascii_case("INBOX"))
    {
        queue.push("INBOX".to_string());
    }
    for folder in warmed {
        if folder.eq_ignore_ascii_case("INBOX")
            || !listed.contains(folder)
            || queue.contains(folder)
        {
            continue;
        }
        queue.push(folder.clone());
    }
    queue
}

fn sync_imap(account: &Account, open: Option<&str>, tx: &Sender<Event>) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        // One folder that fails — a mailbox the server refuses, a connection
        // that dropped — must not cost the others their sync. The first error
        // is reported once the rest is done, and the mailbox list is one of the
        // things that can fail.
        let mut failure: Option<MailError> = None;
        let folders = match client.list_folders() {
            Ok(folders) => {
                // Remember the mailboxes first: a folder the database does not
                // know would refuse the mail fetched for it below (the messages
                // table has a foreign key onto folders). The answer also says
                // which folders the server still has — one it left out keeps
                // its cached mail and is drawn as one the server no longer
                // lists.
                db::save_folder_list(&account.id, &folders)?;
                let _ = tx.send(Event::Folders {
                    account_id: account.id.clone(),
                    folders: folders.clone(),
                });
                folders
            }
            // A `LIST` that failed is not a mailbox list, and it must not be
            // mistaken for one: the stored list stands — the column keeps
            // drawing it, unchanged — and the sweep falls back on the mailboxes
            // the last answer named, so a server that will not list its
            // mailboxes can still have their mail fetched, INBOX first.
            Err(error) => {
                failure = Some(error);
                db::listed_folders(&account.id)?
            }
        };
        let queue = sync_queue(open, &folders, &db::synced_folders(&account.id)?);
        for folder in queue {
            if let Err(error) = fetch_folder_into(client, &folder, account, tx) {
                failure = failure.or(Some(error));
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
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
    let resume = resume_uid(&db::folder_sync(&account.id, folder)?, info.uidvalidity);
    // How far down the mailbox goes is the server's own answer, and this is
    // where it is asked for: one `FETCH 1 (UID)` per folder, kept in the
    // database so the list knows about older mail without a round trip of its
    // own (see `Job::LoadOlder`).
    let oldest = client.oldest_uid()?;
    let mut floor: Option<u32> = None;
    let mut messages = Vec::new();
    if info.exists > 0 {
        // A folder that was fetched before is asked for the UIDs it does not
        // have yet: `<next>:*` is everything from the mark on. A server may
        // answer that with the last message when the mark is already past the
        // end, which the filter drops. A folder that was never fetched takes
        // the newest `BACKFILL_LIMIT` messages, by sequence number — and the
        // lowest UID of that window is the floor a `Load older mail` click
        // starts below.
        let fetched = match resume {
            Some(next) => client
                .fetch_bodies(&format!("{next}:*"), true)?
                .into_iter()
                .filter(|message| message.uid >= next)
                .collect(),
            None => {
                let start = backfill_start(info.exists as usize) as u32;
                let fetched = client.fetch_bodies(&format!("{start}:{}", info.exists), false)?;
                floor = fetched.iter().map(|message| message.uid).min();
                fetched
            }
        };
        for fetched in fetched {
            let seen = fetched
                .flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case("\\Seen"));
            messages.push(mail::parse(&fetched.raw, &fetched.uid.to_string(), seen));
        }
    }
    // Store before announcing: nothing may appear in the window that the
    // database does not already have. The batch moves the folder's mark in the
    // same transaction. A warm folder with nothing new is left untouched: no
    // write, and no event for a list that did not change.
    if resume.is_none() || !messages.is_empty() {
        db::save_messages(&account.id, folder, info.uidvalidity, &messages)?;
        let _ = tx.send(Event::Messages {
            account_id: account.id.clone(),
            folder: folder.to_string(),
            uidvalidity: info.uidvalidity,
            messages,
        });
    }
    // The two marks the button is drawn from, written after the batch and its
    // rotation: a window that was just fetched is the floor, and the answer is
    // what the server said. Nothing is reported from a generation the store
    // did not keep — a rotation clears both, and this puts the new ones back.
    if let Some(floor) = floor {
        db::set_first_num(&account.id, folder, floor)?;
    }
    db::set_oldest_uid(&account.id, folder, oldest)?;
    let mark = db::folder_sync(&account.id, folder)?;
    let _ = tx.send(Event::OlderMail {
        account_id: account.id.clone(),
        folder: folder.to_string(),
        available: db::older_available(mark.first_num, mark.oldest_uid),
    });
    Ok(())
}

/// Fetch the older mail of one folder: the batch below the floor it has been
/// fetched down to. IMAP names the range by UID, POP3 by message number.
fn load_older(account: &Account, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    match account.protocol {
        Protocol::Imap => load_older_imap(account, folder, tx),
        Protocol::Pop3 => load_older_pop3(account, tx),
    }
}

fn load_older_imap(account: &Account, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        let info = client.select(folder)?;
        let oldest = client.oldest_uid()?;
        let mark = db::folder_sync(&account.id, folder)?;
        // The stored UIDs are the address of the mail this folder holds. Under
        // another UIDVALIDITY they address different mail, and a batch fetched
        // now would replace them with a slice out of the middle of the mailbox:
        // the sync is what rotates a folder, so this only says what it found.
        let same_generation = matches!(
            (mark.uidvalidity, info.uidvalidity),
            (Some(stored), Some(selected)) if stored == selected
        );
        if !same_generation {
            return Err(MailError::Protocol(
                "the folder changed on the server — sync it before loading older mail".to_string(),
            ));
        }
        // The answer is recorded either way: the button is drawn from it, and
        // "this mailbox is empty" is an answer.
        db::set_oldest_uid(&account.id, folder, oldest)?;
        let mut more = false;
        let range = match (mark.first_num, oldest) {
            (Some(floor), Some(oldest)) => {
                older_uids(floor, oldest, OLDER_BATCH).map(|range| (range, oldest))
            }
            _ => None,
        };
        if let Some(((low, high), oldest)) = range {
            let fetched = client.fetch_bodies(&format!("{low}:{high}"), true)?;
            let mut messages = Vec::new();
            for fetched in fetched {
                let seen = fetched
                    .flags
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case("\\Seen"));
                messages.push(mail::parse(&fetched.raw, &fetched.uid.to_string(), seen));
            }
            // The floor moves whether or not the range held anything: it is
            // where this folder has been asked from, and a mailbox with gaps in
            // it would otherwise be asked for the same empty range forever.
            db::set_first_num(&account.id, folder, low)?;
            if !messages.is_empty() {
                db::save_messages(&account.id, folder, info.uidvalidity, &messages)?;
                let _ = tx.send(Event::Messages {
                    account_id: account.id.clone(),
                    folder: folder.to_string(),
                    uidvalidity: info.uidvalidity,
                    messages,
                });
            }
            more = low > oldest;
        }
        let _ = tx.send(Event::OlderMail {
            account_id: account.id.clone(),
            folder: folder.to_string(),
            available: more,
        });
        Ok(())
    })
}

/// Store the flag locally and, for IMAP, push it to the server. The local
/// write happens first: the mail is read either way, and a server that refuses
/// the `STORE` must not lose that.
fn mark_seen(account: &Account, folder: &str, uid: &str) -> MailResult<()> {
    db::set_seen(&account.id, folder, uid)?;
    if account.protocol == Protocol::Imap {
        let number = uid
            .parse::<u32>()
            .map_err(|_| MailError::Protocol(format!("not an IMAP UID: {uid}")))?;
        imap_mark_seen(account, folder, number)?;
    }
    Ok(())
}

fn imap_mark_seen(account: &Account, folder: &str, uid: u32) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        client.select(folder)?;
        client.uid_store_flags_silent(uid, "+FLAGS.SILENT (\\Seen)")
    })
}

/// Delete one message. The remote half goes first and the database row only
/// after it: mail that is still on the server must not vanish from here, or
/// the next sync would bring it back and the deletion would look like it
/// never happened. Mail the server no longer has is dropped locally — there
/// is nothing remote to delete, and the local copy is the only one left.
fn delete_message(account: &Account, folder: &str, uid: &str) -> MailResult<()> {
    match account.protocol {
        Protocol::Imap => imap_delete(account, folder, uid)?,
        Protocol::Pop3 => pop3_delete(account, uid)?,
    }
    db::delete_message(&account.id, folder, uid)
}

fn imap_delete(account: &Account, folder: &str, uid: &str) -> MailResult<()> {
    let number = uid
        .parse::<u32>()
        .map_err(|_| MailError::Protocol(format!("not an IMAP UID: {uid}")))?;
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        client.delete_message(folder, number)
    })
}

/// POP3 has no folders, so the UIDL is the whole address of the message. The
/// deletion is committed by `QUIT`, which `quit` sends whatever happened above
/// it — a session that ends any other way leaves the mail on the server.
fn pop3_delete(account: &Account, uid: &str) -> MailResult<()> {
    let mut client = Pop3Client::connect(account)?;
    let result = (|| -> MailResult<()> {
        client.login(&account.username, &account.password)?;
        client.delete_by_uidl(uid)
    })();
    client.quit();
    result
}

/// Export one message: its original source, as the server has it, written into
/// `dir` as `<subject>.eml`. The fetch is the whole job — what Kimo stores is
/// the parsed mail, with the attachments by name only, so an `.eml` rebuilt
/// from the database would not be the message the sender wrote. The other side
/// of that: mail the server no longer has cannot be exported at all, and says
/// so rather than writing something close.
fn export_message(
    account: &Account,
    folder: &str,
    uid: &str,
    dir: &Path,
    subject: &str,
) -> MailResult<PathBuf> {
    let raw = match account.protocol {
        Protocol::Imap => imap_body(account, folder, uid)?,
        Protocol::Pop3 => pop3_body(account, uid)?,
    };
    write_export(dir, &export_stem(subject), &raw)
}

fn imap_body(account: &Account, folder: &str, uid: &str) -> MailResult<Vec<u8>> {
    let number = uid
        .parse::<u32>()
        .map_err(|_| MailError::Protocol(format!("not an IMAP UID: {uid}")))?;
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        client.select(folder)?;
        client.uid_body(number)?.ok_or_else(mail_is_gone)
    })
}

/// POP3 has no folders, so the UIDL is the whole address of the message. The
/// `QUIT` at the end is what *would* commit a deletion, and none was sent: the
/// session ends without touching the drop.
fn pop3_body(account: &Account, uid: &str) -> MailResult<Vec<u8>> {
    let mut client = Pop3Client::connect(account)?;
    let result = (|| -> MailResult<Vec<u8>> {
        client.login(&account.username, &account.password)?;
        client.retr_by_uidl(uid)?.ok_or_else(mail_is_gone)
    })();
    client.quit();
    result
}

/// The server does not answer for this message any more. There is no original
/// left to export, and the local copy is not a substitute for one.
fn mail_is_gone() -> MailError {
    MailError::Protocol("the server no longer has this mail".to_string())
}

/// How many bytes of a subject a file name keeps. A file name is 255 bytes on
/// the filesystems Kimo writes to, and the `.eml` suffix and a `-12` counter
/// have to fit in that with it.
const EXPORT_STEM_BYTES: usize = 100;

/// How many names one export tries before giving up: `<stem>.eml`, then
/// `<stem>-2.eml` and on. Nobody has a thousand mails with one subject, and the
/// search has to end somewhere.
const EXPORT_NAME_TRIES: u32 = 1000;

/// A subject as a file name: what a file name cannot carry — the two path
/// separators, the characters Windows forbids, control characters — becomes a
/// space, runs of them collapse into one, and the ends lose their dots and
/// spaces. `slug` is the wrong tool here: it lowercases and ASCII-fies, which
/// would throw away every non-Latin subject ("发票号码 12345" is a file the user
/// recognises; "account" is not). A subject with no usable character left still
/// needs a name, and the numbering tells one such mail from the next.
fn export_stem(subject: &str) -> String {
    let mut stem = String::new();
    for c in subject.chars() {
        let c = if c.is_control()
            || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        {
            ' '
        } else {
            c
        };
        if c == ' ' && (stem.is_empty() || stem.ends_with(' ')) {
            continue;
        }
        if stem.len() + c.len_utf8() > EXPORT_STEM_BYTES {
            break;
        }
        stem.push(c);
    }
    let stem = stem.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if stem.is_empty() {
        "mail".to_string()
    } else {
        stem.to_string()
    }
}

/// Write `raw` into `dir` as `<stem>.eml`, numbered if that name is taken, and
/// answer with the path it went to. The file is *created* rather than merely
/// asked about, so two exports of the same mail cannot land on one name: the
/// loser of that race moves to the next number instead of overwriting the
/// winner. A write that fails halfway takes its partial file with it — a
/// half-written file with a `.eml` suffix looks like a mail and is not one.
fn write_export(dir: &Path, stem: &str, raw: &[u8]) -> MailResult<PathBuf> {
    fs::create_dir_all(dir)?;
    for n in 1..=EXPORT_NAME_TRIES {
        let name = if n == 1 {
            format!("{stem}.eml")
        } else {
            format!("{stem}-{n}.eml")
        };
        let path = dir.join(name);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file.write_all(raw) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        return Ok(path);
    }
    Err(MailError::Storage(format!(
        "no free file name for {stem}.eml in {}",
        dir.display()
    )))
}

fn load_older_pop3(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
    let mut client = Pop3Client::connect(account)?;
    let result = (|| -> MailResult<()> {
        client.login(&account.username, &account.password)?;
        let uidls = client.uidl()?;
        let mark = db::folder_sync(&account.id, "INBOX")?;
        let mut more = false;
        if let Some((low, high)) = mark
            .first_num
            .and_then(|floor| older_numbers(floor, OLDER_BATCH))
        {
            let known = db::stored_uids(&account.id, "INBOX")?;
            let mut messages = Vec::new();
            for (num, uid) in uidls {
                let num = num as u32;
                if num < low || num > high || known.contains(&uid) {
                    continue;
                }
                let raw = client.retr(num as usize)?;
                messages.push(mail::parse(&raw, &uid, false));
            }
            // As on the IMAP side: the floor moves even when the range held
            // nothing, or the next click would ask for the same numbers again.
            db::set_first_num(&account.id, "INBOX", low)?;
            if !messages.is_empty() {
                db::save_messages(&account.id, "INBOX", None, &messages)?;
                let _ = tx.send(Event::Messages {
                    account_id: account.id.clone(),
                    folder: "INBOX".to_string(),
                    uidvalidity: None,
                    messages,
                });
            }
            more = low > 1;
        }
        let _ = tx.send(Event::OlderMail {
            account_id: account.id.clone(),
            folder: "INBOX".to_string(),
            available: more,
        });
        Ok(())
    })();
    client.quit();
    result
}

fn sync_pop3(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
    let mut client = Pop3Client::connect(account)?;
    let result = (|| -> MailResult<()> {
        client.login(&account.username, &account.password)?;
        let count = client.stat()?;
        let uidls = client.uidl()?;
        let mark = db::folder_sync(&account.id, "INBOX")?;
        let known = db::stored_uids(&account.id, "INBOX")?;
        // The first sync of a drop takes the newest messages only; the floor it
        // remembers is what keeps every later sync from coming back for the
        // thousands it deliberately skipped. UIDL identity is what makes those
        // later syncs incremental — a message already stored is never
        // downloaded again, whatever its number. The newest-messages window is
        // still consulted, so a drop that shrank below the floor (mail deleted
        // elsewhere, and the numbering with it) cannot disappear.
        let cold = mark.first_num.is_none();
        let window = backfill_start(count);
        let floor = mark
            .first_num
            .map_or(window, |floor| (floor as usize).min(window));
        let mut messages = Vec::new();
        for (num, uid) in uidls {
            if num < floor || known.contains(&uid) {
                continue;
            }
            let raw = client.retr(num)?;
            messages.push(mail::parse(&raw, &uid, false));
        }
        // The floor goes first: a folder with a floor and no mail is fetched
        // from that floor next time, so this order can cost a repeated pass but
        // never a message. A drop's numbering starts at 1, so it is also where
        // the server's oldest message is — the answer the `Load older mail`
        // button is drawn from, and the one thing about a drop nobody has to
        // ask for.
        if cold {
            db::set_first_num(&account.id, "INBOX", window as u32)?;
            db::set_oldest_uid(&account.id, "INBOX", (count > 0).then_some(1))?;
        }
        if cold || !messages.is_empty() {
            db::save_messages(&account.id, "INBOX", None, &messages)?;
            let _ = tx.send(Event::Messages {
                account_id: account.id.clone(),
                folder: "INBOX".to_string(),
                uidvalidity: None,
                messages,
            });
        }
        let mark = db::folder_sync(&account.id, "INBOX")?;
        let _ = tx.send(Event::OlderMail {
            account_id: account.id.clone(),
            folder: "INBOX".to_string(),
            available: db::older_available(mark.first_num, mark.oldest_uid),
        });
        Ok(())
    })();
    client.quit();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_sync_takes_the_newest_messages() {
        assert_eq!(backfill_start(0), 1);
        assert_eq!(backfill_start(1), 1);
        assert_eq!(backfill_start(BACKFILL_LIMIT as usize), 1);
        assert_eq!(backfill_start(BACKFILL_LIMIT as usize + 1), 2);
        assert_eq!(backfill_start(5000), 5000 - BACKFILL_LIMIT as usize + 1);
    }

    /// What one `Load older mail` click asks an IMAP folder for: the batch
    /// below the floor, the floor being the oldest UID ever fetched. The
    /// server's own oldest message is where the walk stops — asking below it
    /// would be asking for mail that is not there — and a mailbox whose oldest
    /// message is one already held has nothing to load at all.
    #[test]
    fn a_click_asks_for_the_batch_below_the_floor() {
        // A floor in the middle of a mailbox: one batch, and no further.
        assert_eq!(older_uids(5000, 1, 50), Some((4950, 4999)));
        // A mailbox whose mail starts just below the floor: only what there is.
        assert_eq!(older_uids(5000, 4975, 50), Some((4975, 4999)));
        // And then the bottom, which is the end of the walk.
        assert_eq!(older_uids(4975, 4975, 50), None);
        assert_eq!(older_uids(4975, 4976, 50), None, "deleted elsewhere");
        assert_eq!(older_uids(1, 1, 50), None, "the floor is the first UID");
        // A floor of 2 asks for UID 1 alone, whatever the batch size.
        assert_eq!(older_uids(2, 1, 50), Some((1, 1)));
        // `older_uids` never proposes UID 0: the range starts at the first UID
        // the protocol has, whatever the arithmetic below the batch says.
        assert_eq!(older_uids(2, 1, 500), Some((1, 1)));
    }

    /// The same walk for a POP3 drop, whose numbering starts at 1 — so the
    /// floor alone says how much is left below it, and the walk ends there.
    #[test]
    fn a_drop_is_walked_down_to_its_first_message() {
        assert_eq!(older_numbers(4951, 50), Some((4901, 4950)));
        assert_eq!(older_numbers(60, 50), Some((10, 59)));
        assert_eq!(older_numbers(51, 50), Some((1, 50)));
        assert_eq!(older_numbers(50, 50), Some((1, 49)));
        // A floor *below* one batch — a drop that was backfilled from number
        // 21, say — takes what is left instead of underflowing.
        assert_eq!(older_numbers(21, 50), Some((1, 20)));
        assert_eq!(older_numbers(2, 50), Some((1, 1)));
        assert_eq!(older_numbers(1, 50), None, "the drop starts at number 1");
        assert_eq!(older_numbers(0, 50), None);
    }

    /// The order a sync sweeps folders in: what the user asked for, INBOX, then
    /// the warm ones — each asked for once, and only if the server still lists
    /// it.
    #[test]
    fn a_sync_sweeps_the_open_folder_then_inbox_then_the_warm_ones() {
        let listed: Vec<String> = ["INBOX", "Archive", "Confidential"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let warm = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sync_queue(None, &listed, &warm(&["Archive"])),
            ["INBOX", "Archive"]
        );
        // The folder the window is looking at goes first: it is the one the
        // user just asked for, and it has no mark, so the warm list is not
        // where it would have come from.
        assert_eq!(
            sync_queue(Some("Confidential"), &listed, &warm(&["Archive"])),
            ["Confidential", "INBOX", "Archive"]
        );
        // Not twice when it is warm as well, and INBOX not twice when it is the
        // open folder.
        assert_eq!(
            sync_queue(Some("Archive"), &listed, &warm(&["Archive"])),
            ["Archive", "INBOX"]
        );
        assert_eq!(sync_queue(Some("INBOX"), &listed, &[]), ["INBOX"]);
        // A folder the server no longer lists is asked for nowhere: neither the
        // open one nor a warm one.
        assert_eq!(
            sync_queue(Some("Gone"), &listed, &warm(&["Old"])),
            ["INBOX"]
        );
    }

    #[test]
    fn a_folder_resumes_from_its_mark_unless_the_uidvalidity_moved() {
        let mark = db::FolderSync {
            uidvalidity: Some(7),
            uid_next: Some(13),
            first_num: None,
            oldest_uid: None,
        };
        assert_eq!(resume_uid(&mark, Some(7)), Some(13));
        // Another generation: the stored UIDs describe mail that is gone.
        assert_eq!(resume_uid(&mark, Some(8)), None);
        // A server that reports no UIDVALIDITY promises nothing about its UIDs
        // surviving, so the folder is fetched afresh, as it always was.
        assert_eq!(resume_uid(&mark, None), None);

        // Nothing stored, a mark without a generation, or a nonsense UID.
        assert_eq!(resume_uid(&db::FolderSync::default(), Some(7)), None);
        let generation_only = db::FolderSync {
            uidvalidity: Some(7),
            ..Default::default()
        };
        assert_eq!(resume_uid(&generation_only, Some(7)), None);
        let nonsense = db::FolderSync {
            uidvalidity: Some(7),
            uid_next: Some(0),
            ..Default::default()
        };
        assert_eq!(resume_uid(&nonsense, Some(7)), None, "UID 0 is no UID");
    }

    /// An exported file is named after the subject, which keeps its own
    /// characters — non-Latin ones included — and loses only what a file name
    /// cannot carry.
    #[test]
    fn an_export_is_named_after_the_subject() {
        assert_eq!(export_stem("Invoice 42"), "Invoice 42");
        assert_eq!(export_stem("发票号码 12345"), "发票号码 12345");
        // Path separators and the characters Windows forbids, each becoming
        // one space rather than a gap per character.
        assert_eq!(
            export_stem("Re: a/b\\c:d*e?f\"g<h>i|j"),
            "Re a b c d e f g h i j"
        );
        // A newline would end up in a terminal or a shell's output.
        assert_eq!(export_stem("a\nb\tc"), "a b c");
        // Nothing usable left: the file still needs a name.
        assert_eq!(export_stem(""), "mail");
        assert_eq!(export_stem("   "), "mail");
        assert_eq!(export_stem(".. .."), "mail");
        // Hidden files and trailing dots are a file name's own conventions.
        assert_eq!(export_stem("..hidden.."), "hidden");
        assert_eq!(export_stem("v1.2 release."), "v1.2 release");
        // The suffix and a counter have to fit in a 255-byte file name. The
        // cap is on bytes and a character is never cut in half, so the name
        // ends just below it rather than exactly on it ("日" is three bytes).
        let long = export_stem(&"日".repeat(200));
        assert!(long.len() <= EXPORT_STEM_BYTES, "{} bytes", long.len());
        assert!(long.len() > EXPORT_STEM_BYTES - 4, "{} bytes", long.len());
        assert!(long.chars().all(|c| c == '日'));
    }

    /// The bytes go to disk as they came off the server, and a name that is
    /// taken is numbered rather than overwritten: exporting the same mail twice
    /// is two files, not one that replaced the other.
    #[test]
    fn writing_an_export_numbers_a_name_that_is_taken() {
        let dir = std::env::temp_dir().join(format!("kimo-export-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let first = write_export(&dir, "Invoice 42", b"one").unwrap();
        let second = write_export(&dir, "Invoice 42", b"two").unwrap();
        assert_eq!(first, dir.join("Invoice 42.eml"));
        assert_eq!(second, dir.join("Invoice 42-2.eml"));
        assert_eq!(fs::read(&first).unwrap(), b"one");
        assert_eq!(fs::read(&second).unwrap(), b"two");
        // The directory is made rather than refused when it is not there yet.
        assert!(dir.is_dir());

        fs::remove_dir_all(&dir).unwrap();
    }
}
