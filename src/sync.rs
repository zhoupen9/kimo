//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket, and it
//! never writes to the database either: these jobs do that (see `crate::db`).
//! Besides mail sync, jobs also fetch remote HTML-mail resources (images,
//! stylesheets) over HTTPS.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::db;
use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::{Account, MailMessage, Protocol};
use crate::protocol::imap::ImapClient;
use crate::protocol::pop3::Pop3Client;
use crate::storage;

/// How many of the newest messages to fetch per folder per sync.
pub const FETCH_LIMIT: u32 = 50;

/// Hard caps for remote mail resources (images / stylesheets).
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_STYLESHEET_BYTES: u64 = 512 * 1024;
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(20);

pub enum Job {
    /// Folder list (IMAP) plus the INBOX contents; for POP3, the whole drop.
    /// `purge` drops the account's stored mail before fetching — for a new
    /// account, or one whose endpoint just moved to another mailbox.
    Sync { account: Account, purge: bool },
    /// Fetch one specific IMAP folder.
    SyncFolder { account: Account, folder: String },
    /// Push `\Seen` back to the server (IMAP only) and store the flag in the
    /// database (both protocols; POP3 has no server-side flags).
    MarkSeen {
        account: Account,
        folder: String,
        uid: String,
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
        account_job => {
            let account_id = match &account_job {
                Job::Sync { account, .. } | Job::SyncFolder { account, .. } => account.id.clone(),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts
                | Job::Search { .. }
                | Job::MarkSeen { .. }
                | Job::DeleteAccount { .. } => unreachable!(),
            };
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            let result = match account_job {
                Job::Sync { account, purge } => {
                    // The account row comes first: the mail below refers to it,
                    // and a brand-new account has to be durable before its mail
                    // is. Purging, when the endpoint moved, is part of the same
                    // job so nothing can reorder those steps.
                    db::save_account(&account).and_then(|()| {
                        if purge {
                            db::purge_mail(&account.id)?;
                        }
                        match account.protocol {
                            Protocol::Imap => sync_imap_inbox(&account, &tx),
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

fn sync_imap_inbox(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
    with_imap(account, |client| {
        client.login(&account.username, &account.password)?;
        let folders = client
            .list_folders()
            .unwrap_or_else(|_| vec!["INBOX".to_string()]);
        // Remember the mailboxes first: a folder the database does not know
        // would refuse the mail fetched for it below (the messages table has a
        // foreign key onto folders).
        db::save_folder_list(&account.id, &folders)?;
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
    // Store before announcing: nothing may appear in the window that the
    // database does not already have.
    db::save_messages(&account.id, folder, info.uidvalidity, &messages)?;
    let _ = tx.send(Event::Messages {
        account_id: account.id.clone(),
        folder: folder.to_string(),
        uidvalidity: info.uidvalidity,
        messages,
    });
    Ok(())
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
        db::save_messages(&account.id, "INBOX", None, &messages)?;
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
