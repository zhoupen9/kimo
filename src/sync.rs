//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket, and it
//! never writes to the database either: these jobs do that (see `crate::db`).
//! Besides mail sync, jobs also send written mail over SMTP (`Job::SendMessage`)
//! and fetch remote HTML-mail resources (images, stylesheets) over HTTPS.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::db;
use crate::error::{MailError, MailResult};
use crate::folders::{self, FolderKind};
use crate::mail;
use crate::models::{Account, MailMessage, Protocol};
use crate::outgoing::Outgoing;
use crate::protocol::imap::{decode_mailbox_name, ImapClient};
use crate::protocol::pop3::Pop3Client;
use crate::protocol::smtp::SmtpClient;
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
    /// Put exactly these labels on one message, creating any that is new
    /// (`db::set_labels`). The popup's "create a label for this mail" is this
    /// job and nothing else, and a label removed from the mail is a call with
    /// the set it was removed from.
    ///
    /// Not a sync and not a round trip: labels are the user's own marks and
    /// never the server's, so nothing is asked of anybody but the database —
    /// and the app has already made the change in memory, which is why nothing
    /// comes back on success.
    SetLabels {
        account_id: String,
        folder: String,
        uid: String,
        labels: Vec<String>,
    },
    /// Delete messages of one folder — the title bar's Delete, which takes the
    /// mail the reading pane shows or, in select mode, every mail the list has
    /// checked. The server is told first and each database row goes only once
    /// it has agreed (see `delete_messages`), so nothing is ever dropped here
    /// while the server still has it.
    DeleteMessage {
        account: Account,
        folder: String,
        uids: Vec<String>,
    },
    /// Fetch one message's original RFC 5322 source and write it into `dir` as
    /// `<subject>.eml` — the title bar's Export. The server's copy is the
    /// message whole (every header, every body part, every attachment), which
    /// is why this is a round trip rather than a rebuild from the database:
    /// what is stored here is the parsed mail, with each attachment's name,
    /// type and size and never its bytes.
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
    /// Download one attachment of one message into `dir`: the mail's original
    /// source is fetched, the part is taken out of it and its bytes are written
    /// as the file the sender named — the click on an attachment card.
    ///
    /// A round trip, like an export's, and for the same reason: what is stored
    /// here is the parsed mail, with each attachment's name, type and size but
    /// never its bytes, so the file itself only exists on the server.
    DownloadAttachment {
        account: Account,
        folder: String,
        uid: String,
        /// Which attachment, as the reading pane enumerated the mail's parts.
        /// It is the same order the database stored them in (`ordinal`), and
        /// the worker re-parses the original to find the part again.
        ordinal: usize,
        /// The name the card showed, which is what the part is checked against
        /// and what the file is named after (`download_name`).
        name: String,
        /// The directory the file goes in (`storage::downloads_dir`).
        dir: PathBuf,
    },
    /// Send one written mail (`crate::outgoing`) through the account's SMTP
    /// server and, when the account has a Sent mailbox to keep it in, put the
    /// copy there too.
    ///
    /// The compose card that asked owns the outcome — `Event::MessageSent`, or
    /// `Event::MessageSendFailed` with the server's own words — so a failed
    /// send is never reported as a failed *sync* on the account.
    SendMessage {
        account: Account,
        mail: Box<Outgoing>,
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
    /// One folder's mail as a fetch found it. `new_mail` is what tells the
    /// window whether this batch is *news* — mail that arrived since the folder
    /// was last fetched (an IMAP fetch resumed at the folder's mark, or POP3
    /// UIDLs the database did not hold) — or mail the fetch went looking for:
    /// a folder's first fill, the batch below it that `Load older mail` asked
    /// for, or the copy of a mail that was just sent. It is what a desktop
    /// notification is raised from (`crate::notify`), which is why a backfill
    /// of fifty mails must not arrive as fifty notifications.
    Messages {
        account_id: String,
        folder: String,
        uidvalidity: Option<u32>,
        messages: Vec<MailMessage>,
        new_mail: bool,
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
    /// One attachment was written out, at this path — the notice above the
    /// message list says where. Like an export's path, this is the one thing
    /// the UI cannot know by itself: a name that was taken is numbered.
    AttachmentDownloaded {
        path: PathBuf,
    },
    /// The mail is out. `recipients` is everybody the envelope went to, which
    /// is what the notice above the message list names; `copy` is what became
    /// of the copy in the account's Sent mailbox.
    MessageSent {
        recipients: Vec<String>,
        copy: SentCopy,
    },
    /// The mail did not go out, with the server's own words when it was the
    /// server that said no. Nothing is queued and nothing is retried: the
    /// draft is still in the compose card, and Send is there to be pressed
    /// again.
    MessageSendFailed {
        error: String,
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

/// What became of the copy of a mail that was just sent.
#[derive(Debug)]
pub enum SentCopy {
    /// Nothing was appended: a POP3 account, which has no folders to append
    /// to, or an IMAP server whose folder list names no Sent mailbox.
    None,
    /// The copy is in this mailbox (IMAP wire form; the UI decodes it for
    /// display) and has been fetched back into the database.
    Saved { mailbox: String },
    /// The mail went out but the copy could not be stored. The notice says so —
    /// a sent mail whose copy is missing is worth knowing about, and there is
    /// nothing to do about it here.
    Failed(String),
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
        Job::SetLabels {
            account_id,
            folder,
            uid,
            labels,
        } => {
            // Not a sync: no spinner for a write of the user's own marks. The
            // failure is reported on the account's row the way a flag write's
            // is, and the app's copy of the labels stands: what did not reach
            // the disk is still what the user asked for.
            if let Err(error) = db::set_labels(&account_id, &folder, &uid, &labels) {
                let _ = tx.send(Event::SyncFailed {
                    account_id,
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
            uids,
        } => {
            // Not a sync, but it is a round trip: the account's spinner is
            // what says the mail is on its way out while the server is asked.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            let (deleted, error) = delete_messages(&account, &folder, &uids);
            // The rows leave the list only now, with the database already
            // short of them. Each one is its own event: a batch that went
            // halfway is a batch that went halfway.
            for uid in deleted {
                let _ = tx.send(Event::MessageDeleted {
                    account_id: account_id.clone(),
                    folder: folder.clone(),
                    uid,
                });
            }
            match error {
                // The local copy of a mail the server refused is still there —
                // that is the point of the order above — and the account
                // carries the first reason.
                Some(error) => {
                    let _ = tx.send(Event::SyncFailed {
                        account_id,
                        error: format!("delete: {error}"),
                    });
                }
                None => {
                    let _ = tx.send(Event::SyncDone { account_id });
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
        Job::DownloadAttachment {
            account,
            folder,
            uid,
            ordinal,
            name,
            dir,
        } => {
            // Bracketed like an export: the account's spinner is what says a
            // round trip is in flight while the server is asked for the mail.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            match download_attachment(&account, &folder, &uid, ordinal, &name, &dir) {
                Ok(path) => {
                    let _ = tx.send(Event::AttachmentDownloaded { path });
                    let _ = tx.send(Event::SyncDone { account_id });
                }
                // Nothing was written, and a write that failed halfway took its
                // partial file with it.
                Err(error) => {
                    let _ = tx.send(Event::SyncFailed {
                        account_id,
                        error: format!("attachment: {error}"),
                    });
                }
            }
            ctx.request_repaint();
        }
        Job::SendMessage { account, mail } => {
            // Not bracketed with SyncStarted/SyncDone like the other round
            // trips: the card that asked for the send is on screen, cannot be
            // closed while the mail is in flight, and says "Sending…" itself.
            // A failure is that card's to show, not the account row's.
            match send_message(&account, &mail, &tx) {
                Ok((recipients, copy)) => {
                    let _ = tx.send(Event::MessageSent { recipients, copy });
                }
                Err(error) => {
                    let _ = tx.send(Event::MessageSendFailed {
                        error: error.to_string(),
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
                | Job::DownloadAttachment { .. }
                | Job::SendMessage { .. }
                | Job::LoadOlder { .. }
                | Job::SetLabels { .. }
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
                | Job::DownloadAttachment { .. }
                | Job::SendMessage { .. }
                | Job::LoadOlder { .. }
                | Job::SetLabels { .. }
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
            // A fetch that resumed at the folder's mark asked for exactly the
            // UIDs it did not have: what came back arrived since the last one.
            // A folder filled for the first time (or after a UIDVALIDITY
            // rotation) is the folder's backfill, not news.
            new_mail: resume.is_some(),
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
                    // Older mail: whatever the click was for, it is not news.
                    new_mail: false,
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

/// Delete messages of one folder: the remote half goes first and each database
/// row only after it, so mail that is still on the server never vanishes from
/// here — the next sync would bring it back and the deletion would look like it
/// never happened. Mail the server no longer has is dropped locally: there is
/// nothing remote to delete, and the local copy is the only one left.
///
/// The rule is applied one mail at a time, and the two answers are what came of
/// it: the uids whose local rows are gone (their remote copies are too), and
/// the first refusal. The rest of the run is asked anyway — a batch that
/// stopped at the first failure would leave mail checked on screen with
/// nothing said about the mails it never got to — and one connection carries
/// the whole batch, because a click that picked twenty mails should not be
/// twenty logins.
fn delete_messages(
    account: &Account,
    folder: &str,
    uids: &[String],
) -> (Vec<String>, Option<MailError>) {
    match account.protocol {
        Protocol::Imap => imap_delete_many(account, folder, uids),
        Protocol::Pop3 => pop3_delete_many(account, folder, uids),
    }
}

/// The IMAP half of `delete_messages`: one login, one LIST and one SELECT, and
/// each uid then takes the probe, flag and expunge
/// `ImapClient::delete_messages` gives it.
fn imap_delete_many(
    account: &Account,
    folder: &str,
    uids: &[String],
) -> (Vec<String>, Option<MailError>) {
    let mut numbers = Vec::with_capacity(uids.len());
    let mut first_error = None;
    for uid in uids {
        match uid.parse::<u32>() {
            Ok(number) => numbers.push((uid.as_str(), number)),
            // A uid the protocol cannot address is a mail that cannot go, and
            // its local copy stays with it.
            Err(_) => {
                first_error = first_error
                    .or_else(|| Some(MailError::Protocol(format!("not an IMAP UID: {uid}"))))
            }
        }
    }
    if numbers.is_empty() {
        return (Vec::new(), first_error);
    }
    let asked: Vec<u32> = numbers.iter().map(|(_, number)| *number).collect();
    let (gone, error) = match with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        Ok(client.delete_messages(folder, &asked))
    }) {
        Ok(answer) => answer,
        // The connection itself failed, so the server was never asked: no mail
        // went, and every local copy stays where it is.
        Err(error) => (Vec::new(), Some(error)),
    };
    let mut first_error = first_error.or(error);
    let mut deleted = Vec::new();
    for (uid, number) in numbers {
        if !gone.contains(&number) {
            continue;
        }
        match db::delete_message(&account.id, folder, uid) {
            Ok(()) => deleted.push(uid.to_string()),
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    (deleted, first_error)
}

/// The POP3 half of `delete_messages`. POP3 has no folders, so the UIDL is the
/// whole address of a message, and the deletion is committed by `QUIT`: every
/// local row goes once the session has ended, never before, because a session
/// that ends any other way leaves the mail on the server (`quit` sends `QUIT`
/// whatever happened above it).
fn pop3_delete_many(
    account: &Account,
    folder: &str,
    uids: &[String],
) -> (Vec<String>, Option<MailError>) {
    let mut client = match Pop3Client::connect(account) {
        Ok(client) => client,
        Err(error) => return (Vec::new(), Some(error)),
    };
    if let Err(error) = client.login(&account.username, &account.password) {
        // Nothing can be deleted in a session the server did not open; it is
        // still ended the polite way.
        client.quit();
        return (Vec::new(), Some(error));
    }
    let mut marked = Vec::new();
    let mut first_error = None;
    for uid in uids {
        match client.delete_by_uidl(uid) {
            Ok(()) => marked.push(uid.clone()),
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    client.quit();
    let mut deleted = Vec::new();
    for uid in marked {
        match db::delete_message(&account.id, folder, &uid) {
            Ok(()) => deleted.push(uid),
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    (deleted, first_error)
}

/// Export one message: its original source, as the server has it, written into
/// `dir` as `<subject>.eml`. The fetch is the whole job — what Kimo stores is
/// the parsed mail, with each attachment's name, type and size and never its
/// bytes, so an `.eml` rebuilt
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
    let raw = original_source(account, folder, uid)?;
    write_file(dir, &export_stem(subject), ".eml", &raw)
}

/// Download one attachment: the mail's original source, the part the card
/// named, and the file it becomes, written into `dir`.
///
/// Both halves can refuse, and each says which half it was. The server may no
/// longer have the mail at all (`original_source`), and the part may no longer
/// be where the card said it was — a different mail under the same uid, a
/// re-parse that counts the parts differently — in which case there is no file
/// to hand over and saying so beats writing whatever part *is* there.
fn download_attachment(
    account: &Account,
    folder: &str,
    uid: &str,
    ordinal: usize,
    name: &str,
    dir: &Path,
) -> MailResult<PathBuf> {
    let raw = original_source(account, folder, uid)?;
    let bytes = mail::attachment_bytes(&raw, ordinal, name).ok_or_else(|| {
        MailError::Protocol(format!("the server's copy of this mail has no {name}"))
    })?;
    let (stem, extension) = download_name(name);
    write_file(dir, &stem, &extension, &bytes)
}

/// One message's original RFC 5322 source, as the server has it — the fetch an
/// export and a download both start with. What Kimo stores is the parsed mail,
/// with each attachment's name, type and size but never its bytes, so this is
/// the only way back to what the sender sent; the other side of it is that mail
/// the server no longer has cannot be exported or downloaded at all.
fn original_source(account: &Account, folder: &str, uid: &str) -> MailResult<Vec<u8>> {
    match account.protocol {
        Protocol::Imap => imap_body(account, folder, uid),
        Protocol::Pop3 => pop3_body(account, uid),
    }
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

/// Send one written mail over SMTP and, when the account can keep a copy, put
/// it in the server's Sent mailbox.
///
/// Answers the recipients the envelope went to and what became of the copy. The
/// address check here is the card's own rule run again: nothing leaves this
/// machine as mail on the strength of a caller having checked it.
fn send_message(
    account: &Account,
    mail: &Outgoing,
    tx: &Sender<Event>,
) -> MailResult<(Vec<String>, SentCopy)> {
    let invalid = mail.invalid();
    if !invalid.is_empty() {
        return Err(MailError::Protocol(format!(
            "not an address: {}",
            invalid.join(", ")
        )));
    }
    let Some(smtp) = &account.smtp else {
        return Err(MailError::Protocol(
            "this account has no outgoing server".to_string(),
        ));
    };
    let recipients: Vec<String> = mail.recipients().into_iter().map(str::to_string).collect();
    // The bytes are built once, and the same ones go to the server and — when
    // there is somewhere to keep it — into the Sent mailbox: a copy that
    // differs from what was delivered is worse than no copy at all.
    let raw = crate::outgoing::render(mail, unix_now());
    let mut client = SmtpClient::connect(&smtp.host, smtp.port, smtp.tls)?;
    // No username is a submission server that wants no login — a local relay,
    // or a server that authenticates by address alone.
    if let Some((user, pass)) = account.smtp_login() {
        if !user.is_empty() {
            client.login(user, pass)?;
        }
    }
    client.send(&mail.from_addr, &recipients, &raw)?;
    client.quit();
    let copy = append_to_sent(account, &raw, tx);
    Ok((recipients, copy))
}

/// The current time in unix seconds: what the `Date:` header says. A clock
/// before the epoch answers 0, which is the one wrong date a mail can carry
/// without anybody minding.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// The copy in the account's Sent mailbox, or why there is none.
///
/// POP3 has nowhere to put one: the drop is what arrives, not what leaves. An
/// IMAP server that lists no Sent mailbox gets none either — a copy filed under
/// a name the user does not recognize is not a copy they will find.
fn append_to_sent(account: &Account, raw: &[u8], tx: &Sender<Event>) -> SentCopy {
    if account.protocol != Protocol::Imap {
        return SentCopy::None;
    }
    let mailbox = match db::listed_folders(&account.id).map(|listed| sent_mailbox_in(&listed)) {
        Ok(Some(mailbox)) => mailbox,
        // Nothing to append to, and nothing wrong either.
        Ok(None) => return SentCopy::None,
        Err(error) => return SentCopy::Failed(error.to_string()),
    };
    match imap_save_to_sent(account, &mailbox, raw, tx) {
        Ok(()) => SentCopy::Saved { mailbox },
        Err(error) => SentCopy::Failed(error.to_string()),
    }
}

/// The mailbox a sent copy belongs in, from the folders the last `LIST` named:
/// the first name whose *decoded* form classifies as Sent — `crate::folders`
/// is the table that knows `Sent`, `Sent Items` and 已发送邮件 alike. The
/// answer is the wire form, which is what `APPEND` has to be given.
fn sent_mailbox_in(listed: &[String]) -> Option<String> {
    listed
        .iter()
        .find(|name| folders::kind_of(&decode_mailbox_name(name)) == FolderKind::Sent)
        .cloned()
}

/// Put the copy in the Sent mailbox and fetch it back: what was sent is in the
/// folder the user looks in now, not at the next sync.
///
/// The fetch is a folder's ordinary one (`fetch_folder_into`), run on the
/// session that just appended — the appended message is newer than the folder's
/// mark, so it is exactly what comes down, and the mail arrives as
/// `Event::Messages` like any other fetch.
fn imap_save_to_sent(
    account: &Account,
    mailbox: &str,
    raw: &[u8],
    tx: &Sender<Event>,
) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        client.append(mailbox, raw)?;
        fetch_folder_into(client, mailbox, account, tx)
    })
}

/// How many bytes of a subject a file name keeps. A file name is 255 bytes on
/// the filesystems Kimo writes to, and the `.eml` suffix and a `-12` counter
/// have to fit in that with it.
const EXPORT_STEM_BYTES: usize = 100;

/// How many names one write tries before giving up: `<stem>.eml`, then
/// `<stem>-2.eml` and on (or the same for a downloaded file's own extension).
/// Nobody has a thousand mails with one subject, or a thousand downloads of one
/// attachment, and the search has to end somewhere.
const EXPORT_NAME_TRIES: u32 = 1000;

/// The longest tail a downloaded attachment keeps as its extension. Anything
/// longer is not an extension but the rest of the name, and it is safer inside
/// the stem, where it is cleaned like the rest of the text.
const DOWNLOAD_EXTENSION_BYTES: usize = 12;

/// The name a downloaded attachment gets, as `(stem, extension)`: the last
/// path component of the name the sender gave it — `../../.bashrc` and
/// `C:\Users\ada\report.pdf` both have to land *in* `dir` and not outside it —
/// with the stem cleaned the way an export's is. The extension is kept, because
/// it is what says which program opens the file, but it has to look like one:
/// ASCII alphanumerics, no longer than `DOWNLOAD_EXTENSION_BYTES`, after a stem
/// that is not empty. Anything else is part of the stem, dots and all.
fn download_name(name: &str) -> (String, String) {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= DOWNLOAD_EXTENSION_BYTES
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            (safe_stem(stem, "attachment"), format!(".{extension}"))
        }
        _ => (safe_stem(base, "attachment"), String::new()),
    }
}

/// A subject as a file name (see `safe_stem`): an export is named after the
/// mail, and "mail" is what a subject that is nothing but punctuation becomes.
fn export_stem(subject: &str) -> String {
    safe_stem(subject, "mail")
}

/// `text` as a file name stem: what a file name cannot carry — the two path
/// separators, the characters Windows forbids, control characters — becomes a
/// space, runs of them collapse into one, and the ends lose their dots and
/// spaces. `slug` is the wrong tool here: it lowercases and ASCII-fies, which
/// would throw away every non-Latin name ("发票号码 12345" is a file the user
/// recognises; "account" is not). A text with no usable character left still
/// needs a name, and `fallback` is it — the numbering tells one such file from
/// the next.
fn safe_stem(text: &str, fallback: &str) -> String {
    let mut stem = String::new();
    for c in text.chars() {
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
        fallback.to_string()
    } else {
        stem.to_string()
    }
}

/// Write `bytes` into `dir` as `<stem><extension>`, numbered before the
/// extension if that name is taken, and answer with the path it went to — the
/// one writer an exported `.eml` and a downloaded attachment share. The file is
/// *created* rather than merely asked about, so two exports of one mail, or two
/// clicks on one attachment card, cannot land on one name: the loser of that
/// race moves to the next number instead of overwriting the winner. A write that
/// fails halfway takes its partial file with it — a half-written file with a
/// `.eml` or `.pdf` suffix looks like a document and is not one.
fn write_file(dir: &Path, stem: &str, extension: &str, bytes: &[u8]) -> MailResult<PathBuf> {
    fs::create_dir_all(dir)?;
    for n in 1..=EXPORT_NAME_TRIES {
        let name = if n == 1 {
            format!("{stem}{extension}")
        } else {
            format!("{stem}-{n}{extension}")
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
        if let Err(error) = file.write_all(bytes) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        return Ok(path);
    }
    Err(MailError::Storage(format!(
        "no free file name for {stem}{extension} in {}",
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
                    // Older mail, like the IMAP half above.
                    new_mail: false,
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
                // A warm drop's batch is the UIDLs the database did not have;
                // the first sync of one is the window it chose to fetch, which
                // is old mail however new it looks.
                new_mail: !cold,
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

    /// A `LIST` answer names the mailbox a copy of a sent mail belongs in: the
    /// first one that classifies as Sent, whatever the server calls it — and it
    /// answers the *wire* form, which is what `APPEND` has to be given.
    #[test]
    fn the_sent_mailbox_is_the_one_the_server_calls_sent() {
        let listed = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sent_mailbox_in(&listed(&["INBOX", "Sent", "Trash"])).as_deref(),
            Some("Sent")
        );
        assert_eq!(
            sent_mailbox_in(&listed(&["INBOX", "Sent Items"])).as_deref(),
            Some("Sent Items")
        );
        assert_eq!(
            sent_mailbox_in(&listed(&["INBOX", "[Gmail]/Sent Mail"])).as_deref(),
            Some("[Gmail]/Sent Mail")
        );
        // Modified UTF-7 on the wire, 已发送 on screen.
        assert_eq!(
            sent_mailbox_in(&listed(&["INBOX", "&XfJT0ZAB-"])).as_deref(),
            Some("&XfJT0ZAB-")
        );
        // Nothing that classifies as Sent: nowhere to put a copy.
        assert_eq!(sent_mailbox_in(&listed(&["INBOX", "Archive"])), None);
        assert_eq!(sent_mailbox_in(&[]), None);
    }

    /// A mail that goes out: the account is the envelope's sender, its
    /// credentials are what the submission logs in with, and the message on the
    /// wire is the one the renderer built. A POP3 account has nowhere to file a
    /// copy, and says so rather than inventing a folder.
    #[test]
    fn a_sent_mail_goes_out_with_the_account_as_its_sender() {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"220 canned ready\r\n").unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut commands = Vec::new();
            let mut message = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let command = line.trim_end().to_string();
                let verb = command.split(' ').next().unwrap_or_default().to_uppercase();
                commands.push(command);
                match verb.as_str() {
                    // No AUTH: this is the local-relay case, which must not be
                    // sent credentials it never asked for.
                    "EHLO" => socket
                        .write_all(b"250-localhost\r\n250 SIZE 10240000\r\n")
                        .unwrap(),
                    "MAIL" | "RCPT" => socket.write_all(b"250 2.1.0 ok\r\n").unwrap(),
                    "DATA" => {
                        socket.write_all(b"354 go ahead\r\n").unwrap();
                        loop {
                            let mut body = String::new();
                            if reader.read_line(&mut body).unwrap() == 0 {
                                break;
                            }
                            if body == ".\r\n" {
                                break;
                            }
                            message.push_str(&body);
                        }
                        socket.write_all(b"250 2.0.0 queued\r\n").unwrap();
                    }
                    "QUIT" => {
                        socket.write_all(b"221 2.0.0 bye\r\n").unwrap();
                        break;
                    }
                    _ => socket.write_all(b"250 ok\r\n").unwrap(),
                }
            }
            (commands, message)
        });

        let mut account = Account {
            id: "drop".to_string(),
            name: "Ada Lovelace".to_string(),
            protocol: Protocol::Pop3,
            host: "127.0.0.1".to_string(),
            port: 1,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            tls: crate::models::TlsMode::Plain,
            smtp: Some(crate::models::SmtpSettings {
                host: "127.0.0.1".to_string(),
                port,
                tls: crate::models::TlsMode::Plain,
                login: None,
            }),
        };
        let mail = Outgoing {
            from_name: account.name.clone(),
            from_addr: account.username.clone(),
            to: vec!["bob@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Invoice 42".to_string(),
            body: "Please pay it.\n".to_string(),
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let (recipients, copy) = send_message(&account, &mail, &tx).unwrap();
        assert_eq!(recipients, ["bob@example.com"]);
        assert!(
            matches!(copy, SentCopy::None),
            "a POP3 drop keeps no copy of what leaves"
        );
        assert!(
            rx.try_recv().is_err(),
            "the outcome is the caller's to send"
        );

        let (commands, message) = server.join().unwrap();
        assert!(
            commands.iter().any(|c| c == "MAIL FROM:<me@example.com>"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "RCPT TO:<bob@example.com>"),
            "{commands:?}"
        );
        assert!(
            !commands.iter().any(|c| c.starts_with("AUTH")),
            "a server that asks for no login is not sent one: {commands:?}"
        );
        assert!(message.contains("Subject: Invoice 42"), "{message}");
        assert!(
            message.contains("From: Ada Lovelace <me@example.com>"),
            "{message}"
        );
        assert!(
            message.contains("Content-Transfer-Encoding: base64"),
            "{message}"
        );

        // The two things that stop a send before a socket is opened: an account
        // with no outgoing server, and a field that is not an address.
        account.smtp = None;
        assert!(send_message(&account, &mail, &tx).is_err());
        account.smtp = Some(crate::models::SmtpSettings {
            host: "127.0.0.1".to_string(),
            port,
            tls: crate::models::TlsMode::Plain,
            login: None,
        });
        let mut junk = mail.clone();
        junk.to = vec!["not an address".to_string()];
        let error = send_message(&account, &junk, &tx).unwrap_err();
        assert!(error.to_string().contains("not an address"), "{error}");
    }

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
    /// taken is numbered rather than overwritten: exporting the same mail twice —
    /// or downloading one attachment twice — is two files, not one that
    /// replaced the other.
    #[test]
    fn writing_a_file_out_numbers_a_name_that_is_taken() {
        let dir = std::env::temp_dir().join(format!("kimo-export-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let first = write_file(&dir, "Invoice 42", ".eml", b"one").unwrap();
        let second = write_file(&dir, "Invoice 42", ".eml", b"two").unwrap();
        assert_eq!(first, dir.join("Invoice 42.eml"));
        assert_eq!(second, dir.join("Invoice 42-2.eml"));
        assert_eq!(fs::read(&first).unwrap(), b"one");
        assert_eq!(fs::read(&second).unwrap(), b"two");
        // The directory is made rather than refused when it is not there yet.
        assert!(dir.is_dir());

        // A downloaded attachment numbers the same way, before its own
        // extension rather than after it.
        let chart = write_file(&dir, "chart", ".png", b"png").unwrap();
        let chart_again = write_file(&dir, "chart", ".png", b"png").unwrap();
        assert_eq!(chart, dir.join("chart.png"));
        assert_eq!(chart_again, dir.join("chart-2.png"));

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A click on an attachment card, end to end: the drop is asked for the
    /// mail by UIDL, the part the card named is taken out of the original, and
    /// the file lands in the directory under the sender's own name. The part is
    /// the *second* attachment, so which file came out is the ordinal's doing
    /// and not the only choice there was.
    #[test]
    fn a_download_writes_the_part_the_card_named() {
        use std::io::BufRead;

        // A text body and two files: a text one first, the PDF second.
        let raw = "From: Ada <ada@example.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Quarterly report\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   \r\n\
                   See attached.\r\n\
                   --B\r\n\
                   Content-Type: text/plain; name=\"notes.txt\"\r\n\
                   Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
                   \r\n\
                   the first file\r\n\
                   --B\r\n\
                   Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                   \r\n\
                   aGVsbG8gd29ybGQ=\r\n\
                   --B--\r\n";

        // Three sessions reach this drop: a successful download and the two
        // refusals below, which fetch the mail *before* they find out that the
        // part is not the one the card named.
        const SESSIONS: usize = 3;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = raw.to_string();
        let server = std::thread::spawn(move || {
            let mut commands = Vec::new();
            for _ in 0..SESSIONS {
                let (mut socket, _) = listener.accept().unwrap();
                socket.write_all(b"+OK canned ready\r\n").unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    let command = line.trim_end().to_string();
                    let verb = command.split(' ').next().unwrap_or_default().to_uppercase();
                    commands.push(command);
                    match verb.as_str() {
                        "USER" | "PASS" => socket.write_all(b"+OK\r\n").unwrap(),
                        "UIDL" => socket.write_all(b"+OK\r\n1 the-uidl\r\n.\r\n").unwrap(),
                        "RETR" => {
                            socket
                                .write_all(format!("+OK {} octets\r\n", served.len()).as_bytes())
                                .unwrap();
                            socket.write_all(served.as_bytes()).unwrap();
                            socket.write_all(b".\r\n").unwrap();
                        }
                        "QUIT" => {
                            socket.write_all(b"+OK bye\r\n").unwrap();
                            break;
                        }
                        _ => socket.write_all(b"-ERR unknown command\r\n").unwrap(),
                    }
                }
            }
            commands
        });

        let account = Account {
            id: "drop".to_string(),
            name: "Ada Lovelace".to_string(),
            protocol: Protocol::Pop3,
            host: "127.0.0.1".to_string(),
            port,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            tls: crate::models::TlsMode::Plain,
            smtp: None,
        };
        let dir = std::env::temp_dir().join(format!("kimo-download-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let path =
            download_attachment(&account, "INBOX", "the-uidl", 1, "report.pdf", &dir).unwrap();
        assert_eq!(path, dir.join("report.pdf"));
        assert_eq!(fs::read(&path).unwrap(), b"hello world");

        // The part is checked against the name the card showed, and the ordinal
        // against the parts the mail really has: neither a name that lands on
        // another file nor an ordinal that lands past the end writes anything.
        for (ordinal, name) in [(1, "notes.txt"), (7, "report.pdf")] {
            let error = download_attachment(&account, "INBOX", "the-uidl", ordinal, name, &dir)
                .unwrap_err();
            assert!(error.to_string().contains(name), "{error}");
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "one file, not three"
        );

        let commands = server.join().unwrap();
        assert_eq!(
            commands.iter().filter(|c| *c == "RETR 1").count(),
            SESSIONS,
            "every download fetched the mail itself: {commands:?}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A downloaded attachment is named after the file the sender sent, and
    /// only ever *inside* the downloads directory: a name that arrives as a path
    /// — a Windows one, or one reaching for a home directory — keeps its last
    /// component, and a file name is not allowed to grow past what a filesystem
    /// takes.
    #[test]
    fn a_download_is_named_for_the_file_and_never_for_a_path() {
        assert_eq!(
            download_name("report-q3.pdf"),
            ("report-q3".to_string(), ".pdf".to_string())
        );
        assert_eq!(
            download_name("C:\\Users\\ada\\report.pdf"),
            ("report".to_string(), ".pdf".to_string())
        );
        assert_eq!(
            download_name("../../.bashrc"),
            ("bashrc".to_string(), String::new()),
            "a dot file is all name, and the path is gone"
        );
        // A dot with nothing before it is not an extension, and neither is a
        // tail no program would open by.
        assert_eq!(
            download_name(".hidden"),
            ("hidden".to_string(), String::new())
        );
        assert_eq!(
            download_name("archive.tar.gz"),
            ("archive.tar".to_string(), ".gz".to_string())
        );
        // A tail that is not one — too long, or not made of the characters an
        // extension is — stays part of the stem, where it is cleaned like the
        // rest of the name rather than trusted after a dot.
        assert_eq!(
            download_name("report.verylongextension"),
            ("report.verylongextension".to_string(), String::new())
        );
        assert_eq!(
            download_name("notes.this-is-not-an-extension"),
            ("notes.this-is-not-an-extension".to_string(), String::new())
        );
        // Nothing usable is still a name: the numbering tells one such file
        // from the next.
        assert_eq!(
            download_name("/"),
            ("attachment".to_string(), String::new())
        );
        assert_eq!(
            download_name("invoice:2026/01.pdf"),
            ("01".to_string(), ".pdf".to_string())
        );
    }
}
