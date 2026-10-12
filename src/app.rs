//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.
//!
//! Layout: custom title bar on top (nav toggle, sync, preferences cog, window
//! controls), then three columns — collapsible navigation (every account as a
//! collapsible section of its folders), message list, reading pane.
//!
//! Preferences open from the title bar's cog as an `egui::Modal`: the
//! Accounts page holds the account list and the add/edit account form, the
//! Appearance page the theme (which resolves to a `theme::ColorScheme`), the
//! HTML renderer and the fonts. That renderer draws the
//! mail either as egui widgets (`crate::html`) or rasterized by Blitz
//! (`crate::blitz_render`) on a worker thread.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::folders::{self, FolderKind};
use crate::fonts::FontFamily;
use crate::models::{Account, MailMessage, Protocol, TlsMode};
use crate::storage::{self, Settings};
use crate::sync::{self, Event, Job};
use crate::theme::{self, ColorScheme, Theme};

/// Below this window width the navigation bar collapses automatically.
const NAV_AUTO_COLLAPSE_WIDTH: f32 = 640.0;

/// Which renderer draws the HTML body in the reading pane.
///
/// `Simple` is the built-in renderer: the mail becomes native egui widgets,
/// follows the app theme and stays interactive. `Blitz` runs the real browser
/// engine over the same HTML and shows the rasterized page, which is much
/// closer to what the sender designed. The choice lives in
/// Preferences → Appearance and is remembered in `settings.json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HtmlRenderer {
    Simple,
    Blitz,
}

impl HtmlRenderer {
    /// Short label for the Preferences radios.
    fn label(self) -> &'static str {
        match self {
            HtmlRenderer::Simple => "Simple",
            HtmlRenderer::Blitz => "Blitz",
        }
    }

    /// Value stored in `settings.json`.
    fn as_str(self) -> &'static str {
        match self {
            HtmlRenderer::Simple => "egui",
            HtmlRenderer::Blitz => "blitz",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "blitz" => HtmlRenderer::Blitz,
            _ => HtmlRenderer::Simple,
        }
    }

    /// One-line explanation shown under the renderer radios.
    fn description(self) -> &'static str {
        match self {
            HtmlRenderer::Simple => {
                "Mail HTML drawn as native egui widgets: theme-aware, selectable text, best-effort layout."
            }
            HtmlRenderer::Blitz => {
                "Full CSS layout rasterized by a browser engine: closest to the sender's design, but a static image."
            }
        }
    }
}

/// Pages of the preferences modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefsTab {
    Accounts,
    Appearance,
}

pub struct KimoApp {
    ctx: egui::Context,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    accounts: Vec<Account>,
    states: HashMap<String, AccountUiState>,
    selected_account: Option<String>,
    selected_folder: Option<String>,
    selected_message: Option<String>,
    filter: String,
    /// Whether the preferences modal is open.
    show_preferences: bool,
    /// Which preferences page is selected.
    prefs_tab: PrefsTab,
    /// Accounts page: the add/edit account form replaces the account list
    /// while this is set.
    show_account_form: bool,
    account_form: AccountForm,
    confirm_delete: Option<String>,
    nav_collapsed: bool,
    /// Accounts whose section in the navigation column is collapsed; an
    /// account is expanded unless its id is in here. Deliberately not
    /// persisted: which accounts you are reading is a per-session thing.
    collapsed_accounts: HashSet<String>,
    /// Whether the window was narrow last frame, so auto-collapse only
    /// triggers when crossing the threshold (not every frame).
    was_narrow: bool,
    /// Folder-list icons, keyed by (kind, open): only a generic folder has a
    /// second, open glyph. Uploaded once at startup from the embedded PNGs
    /// in `folders::FolderKind::ICONS`.
    folder_icons: HashMap<(FolderKind, bool), egui::TextureHandle>,
    /// The application icon, drawn at the left of the title bar. `None` only
    /// if the embedded PNG fails to decode.
    brand_icon: Option<egui::TextureHandle>,
    theme: Theme,
    /// Cached OS dark-mode preference, so "system" resolution does not hit
    /// the filesystem every frame. Refreshed on startup and whenever the
    /// theme cycles back to System.
    system_dark: bool,
    /// Why the mail database is not being written to disk, when that is the
    /// case; drawn as a one-line banner above the message list.
    storage_warning: Option<String>,
    /// Remote images for HTML mail, keyed by URL; fetched on worker threads
    /// and cached in memory as egui textures (disk cache in storage).
    images: HashMap<String, ImageStatus>,
    /// Remote stylesheets for HTML mail: `Some(text)` once loaded, `None`
    /// while loading or after a failure (no retry).
    stylesheets: HashMap<String, Option<String>>,
    /// Decoded `cid:` inline images, keyed by `"<message-uid>/<cid>"`;
    /// `None` marks a decode failure (not retried). Decoding happens once
    /// when a message is first rendered.
    inline_textures: HashMap<String, Option<egui::TextureHandle>>,
    /// Which renderer the reading pane uses for HTML bodies.
    html_renderer: HtmlRenderer,
    /// Installed font families for the Appearance pickers; `None` while the
    /// worker scan (`Job::ScanFonts`) is still running.
    font_families: Option<Vec<FontFamily>>,
    /// Picked font family names, as stored in `settings.json`; `None` means
    /// egui's built-in font (plus the CJK fallback).
    ui_font: Option<String>,
    mono_font: Option<String>,
    /// Blitz-rendered pages, keyed by message and geometry; `blitz_order`
    /// holds the insertion order so old pages can be evicted (textures are
    /// megabytes each).
    blitz_pages: HashMap<String, BlitzPage>,
    blitz_order: Vec<String>,
    /// Which render is still wanted: every request takes a new ticket and
    /// older ones stop early, so a superseded render does not keep a core
    /// busy for a result that will be dropped.
    blitz_cancel: crate::blitz_render::Cancellation,
    /// Key waiting for the pane geometry to settle, and since when. Splitter
    /// drags and compositor resize animations move the geometry every frame;
    /// see `BLITZ_SETTLE`.
    pending_blitz_key: Option<(String, Instant)>,
    /// Message the reading pane's scroll offset belongs to, so opening another
    /// mail starts at the top instead of mid-page.
    scrolled_message: Option<String>,
}

/// A Blitz page: rendering, rendered, or failed.
enum BlitzPage {
    /// A worker thread is rasterizing it.
    Loading,
    Ready(RasterizedPage),
    Failed(String),
}

/// A finished Blitz page, uploaded as egui textures. Tiles are stacked at
/// their `offset` from the page origin; both are in points.
///
/// Cloning is cheap: `TextureHandle` is a reference to a texture the context
/// owns, which is what lets a page be adopted under several cache keys.
#[derive(Clone)]
struct RasterizedPage {
    tiles: Vec<RasterTile>,
    /// Page size in points.
    size: egui::Vec2,
    /// Clickable link boxes relative to the page origin, in points.
    links: Vec<(egui::Rect, String)>,
    /// The message was too tall to rasterize in full; its tail is missing.
    truncated: bool,
}

#[derive(Clone)]
struct RasterTile {
    texture: egui::TextureHandle,
    /// Offset from the page origin, in points.
    offset: egui::Vec2,
    size: egui::Vec2,
}

enum ImageStatus {
    Loading,
    Failed,
    Ready(egui::TextureHandle),
}

#[derive(Default)]
pub struct AccountUiState {
    pub folders: Vec<String>,
    /// The account's folders and their cached mail, loaded from the database
    /// at startup and kept in memory (the DB is the durable copy).
    pub mailbox: storage::Mailbox,
    pub syncing: bool,
    pub error: Option<String>,
}

/// The fields of the add/edit account form, plus the account it is
/// editing: `editing` is `None` while the form is filling in a new one.
#[derive(Clone, Debug)]
struct AccountForm {
    /// The account being edited, by id. The id is the account's identity —
    /// minted once from its name, and the name of its cache file — so
    /// renaming an account does not move its cached mail.
    editing: Option<String>,
    name: String,
    protocol: Protocol,
    host: String,
    port_str: String,
    username: String,
    password: String,
    tls: TlsMode,
}

impl Default for AccountForm {
    /// An empty form for a new account: IMAP over implicit TLS, the setup
    /// most providers use.
    fn default() -> Self {
        Self {
            editing: None,
            name: String::new(),
            protocol: Protocol::Imap,
            host: String::new(),
            port_str: Protocol::Imap.default_port(TlsMode::Implicit).to_string(),
            username: String::new(),
            password: String::new(),
            tls: TlsMode::Implicit,
        }
    }
}

impl AccountForm {
    /// The same form, pre-filled from a configured account and remembering
    /// its id so that saving updates it instead of adding another one.
    fn edit(account: &Account) -> Self {
        Self {
            editing: Some(account.id.clone()),
            name: account.name.clone(),
            protocol: account.protocol,
            host: account.host.clone(),
            port_str: account.port.to_string(),
            username: account.username.clone(),
            password: account.password.clone(),
            tls: account.tls,
        }
    }

    /// Every required field is filled in (and the port is a port).
    fn is_valid(&self) -> bool {
        !self.name.trim().is_empty()
            && !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && self.port_str.trim().parse::<u16>().is_ok()
    }

    /// The port to save: what was typed, or the protocol/TLS default when
    /// that text is not a number.
    fn port(&self) -> u16 {
        self.port_str
            .trim()
            .parse()
            .unwrap_or_else(|_| self.protocol.default_port(self.tls))
    }

    /// The account this form describes, under an id the caller picks — the
    /// edited account's own, or a fresh one for a new account.
    fn to_account(&self, id: String) -> Account {
        Account {
            id,
            name: self.name.trim().to_string(),
            protocol: self.protocol,
            host: self.host.trim().to_string(),
            port: self.port(),
            username: self.username.trim().to_string(),
            password: self.password.clone(),
            tls: self.tls,
        }
    }

    /// Whether saving this form reaches a different mailbox than `account`
    /// does. The endpoint — protocol, host, port, username — steers which
    /// mailbox is logged into, so a change there throws the cached mail
    /// away; the name is only a label and the password is a credential, so
    /// neither disturbs the cache.
    fn points_elsewhere(&self, account: &Account) -> bool {
        account.protocol != self.protocol
            || account.host != self.host.trim()
            || account.port != self.port()
            || account.username != self.username.trim()
    }
}

impl KimoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // egui's built-in fonts only cover Latin/Cyrillic/Greek, so the CJK
        // fallback goes in immediately; the families picked in Preferences are
        // applied once the worker scan below has resolved their files.
        crate::fonts::install(&cc.egui_ctx, None, None);

        let settings = storage::load_settings();
        // The accounts and their mail live in the database; this is the one
        // piece of storage I/O the UI thread does, and it replaces the JSON
        // parse the app used to do here. Workers do everything else.
        let accounts = crate::db::load_accounts().unwrap_or_else(|error| {
            eprintln!("kimo: cannot read the accounts: {error}");
            Vec::new()
        });
        let mailboxes = crate::db::load_mailboxes(
            &accounts
                .iter()
                .map(|account| account.id.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        let mut states = HashMap::new();
        for account in &accounts {
            let state = AccountUiState {
                mailbox: mailboxes.get(&account.id).cloned().unwrap_or_default(),
                ..AccountUiState::default()
            };
            states.insert(account.id.clone(), state);
        }

        // Folder-list icons: white PNGs (see `folders`), tinted per row.
        let folder_icons = load_folder_icons(&cc.egui_ctx);
        let brand_icon = load_brand_icon(&cc.egui_ctx);

        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            accounts,
            states,
            selected_account: None,
            selected_folder: None,
            selected_message: None,
            filter: String::new(),
            show_preferences: false,
            prefs_tab: PrefsTab::Accounts,
            show_account_form: false,
            account_form: AccountForm::default(),
            confirm_delete: None,
            nav_collapsed: false,
            collapsed_accounts: HashSet::new(),
            was_narrow: false,
            folder_icons,
            brand_icon,
            theme: Theme::parse(&settings.theme),
            system_dark: system_prefers_dark(),
            storage_warning: crate::db::warning(),
            images: HashMap::new(),
            stylesheets: HashMap::new(),
            inline_textures: HashMap::new(),
            html_renderer: HtmlRenderer::parse(settings.html_renderer.as_deref().unwrap_or("")),
            font_families: None,
            ui_font: settings.ui_font.clone(),
            mono_font: settings.mono_font.clone(),
            blitz_pages: HashMap::new(),
            blitz_order: Vec::new(),
            blitz_cancel: crate::blitz_render::Cancellation::default(),
            pending_blitz_key: None,
            scrolled_message: None,
        };

        app.selected_account = app.accounts.first().map(|a| a.id.clone());
        app.selected_folder = Some("INBOX".to_string());

        // System-tray icon (best effort; StatusNotifierItem over D-Bus).
        crate::tray::spawn(app.ctx.clone());

        // Font families for Preferences → Appearance. The scan reads every
        // installed face's name table, so it runs on a worker thread; the
        // picked families are applied when the result arrives.
        sync::spawn(Job::ScanFonts, app.ctx.clone(), app.tx.clone());

        for account in app.accounts.clone() {
            sync::spawn(
                Job::Sync {
                    account,
                    purge: false,
                },
                app.ctx.clone(),
                app.tx.clone(),
            );
        }
        app
    }

    // ----- event plumbing ---------------------------------------------------

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply_event(event);
        }
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::SyncStarted { account_id } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = true;
                    state.error = None;
                }
            }
            Event::Folders {
                account_id,
                folders,
            } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.folders = folders;
                }
            }
            Event::Messages {
                account_id,
                folder,
                uidvalidity,
                messages,
            } => {
                // The worker has already stored this batch; what follows is
                // the in-memory copy the list and the reading pane draw from.
                if let Some(state) = self.states.get_mut(&account_id) {
                    if state.mailbox.folders.iter().all(|f| f.name != folder) {
                        state.mailbox.folders.push(storage::Folder {
                            name: folder.clone(),
                            uidvalidity: None,
                            synced: false,
                            messages: Vec::new(),
                        });
                    }
                    let fc = state
                        .mailbox
                        .folders
                        .iter_mut()
                        .find(|f| f.name == folder)
                        .expect("folder just inserted");
                    if let Some(uv) = uidvalidity {
                        if fc.uidvalidity.is_some() && fc.uidvalidity != Some(uv) {
                            fc.messages.clear(); // new UIDVALIDITY generation: old UIDs are meaningless
                        }
                        fc.uidvalidity = Some(uv);
                    }
                    for m in messages {
                        match fc.messages.iter().position(|x| x.uid == m.uid) {
                            Some(i) => {
                                let keep_seen = fc.messages[i].seen || m.seen;
                                let mut merged = m;
                                merged.seen = keep_seen;
                                fc.messages[i] = merged;
                            }
                            None => fc.messages.push(m),
                        }
                    }
                    sort_messages(&mut fc.messages);
                    fc.synced = true;
                }
            }
            Event::SyncDone { account_id } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                }
            }
            Event::SyncFailed { account_id, error } => {
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    state.error = Some(error);
                }
            }
            Event::ImageLoaded {
                url,
                width,
                height,
                rgba,
            } => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    &rgba,
                );
                let texture = self.ctx.load_texture(&url, image, Default::default());
                self.images.insert(url, ImageStatus::Ready(texture));
            }
            Event::ImageFailed { url } => {
                self.images.insert(url, ImageStatus::Failed);
            }
            Event::StylesheetLoaded { url, text } => {
                self.stylesheets.insert(url, Some(text));
            }
            Event::StylesheetFailed { url } => {
                self.stylesheets.insert(url, None);
            }
            Event::HtmlRendered { key, page } => self.store_blitz_page(key, page),
            Event::HtmlRenderFailed { key, error } => {
                if let Some(slot) = self.blitz_pages.get_mut(&key) {
                    *slot = BlitzPage::Failed(error);
                }
            }
            Event::FontsScanned(families) => {
                self.font_families = Some(families);
                // settings.json may name families that only now resolve.
                self.apply_fonts();
            }
        }
    }

    // ----- actions ----------------------------------------------------------

    fn sync_all(&mut self) {
        for account in self.accounts.clone() {
            sync::spawn(
                Job::Sync {
                    account,
                    purge: false,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }
    }

    /// Unread mail in one folder, from the messages already cached for it.
    fn folder_unread(&self, account_id: &str, folder: &str) -> usize {
        self.states
            .get(account_id)
            .and_then(|state| state.mailbox.folders.iter().find(|f| f.name == folder))
            .map(|f| f.messages.iter().filter(|m| !m.seen).count())
            .unwrap_or(0)
    }

    /// Unread mail across every cached folder of one account — the number on
    /// its header in the navigation column. Folders that were never fetched
    /// are simply not counted; nothing here goes to the network.
    fn account_unread(&self, account_id: &str) -> usize {
        self.states
            .get(account_id)
            .map(|state| {
                state
                    .mailbox
                    .folders
                    .iter()
                    .flat_map(|folder| folder.messages.iter())
                    .filter(|message| !message.seen)
                    .count()
            })
            .unwrap_or(0)
    }

    fn select_folder(&mut self, account_id: String, folder: String, protocol: Protocol) {
        self.selected_account = Some(account_id.clone());
        self.selected_folder = Some(folder.clone());
        self.selected_message = None;

        let needs_fetch = match self.states.get(&account_id) {
            Some(state) => match state.mailbox.folders.iter().find(|f| f.name == folder) {
                Some(fc) => !fc.synced,
                None => true,
            },
            None => false,
        };
        if !needs_fetch {
            return;
        }
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            let job = match protocol {
                Protocol::Imap => Job::SyncFolder { account, folder },
                Protocol::Pop3 => Job::Sync {
                    account,
                    purge: false,
                },
            };
            sync::spawn(job, self.ctx.clone(), self.tx.clone());
        }
    }

    fn open_message(&mut self, msg: &MailMessage) {
        self.selected_message = Some(msg.uid.clone());
        if msg.seen {
            return;
        }
        let Some(account_id) = self.selected_account.clone() else {
            return;
        };
        let folder = self.selected_folder.clone().unwrap_or_default();

        // Mark seen locally first so the UI reacts immediately; the row in the
        // database is written by the job below, never on this thread.
        if let Some(state) = self.states.get_mut(&account_id) {
            if let Some(fc) = state.mailbox.folders.iter_mut().find(|f| f.name == folder) {
                if let Some(existing) = fc.messages.iter_mut().find(|m| m.uid == msg.uid) {
                    existing.seen = true;
                }
            }
        }

        // Then hand the flag to a worker: IMAP pushes `\\Seen` to the server,
        // POP3 has nothing to push, and both store it locally.
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            sync::spawn(
                Job::MarkSeen {
                    account,
                    folder,
                    uid: msg.uid.clone(),
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }
    }

    fn delete_account(&mut self, account_id: &str) {
        self.accounts.retain(|a| a.id != account_id);
        self.states.remove(account_id);
        if self.selected_account.as_deref() == Some(account_id) {
            self.selected_account = self.accounts.first().map(|a| a.id.clone());
            self.selected_folder = Some("INBOX".to_string());
            self.selected_message = None;
        }
        // The account row, and through its cascades every message of it, goes
        // on a worker thread; the UI has already dropped its own copy.
        sync::spawn(
            Job::DeleteAccount {
                account_id: account_id.to_string(),
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
        self.forget_blitz_pages(account_id);
    }

    /// Save the add/edit form: mint an account (and a fresh id) for a new
    /// one, or replace the edited account in place, then sync it so the
    /// window shows what the credentials actually reach.
    fn save_account_form(&mut self) {
        let form = self.account_form.clone();
        let edited = form
            .editing
            .clone()
            .and_then(|id| self.accounts.iter().position(|a| a.id == id));
        let account = match edited {
            Some(index) => form.to_account(self.accounts[index].id.clone()),
            None => form.to_account(self.unique_account_id(&slug(&form.name))),
        };
        let account_id = account.id.clone();

        let switches_mailbox = match edited {
            Some(index) => {
                let previous = std::mem::replace(&mut self.accounts[index], account.clone());
                form.points_elsewhere(&previous)
            }
            // A new account has no cached mail yet; treat it as a switch so
            // the (empty) state below is built the same way.
            None => {
                self.accounts.push(account.clone());
                true
            }
        };

        if switches_mailbox {
            // What is stored describes the mailbox the old settings reached,
            // and the new ones may be a different server entirely, so the job
            // below purges it before fetching rather than show mail from an
            // account that is gone. Its in-flight renders go too.
            self.states
                .insert(account_id.clone(), AccountUiState::default());
            self.forget_blitz_pages(&account_id);
        }

        // One job saves the account row, purges the mail when the endpoint
        // moved and then syncs, so no second job can reorder those steps.
        sync::spawn(
            Job::Sync {
                account,
                purge: switches_mailbox,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );

        if edited.is_none() {
            // Adding an account takes the window, like it always did.
            self.selected_account = Some(account_id.clone());
            self.selected_folder = Some("INBOX".to_string());
            self.selected_message = None;
        } else if switches_mailbox && self.selected_account.as_deref() == Some(account_id.as_str())
        {
            // The folder list and its mail are about to be replaced.
            self.selected_folder = Some("INBOX".to_string());
            self.selected_message = None;
        }
        self.show_account_form = false;
    }

    /// An id for a new account: the slug of its name, numbered if that is
    /// taken. Ids name the cache files, so they are never reused.
    fn unique_account_id(&self, base: &str) -> String {
        let mut id = base.to_string();
        let mut n = 1;
        while self.accounts.iter().any(|a| a.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        id
    }

    /// Drop every cached Blitz page belonging to an account (keys start
    /// with its id and a `/`, so `work` cannot match `work-2`).
    fn forget_blitz_pages(&mut self, account_id: &str) {
        let prefix = format!("{account_id}/");
        self.blitz_pages.retain(|key, _| !key.starts_with(&prefix));
        self.blitz_order
            .retain(|key| self.blitz_pages.contains_key(key));
        if self
            .pending_blitz_key
            .as_ref()
            .is_some_and(|(key, _)| key.starts_with(&prefix))
        {
            self.pending_blitz_key = None;
        }
    }

    // ----- queries ----------------------------------------------------------

    /// Ask a worker thread to fetch a remote mail image (once per URL).
    fn request_image(&mut self, url: String) {
        if self.images.contains_key(&url) {
            return; // already loading, loaded or failed
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            self.images.insert(url, ImageStatus::Failed);
            return;
        }
        self.images.insert(url.clone(), ImageStatus::Loading);
        sync::spawn(Job::FetchImage { url }, self.ctx.clone(), self.tx.clone());
    }

    /// Ask a worker thread to fetch a remote stylesheet (once per URL).
    fn request_stylesheet(&mut self, url: String) {
        if self.stylesheets.contains_key(&url) {
            return;
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            self.stylesheets.insert(url.clone(), None);
            return;
        }
        self.stylesheets.insert(url.clone(), None);
        sync::spawn(
            Job::FetchStylesheet { url },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    // ----- Blitz pages ------------------------------------------------------

    /// Identity of a rendered page: the mail it shows. Two pages with the
    /// same identity are the same document, rendered at different widths.
    fn blitz_identity(&self, msg: &MailMessage) -> String {
        format!(
            "{}/{}/{}/",
            self.selected_account.as_deref().unwrap_or("-"),
            self.selected_folder.as_deref().unwrap_or("-"),
            msg.uid
        )
    }

    /// Ask a worker thread to rasterize this message's HTML with Blitz.
    ///
    /// Rendering a newsletter takes hundreds of milliseconds, so it never
    /// happens on the UI thread (hard rule 3); the finished page arrives as
    /// `Event::HtmlRendered`.
    fn start_blitz_render(
        &mut self,
        key: String,
        msg: &MailMessage,
        html: &str,
        width: u32,
        viewport_height: u32,
        scale: f32,
    ) {
        // A new request cancels whatever render is still running (its ticket
        // stops being current, see `blitz_render::Cancellation`), and the
        // Loading entry it left behind goes with it.
        let ticket = self.blitz_cancel.issue();
        let stale: Vec<String> = self
            .blitz_pages
            .iter()
            .filter(|(_, page)| matches!(page, BlitzPage::Loading))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.blitz_pages.remove(&key);
        }
        self.blitz_order
            .retain(|entry| self.blitz_pages.contains_key(entry));

        let request = crate::blitz_render::PageRequest {
            html: html.to_string(),
            width,
            viewport_height,
            scale,
            inline_images: msg
                .inline_images
                .iter()
                .map(|image| (image.cid.clone(), image.data.clone()))
                .collect(),
            ticket,
        };
        self.pending_blitz_key = None;
        self.blitz_pages.insert(key.clone(), BlitzPage::Loading);
        self.touch_blitz_page(&key);
        self.evict_blitz_pages();
        sync::spawn(
            Job::RenderHtml {
                key,
                request: Box::new(request),
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// Turn a finished render into egui textures.
    ///
    /// Runs on the UI thread, but only uploads what the worker already
    /// rasterized. A result whose key is no longer pending (the user selected
    /// another message or resized the pane) is dropped.
    fn store_blitz_page(&mut self, key: String, page: crate::blitz_render::Page) {
        if !matches!(self.blitz_pages.get(&key), Some(BlitzPage::Loading)) {
            return;
        }
        // The renderer may have rasterized at a lower scale than the one we
        // asked for (a page too large for its pixel budget), so sizes come
        // from the page itself, never from the requested scale.
        let to_points = |pixels: u32| pixels as f32 / page.scale;

        let mut tiles = Vec::with_capacity(page.tiles.len());
        for (index, tile) in page.tiles.iter().enumerate() {
            // vello renders premultiplied alpha, which is also how egui
            // stores and blends its textures.
            let image = egui::ColorImage::from_rgba_premultiplied(
                [tile.width as usize, tile.height as usize],
                &tile.rgba,
            );
            tiles.push(RasterTile {
                texture: self.ctx.load_texture(
                    format!("blitz:{key}:{index}"),
                    image,
                    egui::TextureOptions::LINEAR,
                ),
                offset: egui::vec2(0.0, to_points(tile.y)),
                size: egui::vec2(to_points(tile.width), to_points(tile.height)),
            });
        }
        let links = page
            .links
            .iter()
            .map(|link| {
                (
                    egui::Rect::from_min_size(
                        egui::pos2(link.x, link.y),
                        egui::vec2(link.width, link.height),
                    ),
                    link.href.clone(),
                )
            })
            .collect();
        self.blitz_pages.insert(
            key.clone(),
            BlitzPage::Ready(RasterizedPage {
                tiles,
                size: egui::vec2(page.css_width, page.css_height),
                links,
                truncated: page.truncated,
            }),
        );
        self.touch_blitz_page(&key);
        self.evict_blitz_pages();
    }

    /// Adopt a page already rendered for this mail when the pane wants a width
    /// it can simply be scaled down to (see `BLITZ_REUSE_RATIO`). Returns true
    /// when a page was adopted under `key`.
    ///
    /// The adopted entry is another handle on the same textures, not a copy of
    /// them, so this costs nothing next to a render.
    fn reuse_blitz_page(&mut self, identity: &str, width: u32, scale: f32, key: &str) -> bool {
        let wanted = width as f32;
        let source = self.blitz_pages.iter().find_map(|(candidate, page)| {
            let BlitzPage::Ready(page) = page else {
                return None;
            };
            // The identity ends in '/', so a shorter uid cannot match a longer.
            let reusable =
                candidate.starts_with(identity) && blitz_page_reusable(page.size.x * scale, wanted);
            reusable.then(|| candidate.clone())
        });
        let Some(source) = source else {
            return false;
        };
        let Some(BlitzPage::Ready(page)) = self.blitz_pages.get(&source) else {
            return false;
        };
        let page = page.clone();
        self.blitz_pages
            .insert(key.to_string(), BlitzPage::Ready(page));
        self.pending_blitz_key = None;
        self.touch_blitz_page(key);
        self.evict_blitz_pages();
        true
    }

    /// Move `key` to the back of the cache order, so eviction always drops
    /// the least recently used page.
    fn touch_blitz_page(&mut self, key: &str) {
        self.blitz_order.retain(|entry| entry != key);
        self.blitz_order.push(key.to_string());
    }

    /// Drop the oldest rendered pages; each one is a few megabytes of texture.
    fn evict_blitz_pages(&mut self) {
        while self.blitz_order.len() > BLITZ_PAGES_CACHED {
            let oldest = self.blitz_order.remove(0);
            self.blitz_pages.remove(&oldest);
        }
    }

    /// Reading pane in Blitz mode: request a render if this message and
    /// geometry have none yet, then blit the finished page.
    fn blitz_view(&mut self, ui: &mut egui::Ui, msg: &MailMessage, html: &str) {
        let scale = ui.ctx().pixels_per_point();
        let available = ui.available_width().max(160.0);
        let css_width = (available - BLITZ_PAGE_MARGIN * 2.0).max(160.0);
        let width = quantize(css_width * scale, BLITZ_WIDTH_STEP);
        // The pane height reaches Blitz's viewport (`vh` and friends) but not
        // the text's layout width, so it is sampled when a render starts
        // instead of being part of the key: a window resize must not make an
        // otherwise identical page render again.
        let viewport_height = (ui.available_height().max(160.0) * scale).round() as u32;
        let identity = self.blitz_identity(msg);
        let key = blitz_key(&identity, width, scale);

        if !self.blitz_pages.contains_key(&key)
            && !self.reuse_blitz_page(&identity, width, scale, &key)
        {
            // Let the geometry settle first: while it keeps changing (splitter
            // drag, animated window resize) no render is started at all. The
            // repaint request is what makes the render happen once the pointer
            // stops moving — egui has no other reason to draw another frame.
            let now = Instant::now();
            let held = self.pending_blitz_key.clone();
            let pending = held.as_ref().map(|(key, since)| (key.as_str(), *since));
            match blitz_settle(pending, &key, now) {
                BlitzWait::Start => {
                    self.start_blitz_render(key.clone(), msg, html, width, viewport_height, scale);
                }
                BlitzWait::Wait(remaining) => {
                    if held.as_ref().map(|(key, _)| key.as_str()) != Some(key.as_str()) {
                        // First sighting of this geometry: start the clock.
                        self.pending_blitz_key = Some((key.clone(), now));
                    }
                    ui.ctx().request_repaint_after(remaining);
                }
            }
        }

        let mut retry = false;
        match self.blitz_pages.get(&key) {
            Some(BlitzPage::Ready(page)) => draw_blitz_page(ui, page),
            Some(BlitzPage::Failed(error)) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(egui::RichText::new("Blitz could not render this mail").weak());
                    ui.label(egui::RichText::new(error.as_str()).small().weak());
                    retry = ui.button("Try again").clicked();
                });
            }
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.add(egui::Spinner::new().color(theme::of(ui.ctx()).accent));
                    ui.label(
                        egui::RichText::new("Rendering HTML with Blitz…")
                            .small()
                            .weak(),
                    );
                });
            }
        }
        if retry {
            self.blitz_pages.remove(&key);
            self.blitz_order.retain(|entry| entry != &key);
            self.pending_blitz_key = None;
        }
    }

    /// Write the current UI settings back to `settings.json`.
    fn persist_settings(&self) {
        let _ = storage::save_settings(&Settings {
            theme: self.theme.as_str().to_string(),
            html_renderer: Some(self.html_renderer.as_str().to_string()),
            ui_font: self.ui_font.clone(),
            mono_font: self.mono_font.clone(),
        });
    }

    /// Re-install the egui fonts from the picked family names. Reads one file
    /// per pick, which is cheap; the directory scan behind
    /// `font_families` is a worker job.
    fn apply_fonts(&self) {
        let families = self.font_families.as_deref().unwrap_or_default();
        let pick = |name: &Option<String>| {
            name.as_deref()
                .and_then(|name| crate::fonts::find(families, name))
                .cloned()
        };
        crate::fonts::install(
            &self.ctx,
            pick(&self.ui_font).as_ref(),
            pick(&self.mono_font).as_ref(),
        );
    }

    /// Apply the selected theme to the context, resolving "system" against
    /// the cached OS preference. The resolved scheme is what the hand-painted
    /// parts of the UI read back through `theme::of`.
    fn apply_theme(&self, ctx: &egui::Context) {
        theme::install(ctx, self.theme.scheme(self.system_dark));
    }

    /// Messages of the currently selected folder, filtered and sorted for display.
    fn current_messages(&self) -> Vec<MailMessage> {
        let Some(account_id) = &self.selected_account else {
            return Vec::new();
        };
        let Some(folder) = &self.selected_folder else {
            return Vec::new();
        };
        let Some(state) = self.states.get(account_id) else {
            return Vec::new();
        };
        let Some(fc) = state.mailbox.folders.iter().find(|f| f.name == *folder) else {
            return Vec::new();
        };
        let needle = self.filter.trim().to_lowercase();
        fc.messages
            .iter()
            .filter(|m| {
                needle.is_empty()
                    || m.subject.to_lowercase().contains(&needle)
                    || m.from.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
    }

    fn selected_folder_synced(&self) -> bool {
        let Some(account_id) = &self.selected_account else {
            return false;
        };
        let Some(folder) = &self.selected_folder else {
            return false;
        };
        let Some(state) = self.states.get(account_id) else {
            return false;
        };
        state
            .mailbox
            .folders
            .iter()
            .find(|f| f.name == *folder)
            .is_some_and(|f| f.synced)
    }

    fn selected_message_view(&self) -> Option<MailMessage> {
        let uid = self.selected_message.as_ref()?;
        self.current_messages().into_iter().find(|m| m.uid == *uid)
    }

    // ----- UI ----------------------------------------------------------------

    /// Custom window title bar (the window is created undecorated in
    /// `main.rs`). Left to right: nav collapse toggle, app title, account
    /// menu, sync button, per-account status. Right edge: theme switch,
    /// then minimize / maximize / close. The leftover space drags the
    /// window (double-click toggles maximize).
    fn titlebar(&mut self, ui: &mut egui::Ui) {
        let bar = ui.available_rect_before_wrap();
        let visuals = ui.visuals().clone();
        let scheme = theme::of(ui.ctx());
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));

        #[derive(Clone, Copy)]
        enum Ctrl {
            Minimize,
            Maximize,
            Close,
        }
        // Drawn right to left so Close ends up at the very right edge.
        let buttons = [
            (Ctrl::Close, false),
            (Ctrl::Maximize, maximized),
            (Ctrl::Minimize, false),
        ];
        for (idx, (ctrl, is_max)) in buttons.iter().enumerate() {
            let right = bar.right() - TITLEBAR_BUTTON_WIDTH * idx as f32;
            let rect = egui::Rect::from_min_max(
                egui::pos2(right - TITLEBAR_BUTTON_WIDTH, bar.top()),
                egui::pos2(right, bar.bottom()),
            );
            let resp = ui.interact(
                rect,
                egui::Id::new(("titlebar-button", idx)),
                egui::Sense::click(),
            );
            let fg = if resp.hovered() {
                let bg = match ctrl {
                    Ctrl::Close => scheme.danger_hover,
                    _ => visuals.widgets.hovered.bg_fill,
                };
                ui.painter().rect_filled(rect, 0.0, bg);
                match ctrl {
                    // The scheme's on-danger color reads best on the close
                    // red; elsewhere keep the theme text color so the glyph
                    // survives light visuals.
                    Ctrl::Close => scheme.on_danger,
                    _ => visuals.text_color(),
                }
            } else {
                visuals.text_color()
            };
            match (ctrl, is_max) {
                (Ctrl::Minimize, _) => paint_minimize_icon(ui.painter(), rect, fg),
                (Ctrl::Maximize, false) => paint_maximize_icon(ui.painter(), rect, fg),
                (Ctrl::Maximize, true) => {
                    paint_restore_icon(ui.painter(), rect, fg, visuals.panel_fill)
                }
                (Ctrl::Close, _) => paint_close_icon(ui.painter(), rect, fg),
            }
            if resp.clicked() {
                match ctrl {
                    Ctrl::Minimize => ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    Ctrl::Maximize => ui
                        .ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                    Ctrl::Close => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
        }

        // Preferences (cog), just left of the window controls: the theme and
        // HTML renderer toggles used to sit here, and adding/removing accounts
        // in the account menu below — all of it moved into this modal.
        let cog_right = bar.right() - TITLEBAR_BUTTON_WIDTH * buttons.len() as f32;
        let cog_rect = egui::Rect::from_min_max(
            egui::pos2(cog_right - TITLEBAR_BUTTON_WIDTH, bar.top()),
            egui::pos2(cog_right, bar.bottom()),
        );
        let cog_resp = ui.interact(
            cog_rect,
            egui::Id::new("titlebar-preferences"),
            egui::Sense::click(),
        );
        if cog_resp.hovered() {
            ui.painter()
                .rect_filled(cog_rect, 0.0, visuals.widgets.hovered.bg_fill);
        }
        paint_cog_icon(ui.painter(), cog_rect, visuals.text_color());
        if cog_resp.clicked() {
            self.show_preferences = true;
        }
        cog_resp.on_hover_text("Preferences");

        // --- left content row, kept clear of the window buttons ---
        let content_right = cog_rect.left();
        let row_size = egui::vec2((content_right - bar.left()).max(0.0), bar.height());
        let content_end = ui
            .allocate_ui_with_layout(
                row_size,
                egui::Layout::left_to_right(egui::Align::Center),
                |row| {
                    // Brand first: the application icon is the leftmost
                    // thing in the bar (the tray embeds the same PNG).
                    if let Some(icon) = self.brand_icon.clone() {
                        row.add(
                            egui::Image::from_texture(&icon)
                                .fit_to_exact_size(egui::vec2(BRAND_ICON_SIZE, BRAND_ICON_SIZE)),
                        );
                    }

                    // Nav collapse toggle (hamburger, painted as lines).
                    let toggle = row.add_sized([26.0, 22.0], egui::Button::new(""));
                    if toggle.clicked() {
                        self.nav_collapsed = !self.nav_collapsed;
                    }
                    let tr = toggle.rect;
                    for i in -1i32..=1 {
                        let y = tr.center().y + i as f32 * 4.0;
                        row.painter().line_segment(
                            [
                                egui::pos2(tr.left() + 6.0, y),
                                egui::pos2(tr.right() - 6.0, y),
                            ],
                            egui::Stroke::new(1.5_f32, visuals.text_color()),
                        );
                    }
                    toggle.on_hover_text(if self.nav_collapsed {
                        "Show folders"
                    } else {
                        "Hide folders"
                    });

                    // No account menu here: the navigation column lists
                    // every account and its folders, so switching accounts is
                    // picking a folder there. The title bar keeps the global
                    // actions only (add/remove accounts lives in Preferences).
                    //
                    // Sync: icon-only, like the window controls — the tooltip
                    // carries the label.
                    let sync = row.add_sized([24.0, 22.0], egui::Button::new(""));
                    if sync.clicked() {
                        self.sync_all();
                    }
                    paint_sync_icon(row.painter(), sync.rect, visuals.text_color());
                    sync.on_hover_text("Sync all accounts");

                    // Per-account status: spinner while syncing, warning on error.
                    for account in self.accounts.clone() {
                        let Some(state) = self.states.get(&account.id) else {
                            continue;
                        };
                        if state.syncing {
                            row.add(egui::Spinner::new().color(scheme.accent))
                                .on_hover_text(format!("{}: syncing…", account.name));
                        } else if let Some(err) = &state.error {
                            row.label(egui::RichText::new("⚠").color(scheme.danger))
                                .on_hover_text(format!("{}: {err}", account.name));
                        }
                    }

                    row.min_rect().right()
                },
            )
            .inner;

        // --- drag surface: whatever is left between content and buttons ---
        let drag_left = content_end + 4.0;
        let drag_rect = egui::Rect::from_min_max(
            egui::pos2(drag_left.max(bar.left()), bar.top()),
            egui::pos2(content_right, bar.bottom()),
        );
        if drag_rect.is_positive() {
            let drag = ui.interact(
                drag_rect,
                egui::Id::new("titlebar-drag"),
                egui::Sense::click_and_drag(),
            );
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
        }

        ui.painter().line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
        );
    }

    /// Left column: folders of the selected account.
    /// Left column: every account as a collapsible section of its folders.
    /// The selection can be a folder of any of them — switching accounts is
    /// picking a folder here, so the title bar needs no account menu.
    fn nav(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        if self.accounts.is_empty() {
            ui.label("No accounts yet.");
            ui.label(
                egui::RichText::new("Add one in Preferences (the cog in the title bar).")
                    .small()
                    .weak(),
            );
            return;
        }
        let accounts = self.accounts.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(2.0);
                for account in &accounts {
                    self.account_section(ui, scheme, account);
                }
            });
    }

    /// One account in the navigation column: a header that collapses it, then
    /// its folders, indented under it.
    fn account_section(&mut self, ui: &mut egui::Ui, scheme: ColorScheme, account: &Account) {
        let collapsed = self.collapsed_accounts.contains(&account.id);
        let unread = self.account_unread(&account.id);
        if account_row(ui, scheme, &account.name, unread, collapsed).clicked() {
            if collapsed {
                self.collapsed_accounts.remove(&account.id);
            } else {
                self.collapsed_accounts.insert(account.id.clone());
            }
            return;
        }
        if collapsed {
            ui.add_space(2.0);
            return;
        }

        let (folders, syncing, error) = {
            let state = self.states.entry(account.id.clone()).or_default();
            (state.folders.clone(), state.syncing, state.error.clone())
        };
        let folders_were_empty = folders.is_empty();
        // Before the first sync there is nothing to list, but INBOX is worth
        // offering: clicking it is what starts the fetch.
        let folder_list = if folders.is_empty() {
            vec!["INBOX".to_string()]
        } else {
            folders
        };
        ui.indent(("account-folders", &account.id), |ui| {
            for folder in folder_list {
                let selected = self.selected_account.as_deref() == Some(account.id.as_str())
                    && self.selected_folder.as_deref() == Some(folder.as_str());
                let unread = self.folder_unread(&account.id, &folder);
                // `folder` is the IMAP wire form (possibly modified UTF-7);
                // decode it for the label and for the icon lookup, which
                // matches the readable name.
                let display = crate::protocol::imap::decode_mailbox_name(folder.as_str());
                let kind = folders::kind_of(&display);
                // Only a generic folder has an open variant; the special
                // mailboxes keep one glyph.
                let open = selected && kind == FolderKind::Folder;
                let icon = self.folder_icons.get(&(kind, open)).cloned();
                let row = folder_row(ui, scheme, icon.as_ref(), &display, unread, selected);
                if row.clicked() {
                    self.select_folder(account.id.clone(), folder, account.protocol);
                }
            }
        });
        if syncing && folders_were_empty {
            ui.add_space(4.0);
            ui.add(egui::Spinner::new().color(scheme.accent));
        }
        if let Some(err) = error {
            ui.add_space(4.0);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.label(
                egui::RichText::new(format!("⚠ {err}"))
                    .small()
                    .color(scheme.danger),
            );
        }
        ui.add_space(8.0);
    }

    /// Middle column: filter bar plus the message list.
    fn mail_list(&mut self, ui: &mut egui::Ui) {
        // The database could not be opened: the session works, nothing is
        // saved, and this is the only place that says so.
        if let Some(warning) = &self.storage_warning {
            ui.label(
                egui::RichText::new(format!("Mail is not being saved: {warning}"))
                    .small()
                    .color(theme::of(ui.ctx()).danger),
            );
            ui.separator();
        }
        if self.accounts.is_empty() || self.selected_account.is_none() {
            ui.centered_and_justified(|ui| {
                ui.heading("Kimo");
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter by subject or sender…")
                    .desired_width(200.0),
            );
            let count = self.current_messages().len();
            ui.label(
                egui::RichText::new(format!("{count} messages"))
                    .small()
                    .weak(),
            );
        });
        ui.separator();

        let messages = self.current_messages();
        if messages.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if !self.filter.is_empty() {
                    "No messages match the filter."
                } else if self.selected_folder_synced() {
                    "This folder is empty."
                } else {
                    "Not fetched yet — click Sync in the title bar."
                };
                ui.label(egui::RichText::new(text).weak().heading());
            });
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for msg in &messages {
                    self.message_row(ui, msg);
                }
            });
    }

    /// One message card in the list: avatar on the left; sender + timestamp,
    /// subject and a content preview stacked on the right.
    fn message_row(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
        let is_selected = self.selected_message.as_deref() == Some(msg.uid.as_str());
        let scheme = theme::of(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), MESSAGE_ROW_HEIGHT),
            egui::Sense::click(),
        );

        let painter = ui.painter();
        if is_selected {
            painter.rect_filled(rect.shrink(1.0), 4.0, scheme.selection);
        } else if resp.hovered() {
            painter.rect_filled(rect.shrink(1.0), 4.0, scheme.hover);
        }
        painter.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            egui::Stroke::new(1.0_f32, scheme.border),
        );

        // Avatar: deterministic color per sender, initial centered in it.
        let avatar_center = egui::pos2(rect.left() + 8.0 + AVATAR_DIAMETER / 2.0, rect.center().y);
        painter.circle_filled(
            avatar_center,
            AVATAR_DIAMETER / 2.0,
            sender_color(scheme, &msg.from),
        );
        painter.text(
            avatar_center,
            egui::Align2::CENTER_CENTER,
            sender_initial(&msg.from),
            egui::FontId::proportional(AVATAR_FONT_SIZE),
            scheme.avatar_text,
        );

        let text_left = avatar_center.x + AVATAR_DIAMETER / 2.0 + 10.0;
        let text_right = rect.right() - 10.0;
        // Sender and subject always take the body color — read mail is not
        // muted — and unread mail is set heavier instead, so the two states
        // stay apart without dimming half the list. Only the preview below is
        // muted, always.
        let title_color = scheme.text;
        let unread = !msg.seen;

        // First row: timestamp right-aligned, sender truncated before it.
        let date_galley = ui.ctx().fonts(|f| {
            f.layout_no_wrap(
                display_date(msg),
                egui::FontId::proportional(ROW_DATE_FONT_SIZE),
                scheme.text_weak,
            )
        });
        let date_pos = egui::pos2(
            text_right - date_galley.size().x,
            rect.top() + 20.0 - date_galley.size().y * 0.5,
        );
        painter.galley(date_pos, date_galley, scheme.text_weak);
        let sender_max_width = (date_pos.x - text_left - 12.0).max(40.0);
        let sender = truncate_galley(
            ui.ctx(),
            &msg.from,
            ROW_TITLE_FONT_SIZE,
            title_color,
            sender_max_width,
        );
        paint_row_text(
            painter,
            egui::pos2(text_left, rect.top() + 20.0 - sender.size().y * 0.5),
            sender,
            title_color,
            unread,
        );

        // Second row: subject.
        let subject = truncate_galley(
            ui.ctx(),
            &msg.subject,
            ROW_TITLE_FONT_SIZE,
            title_color,
            text_right - text_left,
        );
        paint_row_text(
            painter,
            egui::pos2(text_left, rect.top() + 40.0 - subject.size().y * 0.5),
            subject,
            title_color,
            unread,
        );

        // Third row: content preview, always muted.
        let preview = if msg.preview.is_empty() {
            " ".to_string()
        } else {
            msg.preview.clone()
        };
        let preview = truncate_galley(
            ui.ctx(),
            &preview,
            ROW_PREVIEW_FONT_SIZE,
            scheme.text_weak,
            text_right - text_left,
        );
        painter.galley(
            egui::pos2(text_left, rect.top() + 57.0 - preview.size().y * 0.5),
            preview,
            scheme.text_weak,
        );

        if resp.clicked() {
            self.open_message(msg);
        }
    }

    /// Right column: the selected message, or a placeholder.
    fn mail_content(&mut self, ui: &mut egui::Ui) {
        let Some(msg) = self.selected_message_view() else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("No mail selected").weak().heading());
            });
            return;
        };
        ui.vertical(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(msg.subject.as_str());
            ui.label(
                egui::RichText::new(format!("From: {}", msg.from))
                    .small()
                    .weak(),
            );
            if !msg.to.is_empty() {
                ui.label(
                    egui::RichText::new(format!("To: {}", msg.to))
                        .small()
                        .weak(),
                );
            }
            if !msg.date.is_empty() {
                ui.label(
                    egui::RichText::new(format!("Date: {}", msg.date))
                        .small()
                        .weak(),
                );
            }
            if !msg.attachment_names.is_empty() {
                ui.label(
                    egui::RichText::new(format!("📎 {}", msg.attachment_names.join(", "))).small(),
                );
            }
            ui.separator();
            // A newly opened message starts at the top. The scroll offset is
            // per pane, not per message, so without this the next mail opens
            // wherever the previous one was left — for a Blitz page that means
            // the top of the page is simply not on screen.
            let message_key = format!(
                "{}/{}",
                self.selected_folder.as_deref().unwrap_or("-"),
                msg.uid
            );
            let new_message = self.scrolled_message.as_deref() != Some(message_key.as_str());
            if new_message {
                self.scrolled_message = Some(message_key);
            }
            let mut scroll = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                // Switching renderers swaps the whole pane; a scroll offset
                // from the other renderer would land in empty space.
                .id_salt(("mail-content", self.html_renderer.as_str()));
            if new_message {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            scroll.show(ui, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                let html = msg.body_html.as_deref();
                // Blitz takes the whole body; the built-in renderer below
                // needs the app's resource pipeline, so it stays as is.
                if let (HtmlRenderer::Blitz, Some(html)) = (self.html_renderer, html) {
                    self.blitz_view(ui, &msg, html);
                    return;
                }
                if let Some(html) = html {
                    // Give the renderer what is already loaded and collect
                    // the URLs it still wants.
                    let textures: HashMap<String, egui::TextureHandle> = self
                        .images
                        .iter()
                        .filter_map(|(url, status)| match status {
                            ImageStatus::Ready(texture) => Some((url.clone(), texture.clone())),
                            _ => None,
                        })
                        .collect();
                    let sheets: HashMap<String, String> = self
                        .stylesheets
                        .iter()
                        .filter_map(|(url, text)| text.as_ref().map(|t| (url.clone(), t.clone())))
                        .collect();
                    // Inline `cid:` attachments of this message, decoded
                    // once and cached per (uid, cid).
                    let egui_ctx = self.ctx.clone();
                    let mut inline_map: HashMap<String, egui::TextureHandle> = HashMap::new();
                    for image in &msg.inline_images {
                        let key = format!("{}/{}", msg.uid, image.cid);
                        let texture = self
                            .inline_textures
                            .entry(key)
                            .or_insert_with(|| decode_inline_image(&egui_ctx, image));
                        if let Some(texture) = texture {
                            inline_map.insert(image.cid.clone(), texture.clone());
                        }
                    }
                    let mut html_ctx = crate::html::HtmlContext {
                        images: &textures,
                        inline_images: &inline_map,
                        stylesheets: &sheets,
                        missing_images: Vec::new(),
                        missing_stylesheets: Vec::new(),
                    };
                    crate::html::render(ui, html, &mut html_ctx);
                    let missing_images = std::mem::take(&mut html_ctx.missing_images);
                    let missing_sheets = std::mem::take(&mut html_ctx.missing_stylesheets);
                    for url in missing_images {
                        self.request_image(url);
                    }
                    for url in missing_sheets {
                        self.request_stylesheet(url);
                    }
                } else if !msg.body_text.is_empty() {
                    ui.label(msg.body_text.as_str());
                } else {
                    ui.label(egui::RichText::new("(no text body)").weak());
                }
            });
        });
    }

    /// The preferences modal, opened from the title-bar cog: a sidebar of
    /// pages with the account configuration (previously split between a
    /// standalone window and the account menu) and the appearance switches
    /// (previously title-bar buttons).
    fn preferences_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.show_preferences;
        let modal = egui::Modal::new(egui::Id::new("preferences")).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.horizontal(|ui| {
                ui.heading("Preferences");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        open = false;
                    }
                });
            });
            ui.separator();
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    for (tab, label) in [
                        (PrefsTab::Accounts, "Accounts"),
                        (PrefsTab::Appearance, "Appearance"),
                    ] {
                        if ui.selectable_label(self.prefs_tab == tab, label).clicked() {
                            self.prefs_tab = tab;
                        }
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(390.0);
                    ui.set_min_height(330.0);
                    match self.prefs_tab {
                        PrefsTab::Accounts => self.accounts_page(ui),
                        PrefsTab::Appearance => self.appearance_page(ui),
                    }
                });
            });
        });
        // Clicking the backdrop or pressing Escape closes the modal.
        if modal.should_close() {
            open = false;
        }
        self.show_preferences = open;
    }

    /// Preferences → Accounts: the configured accounts, with the add/edit
    /// account form taking over the page while it is open.
    fn accounts_page(&mut self, ui: &mut egui::Ui) {
        if self.show_account_form {
            self.account_form_page(ui);
            return;
        }

        let mut switch_to: Option<Account> = None;
        let mut edit: Option<String> = None;
        let mut remove: Option<String> = None;

        ui.label(
            egui::RichText::new("Every account syncs; the one in use fills the window.")
                .small()
                .weak(),
        );
        ui.add_space(6.0);

        if self.accounts.is_empty() {
            ui.label("No accounts yet — add one below.");
            ui.add_space(4.0);
        }

        for account in self.accounts.clone() {
            let in_use = self.selected_account.as_deref() == Some(account.id.as_str());
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&account.name).strong());
                    if in_use {
                        ui.label(egui::RichText::new("in use").small().weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Removal is a two-step confirm, like the old menu item.
                        let confirming =
                            self.confirm_delete.as_deref() == Some(account.id.as_str());
                        let label = if confirming {
                            egui::RichText::new("Remove?").color(ui.visuals().error_fg_color)
                        } else {
                            egui::RichText::new("Remove")
                        };
                        if ui.button(label).clicked() {
                            if confirming {
                                remove = Some(account.id.clone());
                            } else {
                                self.confirm_delete = Some(account.id.clone());
                            }
                        }
                        if !in_use && ui.button("Use").clicked() {
                            switch_to = Some(account.clone());
                        }
                        if ui.button("Edit").clicked() {
                            edit = Some(account.id.clone());
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {}:{} · {}",
                        account.protocol.label(),
                        account.host,
                        account.port,
                        account.username
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.add_space(2.0);
        }

        ui.add_space(6.0);
        if ui.button("+ Add account").clicked() {
            self.account_form = AccountForm::default();
            self.show_account_form = true;
            self.confirm_delete = None;
        }

        if let Some(account) = switch_to {
            self.select_folder(account.id.clone(), "INBOX".to_string(), account.protocol);
        }
        if let Some(id) = edit {
            self.open_edit_account_form(&id);
        }
        if let Some(id) = remove {
            self.delete_account(&id);
            self.confirm_delete = None;
        }
    }

    /// Open the account form on a configured account, pre-filled, so the
    /// dialog edits it instead of adding another one.
    fn open_edit_account_form(&mut self, account_id: &str) {
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id) else {
            return;
        };
        self.account_form = AccountForm::edit(account);
        self.show_account_form = true;
        self.confirm_delete = None;
    }

    /// Preferences → Accounts → form: fill in a new account, or edit the
    /// one the form was opened on. The standalone "Add account" window used
    /// to show this.
    fn account_form_page(&mut self, ui: &mut egui::Ui) {
        let editing = self.account_form.editing.is_some();
        ui.label(
            egui::RichText::new(if editing {
                "Edit account"
            } else {
                "New account"
            })
            .strong(),
        );
        ui.add_space(4.0);

        let f = &mut self.account_form;
        egui::Grid::new("account_form_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Account name");
                ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("Personal mail"));
                ui.end_row();

                ui.label("Protocol");
                ui.horizontal(|ui| {
                    let mut changed = false;
                    if ui
                        .radio_value(&mut f.protocol, Protocol::Imap, "IMAP")
                        .changed()
                    {
                        changed = true;
                    }
                    if ui
                        .radio_value(&mut f.protocol, Protocol::Pop3, "POP3")
                        .changed()
                    {
                        changed = true;
                    }
                    if changed {
                        f.port_str = f.protocol.default_port(f.tls).to_string();
                    }
                });
                ui.end_row();

                ui.label("Server");
                ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("imap.example.com"));
                ui.end_row();

                ui.label("Security");
                ui.horizontal(|ui| {
                    let mut changed = false;
                    if ui
                        .radio_value(&mut f.tls, TlsMode::Implicit, "TLS")
                        .changed()
                    {
                        changed = true;
                    }
                    if ui
                        .radio_value(&mut f.tls, TlsMode::StartTls, "STARTTLS")
                        .changed()
                    {
                        changed = true;
                    }
                    if ui.radio_value(&mut f.tls, TlsMode::Plain, "None").changed() {
                        changed = true;
                    }
                    if changed {
                        f.port_str = f.protocol.default_port(f.tls).to_string();
                    }
                });
                ui.end_row();

                ui.label("Port");
                ui.add(egui::TextEdit::singleline(&mut f.port_str).hint_text("993"));
                ui.end_row();

                ui.label("Username");
                ui.add(egui::TextEdit::singleline(&mut f.username).hint_text("you@example.com"));
                ui.end_row();

                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut f.password).password(true));
                ui.end_row();
            });

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Note: credentials are stored in plain text on this machine.")
                .small()
                .weak(),
        );
        ui.add_space(4.0);

        let valid = self.account_form.is_valid();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                self.show_account_form = false;
            }
            let save = if editing {
                "Save & sync"
            } else {
                "Save & connect"
            };
            if ui.add_enabled(valid, egui::Button::new(save)).clicked() {
                self.save_account_form();
            }
        });
    }

    /// Preferences → Appearance: theme and HTML renderer, both persisted to
    /// settings.json on change.
    fn appearance_page(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Theme").strong());
        ui.horizontal(|ui| {
            for theme in Theme::ALL {
                if ui.radio(self.theme == theme, theme.label()).clicked() {
                    self.theme = theme;
                    if theme == Theme::System {
                        self.system_dark = system_prefers_dark();
                    }
                    self.persist_settings();
                }
            }
        });
        ui.label(
            egui::RichText::new("“System” follows the GTK/KDE dark-mode setting.")
                .small()
                .weak(),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("HTML mail renderer").strong());
        ui.horizontal(|ui| {
            for renderer in [HtmlRenderer::Simple, HtmlRenderer::Blitz] {
                if ui
                    .radio(self.html_renderer == renderer, renderer.label())
                    .clicked()
                {
                    self.html_renderer = renderer;
                    self.persist_settings();
                    self.pending_blitz_key = None;
                }
            }
        });
        ui.label(
            egui::RichText::new(self.html_renderer.description())
                .small()
                .weak(),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("Fonts").strong());
        let mut fonts_changed = false;
        egui::Grid::new("font-pickers")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Interface");
                fonts_changed |= font_picker(ui, "ui-font", &self.font_families, &mut self.ui_font);
                ui.end_row();

                ui.label("Fixed-width");
                fonts_changed |=
                    font_picker(ui, "mono-font", &self.font_families, &mut self.mono_font);
                ui.end_row();
            });
        ui.add_space(4.0);
        // Previews, rendered through the named families that fonts::install
        // binds for exactly this purpose.
        ui.label(
            egui::RichText::new("The quick brown fox jumps over the lazy dog — 0123456789")
                .family(egui::FontFamily::Name("kimo_ui".into())),
        );
        ui.label(
            egui::RichText::new("if (mail.is_html()) { render(&body); }  // 0123456789")
                .family(egui::FontFamily::Name("kimo_mono".into()))
                .small(),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Applies to the app UI and to monospace text in the Simple renderer; \
                 Blitz mail keeps its own font set.",
            )
            .small()
            .weak(),
        );
        if fonts_changed {
            self.persist_settings();
            self.apply_fonts();
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        color_scheme_preview(ui, self.theme.scheme(self.system_dark));
    }
}

/// Preferences → Appearance: the palette the selected theme resolves to, so
/// the defaults are visible without a mail open. Read-only: the scheme is not
/// configurable yet, it only documents what the app draws with.
fn color_scheme_preview(ui: &mut egui::Ui, scheme: ColorScheme) {
    ui.label(egui::RichText::new("Color scheme").strong());
    ui.label(
        egui::RichText::new(format!("{} — the defaults Kimo ships with.", scheme.name))
            .small()
            .weak(),
    );
    ui.add_space(4.0);
    egui::Grid::new("color-swatches")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (index, (name, color)) in scheme.swatches().into_iter().enumerate() {
                ui.horizontal(|ui| {
                    swatch(ui, color);
                    ui.label(egui::RichText::new(format!("{name} {}", theme::hex(color))).small());
                });
                if index % 2 == 1 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Avatars").small().weak());
        for color in scheme.avatar {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 6.0, color);
        }
    });
}

/// One color chip in the Appearance preview, outlined so that the near-white
/// slots stay visible on a light panel.
fn swatch(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.painter().rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
}

impl eframe::App for KimoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.apply_theme(ctx);

        // Collapse the nav bar automatically when the window becomes narrow;
        // re-expanding is a manual choice and stays until the next crossing.
        let narrow = ctx.screen_rect().width() < NAV_AUTO_COLLAPSE_WIDTH;
        if narrow && !self.was_narrow {
            self.nav_collapsed = true;
        }
        self.was_narrow = narrow;

        egui::TopBottomPanel::top("titlebar")
            .exact_height(TITLEBAR_HEIGHT)
            .show(ctx, |ui| self.titlebar(ui));
        egui::SidePanel::right("mail_content")
            .resizable(true)
            .min_width(280.0)
            .default_width(460.0)
            .show(ctx, |ui| self.mail_content(ui));
        if !self.nav_collapsed {
            egui::SidePanel::left("nav")
                .resizable(true)
                .min_width(150.0)
                .default_width(200.0)
                .show(ctx, |ui| self.nav(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.mail_list(ui));

        if self.show_preferences {
            self.preferences_modal(ctx);
        }
    }
}

// ----- helpers --------------------------------------------------------------

const TITLEBAR_HEIGHT: f32 = 30.0;
const TITLEBAR_BUTTON_WIDTH: f32 = 42.0;
/// Side of the square brand icon at the left of the title bar.
const BRAND_ICON_SIZE: f32 = 18.0;

/// Blitz reading pane: page margin, width quantization for renders, and how
/// many rendered pages stay in memory (each is megabytes of texture).
const BLITZ_PAGE_MARGIN: f32 = 8.0;
const BLITZ_WIDTH_STEP: u32 = 32;
const BLITZ_PAGES_CACHED: usize = 2;

/// How long the pane geometry must hold still before a render starts.
/// Dragging the splitter, or a compositor animating a window resize, changes
/// it every frame; rendering each intermediate step would keep a core busy for
/// results that are all discarded but the last.
const BLITZ_SETTLE: Duration = Duration::from_millis(180);

/// A finished page may be scaled down instead of re-rendered while the pane
/// stays within this factor of the width the page was laid out for. Re-flowing
/// the text for a few percent is not worth a render — and it keeps the
/// vertical scrollbar, which narrows the pane when it appears, from
/// invalidating the very page that made it appear.
const BLITZ_REUSE_RATIO: f32 = 1.25;

/// What to do about a page that is not cached yet.
#[derive(Debug, PartialEq, Eq)]
enum BlitzWait {
    /// The geometry has held still for `BLITZ_SETTLE`: render now.
    Start,
    /// Still settling; look again after this long.
    Wait(Duration),
}

/// Debounce for the reading pane's geometry. `pending` is the key seen last
/// time and when it was first seen, `now` the current instant.
fn blitz_settle(pending: Option<(&str, Instant)>, key: &str, now: Instant) -> BlitzWait {
    match pending {
        Some((pending, since)) if pending == key => {
            let waited = now.saturating_duration_since(since);
            if waited >= BLITZ_SETTLE {
                BlitzWait::Start
            } else {
                BlitzWait::Wait(BLITZ_SETTLE - waited)
            }
        }
        // Another geometry was pending, or none: this one starts settling now.
        _ => BlitzWait::Wait(BLITZ_SETTLE),
    }
}

/// Cache key of a rendered page: the mail's identity plus the width, in
/// physical pixels, it was rendered for. Deliberately without the pane height
/// — see `crate::blitz_render::PageRequest::viewport_height`.
fn blitz_key(identity: &str, width: u32, scale: f32) -> String {
    format!("{identity}{width}@{scale:.2}")
}

/// Whether a page rasterized `rendered` device pixels wide can be scaled down
/// for a pane wanting `wanted` instead of being rendered again. Only downwards:
/// a raster is never scaled up, that would blur the text.
fn blitz_page_reusable(rendered: f32, wanted: f32) -> bool {
    rendered >= wanted && rendered <= wanted * BLITZ_REUSE_RATIO
}

/// Round `value` up to the next multiple of `step`, so a pane resize only
/// re-renders in coarse steps.
fn quantize(value: f32, step: u32) -> u32 {
    let value = if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    };
    let step = step.max(1) as f32;
    ((value / step).ceil() * step) as u32
}

/// Blit a finished Blitz page and re-attach its links as clickable boxes.
fn draw_blitz_page(ui: &mut egui::Ui, page: &RasterizedPage) {
    if page.truncated {
        ui.label(
            egui::RichText::new("This message is too tall to render in full — the end is missing.")
                .small()
                .weak(),
        );
    }
    let row_width = ui.available_width();
    // The page can be wider than the pane: fixed-width mail is laid out at its
    // own width and scaled down here, so nothing is cut off.
    let fit = ((row_width - BLITZ_PAGE_MARGIN * 2.0) / page.size.x).min(1.0);
    let (_, space) = ui.allocate_space(egui::vec2(
        row_width,
        page.size.y * fit + BLITZ_PAGE_MARGIN * 2.0,
    ));
    let origin = egui::pos2(
        space.left() + BLITZ_PAGE_MARGIN,
        space.top() + BLITZ_PAGE_MARGIN,
    );
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    for tile in &page.tiles {
        let rect = egui::Rect::from_min_size(origin + tile.offset * fit, tile.size * fit);
        // White is egui's "no tint" (it multiplies), not a scheme color: the
        // page is shown exactly as Blitz rasterized it.
        ui.painter()
            .image(tile.texture.id(), rect, uv, egui::Color32::WHITE);
    }

    for (rect, href) in &page.links {
        let rect = egui::Rect::from_min_size(origin + rect.min.to_vec2() * fit, rect.size() * fit);
        let id = egui::Id::new((
            "blitz-link",
            href,
            (rect.min.x * 10.0) as i32,
            (rect.min.y * 10.0) as i32,
        ));
        let response = ui.interact(rect, id, egui::Sense::click());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(
                rect,
                2.0,
                ui.visuals().selection.bg_fill.gamma_multiply(0.35),
            );
        }
        if response.clicked() {
            crate::html::open_url(ui, href);
        }
        response.on_hover_text(href.as_str());
    }
}

/// Message-card geometry in the mail list.
const MESSAGE_ROW_HEIGHT: f32 = 68.0;
const AVATAR_DIAMETER: f32 = 32.0;
/// Font size of the avatar's initial — a bit under half the diameter.
const AVATAR_FONT_SIZE: f32 = 14.0;
/// Font sizes of the row's three lines, in points: the timestamp, the title
/// line (sender over subject, the one that carries the unread weight) and the
/// muted preview under it.
const ROW_DATE_FONT_SIZE: f32 = 11.0;
const ROW_TITLE_FONT_SIZE: f32 = 12.0;
const ROW_PREVIEW_FONT_SIZE: f32 = 11.0;

/// Decode a stored inline image into an egui texture. Runs on the UI thread
/// but only once per message/cid; inline mail images are small.
fn decode_inline_image(
    ctx: &egui::Context,
    image: &crate::models::InlineImage,
) -> Option<egui::TextureHandle> {
    let decoded = image::load_from_memory(&image.data).ok()?;
    let rgba = decoded.to_rgba8();
    let pixels = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        &rgba,
    );
    Some(ctx.load_texture("inline-image", pixels, Default::default()))
}

/// Deterministic avatar fill for a sender, from the scheme's palette — the
/// same palette in both themes, so a sender keeps its color.
fn sender_color(scheme: ColorScheme, from: &str) -> egui::Color32 {
    let hash = from
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    scheme.avatar[(hash as usize) % scheme.avatar.len()]
}

/// First character of the display name (or e-mail local part), for avatars.
fn sender_initial(from: &str) -> String {
    let name = from.split('<').next().unwrap_or("").trim();
    let source = if name.is_empty() {
        from.split('@').next().unwrap_or("")
    } else {
        name
    };
    source
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Lay out a single line of text, cut with an ellipsis (by binary search on
/// characters) to fit `max_width` pixels.
fn truncate_galley(
    ctx: &egui::Context,
    text: &str,
    size: f32,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let font = egui::FontId::proportional(size);
    let full = ctx.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), color));
    if full.size().x <= max_width {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    // Find the longest prefix + "…" that fits (lo fits, hi never does).
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        let cut: String = chars[..mid].iter().collect::<String>() + "…";
        let fits = ctx
            .fonts(|f| f.layout_no_wrap(cut, font.clone(), color))
            .size()
            .x
            <= max_width;
        if fits {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let cut: String = chars[..lo].iter().collect::<String>() + "…";
    ctx.fonts(|f| f.layout_no_wrap(cut, font, color))
}

/// How far the second strike of bold text sits from the first, in points.
const BOLD_STRIKE: f32 = 0.5;

/// Draw one line of the message list, heavier when the mail is unread.
///
/// epaint has no synthetic bold (`FontTweak` only scales and shifts glyphs),
/// and the installed face may ship no bold at all — Adwaita Sans, the default
/// on this desktop, has only Regular and Italic. Striking the same galley twice,
/// half a point apart, thickens the stems enough to read as bold at list sizes,
/// and costs nothing: the layout was already measured for one strike, so the
/// row cannot reflow.
fn paint_row_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    galley: std::sync::Arc<egui::Galley>,
    color: egui::Color32,
    bold: bool,
) {
    painter.galley(pos, galley.clone(), color);
    if bold {
        painter.galley(pos + egui::vec2(BOLD_STRIKE, 0.0), galley, color);
    }
}

/// One row of the folder list: kind icon, name, and the unread count on the
/// right. The whole row is the click target and carries the selection
/// highlight — a `selectable_label` would leave the icon outside its own
/// highlight and put the count inside the name. The colors come straight from
/// the scheme instead of from egui's widget states, which is what lets an
/// unread folder read brighter than a read one.
/// Account header in the navigation column: a disclosure triangle, the
/// account name and its unread total. Clicking the row collapses or expands
/// the account's folders.
fn account_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    name: &str,
    unread: usize,
    collapsed: bool,
) -> egui::Response {
    const ROW_HEIGHT: f32 = 24.0;

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if response.hovered() {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.hover);
    }

    // Disclosure triangle: pointing right while collapsed, down while open.
    let center = egui::pos2(rect.left() + 10.0, rect.center().y);
    let triangle = if collapsed {
        vec![
            egui::pos2(center.x - 3.0, center.y - 5.0),
            egui::pos2(center.x + 4.0, center.y),
            egui::pos2(center.x - 3.0, center.y + 5.0),
        ]
    } else {
        vec![
            egui::pos2(center.x - 5.0, center.y - 3.0),
            egui::pos2(center.x + 5.0, center.y - 3.0),
            egui::pos2(center.x, center.y + 4.0),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        triangle,
        scheme.text_weak,
        egui::Stroke::NONE,
    ));

    let left = center.x + 10.0;
    let mut right = rect.right() - 6.0;
    if unread > 0 {
        let small = egui::TextStyle::Small.resolve(ui.style());
        let count = ui
            .ctx()
            .fonts(|fonts| fonts.layout_no_wrap(unread.to_string(), small, scheme.text_weak));
        right -= count.size().x;
        ui.painter().galley(
            egui::pos2(right, rect.center().y - count.size().y / 2.0),
            count,
            scheme.text_weak,
        );
        right -= 6.0;
    }

    // Always at full strength, unlike a folder row: this is the section's
    // heading, and it stays readable while the mail under it is all read.
    let body = egui::TextStyle::Body.resolve(ui.style());
    let name = truncate_galley(
        ui.ctx(),
        name,
        body.size,
        scheme.text,
        (right - left).max(1.0),
    );
    ui.painter().galley(
        egui::pos2(left, rect.center().y - name.size().y / 2.0),
        name,
        scheme.text,
    );
    response
}

fn folder_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    icon: Option<&egui::TextureHandle>,
    name: &str,
    unread: usize,
    selected: bool,
) -> egui::Response {
    const ROW_HEIGHT: f32 = 22.0;
    const ICON_SIZE: f32 = 16.0;

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    // The scheme, not egui's widget states: a selected row gets the selection
    // fill, a hovered one the hover fill, and nothing else.
    if selected {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.selection);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 3.0_f32, scheme.hover);
    }
    // Unread folders read brighter than read ones, as the plain labels did.
    let color = if selected || unread > 0 {
        scheme.text
    } else {
        scheme.text_weak
    };

    let mut left = rect.left() + 4.0;
    if let Some(icon) = icon {
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(left, rect.center().y - ICON_SIZE / 2.0),
            egui::vec2(ICON_SIZE, ICON_SIZE),
        );
        // The icon is white on transparency, so this tint is its color.
        ui.painter().image(
            icon.id(),
            icon_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            color,
        );
        left = icon_rect.right() + 6.0;
    }

    let mut right = rect.right() - 6.0;
    if unread > 0 {
        let small = egui::TextStyle::Small.resolve(ui.style());
        let count = ui
            .ctx()
            .fonts(|fonts| fonts.layout_no_wrap(unread.to_string(), small, color));
        right -= count.size().x;
        ui.painter().galley(
            egui::pos2(right, rect.center().y - count.size().y / 2.0),
            count,
            color,
        );
        right -= 6.0;
    }

    let body = egui::TextStyle::Body.resolve(ui.style());
    let name = truncate_galley(ui.ctx(), name, body.size, color, (right - left).max(1.0));
    ui.painter().galley(
        egui::pos2(left, rect.center().y - name.size().y / 2.0),
        name,
        color,
    );
    response
}

/// The application icon for the title bar: the same embedded 256 px PNG the
/// tray serves, downscaled once here — painting it at 18 pt straight from the
/// 256 px texture would alias badly, since egui has no mipmaps.
fn load_brand_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let decoded = image::load_from_memory(crate::tray::APP_PNG)
        .ok()?
        .to_rgba8();
    let small = image::imageops::resize(&decoded, 64, 64, image::imageops::FilterType::Lanczos3);
    let pixels = egui::ColorImage::from_rgba_unmultiplied([64, 64], small.as_raw());
    Some(ctx.load_texture("brand-icon", pixels, Default::default()))
}

/// Decode the embedded folder icons (see `folders::FolderKind::ICONS`) into
/// textures, once at startup. A failed decode is skipped: that row then draws
/// without its icon, which is a cosmetic loss at worst.
fn load_folder_icons(ctx: &egui::Context) -> HashMap<(FolderKind, bool), egui::TextureHandle> {
    FolderKind::ICONS
        .iter()
        .filter_map(|(kind, open, png)| {
            let decoded = image::load_from_memory(png).ok()?.to_rgba8();
            let size = [decoded.width() as usize, decoded.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
            let name = format!("folder-{kind:?}-{open}");
            Some((
                (*kind, *open),
                ctx.load_texture(name, pixels, Default::default()),
            ))
        })
        .collect()
}

// Title bar window-control glyphs, painted as vector shapes so they render
// identically regardless of which fonts are installed.

fn paint_minimize_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let y = rect.center().y + 4.0;
    let x = rect.center().x;
    p.line_segment(
        [egui::pos2(x - 5.0, y), egui::pos2(x + 5.0, y)],
        egui::Stroke::new(1.5_f32, color),
    );
}

fn paint_maximize_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    p.rect_stroke(
        egui::Rect::from_center_size(rect.center(), egui::vec2(10.0, 10.0)),
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
}

fn paint_restore_icon(
    p: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    bg: egui::Color32,
) {
    let c = rect.center();
    let back = egui::Rect::from_min_size(egui::pos2(c.x - 2.0, c.y - 6.0), egui::vec2(10.0, 10.0));
    let front = egui::Rect::from_min_size(egui::pos2(c.x - 6.0, c.y - 4.0), egui::vec2(10.0, 10.0));
    p.rect_stroke(
        back,
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
    // Occlude the back window where they overlap so the two read as stacked.
    p.rect_filled(front, 0.0, bg);
    p.rect_stroke(
        front,
        0.0,
        egui::Stroke::new(1.2_f32, color),
        egui::StrokeKind::Middle,
    );
}

fn paint_close_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 4.5;
    p.line_segment(
        [egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)],
        egui::Stroke::new(1.5_f32, color),
    );
    p.line_segment(
        [egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)],
        egui::Stroke::new(1.5_f32, color),
    );
}

/// A Preferences font picker: "Default" plus every installed family, each
/// entry drawn in its own shape so fixed-width families stand out. Returns
/// true when the selection changed.
fn font_picker(
    ui: &mut egui::Ui,
    id: &str,
    families: &Option<Vec<FontFamily>>,
    current: &mut Option<String>,
) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(current.clone().unwrap_or_else(|| "Default".to_string()))
        .width(240.0)
        .height(320.0)
        .show_ui(ui, |ui| {
            ui.set_min_width(260.0);
            if ui.selectable_label(current.is_none(), "Default").clicked() {
                *current = None;
                changed = true;
            }
            match families {
                None => {
                    ui.label(egui::RichText::new("Scanning installed fonts…").weak());
                }
                Some(families) if families.is_empty() => {
                    ui.label(egui::RichText::new("No fonts found.").weak());
                }
                Some(families) => {
                    for family in families {
                        let selected = current.as_deref() == Some(family.name.as_str());
                        let text = egui::RichText::new(&family.name);
                        let text = if family.monospace {
                            text.monospace()
                        } else {
                            text
                        };
                        if ui.selectable_label(selected, text).clicked() {
                            *current = Some(family.name.clone());
                            changed = true;
                        }
                    }
                }
            }
        });
    changed
}

/// Circular-arrow (refresh) glyph for the sync button: an open ring with an
/// arrow head where it ends, turning the way the ring is drawn.
fn paint_sync_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 5.0;
    let stroke = egui::Stroke::new(1.3_f32, color);

    // Clockwise on screen (y grows downwards) from lower-right to upper-right,
    // leaving a gap on the right for the arrow head.
    let start = 0.6_f32;
    let end = std::f32::consts::TAU - 0.6;
    const STEPS: usize = 20;
    let ring: Vec<egui::Pos2> = (0..=STEPS)
        .map(|i| {
            let angle = start + (end - start) * i as f32 / STEPS as f32;
            c + egui::vec2(angle.cos(), angle.sin()) * r
        })
        .collect();
    p.add(egui::Shape::line(ring, stroke));

    // Head at the end of the ring, pointing along the tangent.
    let dir = egui::vec2(end.cos(), end.sin());
    let tangent = egui::vec2(-end.sin(), end.cos());
    let base = c + dir * r;
    p.add(egui::Shape::convex_polygon(
        vec![
            base + tangent * 3.4,
            base + dir * 2.1 - tangent * 0.3,
            base - dir * 2.1 - tangent * 0.3,
        ],
        color,
        egui::Stroke::NONE,
    ));
}

/// Cog / gear glyph for the preferences button: a hub ring with eight teeth.
fn paint_cog_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 5.5;
    let stroke = egui::Stroke::new(1.2_f32, color);
    p.circle_stroke(c, r * 0.55, stroke);
    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let (sin, cos) = angle.sin_cos();
        let dir = egui::vec2(cos, sin);
        p.line_segment([c + dir * (r * 0.62), c + dir * r], stroke);
    }
}

/// Best-effort OS dark-mode detection without spawning processes or talking
/// to D-Bus: read the common GTK3/GTK4 and KDE configuration files.
fn system_prefers_dark() -> bool {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let Some(config_home) = config_home else {
        return false;
    };
    for rel in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        if let Ok(text) = std::fs::read_to_string(config_home.join(rel)) {
            if ini_flag_is_true(&text, "gtk-application-prefer-dark-theme") {
                return true;
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(config_home.join("kdeglobals")) {
        if kde_scheme_is_dark(&text) {
            return true;
        }
    }
    false
}

/// `key = true` (or 1/yes) in an INI-style file.
fn ini_flag_is_true(text: &str, key: &str) -> bool {
    text.lines().any(|line| match line.trim().split_once('=') {
        Some((k, v)) => {
            let v = v.trim();
            k.trim() == key
                && (v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        }
        None => false,
    })
}

/// KDE `ColorScheme=…` containing "dark" (e.g. BreezeDark).
fn kde_scheme_is_dark(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.strip_prefix("colorscheme=")
            .is_some_and(|scheme| scheme.contains("dark"))
    })
}

fn sort_messages(messages: &mut [MailMessage]) {
    messages.sort_by_key(|m| std::cmp::Reverse(m.timestamp.unwrap_or(0)));
}

fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = s
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "account".to_string()
    } else {
        collapsed
    }
}

fn display_date(m: &MailMessage) -> String {
    m.timestamp
        .map(format_timestamp)
        .unwrap_or_else(|| m.date.clone())
}

fn format_timestamp(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Days-since-epoch → (year, month, day), Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_renderer_round_trips_through_settings() {
        assert_eq!(HtmlRenderer::parse("blitz"), HtmlRenderer::Blitz);
        assert_eq!(HtmlRenderer::parse("egui"), HtmlRenderer::Simple);
        // Anything unknown — including the legacy render_mode field's "web" —
        // falls back to the built-in renderer.
        assert_eq!(HtmlRenderer::parse("web"), HtmlRenderer::Simple);
        assert_eq!(HtmlRenderer::parse(""), HtmlRenderer::Simple);
        assert_eq!(HtmlRenderer::Blitz.as_str(), "blitz");
        assert_eq!(HtmlRenderer::Simple.as_str(), "egui");
    }

    #[test]
    fn blitz_keys_depend_on_width_but_not_pane_height() {
        let identity = "acct/INBOX/1/";
        let key = blitz_key(identity, 864, 2.0);
        // Nothing else feeds the key, so a pane that only grew taller keeps
        // its page (the height is not part of the key at all).
        assert_eq!(key, blitz_key(identity, 864, 2.0));
        // A new width, or a new display scale, is a different raster.
        assert_ne!(key, blitz_key(identity, 896, 2.0));
        assert_ne!(key, blitz_key(identity, 864, 1.0));
        // So is another mail.
        assert_ne!(key, blitz_key("acct/INBOX/2/", 864, 2.0));
        assert_ne!(key, blitz_key("acct/INBOX/18/", 864, 2.0));
    }

    #[test]
    fn blitz_renders_wait_for_the_geometry_to_settle() {
        let start = Instant::now();
        // Never seen before: nothing renders on the first frame.
        assert_eq!(
            blitz_settle(None, "page", start),
            BlitzWait::Wait(BLITZ_SETTLE)
        );
        // Still settling: report what is left of the wait.
        assert_eq!(
            blitz_settle(
                Some(("page", start)),
                "page",
                start + Duration::from_millis(100)
            ),
            BlitzWait::Wait(BLITZ_SETTLE - Duration::from_millis(100))
        );
        // Held still long enough: render.
        assert_eq!(
            blitz_settle(Some(("page", start)), "page", start + BLITZ_SETTLE),
            BlitzWait::Start
        );
        // A geometry that changed again starts a fresh wait, however long the
        // previous one had been pending.
        assert_eq!(
            blitz_settle(
                Some(("page", start)),
                "page-moved",
                start + BLITZ_SETTLE * 4
            ),
            BlitzWait::Wait(BLITZ_SETTLE)
        );
    }

    #[test]
    fn blitz_pages_are_reused_only_for_a_slightly_narrower_pane() {
        // Within the reuse window: scale the existing raster down.
        assert!(blitz_page_reusable(864.0, 864.0));
        assert!(blitz_page_reusable(864.0, 840.0));
        assert!(blitz_page_reusable(864.0, 700.0));
        // Much narrower: re-flowing the text is worth a render.
        assert!(!blitz_page_reusable(864.0, 600.0));
        // Wider than the raster: a page is never scaled up.
        assert!(!blitz_page_reusable(864.0, 900.0));
    }

    #[test]
    fn quantize_rounds_up_to_geometry_steps() {
        assert_eq!(quantize(0.0, 32), 32);
        assert_eq!(quantize(32.0, 32), 32);
        assert_eq!(quantize(32.1, 32), 64);
        assert_eq!(quantize(1000.0, 128), 1024);
        assert_eq!(quantize(f32::NAN, 32), 32);
    }

    #[test]
    fn gtk_dark_flag_parsing() {
        assert!(ini_flag_is_true(
            "[Settings]\ngtk-application-prefer-dark-theme=true\n",
            "gtk-application-prefer-dark-theme"
        ));
        assert!(ini_flag_is_true(
            "[Settings]\ngtk-application-prefer-dark-theme = 1\n",
            "gtk-application-prefer-dark-theme"
        ));
        assert!(!ini_flag_is_true(
            "[Settings]\ngtk-application-prefer-dark-theme=0\n",
            "gtk-application-prefer-dark-theme"
        ));
        assert!(!ini_flag_is_true(
            "[Settings]\n",
            "gtk-application-prefer-dark-theme"
        ));
    }

    #[test]
    fn kde_dark_scheme_parsing() {
        assert!(kde_scheme_is_dark("[General]\nColorScheme=BreezeDark\n"));
        assert!(!kde_scheme_is_dark("[General]\nColorScheme=BreezeLight\n"));
        assert!(!kde_scheme_is_dark("[General]\n"));
    }

    #[test]
    fn edit_form_round_trips_and_keeps_the_account_id() {
        let account = test_account("imap.example.com");
        let form = AccountForm::edit(&account);
        assert_eq!(form.editing.as_deref(), Some("work"));
        assert_eq!(form.to_account(account.id.clone()), account);
    }

    #[test]
    fn renaming_or_retyping_a_password_keeps_the_cached_mail() {
        let account = test_account("imap.example.com");
        let mut form = AccountForm::edit(&account);
        form.name = "Home mail".to_string();
        form.password = "a new secret".to_string();
        assert!(!form.points_elsewhere(&account));
    }

    #[test]
    fn another_endpoint_is_another_mailbox() {
        let account = test_account("imap.example.com");
        let mut form = AccountForm::edit(&account);
        form.host = "imap.other.example".to_string();
        assert!(form.points_elsewhere(&account));

        let mut form = AccountForm::edit(&account);
        form.port_str = "143".to_string();
        assert!(form.points_elsewhere(&account));

        let mut form = AccountForm::edit(&account);
        form.username = "someone@example.com".to_string();
        assert!(form.points_elsewhere(&account));

        let mut form = AccountForm::edit(&account);
        form.protocol = Protocol::Pop3;
        assert!(form.points_elsewhere(&account));

        // Trailing space is trimmed, not a different server.
        let mut form = AccountForm::edit(&account);
        form.host = "  imap.example.com  ".to_string();
        assert!(!form.points_elsewhere(&account));
    }

    fn test_account(host: &str) -> Account {
        Account {
            id: "work".to_string(),
            name: "Work".to_string(),
            protocol: Protocol::Imap,
            host: host.to_string(),
            port: 993,
            username: "me@example.com".to_string(),
            password: "hunter2".to_string(),
            tls: TlsMode::Implicit,
        }
    }

    #[test]
    fn slug_sanitizes_names() {
        assert_eq!(slug("Gmail 主力"), "gmail");
        assert_eq!(slug("  Work! Mail "), "work-mail");
        assert_eq!(slug("???"), "account");
    }

    #[test]
    fn sender_initial_prefers_name_over_email() {
        assert_eq!(sender_initial("Alice <alice@example.com>"), "A");
        assert_eq!(sender_initial("张三 <zhang@nroad.com.cn>"), "张");
        assert_eq!(sender_initial("bob@example.com"), "B");
        assert_eq!(sender_initial(""), "?");
    }

    #[test]
    fn sender_color_is_deterministic() {
        assert_eq!(
            sender_color(theme::DARK, "a@b.c"),
            sender_color(theme::LIGHT, "a@b.c")
        );
        assert_ne!(
            sender_color(theme::DARK, "a@b.c"),
            sender_color(theme::DARK, "other@b.c")
        );
    }

    #[test]
    fn truncate_galley_cuts_to_fit_and_appends_ellipsis() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let color = egui::Color32::WHITE;
            let short = truncate_galley(ctx, "hi", 14.0, color, 500.0);
            assert_eq!(short.text(), "hi");

            let long = truncate_galley(ctx, &"很长的一段中文内容。".repeat(10), 14.0, color, 100.0);
            assert!(long.size().x <= 100.0);
            assert!(long.text().ends_with('…'));
        });
    }

    #[test]
    fn timestamp_formatting() {
        // 2026-10-09T00:00:00Z
        assert_eq!(format_timestamp(1_791_504_000), "2026-10-09 00:00");
    }

    #[test]
    fn sorting_puts_newest_first() {
        let mut msgs = vec![
            MailMessage {
                uid: "1".into(),
                subject: String::new(),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                timestamp: Some(100),
                seen: false,
                body_text: String::new(),
                body_html: None,
                preview: String::new(),
                inline_images: vec![],
                attachment_names: vec![],
            },
            MailMessage {
                uid: "2".into(),
                subject: String::new(),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                timestamp: Some(200),
                seen: false,
                body_text: String::new(),
                body_html: None,
                preview: String::new(),
                inline_images: vec![],
                attachment_names: vec![],
            },
        ];
        sort_messages(&mut msgs);
        assert_eq!(msgs[0].uid, "2");
    }
}
