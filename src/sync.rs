//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket.
//! Besides mail sync, jobs also fetch remote HTML-mail resources (images,
//! stylesheets) over HTTPS.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

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
    Sync { account: Account },
    /// Fetch one specific IMAP folder.
    SyncFolder { account: Account, folder: String },
    /// Push `\Seen` back to the server (IMAP only).
    MarkSeen {
        account: Account,
        folder: String,
        uid: u32,
    },
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
            let event = match crate::blitz_render::render(&request) {
                Ok(page) => Event::HtmlRendered { key, page },
                Err(error) => Event::HtmlRenderFailed { key, error },
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        }
        Job::ScanFonts => {
            let _ = tx.send(Event::FontsScanned(crate::fonts::scan()));
            ctx.request_repaint();
        }
        account_job => {
            let account_id = match &account_job {
                Job::Sync { account }
                | Job::SyncFolder { account, .. }
                | Job::MarkSeen { account, .. } => account.id.clone(),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts => unreachable!(),
            };
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            let result = match account_job {
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
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts => unreachable!(),
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
