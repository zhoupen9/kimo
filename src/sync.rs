//! Background sync jobs. The UI spawns a short-lived `std::thread` per job;
//! results stream back as `Event`s over an mpsc channel and the egui context
//! is nudged to repaint. The UI thread itself never touches a socket, and it
//! never writes to the database either: these jobs do that (see `crate::db`).
//! Besides mail sync, jobs also send written mail over SMTP (`Job::SendMessage`)
//! and fetch remote HTML-mail resources (images, stylesheets) over HTTPS.
//!
//! Mailbox work goes through `crate::provider::MailProvider`: `provider::open`
//! picks the adapter once, and nothing here branches on a protocol to reach the
//! wire. This module keeps what the adapters deliberately do not — the database,
//! the marks' ordering and the `Event`s.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::db;
use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::{Account, MailMessage};
use crate::outgoing::Outgoing;
use crate::protocol::smtp::SmtpClient;
use crate::provider;
use crate::provider::vendor::{sent_mailbox, VendorKey};
use crate::storage;

/// How many older messages one `Load older mail` click fetches. The same size
/// as a folder's first fetch (`provider::BACKFILL_LIMIT`), so the list grows in
/// the steps it started in.
pub const OLDER_BATCH: u32 = 50;

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
    /// `save` writes the account row first, and only the add/edit form's job
    /// sets it: a sync that wrote it would put a removed account back the
    /// moment a job spawned before the removal reached the database — the
    /// sweep of a second window does exactly that — and the account would
    /// return at the next start.
    Sync {
        account: Account,
        purge: bool,
        /// A folder to fetch even though it has no mark; ignored when the
        /// server no longer lists it.
        folder: Option<String>,
        save: bool,
    },
    /// Fetch one specific folder: the click that opens a folder nobody has
    /// fetched yet. A protocol without folders (POP3) answers with its drop.
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
    /// Put the reader's flag on one message, or take it off — the title bar's
    /// flag cell. `\Flagged` is the one mail mark the *server* keeps as well
    /// as Kimo, so this is a round trip where `Job::SetLabels` is not: the local
    /// row is written first (the reader asked for it, and a server that refuses
    /// must not lose it) and the IMAP `STORE` follows, for an account that has
    /// one to tell.
    SetFlag {
        account: Account,
        folder: String,
        uid: String,
        flagged: bool,
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
    /// Export one message — the title bar's Export: ask the desktop's own file
    /// dialog where to put it (`crate::portal::save_file`), fetch its original
    /// RFC 5322 source from the server, and write it there. The server's copy is
    /// the message whole (every header, every body part, every attachment),
    /// which is why this is a round trip rather than a rebuild from the
    /// database: what is stored here is the parsed mail, with each attachment's
    /// name, type and size and never its bytes.
    ExportMessage {
        account: Account,
        folder: String,
        uid: String,
        /// The directory the file goes in when there is no dialog to ask for
        /// one, and the folder the dialog is suggested to open on — the same
        /// place either way (`storage::downloads_dir`).
        dir: PathBuf,
        /// What the file is named after: the subject, which the worker turns
        /// into a file name (`export_stem`) — the name the dialog is offered as
        /// what to call the file, and the name it gets when there was no dialog
        /// to offer it to.
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
        /// The provider whose policy decides where the Sent copy goes. The
        /// window owns `settings.json`, so it passes `Custom` when the
        /// account's vendor is switched off in Preferences: a disabled preset
        /// then behaves exactly like an ordinary server.
        vendor: VendorKey,
    },
    /// Remove an account: its row, its folders and, through the cascades, every
    /// message cached for it. The account travels with the job so a failure can
    /// hand it back ([`Event::AccountRemoveFailed`]).
    DeleteAccount { account: Account },
    /// Sign a plugin provider account in with OAuth2: the module builds the
    /// authorization URL, the host opens the desktop's browser on it and
    /// catches the loopback redirect, and the module exchanges the code for
    /// tokens, which land in the account row. The browser leg is why this is
    /// a job like any other — it waits minutes, not milliseconds.
    PluginSignIn { account: Account },
    /// Ask the desktop for the files to attach to the mail being written: the
    /// XDG portal's own file chooser, which runs in the *compositor's* process
    /// (`crate::portal`). It is a job because the call does not come back until
    /// the user has answered the dialog, and the UI thread waits for nothing.
    PickFiles,
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
    /// Look at the machine's graphics cards and the Vulkan drivers installed
    /// for them — Preferences → Graphics' list and what its foot says the pick
    /// will load (`crate::gpus`). Results come back as `Event::Gpus`.
    ScanGpus,
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
    /// The files the desktop's file dialog came back with: empty when it was
    /// cancelled, which is not a failure and says nothing on screen.
    FilesChosen {
        files: Vec<PathBuf>,
    },
    /// And why it could not be opened at all — a session with no portal, a bus
    /// that is not there. The card says so beside its own path box, which is
    /// exactly what that box is for.
    FileChoiceFailed {
        error: String,
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
    /// Removing an account failed, so its row is still in the database and the
    /// next start will load it again. The whole account travels back with the
    /// error: the window took it out of the list on the click, and a failure
    /// has to put it back where the reader can see it, rather than let it
    /// reappear at the next start with nothing said.
    AccountRemoveFailed {
        account: Account,
        error: String,
    },
    /// A plugin provider sign-in finished. `error: None` is tokens in the
    /// account row and mail about to sync; `Some` is the browser flow's own
    /// words — refused, timed out, no plugin — for the account form to say.
    PluginAuth {
        account_id: String,
        error: Option<String>,
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
    /// one thing about an export the UI cannot know by itself: it is the name the
    /// file dialog answered with — extension and all, since a save dialog decides
    /// the name — or, when there was no dialog to ask, one the writer numbered
    /// because the name it was offered was taken.
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
    /// The machine's graphics cards and Vulkan drivers, as Preferences →
    /// Graphics lists them.
    Gpus(crate::gpus::Scan),
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

/// Ask for a repaint and wake the loop directly. The direct wake matters while
/// the window is hidden: egui can consider an immediate repaint already served
/// by a window that no longer exists.
fn worker_wake(ctx: &egui::Context) {
    ctx.request_repaint();
    crate::wake::now();
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
            worker_wake(&ctx);
        }
        Job::FetchStylesheet { url } => {
            let event = match fetch_text(&url) {
                Ok(text) => Event::StylesheetLoaded { url, text },
                Err(_) => Event::StylesheetFailed { url },
            };
            let _ = tx.send(event);
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::ScanFonts => {
            let _ = tx.send(Event::FontsScanned(crate::fonts::scan()));
            worker_wake(&ctx);
        }
        Job::ScanGpus => {
            let _ = tx.send(Event::Gpus(crate::gpus::scan()));
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::PickFiles => {
            // Not bracketed with SyncStarted/SyncDone: no account is being
            // fetched, and the card that asked is on screen waiting for the
            // dialog.
            match crate::portal::open_files() {
                Ok(files) => {
                    let _ = tx.send(Event::FilesChosen { files });
                }
                Err(error) => {
                    let _ = tx.send(Event::FileChoiceFailed {
                        error: error.to_string(),
                    });
                }
            }
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::SetFlag {
            account,
            folder,
            uid,
            flagged,
        } => {
            // Not a sync: no spinner for a flag write, and the local half is
            // stored whether or not the server accepts the push.
            if let Err(error) = set_flag(&account, &folder, &uid, flagged) {
                let _ = tx.send(Event::SyncFailed {
                    account_id: account.id.clone(),
                    error: error.to_string(),
                });
            }
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::DeleteAccount { account } => {
            if let Err(error) = db::delete_account(&account.id) {
                // The row is still there, so this has to be said: the window
                // has already taken the account out of the list, and the next
                // start would load it back with nothing on screen to explain
                // why (see `Event::AccountRemoveFailed`).
                eprintln!("kimo: cannot remove account {}: {error}", account.id);
                let _ = tx.send(Event::AccountRemoveFailed {
                    account,
                    error: error.to_string(),
                });
            }
            worker_wake(&ctx);
        }
        Job::PluginSignIn { account } => {
            // No SyncStarted/SyncDone: the thing the user waits for is a
            // browser, and the account form is what reports the outcome.
            let account_id = account.id.clone();
            let error = plugin_sign_in(&account)
                .err()
                .map(|error| error.to_string());
            let _ = tx.send(Event::PluginAuth { account_id, error });
            worker_wake(&ctx);
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
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::ExportMessage {
            account,
            folder,
            uid,
            dir,
            subject,
        } => {
            // The file's name comes first, because both halves of this use it:
            // it is what the dialog is asked to call the file, and what the file
            // is called when there is no dialog to ask.
            let stem = export_stem(&subject);
            // The desktop's own dialog, before anything is fetched and before
            // anything is said: a cancel then costs no round trip, and the
            // account's spinner — which says the *server* is being asked — does
            // not turn while the user is choosing a name.
            let Some(place) = export_place(crate::portal::save_file(&format!("{stem}.eml"), &dir))
            else {
                // Closed: no file, and nothing to say about it.
                worker_wake(&ctx);
                return;
            };
            // A round trip like a delete's, and bracketed the same way: the
            // account's spinner is what says a mail is being fetched while the
            // server is asked.
            let account_id = account.id.clone();
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            match export_message(&account, &folder, &uid, &dir, &stem, &place) {
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
            worker_wake(&ctx);
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
            worker_wake(&ctx);
        }
        Job::SendMessage {
            account,
            mail,
            vendor,
        } => {
            // Not bracketed with SyncStarted/SyncDone like the other round
            // trips: the card that asked for the send is on screen, cannot be
            // closed while the mail is in flight, and says "Sending…" itself.
            // A failure is that card's to show, not the account row's.
            match send_message(&account, &mail, vendor, &tx) {
                Ok((recipients, copy)) => {
                    let _ = tx.send(Event::MessageSent { recipients, copy });
                }
                Err(error) => {
                    let _ = tx.send(Event::MessageSendFailed {
                        error: error.to_string(),
                    });
                }
            }
            worker_wake(&ctx);
        }
        account_job => {
            let account_id = match &account_job {
                Job::Sync { account, .. } | Job::SyncFolder { account, .. } => account.id.clone(),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts
                | Job::ScanGpus
                | Job::Search { .. }
                | Job::MarkSeen { .. }
                | Job::DeleteMessage { .. }
                | Job::ExportMessage { .. }
                | Job::DownloadAttachment { .. }
                | Job::SendMessage { .. }
                | Job::LoadOlder { .. }
                | Job::SetLabels { .. }
                | Job::DeleteAccount { .. }
                | Job::PluginSignIn { .. }
                | Job::SetFlag { .. }
                | Job::PickFiles => unreachable!(),
            };
            let _ = tx.send(Event::SyncStarted {
                account_id: account_id.clone(),
            });
            let result = match account_job {
                Job::Sync {
                    account,
                    purge,
                    folder,
                    save,
                } => {
                    // The form's job writes the account row before the mail
                    // below refers to it, and a brand-new account has to be
                    // durable before its mail is. Purging, when the endpoint
                    // moved, is part of the same job so nothing can reorder
                    // those steps. Every other sync leaves the row alone: see
                    // the variant's comment for what writing it there costs.
                    let written = if save {
                        db::save_account(&account)
                    } else {
                        Ok(())
                    };
                    written.and_then(|()| {
                        if purge {
                            db::purge_mail(&account.id)?;
                        }
                        sync_account(&account, folder.as_deref(), &tx)
                    })
                }
                Job::SyncFolder { account, folder } => sync_folder_job(&account, &folder, &tx),
                Job::FetchImage { .. }
                | Job::FetchStylesheet { .. }
                | Job::RenderHtml { .. }
                | Job::ScanFonts
                | Job::ScanGpus
                | Job::Search { .. }
                | Job::MarkSeen { .. }
                | Job::DeleteMessage { .. }
                | Job::ExportMessage { .. }
                | Job::DownloadAttachment { .. }
                | Job::SendMessage { .. }
                | Job::LoadOlder { .. }
                | Job::SetLabels { .. }
                | Job::DeleteAccount { .. }
                | Job::PluginSignIn { .. }
                | Job::SetFlag { .. }
                | Job::PickFiles => unreachable!(),
            };
            let _ = match result {
                Ok(()) => tx.send(Event::SyncDone { account_id }),
                Err(e) => tx.send(Event::SyncFailed {
                    account_id,
                    error: e.to_string(),
                }),
            };
            worker_wake(&ctx);
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

/// The store's view of a folder, as a provider fetch needs it. The UIDLs are
/// read only for a protocol whose fetch is what filters them (POP3).
fn folder_mark(
    account_id: &str,
    folder: &str,
    stored_uids: bool,
) -> MailResult<provider::FolderMark> {
    let mark = db::folder_sync(account_id, folder)?;
    Ok(provider::FolderMark {
        uidvalidity: mark.uidvalidity,
        uid_next: mark.uid_next,
        first_num: mark.first_num,
        stored: if stored_uids {
            Some(db::stored_uids(account_id, folder)?)
        } else {
            None
        },
    })
}

/// Write the marks a fetch learned. Called before or after the batch, by the
/// adapter's `marks_before_store`: POP3's floor bounds its next fetch and
/// nothing rotates it, while an IMAP `save_messages` clears the marks when the
/// UIDVALIDITY moved, so the new ones have to be written after it.
fn write_marks(
    account_id: &str,
    folder: &str,
    first_num: Option<u32>,
    oldest: Option<Option<u32>>,
) -> MailResult<()> {
    if let Some(first_num) = first_num {
        db::set_first_num(account_id, folder, first_num)?;
    }
    if let Some(oldest) = oldest {
        db::set_oldest_uid(account_id, folder, oldest)?;
    }
    Ok(())
}

/// Tell the list whether the server has mail below what this folder holds.
fn report_older(account_id: &str, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    let mark = db::folder_sync(account_id, folder)?;
    let _ = tx.send(Event::OlderMail {
        account_id: account_id.to_string(),
        folder: folder.to_string(),
        available: db::older_available(mark.first_num, mark.oldest_uid),
    });
    Ok(())
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

/// One account's sync: the folder list (when the protocol has one), then the
/// queue `sync_queue` builds, each folder stored and announced as it arrives.
fn sync_account(account: &Account, open: Option<&str>, tx: &Sender<Event>) -> MailResult<()> {
    provider::with(account, |provider| {
        // One folder that fails — a mailbox the server refuses, a connection
        // that dropped — must not cost the others their sync. The first error
        // is reported once the rest is done, and the mailbox list is one of
        // the things that can fail.
        let mut failure: Option<MailError> = None;
        let folders = if provider.capabilities().folders {
            match provider.folders() {
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
            }
        } else {
            // POP3 has one drop and no LIST: its folder is INBOX, and it never
            // writes a folder list (the first mail it stores makes the row).
            vec![provider::INBOX.to_string()]
        };
        let queue = sync_queue(open, &folders, &db::synced_folders(&account.id)?);
        for folder in queue {
            if let Err(error) = fetch_into(&mut *provider, &folder, account, tx) {
                failure = failure.or(Some(error));
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })
}

/// Fetch one folder: the click that opens a folder nobody has fetched.
fn sync_folder_job(account: &Account, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    provider::with(account, |provider| {
        fetch_into(provider, folder, account, tx)
    })
}

/// Store one folder's fetch and announce it, in the order the adapter says.
fn fetch_into(
    provider: &mut dyn provider::MailProvider,
    folder: &str,
    account: &Account,
    tx: &Sender<Event>,
) -> MailResult<()> {
    let mark = folder_mark(&account.id, folder, provider.capabilities().stored_uids)?;
    let provider::FolderBatch {
        uidvalidity,
        messages,
        kind,
        first_num,
        oldest,
        marks_before_store,
    } = provider.fetch_folder(folder, &mark)?;
    if marks_before_store {
        write_marks(&account.id, folder, first_num, oldest)?;
    }
    // Store before announcing: nothing may appear in the window that the
    // database does not already have. The batch moves the folder's mark in the
    // same transaction. A warm folder with nothing new is left untouched: no
    // write, and no event for a list that did not change.
    if kind == provider::FetchKind::Fill || !messages.is_empty() {
        db::save_messages(&account.id, folder, uidvalidity, &messages)?;
        let _ = tx.send(Event::Messages {
            account_id: account.id.clone(),
            folder: folder.to_string(),
            uidvalidity,
            messages,
            // A fetch that resumed at the folder's mark asked for exactly the
            // UIDs it did not have: what came back arrived since the last one.
            // A folder filled for the first time (or after a UIDVALIDITY
            // rotation) is the folder's backfill, not news.
            new_mail: kind == provider::FetchKind::Resume,
        });
    }
    if !marks_before_store {
        write_marks(&account.id, folder, first_num, oldest)?;
    }
    report_older(&account.id, folder, tx)
}

/// Fetch the older mail of one folder: the batch below the floor it has been
/// fetched down to. IMAP names the range by UID, POP3 by message number; both
/// move the floor whether or not the range held anything.
fn load_older(account: &Account, folder: &str, tx: &Sender<Event>) -> MailResult<()> {
    provider::with(account, |provider| {
        let mark = folder_mark(&account.id, folder, provider.capabilities().stored_uids)?;
        let provider::OlderBatch {
            uidvalidity,
            messages,
            first_num,
            oldest,
            available,
        } = provider.fetch_older(folder, &mark, OLDER_BATCH)?;
        // The marks go first: the floor is where this folder has been asked
        // from, and a range that came back empty still moves it.
        write_marks(&account.id, folder, first_num, oldest)?;
        if !messages.is_empty() {
            db::save_messages(&account.id, folder, uidvalidity, &messages)?;
            let _ = tx.send(Event::Messages {
                account_id: account.id.clone(),
                folder: folder.to_string(),
                uidvalidity,
                messages,
                // Older mail: whatever the click was for, it is not news.
                new_mail: false,
            });
        }
        let _ = tx.send(Event::OlderMail {
            account_id: account.id.clone(),
            folder: folder.to_string(),
            available,
        });
        Ok(())
    })
}

/// Store the flag locally and, for a protocol that keeps it, push it to the
/// server. The local write happens first: the mail is read either way, and a
/// server that refuses the `STORE` must not lose that.
fn mark_seen(account: &Account, folder: &str, uid: &str) -> MailResult<()> {
    db::set_seen(&account.id, folder, uid)?;
    if flags_reach_the_server(account) {
        provider::with(account, |provider| provider.set_seen(folder, uid))?;
    }
    Ok(())
}

/// Whether a flag change has a server half to push: the protocol's table for
/// a built-in account, the module's own descriptor for a plugin one — a
/// module that declares `message.set_flags` is the one asked to carry it.
fn flags_reach_the_server(account: &Account) -> bool {
    if account.is_plugin() {
        return crate::plugin::provider::server_flags(account);
    }
    provider::capabilities(account.protocol).server_flags
}

/// Store the reader's flag locally and, for a protocol that keeps it, push it
/// to the server. The same shape as `mark_seen`, and for the same reason: the
/// local write happens first, so a server that refuses the `STORE` does not
/// cost the reader the flag they just set. The command differs in both
/// directions — `+FLAGS` to set the flag, `-FLAGS` to take it off — because a
/// flag is a state rather than a latch.
fn set_flag(account: &Account, folder: &str, uid: &str, flagged: bool) -> MailResult<()> {
    db::set_flagged(&account.id, folder, uid, flagged)?;
    if flags_reach_the_server(account) {
        provider::with(account, |provider| {
            provider.set_flagged(folder, uid, flagged)
        })?;
    }
    Ok(())
}

/// Delete messages of one folder: the remote half goes first and each database
/// row only after it, so mail that is still on the server never vanishes from
/// here — the next sync would bring it back and the deletion would look like it
/// never happened. Mail the server no longer has is dropped locally: there is
/// nothing remote to delete, and the local copy is the only one left.
///
/// The adapter applies the rule one mail at a time and answers the uids whose
/// remote copies are gone plus the first refusal; the rest of the run is asked
/// anyway — a batch that stopped at the first failure would leave mail checked
/// on screen with nothing said about the mails it never got to — and one
/// connection carries the whole batch, because a click that picked twenty
/// mails should not be twenty logins.
fn delete_messages(
    account: &Account,
    folder: &str,
    uids: &[String],
) -> (Vec<String>, Option<MailError>) {
    let outcome = match provider::with(account, |provider| Ok(provider.delete(folder, uids))) {
        Ok(outcome) => outcome,
        // The connection itself failed, so the server was never asked: no mail
        // went, and every local copy stays where it is.
        Err(error) => return (Vec::new(), Some(error)),
    };
    let mut first_error = outcome.error;
    let mut deleted = Vec::new();
    for uid in outcome.gone {
        match db::delete_message(&account.id, folder, &uid) {
            Ok(()) => deleted.push(uid),
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    (deleted, first_error)
}

/// Where an exported mail goes, from what the desktop's file dialog came to.
#[derive(Debug)]
enum Place {
    /// The path the user named in the dialog: the file goes there, exactly. The
    /// dialog is where the name is decided — extension and all — and a save
    /// dialog has already asked about a name that is taken.
    Chosen(PathBuf),
    /// There was no dialog to ask (`crate::portal` — a session with no portal,
    /// a bus that is not there). The export is then what it was before there was
    /// one: the downloads directory, under the name the mail is named after,
    /// numbered if that name is taken.
    Downloads,
}

/// Where an export writes, from the dialog's answer — and `None` when the user
/// closed it, which is not a failure: nothing is written and nothing is said.
///
/// A dialog that cannot be opened at all is the same answer as no dialog, and
/// nothing is said about that either: a mailbox that refused to save a mail
/// because a desktop has no portal would be a worse app than one that puts the
/// file where it always did and says where. Nothing is hidden by the fallback —
/// the notice names the file that was written, and a failure of the *export*
/// still comes back on the account.
fn export_place(asked: MailResult<Option<PathBuf>>) -> Option<Place> {
    match asked {
        Ok(Some(path)) => Some(Place::Chosen(path)),
        Ok(None) => None,
        Err(_) => Some(Place::Downloads),
    }
}

/// Export one message: its original source, as the server has it, written where
/// `place` says. The fetch is the whole job — what Kimo stores is the parsed
/// mail, with each attachment's name, type and size and never its bytes, so an
/// `.eml` rebuilt from the database would not be the message the sender wrote.
/// The other side of that: mail the server no longer has cannot be exported at
/// all, and says so rather than writing something close.
fn export_message(
    account: &Account,
    folder: &str,
    uid: &str,
    dir: &Path,
    stem: &str,
    place: &Place,
) -> MailResult<PathBuf> {
    let raw = original_source(account, folder, uid)?;
    write_export(dir, stem, place, &raw)
}

/// Write an exported mail the way `place` says: at the path the dialog
/// answered, or into `dir` under the name the mail is named after. Two writers,
/// because the two cases promise different things (`write_at` and
/// `write_file`, each with the reason for its own rule).
fn write_export(dir: &Path, stem: &str, place: &Place, bytes: &[u8]) -> MailResult<PathBuf> {
    match place {
        Place::Chosen(path) => write_at(path, bytes).map(|()| path.clone()),
        Place::Downloads => write_file(dir, stem, ".eml", bytes),
    }
}

/// Write `bytes` at exactly `path`: the file dialog's answer, so the name is
/// the user's and not Kimo's to number. The file is written through a temporary
/// name in the same directory and that is renamed into place, which is what
/// makes replacing a file safe: the name holds either the file that was there or
/// the whole new one, never half of either — and a write that fails leaves
/// nothing of Kimo's behind in the user's directory. A failure says which file
/// it was about, because the path came from a dialog.
fn write_at(path: &Path, bytes: &[u8]) -> MailResult<()> {
    // Hidden, and in the file's own directory rather than in a temporary
    // directory of its own: the rename is then a rename, and not a copy that
    // fails across a mount. The name is short rather than the file's own with a
    // suffix — a name the dialog accepted may be as long as a file name may be —
    // and it counts, so that two exports at once into one directory cannot meet
    // in one temporary.
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let temp = path.with_file_name(format!(
        ".kimo{}_{}.part",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // The path came from a dialog, so a failure says which file it was about:
    // "No such file or directory" on its own would not.
    let failed = |error: std::io::Error| MailError::Storage(format!("{}: {error}", path.display()));
    if let Err(error) = fs::write(&temp, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(failed(error));
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(failed(error));
    }
    Ok(())
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
    provider::with(account, |provider| provider.body(folder, uid))
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
    vendor: VendorKey,
    tx: &Sender<Event>,
) -> MailResult<(Vec<String>, SentCopy)> {
    let invalid = mail.invalid();
    if !invalid.is_empty() {
        return Err(MailError::Protocol(format!(
            "not an address: {}",
            invalid.join(", ")
        )));
    }
    let recipients: Vec<String> = mail.recipients().into_iter().map(str::to_string).collect();
    // The files are read here, on the worker: the card holds their paths
    // (`OutgoingAttachment`) and the UI thread reads no files at all. One that
    // cannot be read stops the send — a mail that goes out without the file its
    // writer attached is worse than one that does not go out.
    let attachments = crate::outgoing::load_attachments(&mail.attachments)?;
    // The bytes are built once, and the same ones go to the server and — when
    // there is somewhere to keep it — into the Sent mailbox: a copy that
    // differs from what was delivered is worse than no copy at all.
    let raw = crate::outgoing::render(mail, &attachments, unix_now());
    // A plugin provider submits through its own module (XOAUTH2 on Gmail,
    // `sendMail` on Graph); everything else is SMTP submission, as it always
    // was.
    if account.is_plugin() {
        crate::plugin::provider::submit_for_account(account, &mail.from_addr, &recipients, &raw)?;
    } else {
        let Some(smtp) = &account.smtp else {
            return Err(MailError::Protocol(
                "this account has no outgoing server".to_string(),
            ));
        };
        let mut client = SmtpClient::connect(&smtp.host, smtp.port, smtp.tls)?;
        // No username is a submission server that wants no login — a local
        // relay, or a server that authenticates by address alone.
        if let Some((user, pass)) = account.smtp_login() {
            if !user.is_empty() {
                client.login(user, pass)?;
            }
        }
        client.send(&mail.from_addr, &recipients, &raw)?;
        client.quit();
    }
    let copy = append_to_sent(account, &raw, vendor, tx);
    Ok((recipients, copy))
}

/// One OAuth2 sign-in for a plugin provider account: the module's URL, the
/// desktop's browser, the loopback redirect, the module's token exchange —
/// and the account row the tokens land in.
fn plugin_sign_in(account: &Account) -> MailResult<()> {
    let provider = account
        .provider
        .as_deref()
        .ok_or_else(|| MailError::Protocol("the account names no plugin provider".to_string()))?;
    let missing = || {
        MailError::Protocol(format!(
            "the {provider} provider plugin is not installed or enabled"
        ))
    };
    let registry = crate::plugin::discovery::installed().ok_or_else(missing)?;
    let loaded = registry.provider(provider).ok_or_else(missing)?;
    let client_id = crate::plugin::provider::client_id_of(account);
    let params = serde_json::json!({
        "client_id": client_id,
        "login_hint": account.username,
    });
    let credentials = crate::plugin::auth::sign_in(
        &loaded.plugin,
        std::sync::Arc::new(crate::plugin::net::StdNetwork::default()),
        params,
    )?;
    db::save_account_tokens(
        &account.id,
        Some(&credentials.access_token),
        credentials.refresh_token.as_deref(),
        credentials.expires_at,
    )?;
    Ok(())
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
///
/// `vendor` is the *effective* one: the window passes `Custom` when the
/// account's preset is switched off in Preferences, which leaves the generic
/// Sent classification deciding.
fn append_to_sent(
    account: &Account,
    raw: &[u8],
    vendor: VendorKey,
    tx: &Sender<Event>,
) -> SentCopy {
    if account.is_plugin() {
        // Graph's `sendMail` and Gmail's own submission file the copy
        // themselves (`send.save_copy`); appending one here would be the
        // same mail filed twice. A module without the capability is asked
        // to append only when it declared `message.append`.
        if crate::plugin::provider::files_own_sent(account) {
            return SentCopy::None;
        }
        if !crate::plugin::provider::declares_public(account, "message.append") {
            return SentCopy::None;
        }
    } else if !provider::capabilities(account.protocol).append {
        return SentCopy::None;
    }
    let mailbox = match db::listed_folders(&account.id) {
        Ok(listed) => match sent_mailbox(vendor.profile(), &listed) {
            Some(mailbox) => mailbox,
            // Nothing to append to, and nothing wrong either.
            None => return SentCopy::None,
        },
        Err(error) => return SentCopy::Failed(error.to_string()),
    };
    // The copy goes in and comes back on one session: what was sent is in the
    // folder the user looks in now, not at the next sync. A failed append still
    // leaves the mail delivered, so the reason is the copy's, not the send's.
    let result = provider::with(account, |provider| {
        provider.append(&mailbox, raw)?;
        fetch_into(provider, &mailbox, account, tx)
    });
    match result {
        Ok(()) => SentCopy::Saved { mailbox },
        Err(error) => SentCopy::Failed(error.to_string()),
    }
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
/// extension if that name is taken, and answer with the path it went to — what
/// an export with no dialog to ask shares with a downloaded attachment, the two
/// files Kimo names itself. The file is *created* rather than merely asked
/// about, so two exports of one mail, or two clicks on one attachment card,
/// cannot land on one name: the loser of that race moves to the next number
/// instead of overwriting the winner. A write that fails halfway takes its
/// partial file with it — a half-written file with a `.eml` or `.pdf` suffix
/// looks like a document and is not one.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Protocol;

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
            provider: None,
            provider_config: None,
            access_token: None,
            refresh_token: None,
            token_expires_at: None,
        };
        let mail = Outgoing {
            from_name: account.name.clone(),
            from_addr: account.username.clone(),
            to: vec!["bob@example.com".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Invoice 42".to_string(),
            body: "Please pay it.\n".to_string(),
            attachments: Vec::new(),
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let (recipients, copy) = send_message(&account, &mail, VendorKey::Custom, &tx).unwrap();
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
        assert!(send_message(&account, &mail, VendorKey::Custom, &tx).is_err());
        account.smtp = Some(crate::models::SmtpSettings {
            host: "127.0.0.1".to_string(),
            port,
            tls: crate::models::TlsMode::Plain,
            login: None,
        });
        let mut junk = mail.clone();
        junk.to = vec!["not an address".to_string()];
        let error = send_message(&account, &junk, VendorKey::Custom, &tx).unwrap_err();
        assert!(error.to_string().contains("not an address"), "{error}");
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

    /// The names in a directory, sorted: what a write left behind, and what it
    /// did not.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the directory is there")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// What the export makes of the dialog: the path it answered, the downloads
    /// directory when there was no dialog to ask at all — an error from the
    /// portal is not a failed export — and nothing to write when the user closed
    /// it, which is not a failure either.
    #[test]
    fn a_closed_dialog_writes_nothing_and_a_missing_one_writes_to_downloads() {
        let chosen = PathBuf::from("/home/you/Invoice 42.eml");
        match export_place(Ok(Some(chosen.clone()))) {
            Some(Place::Chosen(path)) => assert_eq!(path, chosen),
            other => panic!("a chosen file is where the export goes: {other:?}"),
        }
        assert!(export_place(Ok(None)).is_none(), "a cancel writes nothing");
        assert!(
            matches!(
                export_place(Err(MailError::Protocol("no session bus".to_string()))),
                Some(Place::Downloads)
            ),
            "no dialog at all is the downloads directory, not a failed export"
        );
    }

    /// Which writer an export gets. A file the dialog named is written where it
    /// says: a name that is taken is *replaced*, because the dialog asked about
    /// replacing it and the user answered, and no numbered sibling appears beside
    /// it — while an export with no dialog to ask is numbered like a download,
    /// so the file already there keeps its name and its bytes.
    #[test]
    fn an_export_writes_where_the_dialog_said_and_numbers_only_without_one() {
        let dir = std::env::temp_dir().join(format!("kimo-chosen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // The dialog's answer, over a file that is already there.
        let chosen = dir.join("Invoice 42.eml");
        fs::write(&chosen, b"an older export").unwrap();
        let written = write_export(
            &dir,
            "Invoice 42",
            &Place::Chosen(chosen.clone()),
            b"the mail, whole",
        )
        .unwrap();
        assert_eq!(written, chosen);
        assert_eq!(fs::read(&chosen).unwrap(), b"the mail, whole");
        assert_eq!(
            names(&dir),
            ["Invoice 42.eml"],
            "nothing numbered, and nothing left behind"
        );

        // No dialog to ask: the downloads directory, numbered, with the file
        // that is already there left as it is.
        let second = write_export(&dir, "Invoice 42", &Place::Downloads, b"one").unwrap();
        let third = write_export(&dir, "Invoice 42", &Place::Downloads, b"two").unwrap();
        assert_eq!(second, dir.join("Invoice 42-2.eml"));
        assert_eq!(third, dir.join("Invoice 42-3.eml"));
        assert_eq!(fs::read(&chosen).unwrap(), b"the mail, whole");
        assert_eq!(fs::read(&second).unwrap(), b"one");
        assert_eq!(fs::read(&third).unwrap(), b"two");

        // A chosen path that cannot take the file — here a directory of that
        // name — fails instead of half-succeeding, and takes its temporary with
        // it: what was there is still there, and nothing of Kimo's is.
        let taken = dir.join("a directory.eml");
        fs::create_dir_all(&taken).unwrap();
        let error =
            write_export(&dir, "a directory", &Place::Chosen(taken.clone()), b"x").unwrap_err();
        assert!(taken.is_dir(), "what was there is still there");
        assert!(!error.to_string().is_empty(), "the reason travels: {error}");
        assert!(
            !names(&dir).iter().any(|name| name.ends_with(".part")),
            "no temporary is left behind: {:?}",
            names(&dir)
        );

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
            provider: None,
            provider_config: None,
            access_token: None,
            refresh_token: None,
            token_expires_at: None,
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
