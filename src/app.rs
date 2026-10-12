//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.
//!
//! Layout: custom title bar on top (nav toggle, account menu, sync, HTML
//! renderer switch, theme, window controls), then three columns —
//! collapsible navigation, message list, reading pane.
//!
//! The reading pane has two HTML renderers, switched from the title bar: the
//! built-in one lays the mail out as egui widgets (`crate::html`), the other
//! rasterizes it with Blitz (`crate::blitz_render`) on a worker thread.

use std::collections::HashMap;
use std::sync::mpsc;

use crate::models::{Account, MailMessage, Protocol, TlsMode};
use crate::storage::{self, FolderCache, Settings};
use crate::sync::{self, Event, Job};

/// Below this window width the navigation bar collapses automatically.
const NAV_AUTO_COLLAPSE_WIDTH: f32 = 640.0;

/// UI color scheme. "System" follows the OS preference (best-effort
/// detection, see `system_prefers_dark`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    /// Button click order: system → light → dark → system.
    fn next(self) -> Self {
        match self {
            Theme::System => Theme::Light,
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::System,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }
}

/// Which renderer draws the HTML body in the reading pane.
///
/// `Simple` is the built-in renderer: the mail becomes native egui widgets,
/// follows the app theme and stays interactive. `Blitz` runs the real browser
/// engine over the same HTML and shows the rasterized page, which is much
/// closer to what the sender designed. The choice is toggled from the title
/// bar and remembered in `settings.json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HtmlRenderer {
    Simple,
    Blitz,
}

impl HtmlRenderer {
    /// Title-bar label: short enough for a 42px button.
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

    fn toggled(self) -> Self {
        match self {
            HtmlRenderer::Simple => HtmlRenderer::Blitz,
            HtmlRenderer::Blitz => HtmlRenderer::Simple,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            HtmlRenderer::Simple => {
                "HTML renderer: Simple — mail HTML drawn as native egui widgets. Click to render with Blitz."
            }
            HtmlRenderer::Blitz => {
                "HTML renderer: Blitz — full CSS layout, rasterized. Click for the built-in renderer."
            }
        }
    }
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
    show_add_account: bool,
    add_form: AddAccountForm,
    confirm_delete: Option<String>,
    nav_collapsed: bool,
    /// Whether the window was narrow last frame, so auto-collapse only
    /// triggers when crossing the threshold (not every frame).
    was_narrow: bool,
    theme: Theme,
    /// Cached OS dark-mode preference, so "system" resolution does not hit
    /// the filesystem every frame. Refreshed on startup and whenever the
    /// theme cycles back to System.
    system_dark: bool,
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
    /// Blitz-rendered pages, keyed by message and geometry; `blitz_order`
    /// holds the insertion order so old pages can be evicted (textures are
    /// megabytes each).
    blitz_pages: HashMap<String, BlitzPage>,
    blitz_order: Vec<String>,
    /// Geometry requested in the previous frame. A render only starts once
    /// the pane has kept the same size for two frames, so dragging the
    /// splitter queues one render instead of one per frame.
    pending_blitz_key: Option<String>,
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
struct RasterizedPage {
    tiles: Vec<RasterTile>,
    /// Page size in points.
    size: egui::Vec2,
    /// Clickable link boxes relative to the page origin, in points.
    links: Vec<(egui::Rect, String)>,
    /// The message was too tall to rasterize in full; its tail is missing.
    truncated: bool,
}

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
    pub cache: storage::AccountCache,
    pub syncing: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
struct AddAccountForm {
    name: String,
    protocol: Protocol,
    host: String,
    port_str: String,
    username: String,
    password: String,
    tls: TlsMode,
}

impl Default for AddAccountForm {
    fn default() -> Self {
        Self {
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

impl KimoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);

        let accounts = storage::load_accounts();
        let mut states = HashMap::new();
        for account in &accounts {
            let state = AccountUiState {
                cache: storage::load_cache(&account.id),
                ..AccountUiState::default()
            };
            states.insert(account.id.clone(), state);
        }

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
            show_add_account: false,
            add_form: AddAccountForm::default(),
            confirm_delete: None,
            nav_collapsed: false,
            was_narrow: false,
            theme: Theme::parse(&storage::load_settings().theme),
            system_dark: system_prefers_dark(),
            images: HashMap::new(),
            stylesheets: HashMap::new(),
            inline_textures: HashMap::new(),
            html_renderer: HtmlRenderer::parse(
                &storage::load_settings().html_renderer.unwrap_or_default(),
            ),
            blitz_pages: HashMap::new(),
            blitz_order: Vec::new(),
            pending_blitz_key: None,
            scrolled_message: None,
        };

        app.selected_account = app.accounts.first().map(|a| a.id.clone());
        app.selected_folder = Some("INBOX".to_string());

        // System-tray icon (best effort; StatusNotifierItem over D-Bus).
        crate::tray::spawn(app.ctx.clone());

        for account in app.accounts.clone() {
            sync::spawn(Job::Sync { account }, app.ctx.clone(), app.tx.clone());
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
                let mut snapshot = None;
                if let Some(state) = self.states.get_mut(&account_id) {
                    if state.cache.folders.iter().all(|f| f.name != folder) {
                        state.cache.folders.push(FolderCache {
                            name: folder.clone(),
                            uidvalidity: None,
                            synced: false,
                            messages: Vec::new(),
                        });
                    }
                    let fc = state
                        .cache
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
                    snapshot = Some(state.cache.clone());
                }
                if let Some(cache) = snapshot {
                    let _ = storage::save_cache(&account_id, &cache);
                }
            }
            Event::SyncDone { account_id } => {
                let mut snapshot = None;
                if let Some(state) = self.states.get_mut(&account_id) {
                    state.syncing = false;
                    snapshot = Some(state.cache.clone());
                }
                if let Some(cache) = snapshot {
                    let _ = storage::save_cache(&account_id, &cache);
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
        }
    }

    // ----- actions ----------------------------------------------------------

    fn sync_all(&mut self) {
        for account in self.accounts.clone() {
            sync::spawn(Job::Sync { account }, self.ctx.clone(), self.tx.clone());
        }
    }

    fn current_account(&self) -> Option<Account> {
        self.accounts
            .iter()
            .find(|a| self.selected_account.as_deref() == Some(a.id.as_str()))
            .cloned()
    }

    fn select_folder(&mut self, account_id: String, folder: String, protocol: Protocol) {
        self.selected_account = Some(account_id.clone());
        self.selected_folder = Some(folder.clone());
        self.selected_message = None;

        let needs_fetch = match self.states.get(&account_id) {
            Some(state) => match state.cache.folders.iter().find(|f| f.name == folder) {
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
                Protocol::Pop3 => Job::Sync { account },
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

        // Mark seen locally first so the UI reacts immediately.
        let mut snapshot = None;
        if let Some(state) = self.states.get_mut(&account_id) {
            if let Some(fc) = state.cache.folders.iter_mut().find(|f| f.name == folder) {
                if let Some(existing) = fc.messages.iter_mut().find(|m| m.uid == msg.uid) {
                    existing.seen = true;
                    snapshot = Some(state.cache.clone());
                }
            }
        }
        if let Some(cache) = snapshot {
            let _ = storage::save_cache(&account_id, &cache);
        }

        // Then push the flag to the server (IMAP only; POP3 is local-only).
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            if account.protocol == Protocol::Imap {
                if let Ok(uid) = msg.uid.parse::<u32>() {
                    sync::spawn(
                        Job::MarkSeen {
                            account,
                            folder,
                            uid,
                        },
                        self.ctx.clone(),
                        self.tx.clone(),
                    );
                }
            }
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
        let _ = storage::save_accounts(&self.accounts);
        storage::delete_cache(account_id);
    }

    fn save_new_account(&mut self) {
        let form = self.add_form.clone();
        let port: u16 = form
            .port_str
            .trim()
            .parse()
            .unwrap_or_else(|_| form.protocol.default_port(form.tls));

        let base = slug(&form.name);
        let mut id = base.clone();
        let mut n = 1;
        while self.accounts.iter().any(|a| a.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }

        let account = Account {
            id,
            name: form.name.trim().to_string(),
            protocol: form.protocol,
            host: form.host.trim().to_string(),
            port,
            username: form.username.trim().to_string(),
            password: form.password.clone(),
            tls: form.tls,
        };
        let account_id = account.id.clone();
        self.accounts.push(account.clone());
        self.states.entry(account_id.clone()).or_default();
        let _ = storage::save_accounts(&self.accounts);

        sync::spawn(Job::Sync { account }, self.ctx.clone(), self.tx.clone());
        self.selected_account = Some(account_id);
        self.selected_folder = Some("INBOX".to_string());
        self.selected_message = None;
        self.show_add_account = false;
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

    /// Identity of a rendered page: the message, the account it came from and
    /// the geometry it was laid out for. Anything else invalidates the cache
    /// entry and starts a fresh render.
    fn blitz_key(&self, msg: &MailMessage, width: u32, viewport_height: u32, scale: f32) -> String {
        format!(
            "{}/{}/{}/{}x{}@{:.2}",
            self.selected_account.as_deref().unwrap_or("-"),
            self.selected_folder.as_deref().unwrap_or("-"),
            msg.uid,
            width,
            viewport_height,
            scale
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
        // Only one render at a time: resizing the pane invalidates the width
        // repeatedly, and every superseded render would be wasted work. Their
        // results are dropped when they arrive, since the keys are gone.
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
                .map(|image| (image.cid.clone(), image.data_base64.clone()))
                .collect(),
        };
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
        let viewport_height = quantize(ui.available_height().max(160.0) * scale, BLITZ_HEIGHT_STEP);
        let key = self.blitz_key(msg, width, viewport_height, scale);

        if !self.blitz_pages.contains_key(&key) {
            // Wait for the geometry to settle: dragging the pane splitter
            // would otherwise queue a render on every frame.
            if self.pending_blitz_key.as_deref() == Some(key.as_str()) {
                self.start_blitz_render(key.clone(), msg, html, width, viewport_height, scale);
            } else {
                self.pending_blitz_key = Some(key.clone());
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
                    ui.spinner();
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
        });
    }

    /// Apply the selected theme to the context, resolving "system" against
    /// the cached OS preference.
    fn apply_theme(&self, ctx: &egui::Context) {
        let dark = match self.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => self.system_dark,
        };
        ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
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
        let Some(fc) = state.cache.folders.iter().find(|f| f.name == *folder) else {
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
            .cache
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
                    Ctrl::Close => egui::Color32::from_rgb(232, 17, 35),
                    _ => visuals.widgets.hovered.bg_fill,
                };
                ui.painter().rect_filled(rect, 0.0, bg);
                match ctrl {
                    // White reads best on the close red; elsewhere keep the
                    // theme text color so the glyph survives light visuals.
                    Ctrl::Close => egui::Color32::WHITE,
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

        // Theme switch (system → light → dark), just left of the controls.
        let theme_right = bar.right() - TITLEBAR_BUTTON_WIDTH * buttons.len() as f32;
        let theme_rect = egui::Rect::from_min_max(
            egui::pos2(theme_right - TITLEBAR_BUTTON_WIDTH, bar.top()),
            egui::pos2(theme_right, bar.bottom()),
        );
        let theme_resp = ui.interact(
            theme_rect,
            egui::Id::new("titlebar-theme"),
            egui::Sense::click(),
        );
        if theme_resp.hovered() {
            ui.painter()
                .rect_filled(theme_rect, 0.0, visuals.widgets.hovered.bg_fill);
        }
        paint_theme_icon(ui.painter(), theme_rect, visuals.text_color());
        if theme_resp.clicked() {
            self.theme = self.theme.next();
            if self.theme == Theme::System {
                self.system_dark = system_prefers_dark();
            }
            self.persist_settings();
        }
        theme_resp.on_hover_text(format!(
            "Theme: {} — click to switch (system → light → dark)",
            self.theme.label()
        ));

        // HTML renderer switch (Simple ↔ Blitz), left of the theme button.
        // Filled while Blitz is active, so the current renderer is visible at
        // a glance.
        let renderer_right = theme_rect.left();
        let renderer_rect = egui::Rect::from_min_max(
            egui::pos2(renderer_right - TITLEBAR_BUTTON_WIDTH, bar.top()),
            egui::pos2(renderer_right, bar.bottom()),
        );
        let renderer_resp = ui.interact(
            renderer_rect,
            egui::Id::new("titlebar-html-renderer"),
            egui::Sense::click(),
        );
        if renderer_resp.hovered() {
            ui.painter()
                .rect_filled(renderer_rect, 0.0, visuals.widgets.hovered.bg_fill);
        }
        if self.html_renderer == HtmlRenderer::Blitz {
            ui.painter().rect_filled(
                renderer_rect.shrink2(egui::vec2(6.0, 6.0)),
                5.0,
                visuals.selection.bg_fill,
            );
        }
        ui.painter().text(
            renderer_rect.center(),
            egui::Align2::CENTER_CENTER,
            self.html_renderer.label(),
            egui::FontId::proportional(11.0),
            visuals.text_color(),
        );
        if renderer_resp.clicked() {
            self.html_renderer = self.html_renderer.toggled();
            self.persist_settings();
            self.pending_blitz_key = None;
        }
        renderer_resp.on_hover_text(self.html_renderer.tooltip());

        // --- left content row, kept clear of the window buttons ---
        let content_right = renderer_rect.left();
        let row_size = egui::vec2((content_right - bar.left()).max(0.0), bar.height());
        let content_end = ui
            .allocate_ui_with_layout(
                row_size,
                egui::Layout::left_to_right(egui::Align::Center),
                |row| {
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

                    row.label(egui::RichText::new("Kimo").small().strong());
                    row.separator();

                    // Account menu: switch accounts, add, remove (two-step confirm).
                    let selected_name = self
                        .current_account()
                        .map(|a| a.name)
                        .unwrap_or_else(|| "No account".to_string());
                    let mut switch_to: Option<Account> = None;
                    let mut add_clicked = false;
                    egui::ComboBox::from_id_salt("account-selector")
                        .selected_text(selected_name)
                        .width(170.0)
                        .show_ui(row, |ui| {
                            ui.set_min_width(240.0);
                            for account in &self.accounts {
                                let is_sel =
                                    self.selected_account.as_deref() == Some(account.id.as_str());
                                let text = egui::RichText::new(format!(
                                    "{} · {}",
                                    account.name,
                                    account.protocol.label()
                                ));
                                if ui.selectable_label(is_sel, text).clicked() {
                                    switch_to = Some(account.clone());
                                }
                            }
                            if !self.accounts.is_empty() {
                                ui.separator();
                            }
                            if ui.button("+ Add account…").clicked() {
                                add_clicked = true;
                            }
                            if let Some(current) = self
                                .accounts
                                .iter()
                                .find(|a| self.selected_account.as_deref() == Some(a.id.as_str()))
                            {
                                let confirming =
                                    self.confirm_delete.as_deref() == Some(current.id.as_str());
                                let label = if confirming {
                                    egui::RichText::new(format!("Remove \"{}\"?", current.name))
                                        .color(egui::Color32::RED)
                                } else {
                                    egui::RichText::new(format!("Remove \"{}\"", current.name))
                                };
                                if ui.button(label).clicked() {
                                    if confirming {
                                        let id = current.id.clone();
                                        self.delete_account(&id);
                                        self.confirm_delete = None;
                                    } else {
                                        self.confirm_delete = Some(current.id.clone());
                                    }
                                }
                            }
                        });
                    if let Some(account) = switch_to {
                        self.select_folder(
                            account.id.clone(),
                            "INBOX".to_string(),
                            account.protocol,
                        );
                    }
                    if add_clicked {
                        self.add_form = AddAccountForm::default();
                        self.show_add_account = true;
                    }

                    if row.button("Sync").clicked() {
                        self.sync_all();
                    }

                    // Per-account status: spinner while syncing, warning on error.
                    for account in self.accounts.clone() {
                        let Some(state) = self.states.get(&account.id) else {
                            continue;
                        };
                        if state.syncing {
                            row.spinner()
                                .on_hover_text(format!("{}: syncing…", account.name));
                        } else if let Some(err) = &state.error {
                            row.label(egui::RichText::new("⚠").color(egui::Color32::RED))
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
    fn nav(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some(account) = self.current_account() else {
                    ui.label("No account selected.");
                    ui.label(
                        egui::RichText::new("Add one from the account menu in the title bar.")
                            .small()
                            .weak(),
                    );
                    return;
                };
                let (folders, syncing, error) = {
                    let state = self.states.entry(account.id.clone()).or_default();
                    (state.folders.clone(), state.syncing, state.error.clone())
                };

                ui.label(egui::RichText::new("MAILBOXES").small().weak());
                ui.add_space(4.0);

                let folders_were_empty = folders.is_empty();
                let folder_list = if folders.is_empty() {
                    vec!["INBOX".to_string()]
                } else {
                    folders
                };
                for folder in folder_list {
                    let selected = self.selected_account.as_deref() == Some(account.id.as_str())
                        && self.selected_folder.as_deref() == Some(folder.as_str());
                    let unread = self
                        .states
                        .get(&account.id)
                        .and_then(|s| s.cache.folders.iter().find(|f| f.name == folder))
                        .map(|f| f.messages.iter().filter(|m| !m.seen).count())
                        .unwrap_or(0);
                    // `folder` is the IMAP wire form (possibly modified
                    // UTF-7); decode it just for the label.
                    let display = crate::protocol::imap::decode_mailbox_name(folder.as_str());
                    let text = if unread > 0 {
                        egui::RichText::new(format!("📁 {display} ({unread})")).strong()
                    } else {
                        egui::RichText::new(format!("📁 {display}")).weak()
                    };
                    if ui.selectable_label(selected, text).clicked() {
                        self.select_folder(account.id.clone(), folder, account.protocol);
                    }
                }
                if syncing && folders_were_empty {
                    ui.add_space(4.0);
                    ui.spinner();
                }
                if let Some(err) = error {
                    ui.add_space(4.0);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    ui.label(
                        egui::RichText::new(format!("⚠ {err}"))
                            .small()
                            .color(egui::Color32::RED),
                    );
                }
            });
    }

    /// Middle column: filter bar plus the message list.
    fn mail_list(&mut self, ui: &mut egui::Ui) {
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
        let visuals = ui.visuals().clone();
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), MESSAGE_ROW_HEIGHT),
            egui::Sense::click(),
        );

        let painter = ui.painter();
        if is_selected {
            painter.rect_filled(rect.shrink(1.0), 4.0, visuals.selection.bg_fill);
        } else if resp.hovered() {
            painter.rect_filled(rect.shrink(1.0), 4.0, visuals.faint_bg_color);
        }
        painter.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
        );

        // Avatar: deterministic color per sender, initial centered in it.
        let avatar_center = egui::pos2(rect.left() + 8.0 + AVATAR_DIAMETER / 2.0, rect.center().y);
        painter.circle_filled(
            avatar_center,
            AVATAR_DIAMETER / 2.0,
            sender_color(&msg.from),
        );
        painter.text(
            avatar_center,
            egui::Align2::CENTER_CENTER,
            sender_initial(&msg.from),
            egui::FontId::proportional(AVATAR_FONT_SIZE),
            egui::Color32::WHITE,
        );

        let text_left = avatar_center.x + AVATAR_DIAMETER / 2.0 + 10.0;
        let text_right = rect.right() - 10.0;
        // Unread mail keeps the full-contrast text color; read mail is muted.
        let main_color = if msg.seen {
            visuals.weak_text_color()
        } else {
            visuals.text_color()
        };

        // First row: timestamp right-aligned, sender truncated before it.
        let date_galley = ui.ctx().fonts(|f| {
            f.layout_no_wrap(
                display_date(msg),
                egui::FontId::proportional(12.0),
                visuals.weak_text_color(),
            )
        });
        let date_pos = egui::pos2(
            text_right - date_galley.size().x,
            rect.top() + 20.0 - date_galley.size().y * 0.5,
        );
        painter.galley(date_pos, date_galley, visuals.weak_text_color());
        let sender_max_width = (date_pos.x - text_left - 12.0).max(40.0);
        let sender = truncate_galley(ui.ctx(), &msg.from, 14.0, main_color, sender_max_width);
        painter.galley(
            egui::pos2(text_left, rect.top() + 20.0 - sender.size().y * 0.5),
            sender,
            main_color,
        );

        // Second row: subject.
        let subject = truncate_galley(
            ui.ctx(),
            &msg.subject,
            14.0,
            main_color,
            text_right - text_left,
        );
        painter.galley(
            egui::pos2(text_left, rect.top() + 40.0 - subject.size().y * 0.5),
            subject,
            main_color,
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
            12.0,
            visuals.weak_text_color(),
            text_right - text_left,
        );
        painter.galley(
            egui::pos2(text_left, rect.top() + 57.0 - preview.size().y * 0.5),
            preview,
            visuals.weak_text_color(),
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

    fn add_account_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add_account;
        egui::Window::new("Add account")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                let f = &mut self.add_form;
                egui::Grid::new("add_account_grid")
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
                        ui.add(
                            egui::TextEdit::singleline(&mut f.host).hint_text("imap.example.com"),
                        );
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
                        ui.add(
                            egui::TextEdit::singleline(&mut f.username)
                                .hint_text("you@example.com"),
                        );
                        ui.end_row();

                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut f.password).password(true));
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Note: credentials are stored in plain text on this machine.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);

                let valid = {
                    let f = &self.add_form;
                    !f.name.trim().is_empty()
                        && !f.host.trim().is_empty()
                        && !f.username.trim().is_empty()
                        && f.port_str.trim().parse::<u16>().is_ok()
                };
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_add_account = false;
                    }
                    if ui
                        .add_enabled(valid, egui::Button::new("Save & connect"))
                        .clicked()
                    {
                        self.save_new_account();
                    }
                });
            });
        if !open {
            self.show_add_account = false;
        }
    }
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

        if self.show_add_account {
            self.add_account_window(ctx);
        }
    }
}

// ----- helpers --------------------------------------------------------------

const TITLEBAR_HEIGHT: f32 = 30.0;
const TITLEBAR_BUTTON_WIDTH: f32 = 42.0;

/// Blitz reading pane: page margin, geometry quantization for renders, and
/// how many rendered pages stay in memory (each is megabytes of texture).
const BLITZ_PAGE_MARGIN: f32 = 8.0;
const BLITZ_WIDTH_STEP: u32 = 32;
const BLITZ_HEIGHT_STEP: u32 = 128;
const BLITZ_PAGES_CACHED: usize = 2;

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
/// Font size of the avatar's initial, ~0.47 of the diameter (as at 38.0).
const AVATAR_FONT_SIZE: f32 = 15.0;

/// Deterministic, readable-on-white-text avatar colors (Material palette).
const AVATAR_COLORS: &[egui::Color32] = &[
    egui::Color32::from_rgb(229, 57, 53),
    egui::Color32::from_rgb(66, 133, 244),
    egui::Color32::from_rgb(52, 168, 83),
    egui::Color32::from_rgb(251, 140, 0),
    egui::Color32::from_rgb(142, 36, 170),
    egui::Color32::from_rgb(0, 150, 136),
    egui::Color32::from_rgb(121, 85, 72),
    egui::Color32::from_rgb(236, 64, 122),
];

/// Decode a stored inline image (base64) into an egui texture. Runs on the
/// UI thread but only once per message/cid; inline mail images are small.
fn decode_inline_image(
    ctx: &egui::Context,
    image: &crate::models::InlineImage,
) -> Option<egui::TextureHandle> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data_base64)
        .ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    let rgba = decoded.to_rgba8();
    let pixels = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        &rgba,
    );
    Some(ctx.load_texture("inline-image", pixels, Default::default()))
}

fn sender_color(from: &str) -> egui::Color32 {
    let hash = from
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    AVATAR_COLORS[(hash as usize) % AVATAR_COLORS.len()]
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

/// Half-filled circle — the classic theme/contrast glyph.
fn paint_theme_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let r = 5.5;
    p.circle_stroke(c, r, egui::Stroke::new(1.2_f32, color));
    let left_half = egui::Rect::from_min_max(
        egui::pos2(c.x - r - 0.5, c.y - r - 0.5),
        egui::pos2(c.x, c.y + r + 0.5),
    );
    p.with_clip_rect(left_half).circle_filled(c, r - 0.9, color);
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

/// egui's default fonts only cover Latin/Cyrillic/Greek; mail often needs
/// more. Best effort: if a system CJK font exists, use it as a fallback for
/// both proportional and monospace text.
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-sans-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("kimo_cjk".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("kimo_cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
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
        assert_eq!(HtmlRenderer::Simple.toggled(), HtmlRenderer::Blitz);
        assert_eq!(HtmlRenderer::Blitz.toggled(), HtmlRenderer::Simple);
        assert_eq!(HtmlRenderer::Blitz.as_str(), "blitz");
        assert_eq!(HtmlRenderer::Simple.as_str(), "egui");
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
    fn theme_cycles_system_light_dark() {
        assert_eq!(Theme::System.next(), Theme::Light);
        assert_eq!(Theme::Light.next(), Theme::Dark);
        assert_eq!(Theme::Dark.next(), Theme::System);
    }

    #[test]
    fn theme_parses_known_values_and_defaults_to_system() {
        assert_eq!(Theme::parse("dark"), Theme::Dark);
        assert_eq!(Theme::parse("light"), Theme::Light);
        assert_eq!(Theme::parse("nonsense"), Theme::System);
        assert_eq!(Theme::parse(""), Theme::System);
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
        assert_eq!(sender_color("a@b.c"), sender_color("a@b.c"));
        assert_ne!(sender_color("a@b.c"), sender_color("other@b.c"));
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
