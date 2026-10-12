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

/// How many of the newest messages the *first* sync of a folder takes. Later
/// syncs are incremental — the folder's marks say what is already stored — so
/// this is the size of the one-off backfill, not of a sync.
pub const BACKFILL_LIMIT: u32 = 50;

/// The lowest 1-based position a first sync reaches: the newest
/// `BACKFILL_LIMIT` messages of `total`. Everything older is out of scope, and
/// the mark the sync stores is what says so.
fn backfill_start(total: usize) -> usize {
    total.saturating_sub(BACKFILL_LIMIT as usize) + 1
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
    /// Folder list (IMAP) plus new mail: INBOX and every folder that has been
    /// fetched before. For POP3, the new messages of the drop. `purge` drops
    /// the account's stored mail before fetching — for a new account, or one
    /// whose endpoint just moved to another mailbox.
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
                            Protocol::Imap => sync_imap(&account, &tx),
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

fn sync_imap(account: &Account, tx: &Sender<Event>) -> MailResult<()> {
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
            folders: folders.clone(),
        });
        // INBOX first — it is the one the window is looking at — then every
        // folder that has been fetched before, so a sync catches up instead of
        // refetching. A folder the user never opened has no mark and waits for
        // its first click: that is where the backfill belongs. A folder the
        // server no longer lists is skipped; its cached mail stays.
        let mut queue = vec!["INBOX".to_string()];
        for folder in db::synced_folders(&account.id)? {
            if folder.eq_ignore_ascii_case("INBOX") || !folders.contains(&folder) {
                continue;
            }
            queue.push(folder);
        }
        // One folder that fails — a mailbox the server refuses, a connection
        // that dropped — must not cost the others their sync. The first error
        // is reported once the rest is done.
        let mut failure: Option<MailError> = None;
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
    let mut messages = Vec::new();
    if info.exists > 0 {
        // A folder that was fetched before is asked for the UIDs it does not
        // have yet: `<next>:*` is everything from the mark on. A server may
        // answer that with the last message when the mark is already past the
        // end, which the filter drops. A folder that was never fetched takes
        // the newest `BACKFILL_LIMIT` messages, by sequence number.
        let fetched = match resume {
            Some(next) => client
                .fetch_bodies(&format!("{next}:*"), true)?
                .into_iter()
                .filter(|message| message.uid >= next)
                .collect(),
            None => {
                let start = backfill_start(info.exists as usize) as u32;
                client.fetch_bodies(&format!("{start}:{}", info.exists), false)?
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
        // never a message.
        if cold {
            db::set_first_num(&account.id, "INBOX", window as u32)?;
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

    #[test]
    fn a_folder_resumes_from_its_mark_unless_the_uidvalidity_moved() {
        let mark = db::FolderSync {
            uidvalidity: Some(7),
            uid_next: Some(13),
            first_num: None,
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
}
