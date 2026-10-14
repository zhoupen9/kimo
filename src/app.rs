//! The egui application: all UI state and drawing. Network results arrive as
//! `sync::Event`s drained at the top of every frame; opening a socket happens
//! only in worker threads.
//!
//! Layout: custom title bar on top (nav toggle, sync in Debug mode, search,
//! then the mail actions — reply, reply all, forward, label, flag, export,
//! delete — as one flat group a fixed gap to its right, then the preferences
//! cog and the window controls), then three columns — collapsible
//! navigation (every account as a collapsible section of its folders), message
//! list, reading pane.
//!
//! Three `egui::Modal`s open from the title bar, each dimming the columns
//! behind it. Search is the box with the magnifier in it: clicking it opens the
//! popup, which holds the free text, the scope and the filters. The column
//! carries no search chrome of its own — the magnifier turning accent-colored
//! is what says a search is what it is showing. The cog is Preferences: its
//! Accounts page holds the account list and the add/edit account form, its
//! Appearance page the theme (which resolves to a `theme::ColorScheme`), the
//! HTML renderer and the fonts. That renderer draws the mail either as egui
//! widgets (`crate::html`) or rasterized by Blitz (`crate::blitz_render`) on a
//! worker thread. The pen is the compose card, which holds a draft and
//! nothing else — Kimo has no SMTP, so there is nowhere for it to go.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::folders::{self, FolderKind};
use crate::fonts::FontFamily;
use crate::glass::Glass;
use crate::models::{Account, MailMessage, Protocol, TlsMode};
use crate::search::{Scope, SearchHit, SearchQuery, SEARCH_LIMIT};
use crate::storage::{self, Settings};
use crate::sync::{self, Event, Job};
use crate::theme::{self, ColorScheme, Theme};

/// Below this window width the navigation bar collapses automatically.
const NAV_AUTO_COLLAPSE_WIDTH: f32 = 640.0;

/// How long the search popup waits after the last keystroke before asking the
/// database: long enough to swallow a burst of typing, short enough to feel
/// immediate. Enter skips the wait.
const SEARCH_SETTLE: Duration = Duration::from_millis(250);

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

/// How much Kimo does on its own.
///
/// `Release` is the ordinary client: it syncs every account at startup and
/// fetches a folder the first time it is opened. `Debug` keeps every automatic
/// sync off — for working on the app against a real account — leaving the
/// title-bar Sync button, which only Debug shows, and `Save & sync` in
/// Preferences as the only ways to reach the server. The choice lives in
/// Preferences → General and is
/// remembered in `settings.json`; a file that names no mode follows the build,
/// so a debug build starts quiet and a release build behaves as it always has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WorkingMode {
    Debug,
    Release,
}

impl WorkingMode {
    /// The modes, in the order the Preferences radios show them.
    const ALL: [WorkingMode; 2] = [WorkingMode::Release, WorkingMode::Debug];

    /// What a settings file that says nothing means: how this binary was built.
    fn from_build() -> Self {
        if cfg!(debug_assertions) {
            WorkingMode::Debug
        } else {
            WorkingMode::Release
        }
    }

    /// Short label for the Preferences radios.
    fn label(self) -> &'static str {
        match self {
            WorkingMode::Debug => "Debug",
            WorkingMode::Release => "Release",
        }
    }

    /// Value stored in `settings.json`.
    fn as_str(self) -> &'static str {
        match self {
            WorkingMode::Debug => "debug",
            WorkingMode::Release => "release",
        }
    }

    /// `None` for anything unrecognised, so a hand-edited file falls back to
    /// the build instead of silently turning something off.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "debug" => Some(WorkingMode::Debug),
            "release" => Some(WorkingMode::Release),
            _ => None,
        }
    }

    /// The mode a settings value names, or the build's when it names none.
    fn from_settings(value: Option<&str>) -> Self {
        value
            .and_then(WorkingMode::parse)
            .unwrap_or_else(WorkingMode::from_build)
    }

    /// One-line explanation shown under the mode radios.
    fn description(self) -> &'static str {
        match self {
            WorkingMode::Debug => {
                "No automatic syncing: mail is fetched only when you press Sync, or Save & sync for one account."
            }
            WorkingMode::Release => {
                "Syncs every account at startup and fetches each folder the first time you open it."
            }
        }
    }

    /// Whether Kimo may start a sync nobody asked for. A mode that says no
    /// still syncs from the title-bar button and from the account form.
    fn syncs_automatically(self) -> bool {
        matches!(self, WorkingMode::Release)
    }
}

/// Pages of the preferences modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefsTab {
    General,
    Accounts,
    Appearance,
}

/// One of the mail actions in the title bar. All of them act on the mail open
/// in the reading pane; Export and Delete are the ones that do something so
/// far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MailAction {
    Reply,
    ReplyAll,
    Forward,
    Label,
    Flag,
    Export,
    Delete,
}

impl MailAction {
    /// The buttons, in the order the bar draws them: writing a mail out comes
    /// before throwing one away.
    const ALL: [MailAction; 7] = [
        MailAction::Reply,
        MailAction::ReplyAll,
        MailAction::Forward,
        MailAction::Label,
        MailAction::Flag,
        MailAction::Export,
        MailAction::Delete,
    ];

    /// The action's name. The bar carries glyphs rather than words, so this
    /// is the hover text, and the "no mail selected" note beside it.
    fn label(self) -> &'static str {
        match self {
            MailAction::Reply => "Reply",
            MailAction::ReplyAll => "Reply all",
            MailAction::Forward => "Forward",
            MailAction::Label => "Label",
            MailAction::Flag => "Flag",
            MailAction::Export => "Export",
            MailAction::Delete => "Delete",
        }
    }

    /// The hover text while there is mail to act on. The two actions that do
    /// something say what it is: Export where the file goes, and Delete that
    /// it cannot be undone, because it expunges the mail on the server as
    /// well.
    fn tooltip(self) -> &'static str {
        match self {
            MailAction::Export => {
                "Export — saves the original mail as a .eml file in your Downloads directory"
            }
            MailAction::Delete => "Delete — removes this mail from the server too",
            other => other.label(),
        }
    }
}

/// A mail being written in the compose window.
///
/// Nothing about it is persisted and nothing is sent: Kimo speaks IMAP and
/// POP3 only, so the draft is exactly as long-lived as the window holding it,
/// and closing the window — by its ✕ or by Discard — drops it. The fields are
/// here rather than in widgets' own state because a window that is closed and
/// opened again must start empty.
struct ComposeDraft {
    /// The account the mail would go out from, as the window's From list has
    /// it. `None`, with no account configured, is the one case the window
    /// says so instead of offering a choice.
    from: Option<String>,
    to: String,
    cc: String,
    subject: String,
    body: String,
    /// Set on the first frame the window is shown, so the To field takes the
    /// keyboard without a click.
    focus: bool,
}

impl ComposeDraft {
    /// A new, empty draft written from `from` — see `compose_from`.
    fn new(from: Option<String>) -> Self {
        Self {
            from,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            focus: true,
        }
    }
}

/// The account a new draft is written from: the one whose mail is open, or the
/// first configured account when none is. An id that is no longer configured —
/// the account was removed with a draft open — falls back the same way, so the
/// From list never shows an account that is not there.
fn compose_from(accounts: &[Account], selected: Option<&str>) -> Option<String> {
    selected
        .filter(|id| accounts.iter().any(|account| account.id == *id))
        .map(str::to_string)
        .or_else(|| accounts.first().map(|account| account.id.clone()))
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
    /// What the search popup is asking for. While it is active the middle
    /// column shows `search_hits` instead of the selected folder.
    search: SearchQuery,
    /// The sequence number of the search whose results are wanted, so a result
    /// that arrives after the query changed can be dropped.
    search_seq: u64,
    /// The query the settle clock is running for, and since when.
    search_pending: Option<(SearchQuery, Instant)>,
    /// The query the current results (or the request in flight) answer, so the
    /// unchanged bar is not searched again and again.
    search_applied: Option<SearchQuery>,
    search_hits: Vec<SearchHit>,
    search_running: bool,
    search_error: Option<String>,
    /// Whether the search popup is open.
    show_search: bool,
    /// Set when the popup opens, so its text field takes the keyboard on the
    /// next frame instead of waiting for a click.
    search_focus: bool,
    /// The draft the compose window holds, or `None` when it is closed. One
    /// window at a time, and the draft is the only place the mail exists —
    /// nothing is persisted, and nothing is sent (Kimo has no SMTP).
    compose: Option<ComposeDraft>,
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
    /// The frosted backdrop of a floating title bar (see `crate::glass`): the
    /// GL pass is made once, on the first frame the bar floats.
    glass: Glass,
    theme: Theme,
    /// Cached OS dark-mode preference, so "system" resolution does not hit
    /// the filesystem every frame. Refreshed on startup and whenever the
    /// theme cycles back to System.
    system_dark: bool,
    /// Why the mail database is not being written to disk, when that is the
    /// case; drawn as a one-line banner above the message list.
    storage_warning: Option<String>,
    /// What the last export wrote, and when it said so: one line above the
    /// message list for `EXPORT_NOTICE_TIME`. A file that lands in a directory
    /// the user was not watching is a file they cannot find, and the numbered
    /// name it ended up under is the worker's to know — hence the event.
    export_notice: Option<(String, Instant)>,
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
    /// Whether Kimo may sync without being asked (Preferences → General).
    working_mode: WorkingMode,
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
    /// see `GEOMETRY_SETTLE`.
    pending_blitz_key: Option<(String, Instant)>,
    /// The built-in renderer's copy of the mail on screen: the parsed body and
    /// the width it is laid out for. See [`BodyCache`].
    body: BodyCache,
    /// Message the reading pane's scroll offset belongs to, so opening another
    /// mail starts at the top instead of mid-page.
    scrolled_message: Option<String>,
    /// The message list's own rect this frame, set by `mail_list`. The rows'
    /// rules span it so they reach the column's hairlines; nothing else reads
    /// it.
    list_column: egui::Rect,
    /// How far the message list is scrolled, in points — zero when it has no
    /// scroll area at all. The title bar floats over the column once this
    /// leaves the top, and docks again when it comes back.
    list_scroll: f32,
    /// The same for the reading pane's card, which scrolls the whole mail:
    /// either column scrolled is content under the floating bar. Zero while
    /// no mail is open.
    mail_scroll: f32,
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

/// What the built-in renderer keeps between frames for the mail on screen.
///
/// Two things are worth keeping, and a splitter drag is what makes both of
/// them matter. The **parsed body**, because html5ever's parse is the larger
/// half of what a body costs per frame and the tree does not change while the
/// mail is open. And the **width the body was laid out for**, because every
/// new width re-shapes every run of text in it: dragging the splitter changes
/// the pane's width on every frame, so without this the whole body is re-laid
/// out — the most expensive thing the app can be asked to do — once per frame
/// of the drag.
///
/// So the body keeps the width it settled at while the pane is moving, and the
/// pane clips it (the mail is briefly cut off, or briefly short of the pane's
/// new edge). Once the geometry has held still for `GEOMETRY_SETTLE` the body
/// reflows once, for the width the pane has settled at — the same wait Blitz's
/// renderer makes before it renders a new width, for the same reason.
#[derive(Default)]
struct BodyCache {
    /// `<folder>/<uid>` of the mail this holds.
    identity: String,
    /// Hash of the body, so a re-fetch that replaces the body of a uid the
    /// pane is already showing is noticed (`body_fingerprint`).
    fingerprint: u64,
    document: Option<crate::html::Document>,
    /// The width the body is laid out for; `None` until it has been laid out
    /// once.
    width: Option<f32>,
    /// The width the pane wants now — quantized, so that a pixel of jitter is
    /// not a reflow — and since when it has wanted it.
    pending: Option<(u32, Instant)>,
}

impl BodyCache {
    /// The document and the width to lay it out at for this frame, parsing the
    /// body when the cache does not already hold it.
    fn get(
        &mut self,
        ctx: &egui::Context,
        identity: &str,
        fingerprint: u64,
        html: &str,
        wanted: f32,
        now: Instant,
    ) -> (&crate::html::Document, f32) {
        if self.document.is_none() || self.identity != identity || self.fingerprint != fingerprint {
            self.identity = identity.to_string();
            self.fingerprint = fingerprint;
            self.document = Some(crate::html::Document::parse(html));
            // A body the cache has not seen has no earlier layout to hold on
            // to: it is laid out at the pane's own width at once.
            self.width = None;
            self.pending = None;
        }
        let width = self.layout_width(ctx, wanted, now);
        (self.document(), width)
    }

    /// The body, which `get` has made sure is parsed.
    fn document(&self) -> &crate::html::Document {
        self.document.as_ref().expect("filled in by `get`")
    }

    /// The width to lay the body out at, given the width `wanted` the pane has
    /// now. Anything else is a reflow, and a reflow waits for the geometry to
    /// settle (see `BodyCache`); while it waits the pane is asked for a repaint
    /// when the wait is over, since nothing else would make one happen.
    fn layout_width(&mut self, ctx: &egui::Context, wanted: f32, now: Instant) -> f32 {
        let wanted_key = quantize(wanted, 2);
        let Some(width) = self.width else {
            self.width = Some(wanted);
            self.pending = None;
            return wanted;
        };
        if quantize(width, 2) == wanted_key {
            self.pending = None;
            return width;
        }
        match self.pending {
            Some((key, since)) if key == wanted_key => {
                let waited = now.duration_since(since);
                if waited >= GEOMETRY_SETTLE {
                    self.width = Some(wanted);
                    self.pending = None;
                    wanted
                } else {
                    ctx.request_repaint_after(GEOMETRY_SETTLE - waited);
                    width
                }
            }
            _ => {
                // A width the pane has just moved to (or is still moving
                // through) starts the wait over.
                self.pending = Some((wanted_key, now));
                ctx.request_repaint_after(GEOMETRY_SETTLE);
                width
            }
        }
    }

    /// Drop the parsed body: what is on screen is another renderer's, or none.
    fn forget(&mut self) {
        self.document = None;
        self.identity.clear();
        self.fingerprint = 0;
        self.width = None;
        self.pending = None;
    }
}

/// A fingerprint of a body. Length alone would not do: a re-fetch can bring a
/// different body of the same size under a uid the pane already shows.
fn body_fingerprint(html: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut hasher);
    hasher.finish()
}

#[derive(Default)]
pub struct AccountUiState {
    /// The mailboxes the server lists for this account: what the last `LIST` of
    /// this session answered, or — before one has run — the mailboxes the
    /// database recorded as listed. Anything else the account holds is mail
    /// cached for a folder the server no longer lists.
    pub folders: Vec<String>,
    /// The account's folders and their cached mail, loaded from the database
    /// at startup and kept in memory (the DB is the durable copy).
    pub mailbox: storage::Mailbox,
    pub syncing: bool,
    pub error: Option<String>,
    /// Bumped whenever `folders` or the cached mail changes. The navigation
    /// column's rows are derived from it (`FolderRows::rev`), so a frame that
    /// changed nothing re-derives nothing.
    folders_rev: u64,
    /// The column's rows, derived from the two fields above.
    nav: FolderRows,
}

impl AccountUiState {
    /// The rows the navigation column draws, derived from the account's state
    /// when that has changed since the last derivation and reused until the
    /// next change. egui redraws the column every frame; what it holds only
    /// moves when a sync reports a different mailbox list or brings mail.
    fn nav(&mut self) -> &FolderRows {
        if self.nav.rev != Some(self.folders_rev) {
            self.nav = FolderRows::of(&self.mailbox, &self.folders, self.folders_rev);
        }
        &self.nav
    }

    /// The account's folder list or its cached mail changed, so whatever the
    /// column derived from them is stale. Only a real change calls this: a
    /// sync that reports the mailboxes it already reported leaves the rows —
    /// and the column — exactly as they were.
    fn folders_changed(&mut self) {
        self.folders_rev = self.folders_rev.wrapping_add(1);
    }
}

/// One folder as the navigation column draws it. Deriving this is what the
/// column caches: the label (decoded from the IMAP wire form), the kind that
/// picks its icon, its unread count, and whether the server still lists it.
struct FolderRow {
    /// The wire form — the row's identity, and what `SELECT` gets.
    name: String,
    display: String,
    kind: FolderKind,
    unread: usize,
    /// False for a folder the server no longer lists: the row is kept for the
    /// mail cached under it, and is drawn as one that is not there any more.
    listed: bool,
}

/// The navigation column's rows for one account: what the server lists, then
/// the folders it does not, kept because they still hold mail. Deriving them
/// costs a decode, a classification and an unread count per folder; doing it
/// once per change of the account state rather than once per frame is the
/// whole point, and it is also what makes a sync that found nothing new a
/// no-op for the column.
#[derive(Default)]
struct FolderRows {
    /// The `AccountUiState::folders_rev` these were derived from; `None` until
    /// the first derivation, which no revision equals.
    rev: Option<u64>,
    listed: Vec<FolderRow>,
    unlisted: Vec<FolderRow>,
    /// Unread mail in the account, for its header. A folder that was never
    /// fetched counts nothing: nothing here goes to the network.
    unread: usize,
    /// True while the account has no folder at all, when the column offers
    /// INBOX so the first click is what starts its fetch.
    placeholder: bool,
}

impl FolderRows {
    fn of(mailbox: &storage::Mailbox, listed: &[String], rev: u64) -> Self {
        let mut unread_of: HashMap<&str, usize> = HashMap::new();
        for folder in &mailbox.folders {
            unread_of.insert(
                folder.name.as_str(),
                folder.messages.iter().filter(|m| !m.seen).count(),
            );
        }
        let row = |name: &str, listed: bool| {
            // `name` is the IMAP wire form (possibly modified UTF-7); decode it
            // for the label and for the icon lookup, which matches the
            // readable name.
            let display = crate::protocol::imap::decode_mailbox_name(name);
            FolderRow {
                kind: folders::kind_of(&display),
                display,
                name: name.to_string(),
                unread: unread_of.get(name).copied().unwrap_or(0),
                listed,
            }
        };

        let mut listed_names = listed.to_vec();
        crate::protocol::imap::order_mailboxes(&mut listed_names);
        let mut rows = Self {
            rev: Some(rev),
            listed: listed_names.iter().map(|name| row(name, true)).collect(),
            unlisted: Vec::new(),
            unread: unread_of.values().sum(),
            placeholder: false,
        };

        let listed: HashSet<&str> = listed.iter().map(String::as_str).collect();
        let mut unlisted_names: Vec<String> = mailbox
            .folders
            .iter()
            .filter(|folder| !listed.contains(folder.name.as_str()) && !folder.messages.is_empty())
            .map(|folder| folder.name.clone())
            .collect();
        crate::protocol::imap::order_mailboxes(&mut unlisted_names);
        rows.unlisted = unlisted_names.iter().map(|name| row(name, false)).collect();

        // Nothing to list at all: INBOX is worth offering before the first
        // sync, since clicking it is what starts the fetch.
        if rows.listed.is_empty() && rows.unlisted.is_empty() {
            rows.placeholder = true;
            rows.listed.push(row("INBOX", true));
        }
        rows
    }
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
    /// The title bar's glass: the integration reads the rect it recorded for
    /// this frame and blurs that strip of the scene (`crate::gpu`).
    pub(crate) fn glass(&self) -> &Glass {
        &self.glass
    }

    pub fn new(ctx: &egui::Context) -> Self {
        // egui's built-in fonts only cover Latin/Cyrillic/Greek, so the CJK
        // fallback goes in immediately; the families picked in Preferences are
        // applied once the worker scan below has resolved their files.
        crate::fonts::install(ctx, None, None);

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
        let app = Self::assemble(ctx, accounts, mailboxes, &settings);

        // System-tray icon (best effort; StatusNotifierItem over D-Bus).
        crate::tray::spawn(app.ctx.clone());

        // Font families for Preferences → Appearance. The scan reads every
        // installed face's name table, so it runs on a worker thread; the
        // picked families are applied when the result arrives.
        sync::spawn(Job::ScanFonts, app.ctx.clone(), app.tx.clone());

        // The one sync nobody asks for. Debug mode skips it: the cached mail
        // still loads from the database, and the title-bar button is there for
        // when the network is wanted.
        if app.working_mode.syncs_automatically() {
            for account in app.accounts.clone() {
                sync::spawn(
                    Job::Sync {
                        account,
                        purge: false,
                        folder: None,
                    },
                    app.ctx.clone(),
                    app.tx.clone(),
                );
            }
        }

        app
    }

    /// The app's state, with nothing behind it: no tray, no font scan and no
    /// startup sync. `new` is this plus those three; the probes that drive
    /// frames headlessly (`probe_splitter_drag`) build their app here.
    fn assemble(
        ctx: &egui::Context,
        accounts: Vec<Account>,
        mailboxes: HashMap<String, storage::Mailbox>,
        settings: &storage::Settings,
    ) -> Self {
        let mut states = HashMap::new();
        for account in &accounts {
            let mailbox = mailboxes.get(&account.id).cloned().unwrap_or_default();
            // The column opens on the folder list the database cached: the
            // mailboxes the last `LIST` named. The ones it left out are still
            // there — their mail is — and are drawn as folders the server no
            // longer has.
            let folders = mailbox
                .folders
                .iter()
                .filter(|folder| folder.listed)
                .map(|folder| folder.name.clone())
                .collect();
            let state = AccountUiState {
                folders,
                mailbox,
                ..AccountUiState::default()
            };
            states.insert(account.id.clone(), state);
        }

        // Folder-list icons: white PNGs (see `folders`), tinted per row.
        let folder_icons = load_folder_icons(ctx);
        let brand_icon = load_brand_icon(ctx);

        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            ctx: ctx.clone(),
            tx,
            rx,
            accounts,
            states,
            selected_account: None,
            selected_folder: None,
            selected_message: None,
            search: SearchQuery::default(),
            search_seq: 0,
            search_pending: None,
            search_applied: None,
            search_hits: Vec::new(),
            search_running: false,
            search_error: None,
            show_search: false,
            search_focus: false,
            compose: None,
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
            glass: Glass::default(),
            theme: Theme::parse(&settings.theme),
            system_dark: system_prefers_dark(),
            storage_warning: crate::db::warning(),
            export_notice: None,
            images: HashMap::new(),
            stylesheets: HashMap::new(),
            inline_textures: HashMap::new(),
            html_renderer: HtmlRenderer::parse(settings.html_renderer.as_deref().unwrap_or("")),
            working_mode: WorkingMode::from_settings(settings.working_mode.as_deref()),
            font_families: None,
            ui_font: settings.ui_font.clone(),
            mono_font: settings.mono_font.clone(),
            blitz_pages: HashMap::new(),
            blitz_order: Vec::new(),
            blitz_cancel: crate::blitz_render::Cancellation::default(),
            pending_blitz_key: None,
            body: BodyCache::default(),
            scrolled_message: None,
            list_column: egui::Rect::ZERO,
            list_scroll: 0.0,
            mail_scroll: 0.0,
        };

        app.selected_account = app.accounts.first().map(|a| a.id.clone());
        app.selected_folder = Some("INBOX".to_string());

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
                    // A `LIST` that names the mailboxes the column already
                    // shows — a warm sync — is not a change: the rows stay,
                    // and nothing is derived again.
                    if state.folders != folders {
                        state.folders = folders;
                        state.folders_changed();
                    }
                }
            }
            Event::OlderMail {
                account_id,
                folder,
                available,
            } => {
                // What the server said about older mail — the one thing the
                // list's button is drawn from. Nothing else on screen depends
                // on it (the column counts mail, not marks), so no rows are
                // derived again for it.
                if let Some(state) = self.states.get_mut(&account_id) {
                    if let Some(folder) = state
                        .mailbox
                        .folders
                        .iter_mut()
                        .find(|candidate| candidate.name == folder)
                    {
                        folder.can_load_older = available;
                    }
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
                            // Mail was just fetched for it, so the server has
                            // this mailbox, whatever the last `LIST` said: it
                            // is one of the folders the column lists rather
                            // than one kept for its cached mail.
                            listed: true,
                            // The job that fetched the mail reports what the
                            // server said about older mail in `OlderMail`,
                            // which is what sets this for real.
                            can_load_older: false,
                            messages: Vec::new(),
                        });
                    }
                    if !state.folders.iter().any(|name| name == &folder) {
                        state.folders.push(folder.clone());
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
                    // Mail was just fetched for it, so the server has this
                    // mailbox — the `stored` copy of the flag says the same
                    // thing, and the two have to agree across a restart.
                    fc.listed = true;
                    // New mail, and with it a count the column shows.
                    state.folders_changed();
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
            Event::MessageDeleted {
                account_id,
                folder,
                uid,
            } => {
                // The worker has already taken the mail off the server and
                // out of the database; what is left is this in-memory copy.
                if let Some(state) = self.states.get_mut(&account_id) {
                    if let Some(fc) = state.mailbox.folders.iter_mut().find(|f| f.name == folder) {
                        fc.messages.retain(|m| m.uid != uid);
                    }
                    // A mail fewer, and one the folder's count was made of.
                    state.folders_changed();
                }
                if self.selected_account.as_deref() == Some(account_id.as_str())
                    && self.selected_folder.as_deref() == Some(folder.as_str())
                    && self.selected_message.as_deref() == Some(uid.as_str())
                {
                    self.selected_message = None;
                }
                self.forget_blitz_message(&account_id, &folder, &uid);
                // The results are a snapshot of the database from when the
                // query ran, so they have to be asked for again for the
                // deleted mail to leave them too.
                if self.search.is_active() {
                    self.spawn_search();
                }
            }
            Event::MessageExported { path } => {
                // The file is on disk. Where it went is the one thing the app
                // has to say — and the one thing it could not have known when
                // the export was asked for, since a name that was taken is
                // numbered.
                self.export_notice = Some((format!("Saved to {}", path.display()), Instant::now()));
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
            Event::SearchResults { seq, hits } => {
                // A result for a query the user has moved on from never
                // reaches the screen.
                if seq == self.search_seq {
                    self.search_hits = hits;
                    self.search_running = false;
                    self.search_error = None;
                }
            }
            Event::SearchFailed { seq, error } => {
                if seq == self.search_seq {
                    self.search_running = false;
                    self.search_error = Some(error);
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

    /// Every account's folder list and new mail — and, for the account whose
    /// folder is open, that folder's *first* fetch when it has none. The column
    /// says "Not fetched yet — click Sync in the title bar" for exactly that
    /// case, so the button has to honour it: the sweep only revisits folders
    /// that already carry a mark.
    fn sync_all(&mut self) {
        for account in self.accounts.clone() {
            let open = match (&self.selected_account, &self.selected_folder) {
                (Some(selected), Some(folder))
                    if *selected == account.id && !self.selected_folder_synced() =>
                {
                    Some(folder.clone())
                }
                _ => None,
            };
            sync::spawn(
                Job::Sync {
                    account,
                    purge: false,
                    folder: open,
                },
                self.ctx.clone(),
                self.tx.clone(),
            );
        }
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
        // A folder nobody has fetched is fetched on the click that opens it —
        // unless the working mode keeps Kimo off the network, in which case the
        // column says "Not fetched yet" and the title-bar Sync does the fetch.
        if !needs_fetch || !self.working_mode.syncs_automatically() {
            return;
        }
        if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
            let job = match protocol {
                Protocol::Imap => Job::SyncFolder { account, folder },
                Protocol::Pop3 => Job::Sync {
                    account,
                    purge: false,
                    folder: None,
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
                    // One mail fewer to count in the folder and the account.
                    state.folders_changed();
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

    /// Export the mail the reading pane shows: a worker fetches its original
    /// source from the server and writes it into the downloads directory as a
    /// `.eml` (`Job::ExportMessage`, `storage::downloads_dir`). The file is
    /// the message whole — every header, every body part, every attachment —
    /// which is why the server is asked rather than the database read: what
    /// Kimo keeps is the parsed mail, and the attachments by name only.
    ///
    /// The round trip is the catch: mail the server no longer has cannot be
    /// exported at all, and that comes back on the account (`Event::SyncFailed`,
    /// prefixed "export:") like any other job's failure. What went right is
    /// reported as one timed line above the message list
    /// (`Event::MessageExported`).
    fn export_selected_message(&mut self) {
        // The reading pane's own lookup, like Delete's: a uid the pane no
        // longer shows is no mail to export, and the mail's subject is what
        // names the file.
        let Some(msg) = self.selected_message_view() else {
            return;
        };
        let (Some(account_id), Some(folder)) =
            (self.selected_account.clone(), self.selected_folder.clone())
        else {
            return;
        };
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() else {
            return;
        };
        sync::spawn(
            Job::ExportMessage {
                account,
                folder,
                uid: msg.uid,
                dir: storage::downloads_dir(),
                subject: msg.subject,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// Delete the mail the reading pane shows, by way of a worker thread: the
    /// server is told first and the database row goes only once it has agreed
    /// (`Job::DeleteMessage`), so mail that is still on the server is never
    /// dropped here. The row leaves the list when the deletion comes back
    /// (`Event::MessageDeleted`), which is also where the reading pane is
    /// cleared if it was showing it.
    ///
    /// Nothing asks first: the button is the confirmation, and a deletion that
    /// cannot be undone is what the tooltip says it is.
    fn delete_selected_message(&mut self) {
        // The reading pane's own question, so a uid it no longer shows is no
        // mail to delete — the same test the buttons' enabled state makes.
        if !self.has_selected_message() {
            return;
        }
        let (Some(account_id), Some(folder), Some(uid)) = (
            self.selected_account.clone(),
            self.selected_folder.clone(),
            self.selected_message.clone(),
        ) else {
            return;
        };
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() else {
            return;
        };
        sync::spawn(
            Job::DeleteMessage {
                account,
                folder,
                uid,
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
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
                folder: None,
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
        self.forget_blitz_prefix(&format!("{account_id}/"));
    }

    /// Drop the cached pages of one message. The key a page is stored under is
    /// the message's identity — account, folder and uid, each followed by a
    /// `/` (`blitz_identity`) — with the geometry after it, so the identity is
    /// the prefix.
    fn forget_blitz_message(&mut self, account_id: &str, folder: &str, uid: &str) {
        self.forget_blitz_prefix(&format!("{account_id}/{folder}/{uid}/"));
    }

    /// Drop every cached page whose key starts with `prefix`, and give up the
    /// one waiting for the geometry to settle if it is one of them.
    fn forget_blitz_prefix(&mut self, prefix: &str) {
        self.blitz_pages.retain(|key, _| !key.starts_with(prefix));
        self.blitz_order
            .retain(|key| self.blitz_pages.contains_key(key));
        if self
            .pending_blitz_key
            .as_ref()
            .is_some_and(|(key, _)| key.starts_with(prefix))
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
            working_mode: Some(self.working_mode.as_str().to_string()),
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

    /// The cached folder the message list is showing, if one is selected and
    /// its account has been loaded.
    fn current_folder(&self) -> Option<&storage::Folder> {
        let account_id = self.selected_account.as_ref()?;
        let folder = self.selected_folder.as_ref()?;
        self.states
            .get(account_id)?
            .mailbox
            .folders
            .iter()
            .find(|f| f.name == *folder)
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
        let uid = self.selected_message.as_deref()?;
        self.current_folder()?
            .messages
            .iter()
            .find(|m| m.uid == uid)
            .cloned()
    }

    /// Whether a mail is open in the reading pane — the state the title bar's
    /// mail actions are enabled by. It asks the pane's own question, so a uid
    /// left over from a folder or a search the window has moved on from (the
    /// pane shows its placeholder for it) counts as no mail, and it answers
    /// without the clone `selected_message_view` makes.
    fn has_selected_message(&self) -> bool {
        let Some(uid) = self.selected_message.as_deref() else {
            return false;
        };
        self.current_folder()
            .is_some_and(|folder| folder.messages.iter().any(|m| m.uid == uid))
    }

    // ----- UI ----------------------------------------------------------------

    /// The title bar itself, floating over the columns rather than docked
    /// above them: one `egui::Area` fixed to the window's top edge and drawn
    /// after the panels, which is what lets the message list scroll under it.
    ///
    /// While the list is at its top the bar is the solid chrome band it has
    /// always been; once the list is scrolled it turns to glass — its backdrop
    /// blurred (`crate::glass`) and its fill translucent — with a line under
    /// it to say where it ends. Both follow one animated value, so scrolling
    /// back to the top docks the bar again instead of snapping it.
    fn titlebar_area(&mut self, ctx: &egui::Context) {
        let scheme = theme::of(ctx);
        let rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(ctx.viewport_rect().width(), TITLEBAR_HEIGHT),
        );
        // Either column scrolling is the trigger: the message list's offset,
        // or the reading pane's card, which scrolls the whole mail. Any scroll
        // at all is content under the bar, and the top is the only place where
        // nothing is.
        let scrolled = self.list_scroll.max(self.mail_scroll) > 0.5;
        let floating = ctx.animate_bool_with_time(
            egui::Id::new("titlebar-floating"),
            scrolled,
            TITLEBAR_FLOAT_TIME,
        );

        egui::Area::new(egui::Id::new("titlebar"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .movable(false)
            .show(ctx, |ui| {
                let painter = ui.painter().clone();
                // The blurred backdrop goes in first: a callback runs in the
                // order the shape was added, so it has to be under the fill
                // and the glyphs that sit on the glass.
                if floating > 0.0 {
                    self.glass.backdrop(&painter, rect);
                }
                // Opaque chrome at rest, glass while floating — the scheme's
                // `panel` either way, only its alpha moves, so the bar never
                // changes color, just solidity.
                painter.rect_filled(
                    rect,
                    0.0,
                    scheme
                        .panel
                        .gamma_multiply(1.0 - floating * (1.0 - GLASS_ALPHA)),
                );
                if floating > 0.0 {
                    // The line under a floating bar: the one thing that says
                    // where the bar ends and the content continues.
                    painter.hline(
                        rect.x_range(),
                        rect.bottom() - 0.5,
                        egui::Stroke::new(1.0_f32, scheme.row_edge.gamma_multiply(floating)),
                    );
                }
                // The bar's own widgets: the `Area` hands them a Ui the size
                // of the bar, so everything in `titlebar` lays out from the
                // window's top edge exactly as it did in the panel.
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    self.titlebar(ui, floating);
                });
            });
    }

    /// Custom window title bar (the window is created undecorated in
    /// `main.rs`). Left to right: the application mark, the nav collapse
    /// toggle, the compose button, the sync button (Debug mode only), the
    /// account status, the search box, then the group of mail actions — and,
    /// at the window's own right end, the preferences cog and minimize /
    /// maximize / close. The leftover space drags the window (double-click
    /// toggles maximize).
    ///
    /// The bar is laid out as groups, not as one long row of widgets: the
    /// window's own actions at the left, the cog and the window controls at
    /// the right, the status strip against them, and the search box and the
    /// mail actions in the room between (`titlebar_rects`, `titlebar_fit`).
    /// Every group is drawn inside a rect of its own, clipped to it, and two
    /// things follow from that: a bar too narrow for what it carries gives
    /// pieces up in a stated order instead of stacking them on top of one
    /// another, and a group that somehow does not fit is cut off at its edge
    /// rather than drawn over its neighbour — and, since egui hit-tests a
    /// widget through its clip rect, the same holds for clicks.
    ///
    /// `floating` is how far the bar has gone to glass (see `titlebar_area`),
    /// which the search box follows: it is the one control in the bar with a
    /// fill of its own, and it would sit on the glass as a solid sticker.
    fn titlebar(&mut self, ui: &mut egui::Ui, floating: f32) {
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
        // Drawn right to left so Close ends up at the very right edge. This
        // list is also what `titlebar_rects` carves the bar's right end with,
        // so a control added here cannot leave the arithmetic behind.
        let buttons = [
            (Ctrl::Close, false),
            (Ctrl::Maximize, maximized),
            (Ctrl::Minimize, false),
        ];
        let rects = titlebar_rects(bar, buttons.len());
        let content = rects.content;

        // --- left group: the actions that belong to the window itself ---
        // Brand, nav toggle, compose, sync. Laid out from the bar's left edge,
        // and where it came to is what every other group is placed after.
        let left_end = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .max_rect(content)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |row| {
                    clip_to(row, content);
                    // The bar's own rhythm: these cells stand
                    // `TITLEBAR_BUTTON_GAP` apart exactly as the mail actions and
                    // the window controls do.
                    row.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
                    // Brand first: the application icon is the leftmost
                    // thing in the bar (the tray embeds the same PNG), the
                    // edge margin clear of the window's frame.
                    row.add_space(TITLEBAR_EDGE_MARGIN);
                    if let Some(icon) = self.brand_icon.clone() {
                        row.add(
                            egui::Image::from_texture(&icon)
                                .fit_to_exact_size(egui::vec2(BRAND_ICON_SIZE, BRAND_ICON_SIZE)),
                        );
                    }

                    // Nav collapse toggle (hamburger, painted as lines). An
                    // icon cell rather than an `egui::Button`, so it carries no
                    // outline: the bar's icons only fill under the pointer.
                    let toggle = titlebar_button(row, true);
                    paint_nav_icon(row.painter(), toggle.rect, visuals.text_color());
                    if toggle.clicked() {
                        self.nav_collapsed = !self.nav_collapsed;
                    }
                    toggle.on_hover_text(if self.nav_collapsed {
                        "Show folders"
                    } else {
                        "Hide folders"
                    });

                    // Compose: the one mail action that needs no mail open, so
                    // it sits beside the nav toggle rather than in the group on
                    // the right, which acts on the mail in the reading pane. It
                    // opens the compose window (empty; a window that is already
                    // open is left as it is, draft and all).
                    let compose = titlebar_button(row, true);
                    paint_compose_icon(row.painter(), compose.rect, visuals.text_color());
                    if compose.clicked() {
                        self.open_compose();
                    }
                    compose.on_hover_text("Compose a new mail");

                    // No account menu here: the navigation column lists
                    // every account and its folders, so switching accounts is
                    // picking a folder there. The title bar keeps the global
                    // actions only (add/remove accounts lives in Preferences).
                    //
                    // Sync: icon-only, like the window controls — the tooltip
                    // carries the label. Debug mode is the only one that shows
                    // it: Release fetches on its own (at startup, and when a
                    // folder is first opened), while Debug is the mode with no
                    // automatic syncing, so that is where asking for one has to
                    // be possible.
                    if self.working_mode == WorkingMode::Debug {
                        let sync = titlebar_button(row, true);
                        paint_sync_icon(row.painter(), sync.rect, visuals.text_color());
                        if sync.clicked() {
                            self.sync_all();
                        }
                        sync.on_hover_text(
                            "Sync all accounts — Debug mode does not sync on its own. The \
                             folder you have open is fetched too, if it was never fetched.",
                        );
                    }

                    row.min_rect().right()
                },
            )
            .inner;

        // --- account status: right-aligned against the cog ---
        // The spinner of a syncing account, the warning of one whose last sync
        // failed. Held to the right end rather than run after the actions, so
        // that one appearing never moves the search box — and so the strip's
        // own width is a known thing to fit the middle group around.
        let status = egui::Rect::from_min_max(
            egui::pos2(left_end, bar.top()),
            egui::pos2(rects.cog.min.x.max(left_end), bar.bottom()),
        );
        let status_left = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .max_rect(status)
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
                |row| {
                    clip_to(row, status);
                    let mut left = status.right();
                    for account in &self.accounts {
                        let Some(state) = self.states.get(&account.id) else {
                            continue;
                        };
                        let response = if state.syncing {
                            row.add(egui::Spinner::new().color(scheme.accent))
                                .on_hover_text(format!("{}: syncing…", account.name))
                        } else if let Some(err) = &state.error {
                            row.label(egui::RichText::new("⚠").color(scheme.danger))
                                .on_hover_text(format!("{}: {err}", account.name))
                        } else {
                            continue;
                        };
                        left = left.min(response.rect.left());
                    }
                    left
                },
            )
            .inner;
        // Where the middle group has to stop: the status strip's own left
        // edge, with the item spacing between the two so they are not one
        // thing — or the cog, when there is no status to show.
        let middle_right = (status_left
            - if status_left < status.right() {
                ui.spacing().item_spacing.x
            } else {
                0.0
            })
        .max(left_end);

        // --- middle group: the search box and the mail actions ---
        // What the room between the left group and the status can hold. The
        // bar gives that room up in one order (`titlebar_fit`): the clear
        // chrome in front of the actions, then the actions themselves as the
        // one group they are, and only then the box's own width.
        //
        // The run is `MailAction::ALL.len()` of the bar's own cells, which is
        // what the fit has to find room for.
        let actions_width = titlebar_cells_width(MailAction::ALL.len());
        let mut middle_end = left_end;
        if middle_right > left_end {
            let middle = egui::Rect::from_min_max(
                egui::pos2(left_end, bar.top()),
                egui::pos2(middle_right, bar.bottom()),
            );
            let fit = titlebar_fit(middle.width(), actions_width);
            if let Some(box_width) = fit.search {
                // Search, as one box: the query with the magnifier inside
                // its left edge, and the whole box is one click target —
                // the glyph and the text open the same popup, which is
                // where a query is typed (the scope, the filters and the
                // count live there too). The field is non-interactive for
                // that reason: a caret here would only fight the popup's
                // own field for the keyboard, and would leave focus behind
                // when the popup closes. The glyph takes the accent while a
                // search is what the message list shows.
                //
                // The box's frame is stock `TextEdit` chrome, so its
                // corner radius is the widget visuals' — rounded here in a
                // scope, which leaves every other field in the app with
                // egui's tighter default. The field is deliberately
                // non-interactive and egui picks the visuals by `Sense`
                // rather than by hover, so `noninteractive` is the slot it
                // actually draws with; the rest are rounded as well, so that
                // making the field live again would not square the corners.
                //
                // Its fill goes to glass with the bar: the fill keeps only
                // part of the input color, so the blurred mail behind the
                // bar shows through the box the way it shows through the
                // bar itself. The stock frame is drawn from
                // `Visuals::text_edit_bg_color`, which is why this is
                // `background_color` and not another widget slot.
                let box_rect = ui
                    .scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(middle)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |row| {
                            clip_to(row, middle);
                            let margin = SEARCH_BOX_MARGIN;
                            let field = row
                                .scope(|ui| {
                                    let radius = egui::CornerRadius::same(SEARCH_BOX_CORNER_RADIUS);
                                    let widgets = &mut ui.style_mut().visuals.widgets;
                                    for widget in [
                                        &mut widgets.noninteractive,
                                        &mut widgets.inactive,
                                        &mut widgets.hovered,
                                        &mut widgets.active,
                                        &mut widgets.open,
                                    ] {
                                        widget.corner_radius = radius;
                                    }
                                    let fill = scheme
                                        .input
                                        .gamma_multiply(1.0 - floating * (1.0 - GLASS_FIELD_ALPHA));
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.search.text)
                                            .hint_text("Search mail…")
                                            .interactive(false)
                                            .background_color(fill)
                                            // `desired_width` is the whole
                                            // box — the margins are taken out
                                            // of it, not added to it — which is
                                            // the one thing the fit and the
                                            // widget have to agree on.
                                            .desired_width(box_width)
                                            // The bar is taller than a text row, so the
                                            // field is grown to a fixed height and the
                                            // query (or the hint) is centred in it.
                                            .min_size(egui::vec2(0.0, TITLEBAR_SEARCH_HEIGHT))
                                            .vertical_align(egui::Align::Center)
                                            .margin(margin),
                                    )
                                })
                                .inner;
                            // The widget's rect *is* the box: the whole frame,
                            // margins included, which is the rect `desired_width`
                            // sized and what the magnifier and the click target
                            // are placed from. (`TextEdit` used to hand back the
                            // inner text rect, which is where the margins came
                            // off it; the 0.36 rewrite sizes the frame instead,
                            // and re-adding them put the magnifier outside the
                            // box and the click target over the compose cell.)
                            let box_rect = field.rect;
                            // Registered after the field, which is why the click is
                            // the box's: it opens the popup wherever in the box it
                            // lands, the magnifier included.
                            let search = row
                                .interact(
                                    box_rect,
                                    egui::Id::new("titlebar-search"),
                                    egui::Sense::click(),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if search.clicked() {
                                self.open_search();
                            }
                            // Grey while nothing is being asked for — the magnifier
                            // then matches the field's own hint text instead of
                            // reading as a live control — and the accent while a query
                            // is what the message list shows.
                            let search_color = if self.search.is_active() {
                                scheme.accent
                            } else {
                                visuals.weak_text_color()
                            };
                            let icon_rect = egui::Rect::from_center_size(
                                egui::pos2(
                                    box_rect.left() + SEARCH_ICON_INSET as f32 / 2.0,
                                    box_rect.center().y,
                                ),
                                egui::vec2(SEARCH_ICON_SIZE, SEARCH_ICON_SIZE),
                            );
                            paint_search_icon(row.painter(), icon_rect, search_color);
                            search.on_hover_text("Search mail — filters and scope (Ctrl+F)");
                            box_rect
                        },
                    )
                    .inner;
                middle_end = box_rect.right();

                // Mail actions, set well apart to the right of the search
                // box: they act on the mail open in the reading pane, so the
                // group is enabled by whether one is open there. They are
                // placed at an exact x rather than run after the box — the gap
                // in front of them is the fit's, not the row's item spacing —
                // and from where the box actually came to rather than from
                // where the arithmetic above expected it to end: the widget's
                // own width is the truth, so a box wider than it was asked for
                // pushes the group right instead of being drawn over. If the
                // room then runs out, the group is simply not drawn: the bar's
                // promise is that it fits what it draws, never that it draws
                // everything.
                let actions_left = box_rect.right() + fit.padding;
                if fit.actions && actions_left + actions_width <= middle_right + 0.5 {
                    let actions = egui::Rect::from_min_max(
                        egui::pos2(actions_left, bar.top()),
                        egui::pos2(middle_right, bar.bottom()),
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(actions)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |row| {
                            clip_to(row, actions);
                            self.titlebar_mail_actions(row, &visuals);
                        },
                    );
                    middle_end = actions_left + actions_width;
                }
            }
        }

        // --- drag surface: whatever is left of the bar ---
        // The window is undecorated, so this is what moves it (and what
        // double-click maximizes it). A bar filled edge to edge by its groups
        // simply has none — the window controls have the window's right end
        // either way.
        let drag_rect = egui::Rect::from_min_max(
            egui::pos2((middle_end + 4.0).max(bar.left()), bar.top()),
            egui::pos2(middle_right.max(middle_end), bar.bottom()),
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

        // --- right group: the cog and the window controls ---
        // Fixed, and drawn last: they are what the rest of the bar is laid out
        // around, and the one thing a bar of any width keeps. The group's rect
        // starts at the cog, so the clip is also what says which half of the
        // bar a shape belongs to (see the no-overlap test in `tests`).
        let right = egui::Rect::from_min_max(egui::pos2(rects.cog.left(), bar.top()), bar.max);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |row| {
                clip_to(row, right);
                let cog_resp = titlebar_hover(
                    row,
                    row.interact(
                        rects.cog,
                        egui::Id::new("titlebar-preferences"),
                        egui::Sense::click(),
                    ),
                );
                paint_cog_icon(row.painter(), rects.cog, visuals.text_color());
                if cog_resp.clicked() {
                    self.show_preferences = true;
                }
                cog_resp.on_hover_text("Preferences");

                for (idx, (ctrl, is_max)) in buttons.iter().enumerate() {
                    // The list reads from the window's right edge inwards, which
                    // is where the cells are placed from: `buttons[0]` is the
                    // close button, and the run ends
                    // `TITLEBAR_EDGE_MARGIN` short of the window's corner.
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(
                            rects.controls.right()
                                - TITLEBAR_BUTTON_SIZE
                                - idx as f32 * (TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP),
                            rects.controls.top(),
                        ),
                        egui::Vec2::splat(TITLEBAR_BUTTON_SIZE),
                    );
                    let resp = row.interact(
                        rect,
                        egui::Id::new(("titlebar-button", idx)),
                        egui::Sense::click(),
                    );
                    let fg = if resp.hovered() {
                        let bg = match ctrl {
                            Ctrl::Close => scheme.danger_hover,
                            _ => visuals.widgets.hovered.bg_fill,
                        };
                        titlebar_fill(row, rect, bg);
                        match ctrl {
                            // The scheme's on-danger color reads best on the
                            // close red; elsewhere keep the theme text color so
                            // the glyph survives light visuals.
                            Ctrl::Close => scheme.on_danger,
                            _ => visuals.text_color(),
                        }
                    } else {
                        visuals.text_color()
                    };
                    match (ctrl, is_max) {
                        (Ctrl::Minimize, _) => paint_minimize_icon(row.painter(), rect, fg),
                        (Ctrl::Maximize, false) => paint_maximize_icon(row.painter(), rect, fg),
                        (Ctrl::Maximize, true) => {
                            paint_restore_icon(row.painter(), rect, fg, visuals.panel_fill)
                        }
                        (Ctrl::Close, _) => paint_close_icon(row.painter(), rect, fg),
                    }
                    if resp.clicked() {
                        match ctrl {
                            Ctrl::Minimize => row
                                .ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                            Ctrl::Maximize => row
                                .ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                            Ctrl::Close => {
                                row.ctx().send_viewport_cmd(egui::ViewportCommand::Close)
                            }
                        }
                    }
                }
            },
        );
    }

    /// The mail actions the title bar carries — reply, reply all, forward,
    /// label, flag, export, delete — in one run to the right of the search box.
    /// They act on the mail open in the reading pane, so that is what enables
    /// them (`has_selected_message`): with no mail there the seven glyphs are
    /// dimmed, the buttons stop sensing clicks and nothing fills under the
    /// pointer.
    ///
    /// Every cell is one of the bar's buttons — `TITLEBAR_BUTTON_SIZE` square,
    /// `TITLEBAR_BUTTON_GAP` from the next one, empty at rest and filled under
    /// the pointer with a small rounded corner (`titlebar_button`) — which is
    /// what keeps the seven of them reading as one group rather than as seven
    /// framed widgets. They are the same cells the nav toggle, the compose and
    /// sync buttons, the cog and the window controls are; what makes this run a
    /// group of its own is the clear chrome in front of it
    /// (`MAIL_ACTION_PADDING`), not a size or a fill only it has.
    ///
    /// Export and Delete are wired: Export hands the mail to
    /// `export_selected_message`, which has a worker write the server's own
    /// copy of it out as a `.eml`, and Delete hands it to
    /// `delete_selected_message`, which takes it off the server before it
    /// leaves the database. The other four are still drawn only — a click on
    /// one runs nothing, so they stay UI until the actions they name exist.
    fn titlebar_mail_actions(&mut self, row: &mut egui::Ui, visuals: &egui::Visuals) {
        // The run lays itself out: `TITLEBAR_BUTTON_GAP` between the cells, which
        // is the gap `titlebar_cells_width` — and through it the fit — assumes.
        row.spacing_mut().item_spacing.x = TITLEBAR_BUTTON_GAP;
        let enabled = self.has_selected_message();
        for action in MailAction::ALL {
            let response = titlebar_button(row, enabled);
            let color = if enabled {
                visuals.text_color()
            } else {
                visuals.weak_text_color()
            };
            let painter = row.painter();
            match action {
                MailAction::Reply => paint_reply_icon(painter, response.rect, color),
                MailAction::ReplyAll => paint_reply_all_icon(painter, response.rect, color),
                MailAction::Forward => paint_forward_icon(painter, response.rect, color),
                MailAction::Label => paint_label_icon(painter, response.rect, color),
                MailAction::Flag => paint_flag_icon(painter, response.rect, color),
                MailAction::Export => paint_export_icon(painter, response.rect, color),
                MailAction::Delete => paint_delete_icon(painter, response.rect, color),
            }
            let response = response.on_hover_text(if enabled {
                action.tooltip().to_string()
            } else {
                format!("{} — no mail selected", action.label())
            });
            if enabled && response.clicked() {
                match action {
                    MailAction::Export => self.export_selected_message(),
                    MailAction::Delete => self.delete_selected_message(),
                    MailAction::Reply
                    | MailAction::ReplyAll
                    | MailAction::Forward
                    | MailAction::Label
                    | MailAction::Flag => {}
                }
            }
        }
    }

    /// Left column: every account as a collapsible section of its folders.
    /// The selection can be a folder of any of them — switching accounts is
    /// picking a folder here, so the title bar needs no account menu.
    fn nav(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // Folders are indented under their account, but egui's `indent` also
        // marks the indented block with a faint guide line down its left,
        // which in this column reads as a rule drawn through the folder list.
        // The HTML renderer's lists, the only other users of `indent`, keep
        // theirs.
        ui.visuals_mut().indent_has_left_vline = false;
        // The title bar floats over the columns, so this one's content starts
        // below it: what the glass shows here is the panel fill, not folders.
        ui.add_space(TITLEBAR_HEIGHT);
        if self.accounts.is_empty() {
            ui.label("No accounts yet.");
            ui.label(
                egui::RichText::new("Add one in Preferences (the cog in the title bar).")
                    .small()
                    .weak(),
            );
            return;
        }
        // What a section needs of an account: its id and name for the header,
        // its protocol for a folder click. Not the account itself — the
        // password in it has no business being copied once a frame.
        let accounts: Vec<(String, String, Protocol)> = self
            .accounts
            .iter()
            .map(|account| (account.id.clone(), account.name.clone(), account.protocol))
            .collect();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(2.0);
                for (id, name, protocol) in &accounts {
                    self.account_section(ui, scheme, id, name, *protocol);
                }
            });
    }

    /// One account in the navigation column: a header that collapses it, then
    /// its folders, indented under it.
    fn account_section(
        &mut self,
        ui: &mut egui::Ui,
        scheme: ColorScheme,
        account_id: &str,
        account_name: &str,
        protocol: Protocol,
    ) {
        let collapsed = self.collapsed_accounts.contains(account_id);
        // The rows are derived here and only when the account state changed, so
        // the header's count comes from them too — they are where the counts
        // are computed at all.
        let (unread, placeholder, syncing, error) = {
            let state = self.states.entry(account_id.to_string()).or_default();
            let nav = state.nav();
            let (unread, placeholder) = (nav.unread, nav.placeholder);
            (unread, placeholder, state.syncing, state.error.clone())
        };
        if account_row(ui, scheme, account_name, unread, collapsed).clicked() {
            if collapsed {
                self.collapsed_accounts.remove(account_id);
            } else {
                self.collapsed_accounts.insert(account_id.to_string());
            }
            return;
        }
        if collapsed {
            ui.add_space(2.0);
            return;
        }

        // The rows are borrowed from the account state while they are drawn, so
        // a click is remembered as the folder it landed on and opened once the
        // drawing has stopped borrowing — the same shape the message list uses.
        let selected = if self.selected_account.as_deref() == Some(account_id) {
            self.selected_folder.as_deref()
        } else {
            None
        };
        let icons = &self.folder_icons;
        let mut clicked: Option<String> = None;
        {
            let state = self.states.entry(account_id.to_string()).or_default();
            let nav = state.nav();
            ui.indent(("account-folders", account_id), |ui| {
                for row in nav.listed.iter().chain(nav.unlisted.iter()) {
                    let is_selected = selected == Some(row.name.as_str());
                    // Only a generic folder has an open variant; the special
                    // mailboxes keep one glyph.
                    let open = is_selected && row.kind == FolderKind::Folder;
                    let icon = icons.get(&(row.kind, open));
                    let response = folder_row(
                        ui,
                        scheme,
                        icon,
                        &row.display,
                        row.unread,
                        is_selected,
                        row.listed,
                    );
                    if response.clicked() {
                        clicked = Some(row.name.clone());
                    }
                }
            });
        }
        if let Some(folder) = clicked {
            self.select_folder(account_id.to_string(), folder, protocol);
        }
        if syncing && placeholder {
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

    /// Middle column: the search strip, then the search results or the selected
    /// folder's mail — the lighter content surface, framed by a hairline down
    /// each side.
    fn mail_list(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // The column's own rect is the whole ui the frame hands over — the
        // padding moved inside the scroll area. The rows' rules span it (see
        // message_card), and a hairline down each side frames it.
        let column = ui.max_rect();
        self.list_column = column;
        // The mails' own left and right edges, drawn down the whole column so
        // that it stays framed above and below the rows too: the chrome is the
        // step outside the column and the card's gutter the step outside that,
        // and this line is what keeps either from touching the rows. Every row
        // paints it again over its own fill (see message_card).
        let stroke = egui::Stroke::new(1.0_f32, scheme.row_edge);
        ui.painter()
            .vline(column.left() + 0.5, column.y_range(), stroke);
        ui.painter()
            .vline(column.right() - 0.5, column.y_range(), stroke);
        // Two things can stand where the first row would: the database could
        // not be opened (the session works, nothing is saved, and this is the
        // one place that says so), and an export that has just finished (its
        // file went somewhere the user was not watching). The second follows
        // the first rather than clearing the bar's own height again.
        let mut banner_top = TITLEBAR_HEIGHT_I8 + LIST_MARGIN;
        if let Some(warning) = &self.storage_warning {
            list_banner(
                ui,
                banner_top,
                &format!("Mail is not being saved: {warning}"),
                Some(theme::of(ui.ctx()).danger),
            );
            banner_top = LIST_MARGIN;
        }
        // The notice is timed: it answers a click and has said what it has to
        // say once it has been read. The repaint request is what takes it away
        // when nothing else is drawing a frame.
        if self
            .export_notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= EXPORT_NOTICE_TIME)
        {
            self.export_notice = None;
        } else if let Some((_, at)) = &self.export_notice {
            ui.ctx()
                .request_repaint_after(EXPORT_NOTICE_TIME - at.elapsed());
        }
        if let Some((notice, _)) = &self.export_notice {
            list_banner(ui, banner_top, notice, None);
        }
        if self.accounts.is_empty() || self.selected_account.is_none() {
            // Nothing to scroll, so the bar is docked: whatever the last list
            // left behind is not this frame's offset.
            self.list_scroll = 0.0;
            ui.centered_and_justified(|ui| {
                ui.heading("Kimo");
            });
            return;
        }

        // No search chrome in the column: the query, its count and the way out
        // of it are the popup's (the title-bar magnifier opens it, Clear or an
        // emptied box ends the search). Running the search — and the settle
        // clock — is the frame's business, since the popup can be open over
        // any of this.
        if self.search.is_active() {
            self.search_results(ui);
        } else {
            self.folder_messages(ui);
        }
    }

    /// Open the search popup with the keyboard already in its text field.
    fn open_search(&mut self) {
        self.show_search = true;
        self.search_focus = true;
    }

    /// Open the compose window on an empty draft. A window that is already open
    /// is left alone: the button is not a reset, and the draft in it is the
    /// only copy of what has been typed.
    fn open_compose(&mut self) {
        if self.compose.is_none() {
            self.compose = Some(ComposeDraft::new(compose_from(
                &self.accounts,
                self.selected_account.as_deref(),
            )));
        }
    }

    /// The search popup, opened from the title-bar magnifier or Ctrl+F: the
    /// free text, the scope and every filter, which used to be a bar above the
    /// message list. Searching stays live while it is open — the count under
    /// the fields follows the query — and Enter (or Search) applies it at once
    /// and closes, leaving the results on screen. Returns true when the query
    /// should run without waiting for the settle clock.
    fn search_modal(&mut self, ctx: &egui::Context) -> bool {
        let mut open = self.show_search;
        let mut run_now = false;
        // Read before the closure, which edits the query: mixing that with a
        // borrow of the account list is more trouble than copying strings.
        let accounts: Vec<(String, String)> = self
            .accounts
            .iter()
            .map(|account| (account.id.clone(), account.name.clone()))
            .collect();
        let folder_scope = self
            .selected_account
            .clone()
            .zip(self.selected_folder.clone());

        let modal = egui::Modal::new(egui::Id::new("search")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.horizontal(|ui| {
                ui.heading("Search");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        open = false;
                    }
                });
            });
            ui.separator();

            let text = ui.add(
                egui::TextEdit::singleline(&mut self.search.text)
                    .hint_text("Search all mail…")
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.search_focus) {
                // Just opened: type straight away, no click needed.
                text.request_focus();
            }
            if entered(&text, ui) {
                run_now = true;
                open = false;
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Search in");
                let label = match &self.search.scope {
                    Scope::AllMail => "All mail".to_string(),
                    Scope::Account(id) => accounts
                        .iter()
                        .find(|(account_id, _)| account_id == id)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_else(|| "Account".to_string()),
                    Scope::Folder { .. } => "This folder".to_string(),
                };
                egui::ComboBox::from_id_salt("search-scope")
                    .selected_text(label)
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.search.scope, Scope::AllMail, "All mail");
                        for (id, name) in &accounts {
                            ui.selectable_value(
                                &mut self.search.scope,
                                Scope::Account(id.clone()),
                                name,
                            );
                        }
                        if let Some((account_id, folder)) = &folder_scope {
                            ui.selectable_value(
                                &mut self.search.scope,
                                Scope::Folder {
                                    account_id: account_id.clone(),
                                    folder: folder.clone(),
                                },
                                "This folder",
                            );
                        }
                    });
            });

            ui.add_space(4.0);
            ui.separator();
            egui::Grid::new("search-filters")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    // Enter applies the query from any box, not just the text
                    // field; a checkbox applies it through the settle clock.
                    if filter_row(ui, "From", &mut self.search.from, "name or address") {
                        run_now = true;
                        open = false;
                    }
                    if filter_row(ui, "To", &mut self.search.to, "name or address") {
                        run_now = true;
                        open = false;
                    }
                    if filter_row(
                        ui,
                        "Subject",
                        &mut self.search.subject,
                        "words in the subject",
                    ) {
                        run_now = true;
                        open = false;
                    }
                    if filter_row(ui, "After", &mut self.search.after, "YYYY-MM-DD") {
                        run_now = true;
                        open = false;
                    }
                    if filter_row(ui, "Before", &mut self.search.before, "YYYY-MM-DD") {
                        run_now = true;
                        open = false;
                    }
                    ui.label("Only");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.search.unread_only, "Unread");
                        ui.checkbox(&mut self.search.has_attachment, "Has attachment");
                    });
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                if self.search_running {
                    ui.spinner();
                    ui.label(egui::RichText::new("Searching…").small().weak());
                } else if self.search.is_active() {
                    ui.label(
                        egui::RichText::new(results_label(self.search_hits.len()))
                            .small()
                            .weak(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Search").clicked() {
                        run_now = true;
                        open = false;
                    }
                    if self.search.is_active() && ui.button("Clear").clicked() {
                        // Empty the boxes but stay open: clearing one query to
                        // type the next is the common case, and the list behind
                        // returns to the folders either way.
                        self.search = SearchQuery::default();
                        self.search_focus = true;
                        run_now = true;
                    }
                });
            });
        });
        // Backdrop click or Escape closes the popup. What was typed stays
        // applied: the search is live, so closing is not a cancel.
        if modal.should_close() {
            open = false;
        }
        self.show_search = open;
        run_now
    }

    /// The compose window, opened from the title bar: an `egui::Modal`, so the
    /// columns behind it are covered by a backdrop and cannot be used while a
    /// mail is being written.
    ///
    /// It edits a draft and nothing else. Kimo speaks IMAP and POP3, with no
    /// SMTP, so Send is drawn disabled and says why; the draft lives in this
    /// card, and Discard drops it.
    ///
    /// The backdrop click and Escape deliberately do not close it: the draft is
    /// the only copy of the mail, and a click beside the card must not be what
    /// throws it away, so `ModalResponse::should_close` is ignored and Discard
    /// is the way out.
    fn compose_window(&mut self, ctx: &egui::Context) {
        // Taken out for the closure, which edits the draft while reading the
        // account list, and put back unless the draft was discarded.
        let Some(mut draft) = self.compose.take() else {
            return;
        };
        let accounts: Vec<(String, String, String)> = self
            .accounts
            .iter()
            .map(|account| {
                (
                    account.id.clone(),
                    account.name.clone(),
                    account.username.clone(),
                )
            })
            .collect();

        let mut discard = false;
        let _ = egui::Modal::new(egui::Id::new("compose")).show(ctx, |ui| {
            ui.set_width(COMPOSE_WIDTH);
            ui.heading("New mail");
            ui.separator();

            // The card is a fixed width, so the boxes get a constant: what the
            // label column and the gap beside it leave.
            let field_width = COMPOSE_WIDTH - COMPOSE_LABEL_WIDTH - ui.spacing().item_spacing.x;
            let field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                        egui::Label::new(label).halign(egui::Align::RIGHT),
                    );
                    ui.add(
                        egui::TextEdit::singleline(value)
                            .hint_text(hint)
                            .desired_width(field_width),
                    )
                })
                .inner
            };

            let from_label = draft
                .from
                .as_ref()
                .and_then(|id| accounts.iter().find(|(account_id, ..)| account_id == id))
                .map(|(_, name, address)| format!("{name} <{address}>"));
            ui.horizontal(|ui| {
                ui.add_sized(
                    [COMPOSE_LABEL_WIDTH, COMPOSE_FIELD_HEIGHT],
                    egui::Label::new("From").halign(egui::Align::RIGHT),
                );
                match (accounts.is_empty(), from_label) {
                    (true, _) => {
                        ui.label(
                            egui::RichText::new(
                                "No account yet — add one in Preferences → Accounts.",
                            )
                            .weak(),
                        );
                    }
                    (false, Some(label)) => {
                        egui::ComboBox::from_id_salt("compose-from")
                            .selected_text(label)
                            .width(field_width)
                            .show_ui(ui, |ui| {
                                for (id, name, address) in &accounts {
                                    ui.selectable_value(
                                        &mut draft.from,
                                        Some(id.clone()),
                                        format!("{name} <{address}>"),
                                    );
                                }
                            });
                    }
                    // Only reachable if the From account vanished between
                    // two frames; pick the fallback rather than showing a
                    // list with nothing selected.
                    (false, None) => {
                        draft.from = compose_from(&self.accounts, self.selected_account.as_deref());
                    }
                }
            });

            let to = field(ui, "To", &mut draft.to, "name@example.com");
            if std::mem::take(&mut draft.focus) {
                to.request_focus();
            }
            field(ui, "Cc", &mut draft.cc, "name@example.com");
            field(ui, "Subject", &mut draft.subject, "Subject");

            ui.add_space(6.0);
            // The body is a fixed height and scrolls on its own once the
            // mail outgrows it: the card never grows with the draft.
            egui::ScrollArea::vertical()
                .id_salt("compose-body")
                .max_height(COMPOSE_BODY_HEIGHT)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut draft.body)
                            .hint_text("Write your message…")
                            .desired_width(field_width)
                            .desired_rows(12),
                    );
                });

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Kimo cannot send mail: it speaks IMAP and POP3 only, with no SMTP.",
                    )
                    .small()
                    .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled(false, egui::Button::new("Send"))
                        .on_disabled_hover_text("Sending is not implemented — Kimo has no SMTP.");
                    if ui.button("Discard").clicked() {
                        discard = true;
                    }
                });
            });
        });

        // Discarded: the draft goes with the card. There is no other copy of it
        // and nowhere it could have been saved to.
        if !discard {
            self.compose = Some(draft);
        }
    }

    /// The middle column while no search is active: the selected folder's mail.
    ///
    /// The rows are drawn straight out of the account state — mail bodies are
    /// large, so the list never copies them — and only the ones the viewport
    /// shows are laid out at all (see `virtual_rows`). A click is remembered as
    /// the mail it landed on and opened after the list has been drawn, when the
    /// borrow of the state it came from is over.
    fn folder_messages(&mut self, ui: &mut egui::Ui) {
        // A folder the account does not have yet — INBOX before the first
        // sync, which the navigation offers because clicking it is what starts
        // the fetch — is an empty one, and gets the same placeholder.
        let messages = self
            .selected_account
            .as_deref()
            .zip(self.selected_folder.as_deref())
            .and_then(|(account_id, folder)| self.folder_messages_of(account_id, folder))
            .unwrap_or(&[]);
        if messages.is_empty() {
            self.list_scroll = 0.0;
            ui.centered_and_justified(|ui| {
                let text = if self.selected_folder_synced() {
                    "This folder is empty."
                } else {
                    "Not fetched yet — click Sync in the title bar."
                };
                ui.label(egui::RichText::new(text).weak().heading());
            });
            // Nothing to scroll: whatever the folder holds, this *is* the
            // bottom of the list.
            self.load_older_button(ui);
            return;
        }
        let mut clicked: Option<MailMessage> = None;
        let list = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                egui::Frame::new()
                    .inner_margin(LIST_PADDING)
                    .show(ui, |ui| {
                        // No gap between rows: the two pixels that separate two
                        // mails are the rows' own edges, so the layout must not
                        // add space of its own between them.
                        ui.spacing_mut().item_spacing.y = 0.0;
                        virtual_rows(
                            ui,
                            viewport,
                            LIST_PADDING.top as f32,
                            MESSAGE_ROW_HEIGHT,
                            messages.len(),
                            |ui, rows| {
                                for index in rows {
                                    if self.message_row(ui, &messages[index], index == 0).clicked()
                                    {
                                        clicked = Some(messages[index].clone());
                                    }
                                }
                            },
                        );
                    });
            });
        // Scrolled away from the top, the bar floats over this list.
        self.list_scroll = list.state.offset.y;
        // The bottom of the list is the one place where "older" is what the eye
        // is on, so it is where the button belongs. A list shorter than its
        // viewport is at its bottom by definition.
        let end = (list.content_size.y - list.inner_rect.height()).max(0.0);
        if list.state.offset.y >= end - 1.0 {
            self.load_older_button(ui);
        }
        if let Some(msg) = clicked {
            self.open_message(&msg);
        }
    }

    /// `Load older mail`, floating over the bottom of the message list: the
    /// folder's marks say the server has mail below everything this machine
    /// holds, and the list has nothing left below the fold. A click hands the
    /// folder to `Job::LoadOlder`, which asks the server how far down it goes
    /// before it fetches anything — so a click that finds nothing clears the
    /// button instead of failing.
    ///
    /// Drawn after the rows, in the column's own rect, so it reads as floating
    /// over them rather than as the last row: it belongs to the foot of the
    /// column, and scrolling never carries it away. While the account is busy
    /// with the server the button says so and stops taking clicks — the
    /// title bar's spinner is the other half of that.
    fn load_older_button(&mut self, ui: &mut egui::Ui) {
        let (Some(account_id), Some(folder)) = (&self.selected_account, &self.selected_folder)
        else {
            return;
        };
        let (account_id, folder) = (account_id.clone(), folder.clone());
        let Some(state) = self.states.get(&account_id) else {
            return;
        };
        let available = state
            .mailbox
            .folders
            .iter()
            .find(|candidate| candidate.name == folder)
            .is_some_and(|folder| folder.can_load_older);
        if !available {
            return;
        }
        let busy = state.syncing;

        let scheme = theme::of(ui.ctx());
        let column = ui.max_rect();
        let size = egui::vec2(LOAD_OLDER_WIDTH, LOAD_OLDER_HEIGHT);
        let rect = egui::Rect::from_center_size(
            egui::pos2(
                column.center().x,
                column.bottom() - LOAD_OLDER_MARGIN - size.y / 2.0,
            ),
            size,
        );
        let label = if busy {
            "Loading…"
        } else {
            "Load older mail"
        };
        let clicked = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::centered_and_justified(
                        egui::Direction::LeftToRight,
                    )),
                |ui| {
                    // A shadow, so the button reads as floating over the rows
                    // rather than as a row of its own.
                    egui::Frame::new()
                        .shadow(ui.visuals().window_shadow)
                        .show(ui, |ui| {
                            ui.add_enabled(
                                !busy,
                                egui::Button::new(label)
                                    .min_size(size)
                                    .fill(scheme.input)
                                    .corner_radius(LOAD_OLDER_CORNER_RADIUS),
                            )
                        })
                        .inner
                },
            )
            .inner;
        let clicked = clicked.on_hover_text(format!(
            "There is older mail on the server — fetch the next {} messages",
            sync::OLDER_BATCH
        ));
        if clicked.clicked() {
            if let Some(account) = self.accounts.iter().find(|a| a.id == account_id).cloned() {
                sync::spawn(
                    Job::LoadOlder { account, folder },
                    self.ctx.clone(),
                    self.tx.clone(),
                );
            }
        }
    }

    /// The mail of one folder of one account, for as long as the caller keeps
    /// the borrow. Nothing is copied: the list and the reading pane both read
    /// the state as it is.
    fn folder_messages_of(&self, account_id: &str, folder: &str) -> Option<&[MailMessage]> {
        self.states
            .get(account_id)?
            .mailbox
            .folders
            .iter()
            .find(|candidate| candidate.name == folder)
            .map(|folder| folder.messages.as_slice())
    }

    /// The middle column while a search is active: one card per hit.
    fn search_results(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.search_error {
            ui.label(
                egui::RichText::new(error.as_str())
                    .small()
                    .color(theme::of(ui.ctx()).danger),
            );
        }
        if self.search_hits.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if self.search_running {
                    "Searching…"
                } else {
                    "No mail matches this search."
                };
                ui.label(egui::RichText::new(text).weak().heading());
            });
            return;
        }
        // A hit is what a click landed on, opened once the list has been drawn:
        // `open_hit` walks the account state, which the rows are borrowing.
        let mut clicked: Option<SearchHit> = None;
        let hits = &self.search_hits;
        let list = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                egui::Frame::new()
                    .inner_margin(LIST_PADDING)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        virtual_rows(
                            ui,
                            viewport,
                            LIST_PADDING.top as f32,
                            MESSAGE_ROW_HEIGHT,
                            hits.len(),
                            |ui, rows| {
                                for index in rows {
                                    if self.search_row(ui, &hits[index], index == 0).clicked() {
                                        clicked = Some(hits[index].clone());
                                    }
                                }
                            },
                        );
                    });
            });
        // A scrolled result list floats the bar exactly like the folder's does.
        self.list_scroll = list.state.offset.y;
        if let Some(hit) = clicked {
            self.open_hit(&hit);
        }
    }

    /// Ask for what the search popup is showing, once the query has held still;
    /// `force` is Enter (or Search/Clear), which skips the wait.
    fn pump_search(&mut self, force: bool) {
        if force {
            self.search_pending = None;
            self.spawn_search();
            return;
        }
        if self.search_applied.as_ref() == Some(&self.search) {
            return; // the query has not changed since the last request
        }
        let now = Instant::now();
        let pending = self
            .search_pending
            .as_ref()
            .map(|(query, since)| (query, *since));
        match search_settle(pending, &self.search, now) {
            SearchWait::Wait => {}
            SearchWait::Reset => {
                if self.search.is_active() {
                    self.search_pending = Some((self.search.clone(), now));
                } else {
                    // An emptied bar goes back to the folder view at once.
                    self.search_pending = None;
                    self.spawn_search();
                }
            }
            SearchWait::Run => {
                self.search_pending = None;
                self.spawn_search();
            }
        }
    }

    /// Send the current query to a worker. `search_seq` makes the answer
    /// identifiable, so a result for a query the user has moved on from is
    /// dropped instead of shown.
    fn spawn_search(&mut self) {
        self.search_seq += 1;
        self.search_applied = Some(self.search.clone());
        if !self.search.is_active() {
            self.search_hits.clear();
            self.search_running = false;
            self.search_error = None;
            return;
        }
        self.search_running = true;
        self.search_error = None;
        sync::spawn(
            Job::Search {
                seq: self.search_seq,
                query: self.search.clone(),
            },
            self.ctx.clone(),
            self.tx.clone(),
        );
    }

    /// Open a search result: switch to the account and folder it lives in, then
    /// open the message the way a click in the list would.
    fn open_hit(&mut self, hit: &SearchHit) {
        let Some(protocol) = self
            .accounts
            .iter()
            .find(|account| account.id == hit.account_id)
            .map(|account| account.protocol)
        else {
            return;
        };
        self.select_folder(hit.account_id.clone(), hit.folder.clone(), protocol);
        // The whole mailbox is in memory, so the hit is normally right there.
        // If it is not (the account was edited under us), the pane stays empty
        // instead of opening somebody else's mail.
        let message = self
            .states
            .get(&hit.account_id)
            .and_then(|state| state.mailbox.folders.iter().find(|f| f.name == hit.folder))
            .and_then(|folder| folder.messages.iter().find(|m| m.uid == hit.uid))
            .cloned();
        if let Some(message) = message {
            self.open_message(&message);
        }
    }

    /// One message card in the folder list. `first` is the list's own first
    /// card, which is the one that draws no line above itself. The row only
    /// draws itself — the list it belongs to opens whatever was clicked, once
    /// it has stopped borrowing the state this was read from.
    fn message_row(&self, ui: &mut egui::Ui, msg: &MailMessage, first: bool) -> egui::Response {
        let selected = self.selected_message.as_deref() == Some(msg.uid.as_str());
        let preview = if msg.preview.is_empty() {
            " ".to_string()
        } else {
            msg.preview.clone()
        };
        let card = CardText {
            from: &msg.from,
            title: &msg.subject,
            date: &date_label(msg.timestamp, &msg.date),
            third: &preview,
            unread: !msg.seen,
            selected,
        };
        self.message_card(ui, &card, first)
    }

    /// One search result: the same card, with where the mail lives and the line
    /// the query matched as its third line.
    fn search_row(&self, ui: &mut egui::Ui, hit: &SearchHit, first: bool) -> egui::Response {
        let selected = self.selected_message.as_deref() == Some(hit.uid.as_str())
            && self.selected_folder.as_deref() == Some(hit.folder.as_str());
        let account = self
            .accounts
            .iter()
            .find(|account| account.id == hit.account_id)
            .map(|account| account.name.as_str())
            .unwrap_or(hit.account_id.as_str());
        let folder = crate::protocol::imap::decode_mailbox_name(&hit.folder);
        // The paperclip the reading pane uses, so a hit says when it has parts.
        let clip = if hit.attachments > 0 { "📎 " } else { "" };
        let third = format!("{clip}{account} · {folder} — {}", hit.snippet);
        let card = CardText {
            from: &hit.from,
            title: &hit.subject,
            date: &date_label(hit.timestamp, &hit.date),
            third: &third,
            unread: !hit.seen,
            selected,
        };
        self.message_card(ui, &card, first)
    }

    /// One card in the middle column: avatar on the left; sender + timestamp,
    /// title and a muted third line stacked on the right. The folder list and
    /// the search results share it — only what the three lines say differs —
    /// and `first` marks the card the list starts with.
    fn message_card(&self, ui: &mut egui::Ui, card: &CardText, first: bool) -> egui::Response {
        let scheme = theme::of(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), MESSAGE_ROW_HEIGHT),
            egui::Sense::click(),
        );

        // Both highlights are the mail's whole row: a square block that fills
        // the column edge to edge, hairlines included, rather than a rounded
        // pill inset from the text. The row's own rect only spans the mail's
        // text column, and inside the scroll area the painter is clipped to the
        // viewport, so the column is both the rectangle and the clip.
        let painter = ui.painter();
        let column = self.list_column;
        let full = egui::Rect::from_x_y_ranges(column.x_range(), rect.y_range());
        if card.selected {
            painter
                .with_clip_rect(column)
                .rect_filled(full, 0.0, scheme.selection);
        } else if resp.hovered() {
            painter
                .with_clip_rect(column)
                .rect_filled(full, 0.0, scheme.hover);
        }
        // Every mail carries its own edges instead of sharing a rule with its
        // neighbour: the two sides and the bottom take the faint `row_edge`,
        // the top the fainter still `row_edge_top`, so what separates two
        // mails is this row's top edge against the bottom edge of the row above
        // — two pixels, no rule. Drawn after the highlight, which is what keeps
        // a selected mail's frame visible across its fill.
        let edge = egui::Stroke::new(1.0_f32, scheme.row_edge);
        let top_edge = egui::Stroke::new(1.0_f32, scheme.row_edge_top);
        let edges = painter.with_clip_rect(column);
        // The top edge is what separates this mail from the one above, so the
        // first mail of the list has none: an edge there would draw a line
        // under the title bar, whose bottom is deliberately bare.
        if !first {
            edges.line_segment(
                [
                    egui::pos2(column.left(), rect.top() + 0.5),
                    egui::pos2(column.right(), rect.top() + 0.5),
                ],
                top_edge,
            );
        }
        edges.line_segment(
            [
                egui::pos2(column.left(), rect.bottom() - 0.5),
                egui::pos2(column.right(), rect.bottom() - 0.5),
            ],
            edge,
        );
        edges.line_segment(
            [
                egui::pos2(column.left() + 0.5, rect.top()),
                egui::pos2(column.left() + 0.5, rect.bottom()),
            ],
            edge,
        );
        edges.line_segment(
            [
                egui::pos2(column.right() - 0.5, rect.top()),
                egui::pos2(column.right() - 0.5, rect.bottom()),
            ],
            edge,
        );

        // Avatar: deterministic color per sender, initial centered in it.
        let avatar_center = egui::pos2(rect.left() + 8.0 + AVATAR_DIAMETER / 2.0, rect.center().y);
        painter.circle_filled(
            avatar_center,
            AVATAR_DIAMETER / 2.0,
            sender_color(scheme, card.from),
        );
        painter.text(
            avatar_center,
            egui::Align2::CENTER_CENTER,
            sender_initial(card.from),
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

        // First row: timestamp right-aligned, sender truncated before it.
        let date_galley = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(
                card.date.to_string(),
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
            card.from,
            ROW_TITLE_FONT_SIZE,
            title_color,
            sender_max_width,
        );
        paint_row_text(
            painter,
            egui::pos2(text_left, rect.top() + 20.0 - sender.size().y * 0.5),
            sender,
            title_color,
            card.unread,
        );

        // Second row: subject.
        let subject = truncate_galley(
            ui.ctx(),
            card.title,
            ROW_TITLE_FONT_SIZE,
            title_color,
            text_right - text_left,
        );
        paint_row_text(
            painter,
            egui::pos2(text_left, rect.top() + 40.0 - subject.size().y * 0.5),
            subject,
            title_color,
            card.unread,
        );

        // Third row: the preview — or, for a search result, where the mail
        // lives and the line that matched — always muted.
        let third = truncate_galley(
            ui.ctx(),
            card.third,
            ROW_PREVIEW_FONT_SIZE,
            scheme.text_weak,
            text_right - text_left,
        );
        painter.galley(
            egui::pos2(text_left, rect.top() + 57.0 - third.size().y * 0.5),
            third,
            scheme.text_weak,
        );

        resp
    }

    /// Right column: the selected message on the card the reading pane floats
    /// on — the lighter content surface, inset in the chrome by
    /// CONTENT_GUTTER, whose left side is also the splitter. With nothing
    /// selected there is no card at all: the pane is bare chrome and the
    /// placeholder sits directly on it.
    ///
    /// The card is the pane's scroll container: the headers, the rule and the
    /// body are one column inside the card's own scroll area, so the scroll bar
    /// runs down the card's edge rather than down the body's. The card reaches
    /// the window's top edge — the floating title bar lies over it — and its
    /// top padding is the bar's height (see `mail_card`), so the mail scrolls
    /// under the glass exactly like the message list's rows do.
    fn mail_content(&mut self, ui: &mut egui::Ui) {
        let scheme = theme::of(ui.ctx());
        // The chrome is painted here rather than by the panel's frame: a frame
        // fills its *content's* rect, which the card is inset from, so the
        // panel's right edge would be left unpainted.
        let panel = ui.max_rect();
        // Claim the whole panel. egui takes a side panel's *frame* rect as the
        // width to give it next frame, and the card is inset from that, so
        // without this the pane would shrink by the gutter on every frame
        // until it hit its minimum.
        ui.set_min_size(panel.size());
        ui.painter().rect_filled(panel, 0.0, scheme.panel);

        // The title bar floats over the pane's top edge; with nothing open the
        // placeholder centres in what the bar leaves, and there is no scroll
        // left to float for.
        let below_bar = egui::Rect::from_min_max(
            egui::pos2(panel.left(), panel.top() + TITLEBAR_HEIGHT),
            panel.max,
        );
        let Some(msg) = self.selected_message_view() else {
            self.mail_scroll = 0.0;
            // Nothing on the card, so nothing to keep parsed behind it.
            self.body.forget();
            let _ = ui.scope_builder(egui::UiBuilder::new().max_rect(below_bar), |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("No mail selected").weak().heading());
                });
            });
            return;
        };

        // A newly opened message starts at the top. The scroll offset is per
        // pane, not per message, so without this the next mail opens wherever
        // the previous one was left — for a Blitz page that means the top of
        // the page is simply not on screen.
        let message_key = format!(
            "{}/{}",
            self.selected_folder.as_deref().unwrap_or("-"),
            msg.uid
        );
        let new_message = self.scrolled_message.as_deref() != Some(message_key.as_str());
        if new_message {
            self.scrolled_message = Some(message_key);
        }

        // The card hugs the mail instead of always filling the pane, so its
        // height is only known once the mail has been laid out: the painter
        // reserves the shape here and it is filled in below. Painting the card
        // rather than framing the content is also what keeps the scroll bar *on*
        // the card: a frame would be inset by the bar's own strip, leaving the
        // bar over the chrome beside it.
        let where_to_put_card = ui.painter().add(egui::Shape::Noop);
        let card_left = panel.left() + CONTENT_GUTTER;
        let card_right = panel.right() - CONTENT_GUTTER;
        let card_top = panel.top() + CONTENT_GUTTER;
        let card_bottom = panel.bottom() - CONTENT_GUTTER;
        let inner = egui::Rect::from_min_max(
            egui::pos2(card_left, card_top),
            egui::pos2(card_right, card_bottom),
        );
        let used = ui
            .scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                // Both directions on purpose. With horizontal scrolling off,
                // a mail whose layout cannot be narrower than the pane — a
                // fixed-width table, which is most HTML mail — made this
                // scroll area wider than the pane, and through it the panel's
                // *content* rect. egui persists that rect as the panel's width
                // for the next frame, so the splitter sprang back to the
                // mail's width on release: the pane could never be dragged
                // narrower than the mail inside it. Scrolling the overflow
                // sideways keeps the pane's width the user's, and the
                // horizontal bar only appears when the mail really is wider.
                let mut scroll = egui::ScrollArea::both()
                    // Full width, but only as tall as the mail: the card around
                    // a short message hugs it instead of running to the pane's
                    // floor, while a long one takes the pane and scrolls.
                    .auto_shrink([false, true])
                    // Switching renderers swaps the whole card; a scroll offset
                    // from the other renderer would land in empty space.
                    .id_salt(("mail-card", self.html_renderer.as_str()));
                if new_message {
                    // A newly opened mail starts at its top *left*: the
                    // horizontal offset outlives the message otherwise.
                    scroll = scroll
                        .vertical_scroll_offset(0.0)
                        .horizontal_scroll_offset(0.0);
                }
                let output = scroll.show(ui, |ui| self.mail_card(ui, &msg));
                // Scrolled away from the top, the bar floats over this card.
                self.mail_scroll = output.state.offset.y;
            })
            .response
            .rect;
        let card = egui::Rect::from_min_max(
            egui::pos2(card_left, card_top),
            egui::pos2(card_right, used.bottom().min(card_bottom)),
        );
        ui.painter().set(
            where_to_put_card,
            egui::Shape::rect_filled(card, CONTENT_CARD_CORNER_RADIUS, scheme.content),
        );
    }

    /// The mail on its card: the headers, the rule under them and the body in
    /// one padded column, which the card's scroll area scrolls as a whole.
    ///
    /// The padding is inside the scroll area, so the scroll bar sits on the
    /// card's edge and not on the text. Its top is the title bar's height on
    /// top of the card's own: the bar floats over the card, and that is what
    /// keeps the subject below it at rest — scrolling takes the padding away
    /// with the headers, which is what lets them pass under the glass.
    fn mail_card(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: CONTENT_PADDING_I8,
                right: CONTENT_PADDING_I8,
                top: TITLEBAR_HEIGHT_I8 + CONTENT_PADDING_I8,
                bottom: CONTENT_PADDING_I8,
            })
            .show(ui, |ui| {
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
                        egui::RichText::new(format!("📎 {}", msg.attachment_names.join(", ")))
                            .small(),
                    );
                }
                // The rule between the headers and the body spans the card, not
                // just the mail's text column.
                ui.add(egui::Separator::default().grow(CONTENT_PADDING.x));
                self.mail_body(ui, msg);
            });
    }

    /// The mail itself, laid out inside the card's scroll area (`mail_card`),
    /// which owns the padding around it.
    fn mail_body(&mut self, ui: &mut egui::Ui, msg: &MailMessage) {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        let html = msg.body_html.as_deref();
        // Blitz takes the whole body; the built-in renderer below
        // needs the app's resource pipeline, so it stays as is.
        if let (HtmlRenderer::Blitz, Some(html)) = (self.html_renderer, html) {
            self.body.forget();
            self.blitz_view(ui, msg, html);
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
            // The parsed body and the width it is laid out for, both kept
            // between frames (see `BodyCache`): a splitter drag moves the pane
            // width on every frame, and this is what keeps the body from being
            // parsed and re-shaped on every one of them.
            let identity = format!(
                "{}/{}",
                self.selected_folder.as_deref().unwrap_or("-"),
                msg.uid
            );
            let wanted = ui.available_width();
            let (document, width) = self.body.get(
                &egui_ctx,
                &identity,
                body_fingerprint(html),
                html,
                wanted,
                Instant::now(),
            );
            let mut html_ctx = crate::html::HtmlContext {
                images: &textures,
                inline_images: &inline_map,
                stylesheets: &sheets,
                missing_images: Vec::new(),
                missing_stylesheets: Vec::new(),
            };
            // The body is laid out in a scope of its own, and *that* is what
            // holds its width: the card has already drawn its headers across
            // the pane, and a ui never gets narrower than what it has drawn
            // (`Placer::set_max_width` widens the max back to the min rect), so
            // a narrower width has to be a fresh scope rather than a narrower
            // `max_width` on this one.
            let start = ui.cursor().min;
            let body = egui::Rect::from_min_max(
                start,
                egui::pos2(start.x + width, ui.max_rect().bottom()),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
                crate::html::render_document(ui, document, &mut html_ctx);
            });
            let missing_images = std::mem::take(&mut html_ctx.missing_images);
            let missing_sheets = std::mem::take(&mut html_ctx.missing_stylesheets);
            for url in missing_images {
                self.request_image(url);
            }
            for url in missing_sheets {
                self.request_stylesheet(url);
            }
        } else if !msg.body_text.is_empty() {
            self.body.forget();
            ui.label(msg.body_text.as_str());
        } else {
            self.body.forget();
            ui.label(egui::RichText::new("(no text body)").weak());
        }
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
                        (PrefsTab::General, "General"),
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
                        PrefsTab::General => self.general_page(ui),
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

    /// Preferences → General: the working mode, persisted on change.
    fn general_page(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Working mode").strong());
        ui.horizontal(|ui| {
            for mode in WorkingMode::ALL {
                if ui.radio(self.working_mode == mode, mode.label()).clicked() {
                    self.working_mode = mode;
                    self.persist_settings();
                }
            }
        });
        ui.label(
            egui::RichText::new(self.working_mode.description())
                .small()
                .weak(),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "A settings file that names no mode follows the build: debug builds start in Debug,                  release builds in Release.",
            )
            .small()
            .weak(),
        );
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

/// One line standing above the message list's first row, where the rows are
/// padded for the title bar. `top` is that padding, which the second banner in
/// the column drops so the two stack; `color` is left to the label when a
/// banner is not a warning.
fn list_banner(ui: &mut egui::Ui, top: i8, text: &str, color: Option<egui::Color32>) {
    // Padded like the rows are, since the column's frame no longer does it for
    // anything.
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: LIST_MARGIN,
            right: LIST_MARGIN,
            // Below the bar like the rows are, since this stands where the
            // first of them would.
            top,
            bottom: 0,
        })
        .show(ui, |ui| {
            let text = egui::RichText::new(text).small();
            ui.label(match color {
                Some(color) => text.color(color),
                None => text,
            });
            ui.separator();
        });
}

impl KimoApp {
    /// One frame. The integration (`crate::gpu`) hands over the root `Ui` egui
    /// 0.36 draws the whole app into; the context is cloned out of it because
    /// the areas, modals, animations and layer painting below are all
    /// context-level.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.drain_events();
        self.apply_theme(ctx);

        // Collapse the nav bar automatically when the window becomes narrow;
        // re-expanding is a manual choice and stays until the next crossing.
        let narrow = ctx.viewport_rect().width() < NAV_AUTO_COLLAPSE_WIDTH;
        if narrow && !self.was_narrow {
            self.nav_collapsed = true;
        }
        self.was_narrow = narrow;

        // The chrome — title bar, navigation, and the gutter the reading
        // pane's card floats in — is the scheme's panel color; the message list
        // and the card are the lighter content surface.
        let scheme = theme::of(ctx);
        // egui's panel separators are off in both schemes: the palette marks
        // the columns — the list's hairlines, and the card floating in the
        // chrome's gutter — so a separator would only double them.

        // No top panel: the title bar is an overlay (`titlebar_area`), so the
        // columns run from the window's top edge and the message list can
        // scroll under the bar. What each column keeps clear of the bar is its
        // own business — the list's top padding scrolls away with its rows
        // (`LIST_PADDING`), the navigation and the reading pane are inset by
        // the bar's height.
        egui::Panel::right("mail_content")
            .resizable(true)
            .min_size(280.0)
            .default_size(460.0)
            .show_separator_line(false)
            // No frame: the gutter and the card are painted in mail_content.
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.mail_content(ui));
        // The navigation slides out instead of vanishing: `show_collapsible`
        // lays the column out at its full width and slides it off toward the
        // window's edge, clipped, so the folder rows keep their layout as it
        // goes and the list takes the room over frame by frame. The slide runs
        // on egui's own animation time (`Style::animation_time`, 0.2 s), and
        // the panel's stored width survives it — a collapsed column reopens at
        // the width the splitter left it at, not at the width it slid to.
        //
        // Collapsing is the title-bar toggle's business (and the narrow-window
        // rule's, above): `drag_to_open(false)` keeps a collapsed column
        // from leaving an invisible grab strip — and a hover line — across the
        // message list's left edge, and the expanded flag the panel is handed
        // is a copy, so that a splitter drag past the minimum keeps clamping
        // there the way it always has instead of shutting the column by
        // itself.
        let mut expanded = !self.nav_collapsed;
        egui::Panel::left("nav")
            .resizable(true)
            .min_size(150.0)
            .default_size(200.0)
            .show_separator_line(false)
            .drag_to_open(false)
            .show_collapsible(ui, &mut expanded, |ui| self.nav(ui));
        // No padding on the frame: the column's padding lives inside its scroll
        // area (see folder_messages), so the scroll bar can sit on the column's
        // edge instead of on the padding. The column's own rect — what the two
        // side panels leave — is recorded by `mail_list` for the splitter
        // repaint below.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(scheme.content))
            .show(ui, |ui| self.mail_list(ui));
        let list_column = self.list_column;

        // egui paints a brighter line along a panel boundary while its splitter
        // is hovered or dragged, and this UI wants the splitters invisible: put
        // those two pixels back to the chrome every frame, so a hovered
        // splitter looks exactly like a resting one. Painted after the panels,
        // so it lands on top of whatever they drew there; the list's own
        // hairlines sit a pixel inside the column and are untouched.
        let splitter = ctx.layer_painter(egui::LayerId::background());
        let splitter_stroke = egui::Stroke::new(1.0_f32, scheme.panel);
        splitter.vline(
            list_column.left() - 0.5,
            list_column.y_range(),
            splitter_stroke,
        );
        splitter.vline(
            list_column.right() + 0.5,
            list_column.y_range(),
            splitter_stroke,
        );

        // The title bar, over the columns it belongs to: the list scrolls
        // under it and it is glass while it does (`titlebar_area`). Drawn
        // before the compose window, so a draft floats over it, and before
        // the modals, which egui keeps in the foreground layer.
        self.titlebar_area(ctx);

        // The compose modal: over the columns and the bar, and under any modal
        // opened after it (egui stacks the foreground layer in the order its
        // areas are shown).
        if self.compose.is_some() {
            self.compose_window(ctx);
        }

        // Ctrl+F opens the search popup, the way every other application's
        // search does — the title-bar button is the discoverable half of it.
        if ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::F,
            ))
        }) {
            self.open_search();
        }
        // The popup is drawn over whatever the columns just drew. Closing it
        // is not a cancel: the search ran live while it was open.
        let mut run_now = false;
        if self.show_search {
            run_now = self.search_modal(ctx);
        }
        if self.show_preferences {
            self.preferences_modal(ctx);
        }
        // The query is run from here rather than from the message list: the
        // popup edits it over any column, and the settle clock is time-based,
        // so a pending search has to keep the frames coming even when nothing
        // else is moving.
        self.pump_search(run_now);
        if self.search_pending.is_some() {
            ctx.request_repaint_after(SEARCH_SETTLE);
        }
    }
}

// ----- helpers --------------------------------------------------------------

/// The title bar's height. The integer form is the one the message list's
/// top padding wants (`egui::Margin` is in whole points), and the only place
/// the two could drift apart.
const TITLEBAR_HEIGHT_I8: i8 = 40;
const TITLEBAR_HEIGHT: f32 = TITLEBAR_HEIGHT_I8 as f32;
/// Every button in the title bar is a square of this size, centred in the bar's
/// height: the nav toggle, the compose and sync buttons, the mail actions, the
/// cog and the three window controls. One size for all of them is what makes
/// the bar read as one row of the same button, whichever group a cell is in.
const TITLEBAR_BUTTON_SIZE: f32 = 32.0;
/// The clear space between two of those cells. Every group in the bar lays
/// itself out with it, so the same rhythm runs from the brand to the close
/// button.
const TITLEBAR_BUTTON_GAP: f32 = 6.0;
/// The cells' hover fill: a small rounded square, not the full-height block the
/// bar's buttons used to fill under the pointer. Small, because the cell is —
/// the search box's 8 pt on a 24 pt tall box would read as a pill on a 32 pt
/// square.
const TITLEBAR_BUTTON_CORNER_RADIUS: u8 = 6;
/// How long the bar takes to turn from docked chrome into glass and back.
/// Short enough to feel like a state change rather than an animation.
const TITLEBAR_FLOAT_TIME: f32 = 0.16;
/// What the bar's fill keeps of its alpha while it floats: the rest is the
/// blurred mail showing through. At 0.25 the blurred mail *is* the bar's body
/// and the fill is a tint over it; the bar's own icons and query stay crisp
/// because they are painted over that fill at full strength.
const GLASS_ALPHA: f32 = 0.25;
/// The same for the search box's own fill, which is lighter than the bar's to
/// begin with: it keeps a little less of it, so the box is glass too and the
/// mail still shows through it. The stroke is what keeps it reading as a
/// field once the fill has gone.
const GLASS_FIELD_ALPHA: f32 = 0.45;
/// Width of the title bar's search box — the whole box, `SEARCH_BOX_MARGIN`
/// included, which is what `TextEdit::desired_width` sizes and what
/// `titlebar_fit` plans in. Fixed rather than filling: the bar also carries
/// the drag surface, and it should not move around with the length of a query.
const TITLEBAR_SEARCH_WIDTH: f32 = 175.0;
/// The narrowest the box is allowed to get before the bar gives it up
/// altogether (`titlebar_fit`): below this it stops reading as a field. The
/// margins come out of this width like anything else, so the query is left
/// with most of it.
const TITLEBAR_SEARCH_FLOOR: f32 = 96.0;
/// The search box's own margins, *inside* that width: the magnifier's inset on
/// the left, and a hair of room on the right so the query does not touch the
/// box's edge.
const SEARCH_BOX_MARGIN: egui::Margin = egui::Margin {
    left: SEARCH_ICON_INSET,
    ..egui::Margin::symmetric(4, 2)
};
/// Height of that box: the bar's height less the breathing room above and
/// below it, which is what makes the box read as a control rather than as a
/// line of text in the bar.
const TITLEBAR_SEARCH_HEIGHT: f32 = TITLEBAR_HEIGHT - 16.0;
/// Corner radius of that box. egui's stock widget radius is 2, which is right
/// for a button but leaves this box looking like a rectangle; a rounder corner
/// makes it read as the one field in the bar.
const SEARCH_BOX_CORNER_RADIUS: u8 = 8;

/// Clear chrome between the title bar's search box and the group of mail
/// actions to its right: what sets the seven cells apart from the box as one
/// group. It is the bar's first elastic, though — the middle group is laid out
/// at exact x positions (`titlebar_fit`), not by the row's item spacing, so
/// the gap is this wide only while there is room for it.
const MAIL_ACTION_PADDING: f32 = 128.0;
/// How far that gap may close before the bar gives up something else instead.
/// A group that still reads as a group, because the gap only ever shrinks to
/// this while the box is still at its full width.
const MAIL_ACTION_MIN_PADDING: f32 = 16.0;
/// How large the mail actions' glyphs are drawn. Their outlines are designed in
/// a 24×24 box, which `glyph_point` scales to this.
const MAIL_ACTION_GLYPH: f32 = 18.0;

/// How long the line an export leaves above the message list stays there: long
/// enough to read a path and go looking for the file, short enough not to
/// become furniture. The file does not go anywhere when the line does.
const EXPORT_NOTICE_TIME: Duration = Duration::from_secs(8);

/// Padding on the sides of the message list's rows: the hairlines `mail_list`
/// draws down the column's sides sit just outside it.
const LIST_MARGIN: i8 = 8;

/// The floating `Load older mail` button: its size, how far its bottom edge
/// stays clear of the column's, and its corner (the same roundness as the
/// search box, so the two controls the bar and the list float read as one
/// family rather than as two widgets that happen to be on top).
const LOAD_OLDER_WIDTH: f32 = 150.0;
const LOAD_OLDER_HEIGHT: f32 = 26.0;
const LOAD_OLDER_MARGIN: f32 = 12.0;
const LOAD_OLDER_CORNER_RADIUS: u8 = SEARCH_BOX_CORNER_RADIUS;

/// The rows' padding inside the list's scroll area. The top is the title
/// bar's height and it is *inside* the scroll area on purpose: the first mail
/// starts below the bar at rest, and scrolling takes the padding away with
/// the rows, which is what slides them under the floating bar. It belongs to
/// the scroll area rather than to the column, which is what lets the scroll
/// bar reach the column's edge.
const LIST_PADDING: egui::Margin = egui::Margin {
    left: LIST_MARGIN,
    right: LIST_MARGIN,
    top: TITLEBAR_HEIGHT_I8,
    bottom: LIST_MARGIN,
};
/// The gutter around the reading pane's card — the gap between the card and
/// the list column, and the chrome showing between the card and the panel's
/// other edges. It doubles as the splitter: egui's drag area around the panel
/// boundary covers it, and no line is drawn in it.
const CONTENT_GUTTER: f32 = 4.0;
/// Padding between the card's edge and the mail on it — all four sides, so the
/// subject and the body do not sit against the card's corners. The integer
/// form is the one `egui::Margin` wants.
const CONTENT_PADDING_I8: i8 = 12;
const CONTENT_PADDING: egui::Vec2 =
    egui::vec2(CONTENT_PADDING_I8 as f32, CONTENT_PADDING_I8 as f32);
/// Corner radius of the reading pane's card.
const CONTENT_CARD_CORNER_RADIUS: u8 = 8;
/// Room the magnifier takes inside the search box, and the glyph's size. The
/// inset is the field's left margin, so the text begins after the icon.
const SEARCH_ICON_INSET: i8 = 22;
const SEARCH_ICON_SIZE: f32 = 15.0;
/// Side of the square brand icon at the left of the title bar.
const BRAND_ICON_SIZE: f32 = 18.0;
/// How far the bar's content stays clear of the window's own left and right
/// edges: the brand icon starts this far in — as the row's own first item, the
/// space added before it — and the window controls end this far short of the
/// right edge, so the rounded fills at either end are not flush against the
/// frame. Flush, the mark reads as cropped rather than placed and the close
/// button's fill reaches the window's corner.
const TITLEBAR_EDGE_MARGIN: f32 = 10.0;

/// The compose modal: the width of its card (what the boxes are laid out for),
/// the label column of its header fields, the height of one of those one-line
/// fields, and the height of the body box, which scrolls rather than growing
/// the card.
const COMPOSE_WIDTH: f32 = 560.0;
const COMPOSE_LABEL_WIDTH: f32 = 56.0;
const COMPOSE_FIELD_HEIGHT: f32 = 22.0;
const COMPOSE_BODY_HEIGHT: f32 = 320.0;

/// Where the title bar's groups are laid out. The window controls and the cog
/// are the bar's right end, so they are placed from the window's own right
/// edge; `content` is everything left of them, which the window's own actions,
/// the account status, the search box and the mail actions share.
///
/// The three rects never cross, whatever the window's width: a bar too narrow
/// for the fixed groups leaves `content` empty rather than letting the groups
/// run into one another. `cog` and `controls` are *not* clamped to the bar —
/// their cells keep the size every other cell in the bar has — but they are
/// drawn and hit-tested through their clip rect, so a cell the bar cannot show
/// is cut off at the window's edge like anything else off-screen.
#[derive(Clone, Copy, Debug)]
struct TitlebarRects {
    /// Room for the bar's own content: the left group, the status strip, the
    /// search box, the mail actions and the drag surface. Empty — never inside
    /// out — when the window controls and the cog are all the bar can hold.
    content: egui::Rect,
    /// The preferences cog, immediately left of the window controls.
    cog: egui::Rect,
    /// Minimize / maximize / close, ending at the window's right edge.
    controls: egui::Rect,
}

/// Carve the title bar into those rects. `controls` is how many window
/// controls the bar draws — `titlebar`'s own list, so the two cannot drift.
fn titlebar_rects(bar: egui::Rect, controls: usize) -> TitlebarRects {
    // Every cell in the bar is the same square, centred in its height, so the
    // rects here are a cell's own band rather than the bar's full height: what
    // is drawn to them — and what a group is clipped to — is the button, not
    // the column of chrome around it.
    let top = bar.center().y - TITLEBAR_BUTTON_SIZE / 2.0;
    let cell = |left: f32, width: f32| {
        egui::Rect::from_min_size(
            egui::pos2(left, top),
            egui::vec2(width, TITLEBAR_BUTTON_SIZE),
        )
    };
    let controls_left = bar.right() - TITLEBAR_EDGE_MARGIN - titlebar_cells_width(controls);
    let cog_left = controls_left - TITLEBAR_BUTTON_GAP - TITLEBAR_BUTTON_SIZE;
    TitlebarRects {
        content: egui::Rect::from_min_max(
            bar.min,
            egui::pos2(cog_left.clamp(bar.left(), bar.right()), bar.bottom()),
        ),
        cog: cell(cog_left, TITLEBAR_BUTTON_SIZE),
        controls: cell(
            controls_left,
            bar.right() - TITLEBAR_EDGE_MARGIN - controls_left,
        ),
    }
}

/// What the middle of the title bar holds in the room it was given: the width
/// of the search box (the whole box, its margins included), whether the run of
/// mail actions is drawn, and the clear chrome between the two.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TitlebarFit {
    /// The search box's width, or `None` when the bar is too narrow for it.
    search: Option<f32>,
    /// Whether the mail actions are drawn.
    actions: bool,
    /// The gap between the box and the actions (`MAIL_ACTION_PADDING`, down to
    /// `MAIL_ACTION_MIN_PADDING` when the room is tight).
    padding: f32,
}

impl TitlebarFit {
    /// The width the group needs, actions included. The bar's own promise is
    /// that this is never more than the room the fit was given; the drawing
    /// checks the part it can measure (where the box really ended, against the
    /// room the actions really have), and the tests pin the planning itself.
    #[cfg(test)]
    fn width(&self, actions: f32) -> f32 {
        let mut width = self.search.unwrap_or(0.0);
        if self.actions {
            width += self.padding + actions;
        }
        width
    }
}

/// The width of a run of `count` cells in the title bar: `TITLEBAR_BUTTON_SIZE`
/// squares, `TITLEBAR_BUTTON_GAP` apart. Every run is measured with this — the
/// mail actions (which the fit plans the room for) and the window controls
/// (which are placed from the window's right edge) — so no run can disagree
/// with the cells it draws.
fn titlebar_cells_width(count: usize) -> f32 {
    count as f32 * TITLEBAR_BUTTON_SIZE + count.saturating_sub(1) as f32 * TITLEBAR_BUTTON_GAP
}

/// Fit the middle group — the search box, the clear chrome and the run of mail
/// actions — into `room` points.
///
/// The bar gives its room up in one order, so that what it loses is the least
/// useful thing it can: the clear chrome in front of the actions closes from
/// `MAIL_ACTION_PADDING` to `MAIL_ACTION_MIN_PADDING` first, then the actions
/// leave — as the one group they are, because half a run of seven reads as a
/// mistake rather than as a smaller bar — and only then does the search box
/// narrow, down to `TITLEBAR_SEARCH_FLOOR` and no further. The box is the last
/// thing to go, and the one thing the bar keeps to the end is the window's own
/// buttons (`titlebar_rects`).
///
/// Every width is monotone in `room` — a wider bar never holds a narrower box,
/// never loses the actions again — which is what keeps a window being resized
/// from flickering between two shapes. Nothing here is a rect: the caller
/// draws each group inside the room it was given, and clips it to that room.
fn titlebar_fit(room: f32, actions: f32) -> TitlebarFit {
    let widest = TITLEBAR_SEARCH_WIDTH;
    let narrowest = TITLEBAR_SEARCH_FLOOR;
    if room >= widest + MAIL_ACTION_MIN_PADDING + actions {
        // Everything fits: the box at its width, the actions a clear gap away.
        // The gap is what closes first, so the box and the seven cells are both
        // still whole when they are at their closest.
        let padding = (room - widest - actions).clamp(MAIL_ACTION_MIN_PADDING, MAIL_ACTION_PADDING);
        return TitlebarFit {
            search: Some(widest),
            actions: true,
            padding,
        };
    }
    if room >= narrowest {
        // No room for the group. The box takes back what the actions were
        // using — the room is the box's now — and keeps as much of its own
        // width as there is, down to its floor.
        return TitlebarFit {
            search: Some(room.min(widest)),
            actions: false,
            padding: 0.0,
        };
    }
    // Not even the box: the bar keeps the window's own buttons and gives the
    // rest of the room to the drag surface.
    TitlebarFit {
        search: None,
        actions: false,
        padding: 0.0,
    }
}

/// Clip a Ui to the group rect it was given, so that neither what it draws nor
/// what it senses reaches past that rect. `Ui::interact` hit-tests a widget
/// through its clip rect, which is what makes this a rule about clicks as much
/// as about pixels: a group the bar cannot fit is cut off at its edge, and a
/// click past that edge belongs to the group next to it.
fn clip_to(ui: &mut egui::Ui, rect: egui::Rect) {
    let clip = ui.clip_rect().intersect(rect);
    ui.set_clip_rect(clip);
}

/// One button in the title bar: a `TITLEBAR_BUTTON_SIZE` square, centred in the
/// bar's height by the row that lays it out, empty at rest and filled with a
/// rounded corner under the pointer (`titlebar_hover`). Every button in the bar
/// is this — the nav toggle, compose, sync, the mail actions, the cog and the
/// window controls — and `enabled` is the one thing a caller varies: the mail
/// actions' disabled cells neither sense clicks nor fill, while the rest of the
/// bar is always live.
fn titlebar_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, response) = ui.allocate_exact_size(egui::Vec2::splat(TITLEBAR_BUTTON_SIZE), sense);
    if enabled {
        titlebar_hover(ui, response)
    } else {
        response
    }
}

/// Fill a title-bar cell while the pointer is over it. This is the treatment
/// every button in the bar shares, and the reason they are hand-painted rather
/// than `egui::Button`s: a stock button draws a card-colored frame with a
/// border, which in a title bar reads as a widget among window chrome. The
/// glyph is the caller's, painted afterwards so the fill cannot cover it.
fn titlebar_hover(ui: &egui::Ui, response: egui::Response) -> egui::Response {
    if response.hovered() {
        titlebar_fill(ui, response.rect, ui.visuals().widgets.hovered.bg_fill);
    }
    response
}

/// The rounded fill itself, for the cells the bar places at exact x positions
/// rather than in a row (`titlebar_rects`): the cog and the window controls.
/// The close button passes the scheme's danger colour; everything else takes
/// the hover fill `titlebar_hover` does.
fn titlebar_fill(ui: &egui::Ui, rect: egui::Rect, fill: egui::Color32) {
    ui.painter()
        .rect_filled(rect, TITLEBAR_BUTTON_CORNER_RADIUS, fill);
}

/// Blitz reading pane: page margin, width quantization for renders, and how
/// many rendered pages stay in memory (each is megabytes of texture).
const BLITZ_PAGE_MARGIN: f32 = 8.0;
const BLITZ_WIDTH_STEP: u32 = 32;
const BLITZ_PAGES_CACHED: usize = 2;

/// How long the pane geometry must hold still before the reading pane lays the
/// mail out for it again. Dragging the splitter, or a compositor animating a
/// window resize, changes it every frame; both renderers wait the same time,
/// for the same reason — every intermediate width would be work that is
/// discarded as soon as the pointer moves again. What they do while they wait
/// differs: Blitz renders on a worker and has nothing to show until a render
/// lands, while the built-in renderer keeps drawing the width it has (see
/// `BodyCache`) and so reflows only once, when the drag is over.
const GEOMETRY_SETTLE: Duration = Duration::from_millis(180);

/// A finished page may be scaled down instead of re-rendered while the pane
/// stays within this factor of the width the page was laid out for. Re-flowing
/// the text for a few percent is not worth a render — and it keeps the
/// vertical scrollbar, which narrows the pane when it appears, from
/// invalidating the very page that made it appear.
const BLITZ_REUSE_RATIO: f32 = 1.25;

/// Lay out the rows a vertical scroll area actually shows, and nothing else.
///
/// `egui::ScrollArea::show_rows` is this, for a list whose first row starts at
/// the top of the content. This column's does not: the title bar's height is
/// padding *above* the first row, inside the scroll area (`LIST_PADDING`), so
/// rows placed from the content's own origin would sit that much too high, and
/// the row a partly scrolled viewport starts in could be skipped — `show_rows`
/// divides the scroll offset by the row height, which is off by the padding.
///
/// `pad_top` is that padding, in points; `viewport` is the scroll area's own
/// viewport rect, in content coordinates, with zero at the content's top.
fn virtual_rows<R>(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    pad_top: f32,
    row_height: f32,
    total: usize,
    add_contents: impl FnOnce(&mut egui::Ui, std::ops::Range<usize>) -> R,
) -> R {
    // The rows' own height; the frame around them adds its padding back.
    ui.set_height(row_height * total as f32);
    let visible = |y: f32| ((y - pad_top) / row_height).max(0.0);
    let first = (visible(viewport.min.y).floor() as usize).min(total);
    // One past the last row the viewport can reach: the ceil, plus whatever
    // part of the next row is on screen.
    let last = ((visible(viewport.max.y).ceil() as usize) + 1).min(total);
    let top = ui.max_rect().top() + first as f32 * row_height;
    let bottom = ui.max_rect().top() + last as f32 * row_height;
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=bottom);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |rows_ui| {
        // Keep the widgets' auto ids stable as the list scrolls.
        rows_ui.skip_ahead_auto_ids(first);
        add_contents(rows_ui, first..last)
    })
    .inner
}

/// True when a popup field was left by pressing Enter — the search popup takes
/// that as "run it now" from any of its boxes, not only the text one.
fn entered(response: &egui::Response, ui: &egui::Ui) -> bool {
    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// One label + box row of the search popup's filter grid. Returns true when the
/// box was left by pressing Enter.
fn filter_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) -> bool {
    ui.label(label);
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(300.0),
    );
    let apply = entered(&response, ui);
    ui.end_row();
    apply
}

/// "1 result" / "12 results" / "200+ results": at `SEARCH_LIMIT` the database
/// stopped asking, so the count is a floor rather than a total.
fn results_label(count: usize) -> String {
    if count >= SEARCH_LIMIT {
        format!("{count}+ results")
    } else if count == 1 {
        "1 result".to_string()
    } else {
        format!("{count} results")
    }
}

/// What to do about the search popup's current contents.
#[derive(Debug, PartialEq, Eq)]
enum SearchWait {
    /// The query changed: start (or restart) the clock.
    Reset,
    /// The query has not held still long enough yet.
    Wait,
    /// Settled: run it.
    Run,
}

/// The search popup settles the way the Blitz pane does: the database is queried
/// once the user stops typing. `pending` is the query the clock is running for,
/// and when it started.
fn search_settle(
    pending: Option<(&SearchQuery, Instant)>,
    current: &SearchQuery,
    now: Instant,
) -> SearchWait {
    match pending {
        Some((query, since)) if query == current => {
            if now.duration_since(since) >= SEARCH_SETTLE {
                SearchWait::Run
            } else {
                SearchWait::Wait
            }
        }
        // A different query, or no clock yet: start over.
        Some(_) | None => SearchWait::Reset,
    }
}

/// What to do about a page that is not cached yet.
#[derive(Debug, PartialEq, Eq)]
enum BlitzWait {
    /// The geometry has held still for `GEOMETRY_SETTLE`: render now.
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
            if waited >= GEOMETRY_SETTLE {
                BlitzWait::Start
            } else {
                BlitzWait::Wait(GEOMETRY_SETTLE - waited)
            }
        }
        // Another geometry was pending, or none: this one starts settling now.
        _ => BlitzWait::Wait(GEOMETRY_SETTLE),
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
/// What one card in the middle column says: three lines, and the two states
/// that change how they are drawn. Bundled into one value so the folder list
/// and the search results can share the painter.
struct CardText<'a> {
    from: &'a str,
    title: &'a str,
    date: &'a str,
    third: &'a str,
    /// The sender and the title are struck through: the codebase's stand-in
    /// for bold, see the message-row convention in AGENTS.md.
    unread: bool,
    selected: bool,
}

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

/// Lay out a single line of text, cut with an ellipsis to fit `max_width`
/// pixels.
///
/// The cut is epaint's own — one layout job whose `TextWrapping` ends the row
/// at `max_width` and puts the ellipsis there (`max_rows: 1`,
/// `overflow_character`) — rather than a binary search over cut prefixes. A
/// search lays a galley out per probe to measure it: up to `log2(chars)`
/// shapings for one line, every probe left in the galley cache, and its cost at
/// its worst exactly when the width has moved far enough that all of the probes
/// are new — the first frames of a slide or a splitter drag. Here a line costs
/// one shaping whatever the width did, which is what the callers need: the
/// message list lays out every visible row on every frame the column's width
/// moves.
fn truncate_galley(
    ctx: &egui::Context,
    text: &str,
    size: f32,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_format(
        text.to_owned(),
        egui::text::TextFormat::simple(egui::FontId::proportional(size), color),
    );
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    // What `TextWrapping` asks for when `max_rows` is 1: without it a
    // run with no break in it (an address, a CJK subject) would not be cut where
    // the row ends.
    job.wrap.break_anywhere = true;
    ctx.fonts_mut(|f| f.layout_job(job))
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
            .fonts_mut(|fonts| fonts.layout_no_wrap(unread.to_string(), small, scheme.text_weak));
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

/// One folder in the navigation column. `listed` is whether the server still
/// has it: a row kept for the mail cached under a folder it no longer lists is
/// drawn weak whatever its unread count, and says so on hover — it is here for
/// the mail, not for the mailbox. It stays clickable, because its mail is the
/// only copy of it on this machine.
fn folder_row(
    ui: &mut egui::Ui,
    scheme: ColorScheme,
    icon: Option<&egui::TextureHandle>,
    name: &str,
    unread: usize,
    selected: bool,
    listed: bool,
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
    let color = if !listed {
        scheme.text_weak
    } else if selected || unread > 0 {
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
            .fonts_mut(|fonts| fonts.layout_no_wrap(unread.to_string(), small, color));
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
    if listed {
        response
    } else {
        response.on_hover_text("No longer on the server — the mail cached for it is still here")
    }
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

/// Magnifier glyph for the title-bar search button and the results strip: a
/// ring with a handle towards the lower right.
fn paint_search_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center() - egui::vec2(1.0, 1.0);
    let r = 4.5;
    let stroke = egui::Stroke::new(1.4_f32, color);
    p.circle_stroke(c, r, stroke);
    let start = c + egui::vec2(r, r) * std::f32::consts::FRAC_1_SQRT_2;
    p.line_segment([start, start + egui::vec2(3.4, 3.4)], stroke);
}

/// Nav collapse glyph for the title-bar toggle: three stacked lines, drawn at
/// the size of the window controls' glyphs rather than of the cell they sit in
/// (`TITLEBAR_BUTTON_SIZE`), which every button in the bar shares.
fn paint_nav_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.5_f32, color);
    for i in -1i32..=1 {
        let y = c.y + i as f32 * 4.0;
        p.line_segment([egui::pos2(c.x - 7.0, y), egui::pos2(c.x + 7.0, y)], stroke);
    }
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

// The title bar's mail-action glyphs — reply, reply all, forward, label, flag,
// export, delete — painted as vector shapes from their own 24×24 outlines (the
// box the icon set the folder icons come from is drawn in), so they render
// identically regardless of which fonts are installed.

/// Map a point of one of those 24×24 outlines onto a title-bar cell, centred
/// on `center` and drawn at `MAIL_ACTION_GLYPH`.
fn glyph_point(center: egui::Pos2, x: f32, y: f32) -> egui::Pos2 {
    center + egui::vec2(x - 12.0, y - 12.0) * (MAIL_ACTION_GLYPH / 24.0)
}

/// The tail of the reply and forward arrows: a quarter turn rather than a
/// corner, drawn as the outline does it — a quadratic curve whose control sits
/// on the corner the two straight parts would have met at.
fn glyph_tail(
    p: &egui::Painter,
    from: egui::Pos2,
    corner: egui::Pos2,
    to: egui::Pos2,
    stroke: egui::Stroke,
) {
    p.add(egui::epaint::QuadraticBezierShape::from_points_stroke(
        [from, corner, to],
        false,
        egui::Color32::TRANSPARENT,
        stroke,
    ));
}

/// Compose: a page with a pen writing on it — the page an outline of rounded
/// corners whose top-right one is left *open*, and the pen the straight stroke
/// that crosses exactly there. The two are one drawing: the pen passes through
/// the gap, so no stroke of the page ever touches it, which is what says the
/// pen is writing on the page rather than lying on top of it. Like the rest of
/// the bar's glyphs it comes from a 24×24 outline, so it needs no asset.
fn paint_compose_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);

    // The page, cut open where the pen crosses its top-right corner: the top
    // side, then the rest of the way round from where the right side starts
    // again. A corner is a quarter turn drawn the way the icon set draws them
    // — a quadratic whose control is the corner itself (`glyph_tail`).
    p.add(egui::Shape::line(vec![g(7.6, 5.0), g(16.0, 5.0)], stroke));
    p.add(egui::Shape::line(vec![g(20.0, 8.8), g(20.0, 17.4)], stroke));
    glyph_tail(p, g(20.0, 17.4), g(20.0, 21.0), g(16.4, 21.0), stroke);
    p.add(egui::Shape::line(vec![g(16.4, 21.0), g(7.6, 21.0)], stroke));
    glyph_tail(p, g(7.6, 21.0), g(4.0, 21.0), g(4.0, 17.4), stroke);
    p.add(egui::Shape::line(vec![g(4.0, 17.4), g(4.0, 8.6)], stroke));
    glyph_tail(p, g(4.0, 8.6), g(4.0, 5.0), g(7.6, 5.0), stroke);

    // The pen: its nib inside the page, its tip out past the corner.
    p.add(egui::Shape::line(vec![g(9.2, 17.2), g(21.6, 4.8)], stroke));
}

/// Reply: an arrow curving back to the left, its shaft turning down at the
/// tail. Every part is an open path, so the glyph reads as a line drawing like
/// the rest of the bar's icons.
fn paint_reply_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(20.0, 18.0), g(20.0, 16.0)],
        stroke,
    ));
    glyph_tail(p, g(20.0, 16.0), g(20.0, 12.0), g(16.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(16.0, 12.0), g(4.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(9.0, 17.0), g(4.0, 12.0), g(9.0, 7.0)],
        stroke,
    ));
}

/// Reply all: reply's arrow with a second head behind the first.
fn paint_reply_all_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(22.0, 18.0), g(22.0, 16.0)],
        stroke,
    ));
    glyph_tail(p, g(22.0, 16.0), g(22.0, 12.0), g(18.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(18.0, 12.0), g(7.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(12.0, 17.0), g(7.0, 12.0), g(12.0, 7.0)],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![g(7.0, 17.0), g(2.0, 12.0), g(7.0, 7.0)],
        stroke,
    ));
}

/// Forward: reply's arrow mirrored — it leaves to the right instead.
fn paint_forward_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(vec![g(4.0, 18.0), g(4.0, 16.0)], stroke));
    glyph_tail(p, g(4.0, 16.0), g(4.0, 12.0), g(8.0, 12.0), stroke);
    p.add(egui::Shape::line(vec![g(8.0, 12.0), g(20.0, 12.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(15.0, 17.0), g(20.0, 12.0), g(15.0, 7.0)],
        stroke,
    ));
}

/// Label: a tag — a square turned so that it points right, with the hole near
/// its flat end.
fn paint_label_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![
            g(3.0, 3.0),
            g(12.0, 3.0),
            g(21.0, 12.0),
            g(12.0, 21.0),
            g(3.0, 12.0),
            g(3.0, 3.0),
        ],
        stroke,
    ));
    // The hole is a dot rather than a ring: at this size a ring closes up.
    p.circle_filled(g(7.5, 7.5), MAIL_ACTION_GLYPH * 0.055, color);
}

/// Flag: a pole with a swallow-tailed pennant on it.
fn paint_flag_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(vec![g(5.0, 21.0), g(5.0, 3.0)], stroke));
    p.add(egui::Shape::line(
        vec![
            g(5.0, 4.0),
            g(19.0, 4.0),
            g(15.0, 9.0),
            g(19.0, 14.0),
            g(5.0, 14.0),
        ],
        stroke,
    ));
}

/// Export: an arrow coming down into an open tray, the file leaving the app.
/// It is the one glyph of the set that points into something rather than along
/// the mail (reply, forward) or at the mail (label, flag, delete), which is
/// what keeps it from reading as part of the trash can next to it.
fn paint_export_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(
        vec![g(4.0, 14.0), g(4.0, 21.0), g(20.0, 21.0), g(20.0, 14.0)],
        stroke,
    ));
    p.add(egui::Shape::line(vec![g(12.0, 3.0), g(12.0, 15.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(7.0, 10.0), g(12.0, 15.0), g(17.0, 10.0)],
        stroke,
    ));
}

/// Delete: the trash can — lid, handle above it, and the open-topped body. The
/// two lines the outline draws inside the body are left out: at this size they
/// close up into a smudge and the can reads better empty.
fn paint_delete_icon(p: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let g = |x: f32, y: f32| glyph_point(rect.center(), x, y);
    let stroke = egui::Stroke::new(1.5_f32, color);
    p.add(egui::Shape::line(vec![g(3.0, 6.0), g(21.0, 6.0)], stroke));
    p.add(egui::Shape::line(
        vec![g(8.0, 6.0), g(8.0, 3.0), g(16.0, 3.0), g(16.0, 6.0)],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![g(5.0, 6.0), g(5.0, 21.0), g(19.0, 21.0), g(19.0, 6.0)],
        stroke,
    ));
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

fn date_label(timestamp: Option<i64>, date: &str) -> String {
    timestamp
        .map(format_timestamp)
        .unwrap_or_else(|| date.to_string())
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

    /// One headless frame of the real app at a given splitter position, timed
    /// the way the integration times one: egui's own pass plus the
    /// tessellation the renderer would do.
    fn probe_frame(ctx: &egui::Context, app: &mut KimoApp, pane: f32, scale: f32) -> Duration {
        // Where the drag has the splitter: egui keeps a side panel's width as
        // its `PanelState`, and the next frame reads it back — which is
        // exactly what a drag frame updates.
        ctx.data_mut(|d| {
            d.insert_persisted(
                egui::Id::new("mail_content"),
                egui::containers::panel::PanelState {
                    outer_rect: egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(pane, 900.0),
                    ),
                },
            )
        });
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let start = Instant::now();
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        let shapes = std::mem::take(&mut output.shapes);
        let _ = ctx.tessellate(shapes, scale);
        output.textures_delta.clear();
        start.elapsed()
    }

    fn probe_median(times: &mut [Duration]) -> Duration {
        times.sort();
        times[times.len() / 2]
    }

    fn probe_ms(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }

    /// The real database, if this machine has one: the probe drives the app on
    /// the mail actually cached here. Read-only; no workers are started.
    fn probe_app(ctx: &egui::Context) -> Option<KimoApp> {
        let accounts = crate::db::load_accounts().ok()?;
        if accounts.is_empty() {
            return None;
        }
        let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
        let mailboxes = crate::db::load_mailboxes(&ids).unwrap_or_default();
        let settings = storage::load_settings();
        let mut app = KimoApp::assemble(ctx, accounts, mailboxes, &settings);
        // The folder with the most mail, and the largest HTML body in it: the
        // worst case a splitter drag can be asked to re-lay out.
        let (account_id, folder) = app
            .states
            .iter()
            .flat_map(|(id, state)| {
                state
                    .mailbox
                    .folders
                    .iter()
                    .map(move |folder| (id.clone(), folder))
            })
            .max_by_key(|(_, folder)| folder.messages.len())
            .map(|(id, folder)| (id, folder.name.clone()))?;
        let uid = app
            .states
            .get(&account_id)?
            .mailbox
            .folders
            .iter()
            .find(|folder_| folder_.name == folder)?
            .messages
            .iter()
            .max_by_key(|msg| msg.body_html.as_ref().map_or(0, String::len))?
            .uid
            .clone();
        app.selected_account = Some(account_id);
        app.selected_folder = Some(folder);
        app.selected_message = Some(uid);
        Some(app)
    }

    /// How long one frame of the real app takes, at a fixed splitter position
    /// and while the splitter is being dragged:
    ///
    ///     cargo test --release -- --ignored --nocapture bench_frame_cost
    ///
    /// The drag is what this exists for: it moves the reading pane's width
    /// every frame, which used to re-parse and re-lay out the whole mail body
    /// and every row of the message list on each one. It reads the developer's
    /// own `mail.db` (`~/.local/share/kimo/`), picks the folder with the most
    /// mail and the largest HTML body in it — the worst case a drag can be
    /// asked to lay out — and prints medians with the mail open and with
    /// nothing selected. A developer tool, not a test: nothing here asserts.
    #[test]
    #[ignore = "developer tool; prints frame times for a splitter drag and asserts nothing"]
    fn bench_frame_cost() {
        let ctx = egui::Context::default();
        let scale = 2.0;
        let Some(mut app) = probe_app(&ctx) else {
            println!("no mail database: nothing to measure");
            return;
        };
        println!(
            "probe: {} / {} / {} messages, body {} KB",
            app.selected_account.as_deref().unwrap_or("-"),
            app.selected_folder.as_deref().unwrap_or("-"),
            app.folder_messages_of(
                app.selected_account.as_deref().unwrap_or_default(),
                app.selected_folder.as_deref().unwrap_or_default()
            )
            .map_or(0, <[MailMessage]>::len),
            app.selected_message_view()
                .and_then(|msg| msg.body_html)
                .map_or(0, |html| html.len() / 1024),
        );

        for open in [true, false] {
            if !open {
                app.selected_message = None;
            }
            // Fixed geometry, then a drag that moves the splitter 4 pt a frame
            // through the widths a real drag passes over.
            for mode in ["fixed", "drag"] {
                let mut times = Vec::new();
                for step in 0..24 {
                    let pane = if mode == "fixed" {
                        520.0
                    } else {
                        700.0 - step as f32 * 4.0
                    };
                    let elapsed = probe_frame(&ctx, &mut app, pane, scale);
                    if step > 2 {
                        times.push(elapsed);
                    }
                }
                let median = probe_median(&mut times);
                let max = times.last().copied().unwrap_or_default();
                println!(
                    "  {:10}  {:4}  median {:7.2} ms  max {:7.2} ms",
                    if open { "mail open" } else { "no mail" },
                    mode,
                    probe_ms(median),
                    probe_ms(max),
                );
            }
        }
    }

    /// The same question for the navigation column's slide: every frame of the
    /// collapse moves the message column's width, so its visible rows are laid
    /// out for a new width on each one. What it measures, on this machine and
    /// the developer's own mail: with the built-in renderer and the mail open,
    /// a slide frame costs about a millisecond more than a resting one in
    /// release (2.1 ms against 1.2 ms) and about 26 ms more in a debug build,
    /// where shaping is what is slow. All of that is the message list's rows —
    /// with the rows not painted at all, a slide frame and a resting frame are
    /// the same, so the navigation column itself is never re-laid out:
    /// `Panel::show_collapsible` keeps its content at its full width and only
    /// slides and clips it.
    ///
    ///     cargo test --release -- --ignored --nocapture bench_nav_slide
    #[test]
    #[ignore = "developer tool; prints frame times for a nav slide and asserts nothing"]
    fn bench_nav_slide() {
        let ctx = egui::Context::default();
        let scale = 2.0;
        let Some(mut app) = probe_app(&ctx) else {
            println!("no mail database: nothing to measure");
            return;
        };
        const PANE: f32 = 520.0;
        // 60 frames/s, so the slide is done inside the first 12 frames.
        const MOVING: usize = 12;
        let settle = |app: &mut KimoApp| {
            for _ in 0..24 {
                probe_frame(&ctx, app, PANE, scale);
            }
        };
        let run = |app: &mut KimoApp, count: usize| -> Vec<Duration> {
            (0..count)
                .map(|_| probe_frame(&ctx, app, PANE, scale))
                .collect()
        };
        let report = |label: &str, times: &[Duration]| {
            let mut moving = times[..MOVING.min(times.len())].to_vec();
            let median = probe_median(&mut moving);
            let peak = times.iter().copied().max().unwrap_or_default();
            println!(
                "  {label:12}  moving median {:7.2} ms  peak {:7.2} ms",
                probe_ms(median),
                probe_ms(peak),
            );
        };

        println!(
            "nav slide: {} / {}",
            app.selected_account.as_deref().unwrap_or("-"),
            app.selected_folder.as_deref().unwrap_or("-"),
        );
        for open in [true, false] {
            if !open {
                app.selected_message = None;
            }
            let mail = if open { "mail open" } else { "no mail" };

            app.nav_collapsed = false;
            settle(&mut app);
            let rest = run(&mut app, 16);
            report(&format!("{mail} rest"), &rest);

            app.nav_collapsed = true;
            let out = run(&mut app, 16);
            report(&format!("{mail} slide out"), &out);

            settle(&mut app);
            app.nav_collapsed = false;
            let back = run(&mut app, 16);
            report(&format!("{mail} slide in"), &back);
        }
    }

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
    fn the_working_mode_round_trips_and_falls_back_to_the_build() {
        // The two names the file may carry, and nothing else: a typo or a mode
        // from a future version is no mode at all, so the build decides rather
        // than some mode being silently switched off.
        for mode in WorkingMode::ALL {
            assert_eq!(WorkingMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(WorkingMode::parse("debug"), Some(WorkingMode::Debug));
        assert_eq!(WorkingMode::parse("release"), Some(WorkingMode::Release));
        assert_eq!(WorkingMode::parse(""), None);
        assert_eq!(WorkingMode::parse("Debug"), None);
        assert_eq!(WorkingMode::parse("d3bug"), None);

        assert_eq!(
            WorkingMode::from_settings(None),
            WorkingMode::from_build(),
            "a file that names no mode follows the build"
        );
        assert_eq!(
            WorkingMode::from_settings(Some("nonsense")),
            WorkingMode::from_build()
        );
        assert_eq!(
            WorkingMode::from_settings(Some("release")),
            WorkingMode::Release
        );

        // Only Release reaches the network on its own.
        assert!(!WorkingMode::Debug.syncs_automatically());
        assert!(WorkingMode::Release.syncs_automatically());
        let expected = if cfg!(debug_assertions) {
            WorkingMode::Debug
        } else {
            WorkingMode::Release
        };
        assert_eq!(WorkingMode::from_build(), expected);
    }

    #[test]
    fn a_new_draft_is_written_from_the_open_account_or_the_first() {
        let mut personal = test_account("imap.example.com");
        personal.id = "personal".to_string();
        let accounts = vec![test_account("imap.example.com"), personal];

        // The account whose mail is open, while it is still configured.
        assert_eq!(
            compose_from(&accounts, Some("personal")).as_deref(),
            Some("personal")
        );
        // Nothing open — or an account that has since been removed — falls back
        // to the first configured one rather than to no sender at all.
        assert_eq!(compose_from(&accounts, None).as_deref(), Some("work"));
        assert_eq!(
            compose_from(&accounts, Some("deleted")).as_deref(),
            Some("work")
        );
        // With nothing configured there is no sender to pick, which is the one
        // case the window says so.
        assert_eq!(compose_from(&[], Some("work")), None);
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
            BlitzWait::Wait(GEOMETRY_SETTLE)
        );
        // Still settling: report what is left of the wait.
        assert_eq!(
            blitz_settle(
                Some(("page", start)),
                "page",
                start + Duration::from_millis(100)
            ),
            BlitzWait::Wait(GEOMETRY_SETTLE - Duration::from_millis(100))
        );
        // Held still long enough: render.
        assert_eq!(
            blitz_settle(Some(("page", start)), "page", start + GEOMETRY_SETTLE),
            BlitzWait::Start
        );
        // A geometry that changed again starts a fresh wait, however long the
        // previous one had been pending.
        assert_eq!(
            blitz_settle(
                Some(("page", start)),
                "page-moved",
                start + GEOMETRY_SETTLE * 4
            ),
            BlitzWait::Wait(GEOMETRY_SETTLE)
        );
    }

    /// One cached mail, as the fixtures build them: the subject follows the
    /// uid and `seen` is whatever the test needs of it.
    fn test_message(uid: &str, seen: bool) -> MailMessage {
        MailMessage {
            uid: uid.to_string(),
            subject: format!("Subject {uid}"),
            from: "Ada <ada@example.com>".to_string(),
            to: "me@example.com".to_string(),
            date: "2026-01-01".to_string(),
            timestamp: Some(1_700_000_000),
            seen,
            body_text: "body".to_string(),
            body_html: None,
            preview: "preview".to_string(),
            attachment_names: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    /// An app with `count` mails in one folder: no database, no workers, and
    /// nothing to sync. The frame tests below drive it headlessly.
    fn app_with_messages(ctx: &egui::Context, count: usize) -> KimoApp {
        let account = Account {
            id: "acct".to_string(),
            name: "Test".to_string(),
            protocol: Protocol::Imap,
            host: "imap.example.com".to_string(),
            port: 993,
            username: "ada".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
        };
        let messages = (0..count)
            .map(|index| test_message(&index.to_string(), false))
            .collect();
        let folder = storage::Folder {
            name: "INBOX".to_string(),
            uidvalidity: Some(1),
            synced: true,
            listed: true,
            can_load_older: false,
            messages,
        };
        let mut mailboxes = HashMap::new();
        mailboxes.insert(
            account.id.clone(),
            storage::Mailbox {
                folders: vec![folder],
            },
        );
        let mut app =
            KimoApp::assemble(ctx, vec![account], mailboxes, &storage::Settings::default());
        // The fixtures below measure geometry, and the warning the app carries
        // when *this machine's* database cannot be written stands at the top of
        // the middle column, over the first row. It is a fact about the machine,
        // not about the layout the test is pinning.
        app.storage_warning = None;
        app
    }

    /// An app whose one account holds the folders given as
    /// `(name, listed, mails)` — the shape the database hands `assemble` on
    /// startup, unread mail and all. The navigation tests drive it.
    fn app_with_folders(ctx: &egui::Context, folders: &[(&str, bool, usize)]) -> KimoApp {
        let mut app = app_with_messages(ctx, 0);
        let state = app.states.get_mut("acct").unwrap();
        state.mailbox.folders = folders
            .iter()
            .map(|(name, listed, count)| storage::Folder {
                name: name.to_string(),
                uidvalidity: Some(1),
                synced: *count > 0,
                listed: *listed,
                can_load_older: false,
                messages: (0..*count)
                    .map(|index| test_message(&format!("{name}-{index}"), false))
                    .collect(),
            })
            .collect();
        // The column opens on the mailboxes the last `LIST` named, which is how
        // `assemble` fills this in.
        state.folders = state
            .mailbox
            .folders
            .iter()
            .filter(|folder| folder.listed)
            .map(|folder| folder.name.clone())
            .collect();
        state.folders_changed();
        app
    }

    /// The wire names of a group of rows, for the assertions about which
    /// folders are in which.
    fn names_of(rows: &[FolderRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    /// Every run of text one headless frame of the app drew, for the tests that
    /// care what a column says.
    fn frame_texts(ctx: &egui::Context, app: &mut KimoApp) -> Vec<String> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut texts);
        }
        texts
    }

    fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, out);
                }
            }
            _ => {}
        }
    }

    /// A folder the account has not fetched yet — INBOX, offered by the
    /// navigation before the first sync — is empty as far as the list is
    /// concerned, and the column has to say which kind of empty it is rather
    /// than standing bare. (It stands bare if the list gives up on a folder it
    /// cannot find.)
    #[test]
    fn an_unfetched_folder_says_so_instead_of_standing_empty() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let account = Account {
            id: "acct".to_string(),
            name: "Test".to_string(),
            protocol: Protocol::Imap,
            host: "imap.example.com".to_string(),
            port: 993,
            username: "ada".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
        };
        // No folders at all: the account has never synced.
        let mut app = KimoApp::assemble(
            &ctx,
            vec![account],
            HashMap::new(),
            &storage::Settings::default(),
        );
        assert_eq!(app.selected_folder.as_deref(), Some("INBOX"));
        assert!(app.folder_messages_of("acct", "INBOX").is_none());
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts.iter().any(|text| text.contains("Not fetched yet")),
            "the column did not say the folder is unfetched: {texts:?}"
        );

        // A folder that has synced, with no mail in it, says the other thing.
        let mut app = app_with_messages(&ctx, 0);
        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("This folder is empty.")),
            "the column did not say the folder is empty: {texts:?}"
        );
    }

    /// The column opens on the folder list the database cached — what the last
    /// `LIST` named — rather than standing on an offered INBOX until a sync
    /// runs. That list is also where the two kinds of row come from: the
    /// mailboxes the server still has, and the ones it does not, which are here
    /// for the mail cached under them.
    #[test]
    fn the_column_opens_on_the_folder_list_the_database_cached() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(
            &ctx,
            &[("INBOX", true, 1), ("Sent", true, 0), ("Archive", false, 1)],
        );

        let texts = frame_texts(&ctx, &mut app);
        for name in ["INBOX", "Sent", "Archive"] {
            assert!(
                texts.iter().any(|text| text == name),
                "{name} should be in the column before any sync: {texts:?}"
            );
        }

        let nav = app.states.get_mut("acct").unwrap().nav();
        assert_eq!(names_of(&nav.listed), ["INBOX", "Sent"]);
        assert_eq!(names_of(&nav.unlisted), ["Archive"]);
        assert_eq!(nav.unread, 2, "the header counts every cached folder");
    }

    /// An account with nothing cached at all is still offered an INBOX:
    /// clicking it is what starts the first fetch. That row is the only one the
    /// column makes up — every other one comes from a folder the account holds.
    #[test]
    fn an_account_with_no_folders_yet_is_offered_its_inbox() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(&ctx, &[]);

        assert!(frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "INBOX"));
        let nav = app.states.get_mut("acct").unwrap().nav();
        assert!(nav.placeholder);
        assert_eq!(names_of(&nav.listed), ["INBOX"]);
        assert!(nav.unlisted.is_empty());
    }

    /// A sync that reports the mailboxes the column already shows is not a
    /// change: the rows stay the ones already derived. One that reports
    /// different mailboxes is, and it brings the counts with it.
    #[test]
    fn only_a_real_change_rebuilds_the_folder_rows() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(&ctx, &[("INBOX", true, 1)]);
        run_app_frame(&ctx, &mut app, Vec::new());
        assert_eq!(
            app.states.get_mut("acct").unwrap().nav().listed[0].unread,
            1
        );

        // Mail arrives behind the rows' back. Nothing says the account changed,
        // so nothing is derived again — which is exactly what makes a sync that
        // found the same folders cheap.
        let state = app.states.get_mut("acct").unwrap();
        state.mailbox.folders[0]
            .messages
            .push(test_message("late", false));
        assert_eq!(
            state.nav().listed[0].unread,
            1,
            "the rows are the ones already derived"
        );

        // A warm sync: the same mailbox list, so still nothing to derive.
        app.apply_event(Event::Folders {
            account_id: "acct".to_string(),
            folders: vec!["INBOX".to_string()],
        });
        assert_eq!(
            app.states.get_mut("acct").unwrap().nav().listed[0].unread,
            1
        );

        // A sync that found a different list rebuilds the rows, and the count
        // it brings with it is the current one.
        app.apply_event(Event::Folders {
            account_id: "acct".to_string(),
            folders: vec!["INBOX".to_string(), "Sent".to_string()],
        });
        let nav = app.states.get_mut("acct").unwrap().nav();
        assert_eq!(names_of(&nav.listed), ["INBOX", "Sent"]);
        assert_eq!(nav.listed[0].unread, 2);
    }

    /// A folder the server no longer lists keeps its row, in a state of its
    /// own, because the mail cached under it is the only copy of it here — and
    /// it stays openable.
    #[test]
    fn a_folder_the_server_dropped_keeps_its_row_and_its_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_folders(&ctx, &[("INBOX", true, 1), ("Archive", true, 2)]);

        // The next `LIST` does not name Archive.
        app.apply_event(Event::Folders {
            account_id: "acct".to_string(),
            folders: vec!["INBOX".to_string()],
        });
        let nav = app.states.get_mut("acct").unwrap().nav();
        assert_eq!(names_of(&nav.listed), ["INBOX"]);
        assert_eq!(names_of(&nav.unlisted), ["Archive"]);
        assert!(!nav.unlisted[0].listed);
        assert_eq!(nav.unlisted[0].unread, 2);

        // It is still drawn, and its mail is still there to open.
        assert!(frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Archive"));
        assert_eq!(
            app.folder_messages_of("acct", "Archive")
                .map(<[MailMessage]>::len),
            Some(2)
        );
    }

    /// Collapsing the navigation slides the column out over the frames that
    /// follow rather than making it vanish: the list takes its room a step at a
    /// time (`Panel::show_collapsible`). The slide leaves the width the
    /// splitter gave the column alone, so reopening brings that column back —
    /// not the sliver the slide ended on.
    #[test]
    fn collapsing_the_navigation_slides_it_out() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);

        run_app_frame(&ctx, &mut app, Vec::new());
        let open = app.list_column.left();
        assert!(open > 0.0, "the navigation should be open to begin with");

        // The first frame of the collapse has the column on its way out: it has
        // moved, and it has not arrived.
        app.nav_collapsed = true;
        run_app_frame(&ctx, &mut app, Vec::new());
        let sliding = app.list_column.left();
        assert!(
            sliding > 0.0 && sliding < open,
            "the column should be part-way out, not at {sliding} pt (from {open} pt)"
        );

        // A few frames later the slide has run its course and the list reaches
        // the window's edge.
        for _ in 0..20 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        assert_eq!(app.list_column.left(), 0.0, "the slide never finished");

        // Reopening runs the same slide backwards, and lands on the column's
        // own width rather than the width the slide was last seen at.
        app.nav_collapsed = false;
        for _ in 0..20 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        assert_eq!(
            app.list_column.left(),
            open,
            "the column reopened at a width of its own"
        );
    }

    /// One headless frame of an app under test, with the given input events.
    fn run_app_frame(ctx: &egui::Context, app: &mut KimoApp, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
    }

    /// An export answers with the path the worker wrote, and the app says so:
    /// one line above the message list. It is the only place the name the file
    /// ended up under is known — a name that was taken is numbered — and the
    /// line is timed, since the file itself does not go anywhere when it goes.
    #[test]
    fn a_finished_export_says_where_the_file_went() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        app.apply_event(Event::MessageExported {
            path: std::path::PathBuf::from("/home/ada/Downloads/Invoice 42.eml"),
        });

        let texts = frame_texts(&ctx, &mut app);
        assert!(
            texts
                .iter()
                .any(|text| text == "Saved to /home/ada/Downloads/Invoice 42.eml"),
            "{texts:?}"
        );

        // Past its time the line is gone, and the app has stopped asking to be
        // repainted for it.
        let stale = Instant::now()
            .checked_sub(EXPORT_NOTICE_TIME + Duration::from_millis(1))
            .expect("the machine has been up longer than the notice lasts");
        app.export_notice = app.export_notice.take().map(|(text, _)| (text, stale));
        let texts = frame_texts(&ctx, &mut app);
        assert!(app.export_notice.is_none(), "the notice outlived its time");
        assert!(
            !texts.iter().any(|text| text.starts_with("Saved to")),
            "{texts:?}"
        );
    }

    /// Clicking a row opens that mail: the list draws itself from the account
    /// state and only remembers what was clicked, so this is the test that the
    /// click still lands on the mail under the pointer (`folder_messages`).
    #[test]
    fn clicking_a_row_opens_that_mail() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 5);
        run_app_frame(&ctx, &mut app, Vec::new());

        // The second row: below the title bar's padding, one row down, inside
        // the message column (the navigation is 200 pt wide).
        let pos = egui::pos2(300.0, TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT * 1.5);
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            run_app_frame(
                &ctx,
                &mut app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
            );
        }

        assert_eq!(app.selected_message.as_deref(), Some("1"));
        // Opening it marks it seen on the spot — the row's own copy, not just
        // the database's.
        assert!(app.folder_messages_of("acct", "INBOX").unwrap()[1].seen);
    }

    /// Scroll the list, then click a row: the row the pointer lands on is the
    /// mail that opens, which is the whole point of laying out only the rows
    /// the viewport shows (`virtual_rows`) and of the ids that keep them
    /// stable as the list scrolls.
    #[test]
    fn clicking_a_row_after_scrolling_opens_the_mail_that_is_there() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 60);
        run_app_frame(&ctx, &mut app, Vec::new());

        let over_list = egui::pos2(300.0, 400.0);
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(over_list)]);
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -400.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
        );
        // The scroll area animates its offset: let it come to rest.
        for _ in 0..30 {
            run_app_frame(&ctx, &mut app, Vec::new());
        }
        let offset = app.list_scroll;
        assert!(offset > 0.0, "the list did not scroll");

        // Something comfortably inside the viewport, in the list's padding
        // below the title bar; the row there is the one the offset puts there.
        let y = TITLEBAR_HEIGHT + MESSAGE_ROW_HEIGHT * 2.5;
        let expected = ((y - TITLEBAR_HEIGHT + offset) / MESSAGE_ROW_HEIGHT).floor() as usize;
        let pos = egui::pos2(300.0, y);
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            run_app_frame(
                &ctx,
                &mut app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }],
            );
        }

        assert_eq!(
            app.selected_message.as_deref(),
            Some(expected.to_string().as_str())
        );
    }

    /// The floating `Load older mail` button: drawn only while the folder's
    /// marks say the server has mail below everything this machine holds, and
    /// only at the bottom of the list — the one place where "older" is what the
    /// eye is on. A list shorter than its viewport is at its bottom by
    /// definition, which is how the first two frames below are drawn.
    #[test]
    fn the_load_older_button_is_offered_at_the_bottom_of_the_list() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);

        // The marks have not been asked yet: nothing to load, nothing drawn.
        assert!(!frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Load older mail"));

        // The server said there is older mail (as a sync would).
        let folder = app
            .states
            .get_mut("acct")
            .unwrap()
            .mailbox
            .folders
            .iter_mut()
            .find(|folder| folder.name == "INBOX")
            .unwrap();
        folder.can_load_older = true;
        assert!(frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Load older mail"));

        // A long list starts at its top, which is not the bottom.
        let mut long = app_with_messages(&ctx, 60);
        long.states
            .get_mut("acct")
            .unwrap()
            .mailbox
            .folders
            .iter_mut()
            .find(|folder| folder.name == "INBOX")
            .unwrap()
            .can_load_older = true;
        run_app_frame(&ctx, &mut long, Vec::new());
        assert!(
            !frame_texts(&ctx, &mut long)
                .iter()
                .any(|text| text == "Load older mail"),
            "the button belongs to the bottom of the list, not to its top"
        );

        // Scrolled all the way down, it is there. The wheel only reaches a
        // scroll area the pointer is over.
        run_app_frame(
            &ctx,
            &mut long,
            vec![egui::Event::PointerMoved(egui::pos2(300.0, 400.0))],
        );
        run_app_frame(
            &ctx,
            &mut long,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -100_000.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
        );
        for _ in 0..30 {
            run_app_frame(&ctx, &mut long, Vec::new());
        }
        assert!(long.list_scroll > 0.0, "the list did not scroll");
        assert!(
            frame_texts(&ctx, &mut long)
                .iter()
                .any(|text| text == "Load older mail"),
            "the end of the list is where the button is offered"
        );
    }

    /// A click on the button asks the server, and the answer is what the button
    /// is drawn from: the event a job sends when it finds nothing below the
    /// floor takes it away again. (The job itself needs a server; this is the
    /// half of it that is the app's.)
    #[test]
    fn the_answer_about_older_mail_is_what_draws_the_button() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 3);

        app.apply_event(Event::OlderMail {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            available: true,
        });
        assert!(frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Load older mail"));

        app.apply_event(Event::OlderMail {
            account_id: "acct".to_string(),
            folder: "INBOX".to_string(),
            available: false,
        });
        assert!(!frame_texts(&ctx, &mut app)
            .iter()
            .any(|text| text == "Load older mail"));
    }

    /// One headless frame of an app under test at a given window width, with
    /// the shapes it drew — each with the clip rect it was drawn through, which
    /// is what the title-bar test below reads.
    fn frame_shapes_at(
        ctx: &egui::Context,
        app: &mut KimoApp,
        width: f32,
    ) -> Vec<egui::epaint::ClippedShape> {
        frame_shapes_with(ctx, app, width, Vec::new())
    }

    /// `frame_shapes_at` with input events — the pointer moved over a cell,
    /// say — for the tests that read what a hover drew.
    fn frame_shapes_with(
        ctx: &egui::Context,
        app: &mut KimoApp,
        width: f32,
        events: Vec<egui::Event>,
    ) -> Vec<egui::epaint::ClippedShape> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 900.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.ui(ui));
        output.textures_delta.clear();
        output.shapes
    }

    /// Every filled rectangle of a frame that is exactly this size, however
    /// deeply egui nested it. A button's hover fill is what the cell test below
    /// looks for: a `TITLEBAR_BUTTON_SIZE` square.
    fn fills_of_size(
        shapes: &[egui::epaint::ClippedShape],
        size: egui::Vec2,
    ) -> Vec<egui::epaint::RectShape> {
        let mut out = Vec::new();
        for clipped in shapes {
            collect_fills(&clipped.shape, size, &mut out);
        }
        out
    }

    fn collect_fills(
        shape: &egui::Shape,
        size: egui::Vec2,
        out: &mut Vec<egui::epaint::RectShape>,
    ) {
        match shape {
            egui::Shape::Rect(rect) if rect.rect.size() == size => out.push(rect.clone()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_fills(shape, size, out);
                }
            }
            _ => {}
        }
    }

    /// What the pointer fills when it is put at `center`: the cell under it, if
    /// the bar drew one. The pointer is moved in one frame and the shapes are
    /// read from the next — egui hit-tests against the last frame's rects, so a
    /// single frame would still be filling the cell the pointer left.
    fn fill_under_the_pointer(
        ctx: &egui::Context,
        app: &mut KimoApp,
        width: f32,
        center: egui::Pos2,
    ) -> Option<egui::epaint::RectShape> {
        frame_shapes_with(ctx, app, width, vec![egui::Event::PointerMoved(center)]);
        let shapes = frame_shapes_with(ctx, app, width, Vec::new());
        fills_of_size(&shapes, egui::Vec2::splat(TITLEBAR_BUTTON_SIZE))
            .into_iter()
            .find(|fill| fill.rect.center() == center)
    }

    /// The title bar is laid out as groups, and no group's rect crosses
    /// another's — whatever the window's width. That is the whole of why a
    /// narrow window no longer stacks the search box and the mail actions on
    /// top of the window controls: they used to be one run of widgets that
    /// simply drew past the end of the bar.
    #[test]
    fn the_title_bars_groups_never_cross() {
        let bar = |width: f32| {
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT))
        };
        for width in [
            0.0, 40.0, 126.0, 167.0, 168.0, 169.0, 300.0, 480.0, 640.0, 900.0, 1400.0, 4000.0,
        ] {
            let rects = titlebar_rects(bar(width), 3);
            assert_eq!(
                rects.content.min,
                bar(width).min,
                "{width} pt: the content rect does not start at the bar's own corner"
            );
            assert!(
                rects.content.width() >= 0.0 && rects.content.height() == TITLEBAR_HEIGHT,
                "{width} pt: the content rect is inside out: {rects:?}"
            );
            // No two groups overlap. Where the bar is too narrow to hold the
            // fixed ones at all, `content` is simply empty and the bar shows
            // the cog and the window controls alone.
            for (a, b) in [
                (rects.content, rects.cog),
                (rects.cog, rects.controls),
                (rects.content, rects.controls),
            ] {
                assert!(
                    !a.intersect(b).is_positive(),
                    "{width} pt: {a:?} overlaps {b:?}"
                );
            }
            // And while there is room for them, they are in that order: what
            // the right end needs is the cog, the gap, the three controls and
            // the margin from the window's edge.
            let right_end = TITLEBAR_EDGE_MARGIN
                + TITLEBAR_BUTTON_SIZE
                + TITLEBAR_BUTTON_GAP
                + titlebar_cells_width(3);
            if bar(width).width() >= right_end {
                assert!(
                    rects.content.right() <= rects.cog.left()
                        && rects.cog.right() <= rects.controls.left(),
                    "{width} pt: the groups are out of order: {rects:?}"
                );
            }
            // The cells keep their size whatever the bar does: a window too
            // narrow to show them all cuts the ones at its edge off rather
            // than shrinking every control into an unclickable sliver.
            assert_eq!(rects.cog.width(), TITLEBAR_BUTTON_SIZE, "{width} pt");
            assert_eq!(
                rects.controls.width(),
                titlebar_cells_width(3),
                "{width} pt"
            );
            // The run ends clear of the window's corner rather than on it: the
            // close button's fill is a rounded square now, and flush it would
            // reach the corner the compositor cut.
            assert_eq!(
                rects.controls.right(),
                bar(width).right() - TITLEBAR_EDGE_MARGIN,
                "{width} pt"
            );
        }
    }

    /// The mail actions are one group and their order is the bar's: Export
    /// stands immediately before Delete — the mail is written out before it is
    /// thrown away — and every action says what it is, the two wired ones
    /// saying what they do.
    #[test]
    fn export_stands_immediately_before_delete() {
        assert_eq!(
            MailAction::ALL,
            [
                MailAction::Reply,
                MailAction::ReplyAll,
                MailAction::Forward,
                MailAction::Label,
                MailAction::Flag,
                MailAction::Export,
                MailAction::Delete,
            ]
        );
        for action in MailAction::ALL {
            assert!(!action.label().is_empty());
            assert!(!action.tooltip().is_empty());
        }
        // One says where the file goes, the other that it cannot be undone.
        assert!(MailAction::Export.tooltip().contains(".eml"));
        assert!(MailAction::Delete.tooltip().contains("server"));
    }

    /// Every button in the title bar is the same cell: a
    /// `TITLEBAR_BUTTON_SIZE` square, centred in the bar's height and
    /// `TITLEBAR_BUTTON_GAP` from the next one, whose hover fill is a small
    /// rounded square. This walks the whole bar — the left group, the run of
    /// mail actions, the cog and the window controls — and puts the pointer on
    /// each cell in turn, which is also what says every one of them is a real
    /// hit target rather than a glyph painted on the chrome.
    ///
    /// The two runs are measured with `titlebar_cells_width`, which is what the
    /// fit plans with and the bar draws with; the left group's cells are found
    /// from the layout its row makes (the edge margin, the brand icon, then a
    /// cell every pitch).
    #[test]
    fn every_button_in_the_title_bar_is_the_same_cell() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);
        // The run is enabled by the mail the reading pane shows.
        app.selected_message = Some("0".to_string());

        // One frame first: the run starts where the search box ended, and the
        // box's rect is only known once the bar has drawn it.
        frame_shapes_at(&ctx, &mut app, 1400.0);
        let search = ctx
            .read_response(egui::Id::new("titlebar-search"))
            .expect("the bar drew no search box")
            .rect;
        let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, TITLEBAR_HEIGHT));
        let rects = titlebar_rects(bar, 3);
        let pitch = TITLEBAR_BUTTON_SIZE + TITLEBAR_BUTTON_GAP;
        let middle = TITLEBAR_HEIGHT / 2.0;

        // The left group: the brand icon, then the nav toggle, compose and —
        // Debug mode's alone — sync, a cell every pitch after it.
        let first_left = TITLEBAR_EDGE_MARGIN + BRAND_ICON_SIZE + TITLEBAR_BUTTON_GAP;
        let left_cells = if app.working_mode == WorkingMode::Debug {
            3
        } else {
            2
        };
        let mut cells: Vec<egui::Pos2> = (0..left_cells)
            .map(|index| {
                egui::pos2(
                    first_left + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                    middle,
                )
            })
            .collect();

        // The mail actions: the run starts where the search box ended, the
        // fit's clear chrome further right.
        let actions = titlebar_cells_width(MailAction::ALL.len());
        let fit = titlebar_fit(rects.cog.min.x - search.left(), actions);
        assert!(fit.actions, "the bar gave the actions up at 1400 pt");
        let left = search.right() + fit.padding;
        for index in 0..MailAction::ALL.len() {
            cells.push(egui::pos2(
                left + index as f32 * pitch + TITLEBAR_BUTTON_SIZE / 2.0,
                middle,
            ));
        }

        // The right end: the cog, then the three window controls, which end
        // `TITLEBAR_EDGE_MARGIN` short of the window's own edge.
        cells.push(rects.cog.center());
        for index in 0..3 {
            cells.push(egui::pos2(
                rects.controls.right() - TITLEBAR_BUTTON_SIZE / 2.0 - index as f32 * pitch,
                middle,
            ));
        }

        for center in cells {
            let filled = fill_under_the_pointer(&ctx, &mut app, 1400.0, center)
                .unwrap_or_else(|| panic!("nothing filled under {center:?}"));
            assert_eq!(filled.rect.center(), center);
            assert_eq!(
                filled.corner_radius,
                egui::CornerRadius::same(TITLEBAR_BUTTON_CORNER_RADIUS),
                "{center:?}: the hover fill is not the rounded cell"
            );
        }

        // The run of actions is the width the fit planned: it ends before the
        // bar's right end begins.
        assert!(
            left + actions <= rects.cog.min.x,
            "the run draws {} pt into the bar's right end",
            left + actions - rects.cog.min.x
        );
    }

    /// What the bar gives up when it runs out of room, in the order it gives it
    /// up — plus the rule that makes the whole thing safe: the fit never plans
    /// a middle group wider than the room it was given. (The other half of that
    /// rule is the clip every group is drawn through, `clip_to`.)
    #[test]
    fn the_title_bar_gives_its_room_up_in_one_order() {
        let actions = titlebar_cells_width(MailAction::ALL.len());
        let widest = TITLEBAR_SEARCH_WIDTH;
        let narrowest = TITLEBAR_SEARCH_FLOOR;

        // A bar with room to spare: the box at its full width, the actions a
        // clear gap away, and the rest of the room left to the drag surface.
        assert_eq!(
            titlebar_fit(4000.0, actions),
            TitlebarFit {
                search: Some(widest),
                actions: true,
                padding: MAIL_ACTION_PADDING,
            }
        );

        // Narrower: the gap closes before anything is lost, and both the box
        // and the seven cells are whole when the two are at their closest.
        let tight = widest + MAIL_ACTION_MIN_PADDING + actions;
        assert_eq!(
            titlebar_fit(tight, actions),
            TitlebarFit {
                search: Some(widest),
                actions: true,
                padding: MAIL_ACTION_MIN_PADDING,
            }
        );

        // Narrower still: the actions go, as the one group they are, and the
        // box takes back the room they were using instead of being squeezed
        // into what is left of it.
        assert_eq!(
            titlebar_fit(tight - 1.0, actions),
            TitlebarFit {
                search: Some(widest),
                actions: false,
                padding: 0.0,
            }
        );

        // Then the box narrows — down to its floor, and no further.
        assert_eq!(
            titlebar_fit(narrowest + 30.0, actions).search,
            Some(narrowest + 30.0)
        );
        assert_eq!(titlebar_fit(narrowest, actions).search, Some(narrowest));
        assert_eq!(
            titlebar_fit(narrowest - 1.0, actions),
            TitlebarFit {
                search: None,
                actions: false,
                padding: 0.0,
            }
        );

        // Every width in between, not just those: what the fit plans fits the
        // room it was given, and neither the box nor the actions ever come back
        // smaller for a wider bar (a window being resized from flickering
        // between two shapes).
        let mut previous = TitlebarFit {
            search: Some(0.0),
            actions: false,
            padding: 0.0,
        };
        for step in 0..6000 {
            let room = step as f32 / 4.0;
            let fit = titlebar_fit(room, actions);
            assert!(
                fit.width(actions) <= room + 0.01,
                "at {room} pt the fit plans more than the room: {fit:?}"
            );
            assert!(
                fit.search.unwrap_or(0.0) >= previous.search.unwrap_or(0.0)
                    && (fit.actions || !previous.actions),
                "at {room} pt a wider bar lost something: {fit:?} after {previous:?}"
            );
            if fit.actions {
                assert!(
                    (MAIL_ACTION_MIN_PADDING..=MAIL_ACTION_PADDING).contains(&fit.padding),
                    "at {room} pt the gap is not a gap: {fit:?}"
                );
            }
            if let Some(search) = fit.search {
                assert!(
                    (narrowest..=widest).contains(&search),
                    "at {room} pt the box is not a box: {fit:?}"
                );
            }
            previous = fit;
        }
    }

    /// The bar draws its groups in the rects `titlebar_rects` carves out of
    /// it — every group clipped to its own — so nothing it draws is ever in
    /// the window controls' half of the bar, whatever the window's width. This
    /// is the frame-level half of the two tests above: the arithmetic can be
    /// right while the bar draws from the window's left edge anyway.
    ///
    /// The shapes are sorted by the clip they were drawn through, which is the
    /// group rect itself, so the test can tell the bar's right end from a
    /// widget that has wandered into it — which is exactly what used to
    /// happen, when the bar was one run of widgets that drew past the end of
    /// the window and the mail cells landed on minimize / maximize / close.
    #[test]
    fn nothing_in_the_title_bar_is_drawn_into_the_window_controls() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);

        for width in [1400.0, 1000.0, 900.0, 800.0, 560.0, 480.0, 400.0, 300.0] {
            let bar =
                egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, TITLEBAR_HEIGHT));
            let rects = titlebar_rects(bar, 3);
            // The bar's own right end: the cog and the window controls, from
            // the cog's left edge to the window's corner. That is the rect the
            // right group draws through — its cells are `TITLEBAR_BUTTON_SIZE`
            // squares inside it — so a shape clipped to it is the group's own,
            // and anything else that reaches into it is what this catches.
            let right_end =
                egui::Rect::from_min_max(egui::pos2(rects.cog.left(), bar.top()), bar.max);
            for clipped in frame_shapes_at(&ctx, &mut app, width) {
                // What the frame actually shows of the shape: the bar clips
                // its groups, and egui clips the window.
                let visible = clipped
                    .shape
                    .visual_bounding_rect()
                    .intersect(clipped.clip_rect);
                if !visible.is_positive() {
                    continue;
                }
                // Only what lives in the bar's own band, so that the columns
                // the bar floats over — taller than it, and drawn first — are
                // not mistaken for something in the bar...
                if visible.top() < bar.top() - 0.5 || visible.bottom() > bar.bottom() + 0.5 {
                    continue;
                }
                // ...and the bar's own fill, which spans it by design.
                if visible.width() >= bar.width() - 1.0 {
                    continue;
                }
                if right_end.contains_rect(clipped.clip_rect) {
                    continue;
                }
                assert!(
                    visible.right() <= rects.cog.left() + 0.5,
                    "at {width} pt the bar drew {visible:?} into the window controls, which \
                     start at {}: it was drawn through {clip:?}",
                    rects.cog.left(),
                    clip = clipped.clip_rect
                );
            }
        }
    }

    /// The box the fit plans for and the box the bar draws are the same box:
    /// `titlebar_fit`'s search width is what the widget comes to, margins and
    /// all. Two pieces of arithmetic decide that — the fit's constants, and
    /// egui's `desired_width` — and they are not the same arithmetic, so this
    /// is what keeps them from drifting. (`desired_width` is the *whole* box:
    /// the margins come out of it, they are not added to it.)
    #[test]
    fn the_search_box_is_as_wide_as_the_fit_planned() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 4);
        let actions = titlebar_cells_width(MailAction::ALL.len());

        // A window with room for the whole middle group: the box is at the
        // width the fit gives a bar that has nothing to give up.
        frame_shapes_at(&ctx, &mut app, 1400.0);
        let drawn = ctx
            .read_response(egui::Id::new("titlebar-search"))
            .expect("the bar drew no search box at 1400 pt")
            .rect;
        let planned = titlebar_fit(4000.0, actions).search.unwrap();
        assert!(
            (drawn.width() - planned).abs() < 0.5,
            "the fit plans a {planned} pt box and the bar drew a {} pt one",
            drawn.width()
        );
    }

    /// Dragging the splitter with the pointer, the way a user does: the pane
    /// narrows, and the body keeps the width it settled at until the drag has
    /// stopped — which is the whole fix for a drag that used to re-lay the mail
    /// out on every frame.
    #[test]
    fn a_splitter_drag_holds_the_body_until_the_geometry_settles() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 20);
        let html =
            "<p>one two three four five six seven eight nine ten eleven twelve</p>".repeat(40);
        app.states
            .get_mut("acct")
            .unwrap()
            .mailbox
            .folders
            .first_mut()
            .unwrap()
            .messages
            .first_mut()
            .unwrap()
            .body_html = Some(html);
        app.selected_message = Some("0".to_string());

        let pane_width = |ctx: &egui::Context| {
            egui::containers::panel::PanelState::load(ctx, egui::Id::new("mail_content"))
                .expect("the reading pane stored its width")
                .outer_rect
                .width()
        };
        run_app_frame(&ctx, &mut app, Vec::new());
        let settled = pane_width(&ctx);
        // The body is narrower than the pane by the card's gutter, its padding
        // and the scroll bar; that inset is what a reflow has to keep.
        let body_width = app.body.width.expect("the body was laid out");
        let inset = settled - body_width;
        assert!(inset > 0.0, "the body should be inside the pane");

        // Press on the splitter and pull it right — narrowing the pane, which
        // is the drag that used to cost a reflow a frame. The grab strip
        // straddles the pane's left edge, but the message list's own widget
        // reaches exactly to it and is drawn later, so the half that is
        // reachable is the one just inside the pane (`resize_grab_radius_side`
        // is 3 pt).
        let splitter = egui::pos2(1400.0 - settled + 1.5, 400.0);
        run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(splitter)]);
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos: splitter,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
        );
        // egui stores a panel's width only when the drag ends (`PanelState` is
        // deliberately left at the pre-drag size), so the live edge is the one
        // the message list recorded: its right side is the pane's left.
        let pane_left = |app: &KimoApp| app.list_column.right();
        let start_left = pane_left(&app);
        let mut pos = splitter;
        for _ in 0..10 {
            pos.x += 8.0;
            run_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
            // The mail does not follow the pointer: it is still laid out for
            // the width it settled at, however far the pane has moved.
            assert_eq!(
                app.body.width,
                Some(body_width),
                "the body reflowed mid-drag"
            );
        }
        assert!(
            pane_left(&app) > start_left + 1.0,
            "the pane did not follow the drag"
        );
        run_app_frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );

        // The drag is over: the pane has stored the width it came to rest at,
        // and once the geometry has held still for the settle time the body
        // reflows once for it.
        let narrower = pane_width(&ctx);
        assert!(narrower < settled, "the pane did not settle narrower");
        std::thread::sleep(GEOMETRY_SETTLE + Duration::from_millis(20));
        run_app_frame(&ctx, &mut app, Vec::new());
        let reflowed = app.body.width.expect("the body is still laid out");
        assert!(
            (reflowed - (narrower - inset)).abs() < 1.0,
            "the body reflowed to {reflowed}, expected {}",
            narrower - inset
        );
    }

    /// The scope the body is laid out in is what holds its width: the card has
    /// already drawn its headers across the pane, and egui refuses to make a ui
    /// narrower than what it has drawn, so this is the check that the mail is
    /// really wrapped for the width the cache holds and not for the pane's.
    #[test]
    fn the_body_is_wrapped_for_the_width_the_cache_holds() {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx, None, None);
        let mut app = app_with_messages(&ctx, 1);
        let html = format!(
            "<p>{}</p>",
            "the quick brown fox jumps over the lazy dog ".repeat(40)
        );
        app.states
            .get_mut("acct")
            .unwrap()
            .mailbox
            .folders
            .first_mut()
            .unwrap()
            .messages
            .first_mut()
            .unwrap()
            .body_html = Some(html.clone());
        let msg = app.folder_messages_of("acct", "INBOX").unwrap()[0].clone();

        let images: HashMap<String, egui::TextureHandle> = HashMap::new();
        let inline: HashMap<String, egui::TextureHandle> = HashMap::new();
        let sheets: HashMap<String, String> = HashMap::new();
        // The height of a body laid out inside a `pane`-wide ui, through a
        // scope of `laid_out` width — the nesting `mail_body` makes.
        let height = |app: &mut KimoApp, pane: f32, laid_out: f32, body: Option<&MailMessage>| {
            let mut measured = 0.0_f32;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 4000.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let pane_rect =
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(pane, 4000.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(pane_rect), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    match body {
                        Some(msg) => app.mail_body(ui, msg),
                        None => {
                            let body_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(laid_out, ui.max_rect().bottom()),
                            );
                            let document = crate::html::Document::parse(&html);
                            let mut html_ctx = crate::html::HtmlContext {
                                images: &images,
                                inline_images: &inline,
                                stylesheets: &sheets,
                                missing_images: Vec::new(),
                                missing_stylesheets: Vec::new(),
                            };
                            ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                                crate::html::render_document(ui, &document, &mut html_ctx);
                            });
                        }
                    }
                    measured = ui.min_rect().height();
                });
            });
            output.textures_delta.clear();
            measured
        };

        // The same body at two widths: the narrower pane wraps it taller, so
        // the two heights tell the two layouts apart.
        let wide = height(&mut app, 500.0, 500.0, None);
        let narrow = height(&mut app, 300.0, 300.0, None);
        assert!(wide < narrow - 20.0, "{wide} vs {narrow}");

        // The body has settled at 500 pt…
        app.body.get(
            &ctx,
            "INBOX/0",
            body_fingerprint(&html),
            &html,
            500.0,
            Instant::now(),
        );
        // …and the pane is 300 pt wide now. The mail keeps the layout it has:
        // the height is the wide one, not the height the pane would wrap it to.
        let held = height(&mut app, 300.0, 300.0, Some(&msg));
        assert!(
            (held - wide).abs() < 2.0,
            "laid out at {held}, expected the held width's {wide} (narrow would be {narrow})"
        );
    }

    #[test]
    fn the_body_keeps_its_width_while_the_pane_moves() {
        let ctx = egui::Context::default();
        let start = Instant::now();
        let mut cache = BodyCache::default();
        let html = "<p>Hello</p>";
        let fingerprint = body_fingerprint(html);
        let width_at = |cache: &mut BodyCache, wanted, now| {
            cache.get(&ctx, "INBOX/1", fingerprint, html, wanted, now).1
        };

        // A body the cache has not seen is laid out at the pane's own width.
        assert_eq!(width_at(&mut cache, 500.0, start), 500.0);
        // A pane that moved is not a reflow: the body keeps the width it has,
        // and the clock starts.
        assert_eq!(width_at(&mut cache, 480.0, start), 500.0);
        // Still moving: same width, clock restarted.
        assert_eq!(
            width_at(&mut cache, 460.0, start + Duration::from_millis(100)),
            500.0
        );
        // Held still for the settle time: the body reflows, once.
        assert_eq!(
            width_at(
                &mut cache,
                460.0,
                start + Duration::from_millis(100) + GEOMETRY_SETTLE
            ),
            460.0
        );
        // The width it has now is not a reflow again.
        assert_eq!(
            width_at(
                &mut cache,
                460.0,
                start + Duration::from_millis(100) + GEOMETRY_SETTLE * 2
            ),
            460.0
        );
        // And a width the pane only visits for a moment never reaches the body.
        assert_eq!(
            width_at(
                &mut cache,
                700.0,
                start + Duration::from_millis(100) + GEOMETRY_SETTLE * 3
            ),
            460.0
        );
    }

    #[test]
    fn the_body_cache_reparses_a_body_that_changed() {
        let ctx = egui::Context::default();
        let now = Instant::now();
        let mut cache = BodyCache::default();
        let first = "<p>one</p>";
        let second = "<p>two</p>";
        let laid_out_at = |cache: &mut BodyCache, identity: &str, html: &str, wanted| {
            cache
                .get(&ctx, identity, body_fingerprint(html), html, wanted, now)
                .1
        };

        // First sight of a body: laid out at the pane's own width.
        assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 400.0), 400.0);
        // The same body a frame later, with the pane moved: the width it has.
        assert_eq!(laid_out_at(&mut cache, "INBOX/1", first, 380.0), 400.0);
        // Another mail is a new body, and a new body is laid out at once
        // rather than held at the width the old one was laid out for.
        assert_eq!(cache.identity, "INBOX/1");
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", first, 320.0), 320.0);
        assert_eq!(cache.identity, "INBOX/2");
        // So is another body under the same uid: a re-fetch can replace a
        // body the pane is already showing.
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 300.0), 300.0);
        assert_eq!(cache.fingerprint, body_fingerprint(second));
        // Switching renderer, or opening a mail with no HTML at all, drops it.
        cache.forget();
        assert!(cache.document.is_none());
        assert_eq!(laid_out_at(&mut cache, "INBOX/2", second, 280.0), 280.0);
    }

    #[test]
    fn a_search_waits_for_the_typing_to_stop() {
        let start = Instant::now();
        let typed = SearchQuery {
            text: "invoice".to_string(),
            ..SearchQuery::default()
        };
        // The first keystroke starts the clock…
        assert_eq!(search_settle(None, &typed, start), SearchWait::Reset);
        // …which then waits…
        assert_eq!(
            search_settle(Some((&typed, start)), &typed, start + SEARCH_SETTLE / 2),
            SearchWait::Wait
        );
        // …until the query has held still.
        assert_eq!(
            search_settle(Some((&typed, start)), &typed, start + SEARCH_SETTLE),
            SearchWait::Run
        );
        // Another keystroke starts a new clock instead of running the old query.
        let longer = SearchQuery {
            text: "invoices".to_string(),
            ..SearchQuery::default()
        };
        assert_eq!(
            search_settle(Some((&typed, start)), &longer, start + SEARCH_SETTLE),
            SearchWait::Reset
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
        // epaint panics on a dropped `TexturesDelta` (egui 0.36).
        let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            let color = egui::Color32::WHITE;
            let short = truncate_galley(ui.ctx(), "hi", 14.0, color, 500.0);
            assert_eq!(short.text(), "hi");

            let long = truncate_galley(
                ui.ctx(),
                &"很长的一段中文内容。".repeat(10),
                14.0,
                color,
                100.0,
            );
            assert!(long.size().x <= 100.0);
            assert!(long.elided, "the text should say it was cut");
            assert_eq!(long.rows.len(), 1, "the row should stay a single line");
            assert_eq!(
                long.rows[0].glyphs.last().map(|glyph| glyph.chr),
                Some('…'),
                "the row should end in the ellipsis"
            );
        });
        frame.textures_delta.clear();
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

    #[test]
    fn the_result_count_says_when_it_is_a_floor() {
        assert_eq!(results_label(0), "0 results");
        assert_eq!(results_label(1), "1 result");
        assert_eq!(results_label(12), "12 results");
        // At the limit the database stopped asking, so the count is a floor.
        assert_eq!(results_label(SEARCH_LIMIT), "200+ results");
    }
}
