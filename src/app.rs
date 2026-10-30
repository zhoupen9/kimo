//! The egui application root: shared state, shared models, construction and the
//! one-frame `ui` orchestration. Network results arrive as `sync::Event`s
//! drained at the top of every frame; opening a socket happens only in worker
//! threads.
//!
//! The UI itself is split by concern under `src/app/`: titlebar, navigation,
//! list, reading, compose, preferences, menus, search, blitz, events/actions,
//! and the layout/widgets/icons/util leaves. This file keeps the types those
//! modules share (`KimoApp`, the account/list/reading state, forms and caches)
//! plus the module wiring and `KimoApp::ui`; the tests are under `tests/`.
//!
//! Layout: custom title bar on top (the nav toggle, the compose button and the
//! search box, then the mail actions — reply, reply all,
//! forward, label, flag, export, delete — as one flat group a fixed gap to its
//! right, and at the right end the sync cell, the preferences
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
//! worker thread. The Graphics page picks which of the machine's cards Kimo
//! draws with, and with it which Vulkan drivers are loaded at all
//! (`crate::gpus`). The pen is the compose card: a draft, and the Send button
//! that hands it to a worker (`Job::SendMessage`) — the one modal that can put
//! a mail on the wire, and the one that says what the server said when it did
//! not.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::folders::{self, FolderKind};
use crate::fonts::FontFamily;
use crate::glass::Glass;
use crate::gpus::Status;
use crate::models::{Account, Attachment, MailMessage, Protocol, SmtpLogin, SmtpSettings, TlsMode};
use crate::outgoing::{self, Outgoing, OutgoingAttachment};
use crate::provider::vendor::{self, VendorKey};
use crate::search::{Scope, SearchHit, SearchQuery, SEARCH_LIMIT};
use crate::storage::{self, Settings};
use crate::sync::{self, Event, Job, SentCopy};
use crate::theme::{self, Accent, ColorScheme, Theme};
use crate::threads::{self, Thread};

mod actions;
mod blitz;
mod compose;
mod events;
mod icons;
mod layout;
mod list;
mod menus;
mod navigation;
mod preferences;
mod reading;
mod search;
mod titlebar;
mod util;
mod widgets;

use self::blitz::*;
use self::compose::*;
use self::events::*;
use self::icons::*;
use self::layout::*;
use self::list::*;
use self::menus::*;
#[cfg(test)]
use self::preferences::*;
use self::reading::*;
use self::search::*;
use self::titlebar::*;
use self::util::*;
use self::widgets::*;

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

/// Which paper a mail's Blitz page is drawn on, when the reader has said so.
///
/// White is the paper mail is written for and the default; `Dark` is the
/// surface a reader who does not want white asks for. The choice is per mail and
/// lasts the session — nothing about it is written to `settings.json` — and it
/// only ever shows where the mail itself painted nothing: a mail that declares an
/// html or body background has it painted across the whole page by the renderer
/// (see `draw_blitz_page`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Paper {
    #[default]
    White,
    Dark,
}

impl Paper {
    /// The colour the pane fills the page with before the mail's own raster.
    fn color(self) -> egui::Color32 {
        match self {
            Paper::White => egui::Color32::WHITE,
            Paper::Dark => BLITZ_PAPER_DARK,
        }
    }

    /// What the click on the mail's paper cell would do, which is what the cell
    /// says — the way the flag cell says "Unflag…" while the flag stands.
    fn tooltip(self) -> &'static str {
        match self {
            Paper::White => "Use a dark paper for this mail",
            Paper::Dark => "Use white paper for this mail",
        }
    }
}

/// Pages of the preferences modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefsTab {
    General,
    Accounts,
    Plugins,
    Appearance,
    Graphics,
    About,
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

    /// The hover text while there is mail to act on. The actions that do
    /// something say what it is: Export where the file goes, Delete that it
    /// cannot be undone — it expunges the mail on the server as well — and Flag
    /// what the mark does, which the bar swaps for its opposite while the flag
    /// stands (`titlebar_mail_actions`).
    fn tooltip(self) -> &'static str {
        match self {
            MailAction::Export => "Export — asks where to save the original mail as a .eml file",
            MailAction::Delete => "Delete — removes this mail from the server too",
            // The bar swaps this one for "Unflag…" while the flag stands: a
            // flag is a state, and the click is what changes it.
            MailAction::Flag => "Flag — marks the mail with a warning colour",
            other => other.label(),
        }
    }

    /// The flag cell's hover text, which is the one action whose wording turns
    /// over with the mail's state: a flagged mail's cell says what the click
    /// takes off rather than what a flag is. Both flag cells use it — the title
    /// bar's and the one a list row offers.
    fn flag_tooltip(flagged: bool) -> &'static str {
        if flagged {
            "Unflag — takes the warning colour off this mail"
        } else {
            "Flag — marks the mail with a warning colour"
        }
    }
}

/// What a message row's context menu asked for. The same actions the title
/// bar's run offers, plus Open: a right-click is about the row under the
/// pointer, whether or not that row happens to be the one the pane is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MailMenuAction {
    Open,
    Reply,
    ReplyAll,
    Forward,
    Flag,
    Export,
    Delete,
}

/// The right-click menu a mail body can show, with the mail it belongs to.
/// The target is what the pointer landed on — text, a link, an image — and the
/// key is the mail whose body it is, because the menu is drawn a frame later
/// and the pane may already be looking at something else.
struct BodyMenu {
    pos: egui::Pos2,
    target: crate::html::BodyContext,
    key: MailKey,
    /// The mail's text, taken when the menu opened: "Copy message text" is
    /// about this mail, and the body's own copy is not read again per frame.
    text: Option<String>,
}

/// What a body context menu asked for. Copying a selection is an event rather
/// than a string here: the text belongs to egui's label selection, and the
/// event is what asks the labels to copy it.
enum BodyMenuAction {
    CopySelection,
    Copy(String),
    Open(String),
}

/// What a compose card is: a fresh mail, or one of the three ways of
/// answering another. Nothing about the card depends on it but its heading
/// — the fields are the same fields — and what the three are prefilled
/// with is the constructors' own (`ComposeDraft::reply` and its two).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComposeKind {
    New,
    Reply,
    ReplyAll,
    Forward,
}

impl ComposeKind {
    fn label(self) -> &'static str {
        match self {
            ComposeKind::New => "New mail",
            ComposeKind::Reply => "Reply",
            ComposeKind::ReplyAll => "Reply all",
            ComposeKind::Forward => "Forward",
        }
    }
}

/// What a compose card's own buttons said: nothing, Discard (the draft goes
/// with the card), or Send (the caller's to act on, the draft back in
/// place).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComposeOutcome {
    Shown,
    Discard,
    Send,
}

/// Where a compose card is shown. The title bar's mail actions bring theirs
/// up as a popup over everything, the way the pen's new mail always has;
/// the buttons on a mail's own header open theirs *on the spot*: one more
/// card of the reading pane, right under the mail they answer
/// (`compose_inline_card`), which stays on screen above its answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComposeSpot {
    Pane,
    Popup,
}

/// A mail being written in the compose window.
///
/// Nothing about it is persisted: the draft is exactly as long-lived as the
/// window holding it, and closing the window — by its ✕ or by Discard — drops
/// it. Sending is what the window's Send does (`send_draft`): the mail goes to
/// a worker as a `Job::SendMessage`, and the draft stays right here until the
/// server has it. The fields are here rather than in widgets' own state because
/// a window that is closed and opened again must start empty.
struct ComposeDraft {
    /// What the card is — the heading, and nothing else (see
    /// [`ComposeKind`]).
    kind: ComposeKind,
    /// The account the mail would go out from, as the window's From list has
    /// it. `None`, with no account configured, is the one case the window
    /// says so instead of offering a choice.
    from: Option<String>,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
    /// The files that would go out with the mail, in the order they were
    /// attached: the card draws a row per file, and the worker reads them when
    /// the mail is sent (`outgoing::OutgoingAttachment`).
    attachments: Vec<OutgoingAttachment>,
    /// What went wrong with the last file that was asked for — a path that is
    /// not there, a directory, or a desktop with no dialog to open — shown under
    /// the card until the next file is attached.
    attach_error: Option<String>,
    /// Whether the card has its Cc and Bcc lines folded out under To, which is
    /// what the chevron at the end of that row opens and closes. Both
    /// containers fold them away by default (`compose_to_row`): the answer
    /// under its mail is a letter, and the popup's form is From, To, Subject,
    /// the files and the body until a copy is asked for.
    copies_open: bool,
    /// Set on the first frame the card is shown, so the keyboard goes where
    /// the card's own next move is without a click: To on the popup's form,
    /// and on a forward, which has no recipient yet; the body on an answer
    /// that already knows who it goes to, which is where the reply itself is
    /// written (`compose_card`). The pane reads the same flag to bring the new
    /// card into view on that frame (`compose_inline_card`).
    focus: bool,
    /// True while a worker is talking to the submission server: the card's
    /// boxes and buttons are disabled, and Send says `Sending…`.
    sending: bool,
    /// What the server said when the mail did not go out. The card keeps it —
    /// and keeps the draft — until Send is pressed again.
    error: Option<String>,
}

impl ComposeDraft {
    /// A new, empty draft written from `from` — see `compose_from`.
    fn new(from: Option<String>) -> Self {
        Self {
            kind: ComposeKind::New,
            from,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: String::new(),
            attachments: Vec::new(),
            attach_error: None,
            copies_open: false,
            focus: true,
            sending: false,
            error: None,
        }
    }

    /// The three drafts that answer a mail, made of it: the account they go
    /// out from, the two address fields the kind decides, the answered
    /// subject and the quoted original underneath what the sender writes.
    fn prefilled(
        kind: ComposeKind,
        from: Option<String>,
        to: String,
        cc: String,
        subject: String,
        body: String,
    ) -> Self {
        Self {
            kind,
            from,
            to,
            cc,
            bcc: String::new(),
            subject,
            body,
            attachments: Vec::new(),
            attach_error: None,
            copies_open: false,
            focus: true,
            sending: false,
            error: None,
        }
    }

    /// A reply to `mail`: to its sender alone, the subject answered, the
    /// original quoted below.
    fn reply(mail: &MailMessage, from: Option<String>) -> Self {
        Self::prefilled(
            ComposeKind::Reply,
            from,
            mail.from.clone(),
            String::new(),
            answer_subject("Re", &mail.subject),
            quoted_body(mail),
        )
    }

    /// A reply to everyone `mail` was sent to: its sender, and every other
    /// recipient beside the one address that is the user's own — a reply
    /// all that sends the sender their own mail is no reply at all.
    fn reply_all(mail: &MailMessage, from: Option<String>, self_addr: &str) -> Self {
        let others = outgoing::parse_addresses(&mail.to)
            .into_iter()
            .filter(|addr| !addr.eq_ignore_ascii_case(self_addr))
            .collect::<Vec<_>>()
            .join(", ");
        Self::prefilled(
            ComposeKind::ReplyAll,
            from,
            mail.from.clone(),
            others,
            answer_subject("Re", &mail.subject),
            quoted_body(mail),
        )
    }

    /// A forward of `mail`: to nobody in particular — that is the sender's
    /// to choose — the subject passed on, the whole original quoted.
    fn forward(mail: &MailMessage, from: Option<String>) -> Self {
        Self::prefilled(
            ComposeKind::Forward,
            from,
            String::new(),
            String::new(),
            answer_subject("Fwd", &mail.subject),
            quoted_body(mail),
        )
    }

    /// What the three address fields hold, parsed: what the mail's envelope is
    /// made of, and what `send_blocker` judges.
    fn recipients(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            outgoing::parse_addresses(&self.to),
            outgoing::parse_addresses(&self.cc),
            outgoing::parse_addresses(&self.bcc),
        )
    }
}

/// One mail a fetch found *beyond a folder's mark* — the news a notification
/// may be raised from (`Event::Messages`' `new_mail`). Only what a
/// notification shows is kept: who it is from, what it is about, and the id
/// that says whether the account already holds it under another mailbox.
struct Arrival {
    from: String,
    subject: String,
    message_id: String,
}

impl Arrival {
    fn of(mail: &MailMessage) -> Self {
        Arrival {
            from: mail.from.clone(),
            subject: mail.subject.clone(),
            message_id: mail.message_id.clone(),
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
    /// window at a time, and the draft is the only place the mail exists until
    /// Send hands it to a worker (`send_draft`): nothing about it is persisted,
    /// so a card that is closed is a mail that was never written.
    compose: Option<ComposeDraft>,
    /// Where the compose card is shown (`ComposeSpot`): the popup the pen
    /// and the title bar's mail actions bring up, or the pane a mail's own
    /// header opens it on. Only meaningful while `compose` is `Some`.
    compose_spot: ComposeSpot,
    /// The mail the compose answers, when it answers one (`compose_about`):
    /// the pane's spot puts its card right under this mail, and once the
    /// mail has left the pane the draft moves to the popup (`ui`). Only
    /// meaningful while `compose` is `Some`.
    compose_anchor: Option<MailKey>,
    /// Whether the preferences modal is open.
    show_preferences: bool,
    /// Which preferences page is selected.
    prefs_tab: PrefsTab,
    /// The plugins discovered at startup, with their manifests and load
    /// errors — what Preferences → Plugins lists, and what an account's
    /// provider resolves to (the workers hold the same scan).
    plugins: std::sync::Arc<crate::plugin::discovery::Registry>,
    /// Installed plugin ids switched off; the module is not compiled on the
    /// next start. The panel keeps their manifests listed so they can be
    /// switched back on.
    disabled_plugins: BTreeSet<String>,
    /// Accounts page: the add/edit account form replaces the account list
    /// while this is set.
    show_account_form: bool,
    account_form: AccountForm,
    confirm_delete: Option<String>,
    nav_collapsed: bool,
    /// The message list's width as its splitter left it — the width to
    /// bring the list back to when a narrow window stops capping it
    /// (`plan_columns`): egui remembers only what a panel last rendered
    /// at, which while capped is the cap, so the uncapped width is the
    /// app's own to keep.
    list_width: f32,
    /// Whether the list is being capped this frame: egui's memory then
    /// holds the capped width, and `list_width` above is what it is
    /// restored from the moment the cap lifts.
    list_capped: bool,
    /// Whether the pane is a popup over the list this frame — the window
    /// cannot hold the two content columns side by side even at their
    /// minimums (`ColumnPlan::popup`). Set by `ui`, read by
    /// `mail_content`, which is the pane either way.
    pane_popup: bool,
    /// Whether the list is wider than the window this frame
    /// (`ColumnPlan::list_scrolls`): the rows keep the list's minimum and
    /// the column scrolls sideways. Set by `ui`, read by `mail_list`.
    list_scrolls: bool,
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
    /// The app's own icon, uploaded the first time Preferences → About draws
    /// it from the embedded PNG the tray and notifications use.
    app_icon: Option<egui::TextureHandle>,
    /// The frosted backdrop of a floating title bar (see `crate::glass`): the
    /// GL pass is made once, on the first frame the bar floats.
    glass: Glass,
    theme: Theme,
    /// The accent the reader chose in Preferences → Appearance; Kimo's own
    /// until they choose another (`theme::Accent`).
    accent: Accent,
    /// Cached OS dark-mode preference, so "system" resolution does not hit
    /// the filesystem every frame. Refreshed on startup and whenever the
    /// theme cycles back to System.
    system_dark: bool,
    /// Why the mail database is not being written to disk, when that is the
    /// case; drawn as a one-line banner above the message list.
    storage_warning: Option<String>,
    /// Why the last account removal failed, when it did. The account is put
    /// back in the list at the same time, because its row is still there: the
    /// line stands at the top of Preferences → Accounts, where the click was.
    account_error: Option<String>,
    /// The account whose browser sign-in is in flight: the form stays open
    /// on it, saying the browser is the thing to look at, until the job
    /// answers ([`Event::PluginAuth`]).
    plugin_signing_in: Option<String>,
    /// Where a downloaded attachment went — or that a mail was sent — and when
    /// it said so: one line above the message list for `NOTICE_TIME`. A file
    /// that lands in a directory the user was not watching is a file they cannot
    /// find, and the numbered name it ended up under is the worker's to know; a
    /// sent mail has left the machine entirely, which is the other thing worth
    /// saying out loud. Hence the events.
    ///
    /// An *export* does not come here: its click was in the title bar and the
    /// file is not in the list, so the desktop is told instead
    /// (`exported_notification`).
    notice: Option<(String, Instant)>,
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
    /// Whether the automatic sync is switched on, how long between its sweeps,
    /// and when the last one ran — or when the app started, so the first is a
    /// full interval away. The preference is in minutes; the clock wants a
    /// `Duration` (see `pump_auto_sync`).
    auto_sync: bool,
    auto_sync_every: Duration,
    auto_synced_at: Instant,
    /// Whether new mail raises a desktop notification (Preferences → General).
    /// On by default: it is what the feature is for, and the switch is there
    /// for the session that does not want to be interrupted.
    notifications: bool,
    /// Whether the title bar's minimize button hides the window instead of
    /// minimising it (Preferences → General). Off by default, and the hide
    /// only happens while a tray is registered, because the tray is the way
    /// back (`crate::tray::available`).
    minimize_to_tray: bool,
    /// Whether closing the window hides it instead of quitting (Preferences →
    /// General). Off by default, and the hide only happens while a tray is
    /// registered: the tray's Quit is then the only way out
    /// (`crate::tray::available`).
    close_to_tray: bool,
    /// Provider presets switched off in Preferences → Accounts. Custom (plain
    /// IMAP/POP3/SMTP) is not a vendor and is always available. Persisted as
    /// keys in `settings.json`; a key this build does not know is ignored, and
    /// a preset added later starts switched on.
    disabled_vendors: BTreeSet<VendorKey>,
    /// Whether the tray icon is wearing its unread badge: any account's Inbox
    /// holds mail nobody has read (`update_tray_badge`). The mail's own state,
    /// not an event — right at startup, right after a frame that read a mail,
    /// and right after a sync that found one read somewhere else — so the
    /// window's visibility and focus play no part in it.
    tray_badge: bool,
    /// Installed font families for the Appearance pickers; `None` while the
    /// worker scan (`Job::ScanFonts`) is still running.
    font_families: Option<Vec<FontFamily>>,
    /// Picked font family names, as stored in `settings.json`; `None` means
    /// egui's built-in font (plus the CJK fallback).
    ui_font: Option<String>,
    mono_font: Option<String>,
    /// The CJK fallback picked in Preferences; `None` means the automatic
    /// choice (the first installed candidate, with the face the locale asks
    /// for).
    cjk_font: Option<String>,
    /// What the picked CJK face has no glyph for, computed when the pick is
    /// applied rather than per frame (`fonts::missing_cjk_samples` reads the
    /// whole file). The name is kept so a warning left from an earlier pick is
    /// never shown under a newer one.
    cjk_missing: Option<(String, Vec<char>)>,
    /// The graphics card picked in Preferences → Graphics, as its PCI address
    /// (`0000:03:00.0`); `None` is the Vulkan loader's own choice. Read at
    /// start-up by `crate::gpus`, so a change here applies to the next start:
    /// the drivers are opened when the instance is made and cannot be unloaded.
    gpu: Option<String>,
    /// What `crate::gpus::scan` found for that page — the cards, the drivers
    /// and the cards' names. Loaded the first time the page is drawn, on a
    /// worker thread (`Job::ScanGpus`), because it reads sysfs, the manifest
    /// directories and `pci.ids`.
    gpu_scan: Option<crate::gpus::Scan>,
    /// Whether that scan is in flight, so the page asks for it once rather than
    /// every frame.
    gpu_scanning: bool,
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
    /// The paper a mail's Blitz page is drawn on, for the mails the reader has
    /// switched away from white. The map holds only those: a mail switched back
    /// takes its entry with it, so what is here is exactly the list of papers
    /// that differ from the default, and nothing about it is persisted.
    blitz_paper: HashMap<MailKey, Paper>,
    /// The built-in renderer's copy of the mail on screen: the parsed body and
    /// the width it is laid out for. See [`BodyCache`].
    body: BodyCache,
    /// Message the reading pane's scroll offset belongs to, so opening another
    /// mail starts at the top instead of mid-page.
    scrolled_message: Option<String>,
    /// The body context menu a right-click asked for, and whether the popup
    /// holds it open. The target and the mail are kept together because the
    /// menu is drawn on the frame after the click, when the pane may have moved
    /// on.
    body_menu: Option<BodyMenu>,
    body_menu_open: bool,
    /// The message list's own rect this frame, set by `mail_list`. The rows'
    /// rules span it so they reach the column's hairlines; nothing else reads
    /// it.
    list_column: egui::Rect,
    /// How far the message list is scrolled, in points — zero when it has no
    /// scroll area at all. The title bar floats over the column once this
    /// leaves the top, and docks again when it comes back.
    list_scroll: f32,
    /// The same for the navigation column: zero while it is not drawn (the
    /// popup layout, or a fully collapsed panel), because a column that is not
    /// there has no content under the bar to turn to glass.
    nav_scroll: f32,
    /// Whether the list groups the folder's mail into conversations
    /// (Preferences → General, `crate::threads`).
    threaded: bool,
    /// The threads the user has opened, keyed by `Thread::key`. In memory
    /// only: which conversations are open is a per-session thing, the way the
    /// collapsed accounts are.
    open_threads: HashSet<String>,
    /// Bumped whenever `open_threads` changes, so the derived rows know they
    /// are stale without comparing the whole set.
    open_rev: u64,
    /// True while the desktop's file dialog is open for the compose card: asked
    /// for by the card's button, answered by `Event::FilesChosen` or
    /// `Event::FileChoiceFailed`, and what the card is drawn from meanwhile — the
    /// button says the dialog is up and cannot be pressed twice.
    picking_files: bool,
    /// The order the message list is drawn in (the title bar's sort cell): which
    /// way round it runs, and by which of the three keys. Beside the filter,
    /// which is the other thing about the column that is the window's rather
    /// than the mail's.
    list_sort: ListSort,
    /// The views the message list is narrowed to (the title bar's filter cell):
    /// optional, and an AND — see `ListFilters`. A view of the column rather than
    /// part of a query, so it applies to a folder's mail, a label's matches and
    /// a search's hits alike. In memory only, like the open conversations: what
    /// the window is doing now, not a setting.
    list_filters: ListFilters,
    /// The list as it is drawn, derived from the folder's mail rather than
    /// built on every frame — see [`ListRows`].
    list_rows: Option<ListRows>,
    /// Whether the message list is in select mode: every row draws a checkbox
    /// in place of its avatar, and a click on the row itself — anywhere but
    /// the box — leaves the mode instead of opening a mail. In memory only:
    /// which mails are being picked is a per-session thing, the way the open
    /// conversations are.
    select_mode: bool,
    /// The mail checked in select mode, by the identity the database keys a
    /// message by — a search lists hits from every folder at once, so a uid on
    /// its own would not do. Dropped whenever the column moves to another list
    /// (`cancel_select`).
    selected_mails: HashSet<MailKey>,
    /// Every label the user has made (`crate::db::load_labels` at startup, and
    /// one more for each name the popup creates), in the order the navigation
    /// column's label group draws them. A label is a first-class filter beside
    /// the folders: the column's last group, and the list the middle column
    /// shows while one of them is picked.
    labels: Vec<String>,
    /// The labels the message list is filtered by, in name order, and empty
    /// when no filter is on. A click on a label adds it; a click on one that is
    /// already here takes it out. **The filter is an AND**: the list shows the
    /// mail that carries every one of them, so a second label narrows it.
    label_filter: Vec<String>,
    /// The label group's rows and the mail the filter selects, derived from the
    /// mail's own labels rather than built every frame — see [`LabelView`].
    label_view: Option<LabelView>,
    /// What the label popup's new-label box holds. In memory only, and emptied
    /// each time the popup is opened: what was typed for one mail is not a
    /// draft for the next.
    label_draft: String,
    /// Bumped whenever a label is made or put on a mail, so the derived view
    /// knows it is stale without comparing the whole label set of every mail.
    labels_rev: u64,
    /// The same for the reading pane's card, which scrolls the whole mail:
    /// either column scrolled is content under the floating bar. Zero while
    /// no mail is open.
    mail_scroll: f32,
}

/// The rows a folder's conversations become: one per thread, and — under an
/// open one — one per mail it holds, oldest first, so a conversation reads the
/// way it happened. A list that is not threading gets one row per mail, which
/// is the shape the list had before conversations.
fn list_rows_of(
    threads: &[Thread],
    flat: &[usize],
    open: &HashSet<String>,
    threaded: bool,
) -> Vec<ListRow> {
    if !threaded {
        return flat
            .iter()
            .map(|&index| ListRow::Message {
                index,
                in_thread: false,
            })
            .collect();
    }
    let mut rows = Vec::with_capacity(threads.len());
    for (thread, conversation) in threads.iter().enumerate() {
        if conversation.messages.len() == 1 {
            // A conversation of one is a mail: it is drawn like one — no
            // chevron, no indent, nothing to open — and the reading pane shows
            // it the way it always has.
            rows.push(ListRow::Message {
                index: conversation.messages[0],
                in_thread: false,
            });
            continue;
        }
        rows.push(ListRow::Thread { thread });
        if open.contains(&conversation.key) {
            rows.extend(conversation.messages.iter().map(|&index| ListRow::Message {
                index,
                in_thread: true,
            }));
        }
    }
    rows
}

/// The key a conversation's row is sorted by: what the row shows — the newest
/// mail's date and sender, the conversation's own subject — and where that mail
/// sits in the folder, which is what keeps two equal rows in the folder's order.
fn thread_sort_key<'a>(thread: &'a Thread, messages: &'a [MailMessage]) -> SortKey<'a> {
    let newest = &messages[thread.newest];
    SortKey {
        timestamp: newest.timestamp,
        from: &newest.from,
        subject: &thread.subject,
        position: thread.newest,
    }
}

/// The same key for one of the label view's copies of a mail: the fields its
/// row draws. Its position is zero — the label list's own total order is what
/// keeps two equal rows still, because the mail it holds comes from every folder
/// and account at once.
fn labelled_sort_key(mail: &LabelledMail) -> SortKey<'_> {
    SortKey {
        timestamp: mail.timestamp,
        from: &mail.from,
        subject: &mail.subject,
        position: 0,
    }
}

/// The same key for a mail on its own: the fields its own row draws.
fn mail_sort_key<'a>(mail: &'a MailMessage, position: usize) -> SortKey<'a> {
    SortKey {
        timestamp: mail.timestamp,
        from: &mail.from,
        subject: &mail.subject,
        position,
    }
}

/// The order the flat list draws in: the mail the filter lets through, sorted by
/// the column's own sort. What comes back are indices into the folder's mail —
/// the order, rather than the mail itself, because the folder's slice is what
/// the rows are drawn from and it is not this function's to reorder.
fn flat_order(shown: &[bool], messages: &[MailMessage], sort: ListSort) -> Vec<usize> {
    let mut order: Vec<usize> = shown
        .iter()
        .enumerate()
        .filter(|(_, shown)| **shown)
        .map(|(index, _)| index)
        .collect();
    order.sort_by(|&one, &two| {
        sort.compare(
            mail_sort_key(&messages[one], one),
            mail_sort_key(&messages[two], two),
        )
    });
    order
}

/// The facts a filter asks a row about (`FilterFacts`), read off each of the
/// three things a list can hold: the folder's mail, a search's hit, and the
/// labelled view's own copy of a mail. One conversion each, so what a filter is
/// asked is the same question wherever the row came from.
fn mail_facts(mail: &MailMessage) -> FilterFacts<'_> {
    FilterFacts {
        seen: mail.seen,
        flagged: mail.flagged,
        to: &mail.to,
        attachments: mail.attachments.len(),
    }
}

fn hit_facts(hit: &SearchHit) -> FilterFacts<'_> {
    FilterFacts {
        seen: hit.seen,
        flagged: hit.flagged,
        to: &hit.to,
        attachments: hit.attachments,
    }
}

fn labelled_facts(mail: &LabelledMail) -> FilterFacts<'_> {
    FilterFacts {
        seen: mail.seen,
        flagged: mail.flagged,
        to: &mail.to,
        attachments: mail.attachments,
    }
}

/// One conversation, trimmed to the mail a filter lets through — `None` when it
/// has none left, which is a conversation the list does not show at all.
///
/// The thread's own row speaks for the mails under it: its date, its unread
/// count, its paperclip and its flag are all about the mail that is left, so
/// they are recomputed over it. The key and the subject stay the
/// conversation's own — the open/closed set and the reading pane are keyed by
/// the key, and a conversation keeps its subject whatever part of it is shown.
/// `thread.messages` is oldest first, and so is what comes back: the last mail
/// kept is the one the row speaks for, the first is the oldest.
fn thread_of_shown(thread: Thread, messages: &[MailMessage], shown: &[bool]) -> Option<Thread> {
    let kept: Vec<usize> = thread
        .messages
        .iter()
        .copied()
        .filter(|&index| shown[index])
        .collect();
    let &newest = kept.last()?;
    Some(Thread {
        messages: kept.clone(),
        key: thread.key,
        subject: thread.subject,
        newest,
        unread: kept.iter().filter(|&&index| !messages[index].seen).count(),
        has_attachments: kept
            .iter()
            .any(|&index| !messages[index].attachments.is_empty()),
        flagged: kept.iter().any(|&index| messages[index].flagged),
    })
}

/// What the reading pane shows: the mails of one conversation, oldest first,
/// and the key the pane's scroll offset belongs to.
struct Conversation {
    /// The thread's key, or the mail's own uid when it is not part of one — what
    /// `KimoApp::scrolled_message` compares, so that opening another mail of
    /// the conversation already on screen does not jump back to its top.
    key: String,
    messages: Vec<MailMessage>,
}

/// One row of the message list: a thread's own row, or one mail.
///
/// Every row is `MESSAGE_ROW_HEIGHT` tall whatever it is, which is what lets
/// the list keep laying out only the rows its viewport shows
/// (`virtual_rows`): what threading changes is which rows there are, not how
/// tall one is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListRow {
    /// A conversation, `thread` indexing `ListRows::threads`.
    Thread { thread: usize },
    /// One mail, `index` into the folder's messages. `in_thread` marks the
    /// ones an open thread holds, which are drawn indented under it.
    Message { index: usize, in_thread: bool },
}

/// One mail as the selection knows it: the identity the database keys a
/// message by. A uid on its own would not do — a search lists hits from every
/// folder and account at once — and the two extra strings are what a delete
/// groups its jobs by (`delete_batches`).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct MailKey {
    account: String,
    folder: String,
    uid: String,
}

impl MailKey {
    fn new(account: &str, folder: &str, uid: &str) -> Self {
        Self {
            account: account.to_string(),
            folder: folder.to_string(),
            uid: uid.to_string(),
        }
    }
}

/// The mail a row's own cells are about: the mail the row speaks for. A
/// conversation's row shows its newest mail — the one the reading pane opens
/// from it — and that is the one its flag and its delete take; the mails the
/// conversation holds are the list's to mark one by one, or the selection's to
/// take whole.
fn row_action_target<'a>(
    row: ListRow,
    rows: &ListRows,
    messages: &'a [MailMessage],
) -> Option<&'a MailMessage> {
    match row {
        ListRow::Thread { thread } => messages.get(rows.threads[thread].newest),
        ListRow::Message { index, .. } => messages.get(index),
    }
}

/// What select mode's Delete sends: the checked mail grouped by the account and
/// folder it lives in — one job per group, because one job is one connection —
/// with each group in uid order, so the same selection is always the same
/// jobs. A selection made in a folder is one group; one made over a search's
/// hits can be several.
fn delete_batches(mails: &HashSet<MailKey>) -> BTreeMap<(String, String), Vec<String>> {
    let mut batches: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for key in mails {
        batches
            .entry((key.account.clone(), key.folder.clone()))
            .or_default()
            .push(key.uid.clone());
    }
    for uids in batches.values_mut() {
        uids.sort();
    }
    batches
}

/// The message list's model of the folder it is showing: the mail grouped into
/// conversations (`crate::threads`) and the rows drawn from them.
///
/// Grouping is a per-folder cost — a map, a union-find and a sort over every
/// mail the folder holds — and the list is drawn on every frame, so the answer
/// is kept until something it depends on moves: the folder's mail
/// (`AccountUiState::folders_rev`), the threading switch, the folder the pane
/// is showing, or the set of open threads (`open_rev`).
struct ListRows {
    account: String,
    folder: String,
    rev: u64,
    open_rev: u64,
    threaded: bool,
    /// What the column was filtered to when these were derived: a different set
    /// of views is a different list, and one that would show mail the rows do
    /// not.
    filters: ListFilters,
    /// The order the column was sorted in when these were derived.
    sort: ListSort,
    threads: Vec<Thread>,
    rows: Vec<ListRow>,
}

/// Which way the message list runs: the newest mail at the top, or the oldest.
/// What the sort cell's own menu calls the *order* group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SortOrder {
    #[default]
    Newest,
    Oldest,
}

impl SortOrder {
    const ALL: [SortOrder; 2] = [SortOrder::Newest, SortOrder::Oldest];

    fn label(self) -> &'static str {
        match self {
            SortOrder::Newest => "Newest on top",
            SortOrder::Oldest => "Oldest on top",
        }
    }

    /// Whether the comparison runs the other way round: `Newest` is what the
    /// message list has always done — the folder's own order, newest first.
    fn ascending(self) -> bool {
        matches!(self, SortOrder::Oldest)
    }
}

/// What the message list is sorted *by*: the mail's date, its sender, or its
/// subject — what the row itself shows, so sorting a threaded list by "from"
/// sorts by the sender each conversation's row is drawn with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SortBy {
    #[default]
    Date,
    From,
    Subject,
}

impl SortBy {
    const ALL: [SortBy; 3] = [SortBy::Date, SortBy::From, SortBy::Subject];

    fn label(self) -> &'static str {
        match self {
            SortBy::Date => "Date",
            SortBy::From => "From",
            SortBy::Subject => "Subject",
        }
    }
}

/// How the message list is ordered, from the title bar's sort cell: which way
/// round, and by what — the menu's two groups and nothing else. The default is
/// the order the folder arrives in (newest first, by date).
///
/// Like the filter it is a view of the column rather than part of a query, so a
/// change re-derives the list rather than asking the database again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ListSort {
    order: SortOrder,
    by: SortBy,
}

impl ListSort {
    /// What the cell's tooltip says: the two choices by name.
    fn summary(self) -> String {
        format!(
            "{}, by {}",
            self.order.label().to_lowercase(),
            self.by.label().to_lowercase()
        )
    }

    /// Where one row goes, as the comparison the list is sorted with: the key
    /// the menu named — the mail's date, its sender as the row draws it, its
    /// subject as the row draws it — and the order the other group chose, with
    /// where the mail sits as the tie-break, so two mails that compare equal
    /// keep the order the folder is in rather than swapping about.
    fn compare(self, one: SortKey<'_>, two: SortKey<'_>) -> std::cmp::Ordering {
        let by = match self.by {
            SortBy::Date => one.timestamp.cmp(&two.timestamp),
            SortBy::From => compare_names(one.from, two.from),
            SortBy::Subject => compare_names(one.subject, two.subject),
        };
        let by = if self.order.ascending() {
            by
        } else {
            by.reverse()
        };
        by.then(one.position.cmp(&two.position))
    }
}

/// The three things a row can be sorted by, as the row draws them: a mail's own,
/// or — for a conversation — the ones its row speaks for (its newest mail's date
/// and sender, its own subject).
struct SortKey<'a> {
    timestamp: Option<i64>,
    from: &'a str,
    subject: &'a str,
    /// Where the mail sits in the folder, which is what keeps the sort stable.
    position: usize,
}

/// One view of the message list, offered as a checkbox in the title bar's
/// filter menu. They are *optional* and they are an AND: any number of them can
/// be ticked, and a mail is listed only while it passes every one — which is
/// what `ListFilters` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListFilter {
    Unread,
    Flagged,
    ToMe,
    HasFiles,
}

impl ListFilter {
    /// The four, in the order the menu draws them under their "All" row.
    const ALL: [ListFilter; 4] = [
        ListFilter::Unread,
        ListFilter::Flagged,
        ListFilter::ToMe,
        ListFilter::HasFiles,
    ];

    /// What the menu's row and the cell's tooltip call it.
    fn label(self) -> &'static str {
        match self {
            ListFilter::Unread => "Unread",
            ListFilter::Flagged => "Flagged",
            ListFilter::ToMe => "To me",
            ListFilter::HasFiles => "Has files",
        }
    }

    /// The line the menu row's tooltip carries: what ticking it leaves in the
    /// list.
    fn note(self) -> &'static str {
        match self {
            ListFilter::Unread => "Only mail that has not been read",
            ListFilter::Flagged => "Only mail you have flagged",
            ListFilter::ToMe => "Only mail addressed to one of your accounts",
            ListFilter::HasFiles => "Only mail that carries an attachment",
        }
    }

    /// Whether one row passes this view. The facts are what all three lists can
    /// answer about their mail — `MailMessage`, `SearchHit` and `LabelledMail`
    /// each carry them — and `me` is the accounts' own addresses
    /// (`KimoApp::my_addresses`), which is what `ToMe` is asked against.
    fn matches(self, facts: FilterFacts<'_>, me: &[String]) -> bool {
        match self {
            ListFilter::Unread => !facts.seen,
            ListFilter::Flagged => facts.flagged,
            ListFilter::ToMe => addressed_to(facts.to, me),
            ListFilter::HasFiles => facts.attachments > 0,
        }
    }
}

/// The views the message list is narrowed to, as a set: the title bar's filter
/// cell's state.
///
/// They are optional and they are an AND — a mail is listed only while it passes
/// every view that is ticked — and an **empty set is all mail**, which is what
/// the menu's own "All" row is: it is drawn ticked exactly while nothing else
/// is, and ticking it clears the rest. A view of the column rather than part of
/// a query: the mail is all there, some of it hidden, so the set applies to
/// whatever the column lists (a folder's mail, a label's matches, a search's
/// hits), while the search popup's "Only" boxes are the other layer and narrow
/// the query itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ListFilters(Vec<ListFilter>);

impl ListFilters {
    /// Whether nothing is ticked: every mail the column holds is listed.
    fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    fn contains(&self, filter: ListFilter) -> bool {
        self.0.contains(&filter)
    }

    /// Tick a view, or untick it — the checkbox the menu draws. The set keeps
    /// the menu's own order, so what is derived from it does not depend on the
    /// order the boxes were ticked in.
    fn toggle(&mut self, filter: ListFilter) {
        if let Some(index) = self.0.iter().position(|held| *held == filter) {
            self.0.remove(index);
            return;
        }
        self.0.push(filter);
        self.0.sort_by_key(|held| {
            ListFilter::ALL
                .iter()
                .position(|candidate| candidate == held)
                .unwrap_or(usize::MAX)
        });
    }

    /// Untick everything: what the menu's "All" row does.
    fn clear(&mut self) {
        self.0.clear();
    }

    /// Whether one row passes every ticked view — the AND the menu promises, and
    /// nothing at all while none is ticked.
    fn matches(&self, facts: FilterFacts<'_>, me: &[String]) -> bool {
        self.0.iter().all(|filter| filter.matches(facts, me))
    }

    /// What the cell's tooltip says: all mail, or the views in force by name.
    fn summary(&self) -> String {
        if self.is_all() {
            return "all mail".to_string();
        }
        let names: Vec<String> = self
            .0
            .iter()
            .map(|filter| filter.label().to_lowercase())
            .collect();
        names.join(" + ")
    }
}

/// The four things a filter asks a row: the marks the list draws and the header
/// "to me" is matched against. `Copy` because the set of views asks every one of
/// them about the same row.
#[derive(Clone, Copy)]
struct FilterFacts<'a> {
    seen: bool,
    flagged: bool,
    /// The `To` header as the sender wrote it — display names, several
    /// addresses, or a bare address — which is all the app keeps of it.
    to: &'a str,
    attachments: usize,
}

impl ListRows {
    /// The folder's mail this list shows, by index — every row's own mail, a
    /// conversation's whole set included, deduplicated and in the folder's
    /// order. What select-all takes and what the count in its band is about:
    /// mail a filter hides is not in the list to be checked.
    fn shown_indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        for row in &self.rows {
            match row {
                ListRow::Message { index, .. } => indices.push(*index),
                ListRow::Thread { thread } => {
                    indices.extend(self.threads[*thread].messages.iter().copied());
                }
            }
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

/// One label as the navigation column draws it: the user's own name for it, how
/// much mail carries it, and whether it is one of the labels the list is
/// filtered by.
struct LabelCount {
    name: String,
    count: usize,
    selected: bool,
}

/// One mail a label filter selects, as a row needs it: where the mail lives and
/// the fields a card draws. Copied rather than borrowed — the view outlives the
/// state it was read from — the way a search hit is.
#[derive(Clone)]
struct LabelledMail {
    account_id: String,
    folder: String,
    uid: String,
    from: String,
    /// The `To` header as it was written, for the list filter's "to me" — the
    /// same field a search hit carries.
    to: String,
    subject: String,
    date: String,
    timestamp: Option<i64>,
    seen: bool,
    /// Whether the mail is flagged, which the row shows with the same warning
    /// tint a folder's row does.
    flagged: bool,
    attachments: usize,
}

/// What the mail's labels say, derived once per change rather than once per
/// frame (`KimoApp::refresh_label_view`): every label with the amount of mail
/// under it, and the mail the selected labels all agree on.
///
/// The counts are a walk over every mail of every account, which is not a
/// frame's work — keeping the answer is the whole point. `key` is what it was
/// derived from, so a frame that moved nothing derives nothing.
#[derive(Default)]
struct LabelView {
    /// The labels revision, the accounts' own mail revision, the filter and the
    /// sort; `None` until the first derivation, which no key equals.
    key: Option<(u64, u64, Vec<String>, ListSort)>,
    rows: Vec<LabelCount>,
    /// The mail the filter selects, newest first. Empty while no filter is on.
    matches: Vec<LabelledMail>,
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
    /// One entry per mail on screen. A conversation is several mails, and the
    /// pane asks for every one of them on every frame — a cache of a single
    /// body would re-parse all but one of them per frame, which is the cost
    /// this exists to avoid.
    entries: Vec<BodyEntry>,
}

/// One mail's parsed body and the width it settled at.
struct BodyEntry {
    /// `<folder>/<uid>` of the mail this holds.
    identity: String,
    /// Hash of the body, so a re-fetch that replaces the body of a uid the
    /// pane is already showing is noticed (`body_fingerprint`).
    fingerprint: u64,
    document: crate::html::Document,
    /// The width the body is laid out for; `None` until it has been laid out
    /// once.
    width: Option<f32>,
    /// The width the pane wants now — quantized, so that a pixel of jitter is
    /// not a reflow — and since when it has wanted it.
    pending: Option<(u32, Instant)>,
    /// When this body was last asked for, so the entry evicted for a new one is
    /// the mail that has been off screen longest.
    used: Instant,
}

/// How many bodies the built-in renderer keeps parsed. A conversation is a
/// handful of mails and one more is the mail the pane moves to next; eight
/// parsed bodies is a few hundred kilobytes at worst.
const BODY_CACHE: usize = 8;

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
        let index = match self
            .entries
            .iter()
            .position(|entry| entry.identity == identity && entry.fingerprint == fingerprint)
        {
            Some(index) => index,
            None => {
                // The same mail with a new body (a re-fetch under a uid the pane
                // already shows), or one it has not seen: either way what is
                // held is dropped and parsed again.
                self.entries.retain(|entry| entry.identity != identity);
                if self.entries.len() >= BODY_CACHE {
                    if let Some((oldest, _)) = self
                        .entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| entry.used)
                    {
                        self.entries.remove(oldest);
                    }
                }
                self.entries.push(BodyEntry {
                    identity: identity.to_string(),
                    fingerprint,
                    document: crate::html::Document::parse(html),
                    // A body the cache has not seen has no earlier layout to
                    // hold on to: it is laid out at the pane's own width at
                    // once.
                    width: None,
                    pending: None,
                    used: now,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        entry.used = now;
        let width = entry.layout_width(ctx, wanted, now);
        (&entry.document, width)
    }

    /// Drop every parsed body: what is on screen is another renderer's, or none.
    fn forget(&mut self) {
        self.entries.clear();
    }
}

impl BodyEntry {
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
    /// True for a Gmail label: the server names it like a mailbox (a
    /// "synthetic" folder), but it is the user's own tag, so the row wears
    /// the label glyph rather than a folder icon — every label the same one.
    label: bool,
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
        // Gmail names its system mailboxes under a non-selectable '[Gmail]'
        // container and lists its labels beside INBOX. That shape is the
        // only sign the account is Gmail's: the container is not a row, the
        // folders under it drop the prefix, and the rest are labels — drawn
        // with the label glyph rather than a kind icon, so a label called
        // "Sent" does not masquerade as the Sent mailbox. An ordinary IMAP
        // mailbox has no '[Gmail]/' name, and none of this applies to it.
        let gmail = listed
            .iter()
            .chain(mailbox.folders.iter().map(|folder| &folder.name))
            .any(|name| folders::is_gmail_container(name) || folders::gmail_folder(name).is_some());
        let row = |name: &str, listed: bool| {
            // `name` is the IMAP wire form (possibly modified UTF-7); the
            // container prefix is Gmail's display decoration, not part of the
            // name, so strip it before decoding and classifying.
            let label = gmail && folders::is_gmail_label(name);
            let shown = folders::gmail_folder(name).unwrap_or(name);
            let display = crate::protocol::imap::decode_mailbox_name(shown);
            FolderRow {
                kind: folders::kind_of(&display),
                label,
                display,
                name: name.to_string(),
                unread: unread_of.get(name).copied().unwrap_or(0),
                listed,
            }
        };

        // The '[Gmail]' container is a namespace, not a mailbox: SELECT
        // refuses it, so it is never a row — listed or cached.
        let mut listed_names: Vec<String> = listed
            .iter()
            .filter(|name| !folders::is_gmail_container(name))
            .cloned()
            .collect();
        crate::protocol::imap::order_mailboxes(&mut listed_names);
        if gmail {
            folders::order_gmail(&mut listed_names);
        }
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
            .filter(|folder| {
                !listed.contains(folder.name.as_str())
                    && !folder.messages.is_empty()
                    && !folders::is_gmail_container(&folder.name)
            })
            .map(|folder| folder.name.clone())
            .collect();
        crate::protocol::imap::order_mailboxes(&mut unlisted_names);
        if gmail {
            folders::order_gmail(&mut unlisted_names);
        }
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
    /// The provider preset the form opened on, or the one picked since. It is
    /// transient: the account has no vendor field, because the vendor is
    /// inferred from the endpoints (`vendor::VendorKey::of`).
    vendor: VendorKey,
    name: String,
    protocol: Protocol,
    host: String,
    port_str: String,
    username: String,
    password: String,
    tls: TlsMode,
    /// The Sending group: off is an account that only reads mail
    /// (`Account.smtp = None`), which is what every account was before the
    /// group existed.
    smtp_enabled: bool,
    smtp_host: String,
    smtp_port_str: String,
    smtp_tls: TlsMode,
    /// Whether the submission logs in as the account itself, which is what
    /// most providers expect. Off reveals the pair below.
    smtp_own_login: bool,
    smtp_username: String,
    smtp_password: String,
    /// The plugin provider this account runs through ("gmail"), picked from
    /// the Provider row while the plugin is installed. `None` is the
    /// built-in client the rest of the form describes. This is the dedicated
    /// entry: an Outlook plugin lands here as its own line, with no form
    /// changes of its own.
    plugin: Option<String>,
    /// The OAuth client id the plugin's config asks for: the deployment's
    /// own application registered with the provider (a Google Cloud
    /// Desktop client), not something Kimo can ship.
    client_id: String,
    /// The secret that goes with `client_id`, for a deployment whose client
    /// has one. Google issues desktop clients a secret and refuses the token
    /// exchange without it; Entra's public clients have none. It is not a
    /// credential in the desktop sense — Google treats an installed app's
    /// secret as public — but it is still stored in the provider config, not
    /// logged.
    client_secret: String,
}

impl Default for AccountForm {
    /// An empty form for a new account: IMAP over implicit TLS, the setup
    /// most providers use.
    fn default() -> Self {
        Self {
            editing: None,
            vendor: VendorKey::Custom,
            name: String::new(),
            protocol: Protocol::Imap,
            host: String::new(),
            port_str: Protocol::Imap.default_port(TlsMode::Implicit).to_string(),
            username: String::new(),
            password: String::new(),
            tls: TlsMode::Implicit,
            smtp_enabled: false,
            smtp_host: String::new(),
            smtp_port_str: SmtpSettings::default_port(TlsMode::Implicit).to_string(),
            smtp_tls: TlsMode::Implicit,
            smtp_own_login: true,
            smtp_username: String::new(),
            smtp_password: String::new(),
            plugin: None,
            client_id: String::new(),
            client_secret: String::new(),
        }
    }
}

impl AccountForm {
    /// The same form, pre-filled from a configured account and remembering
    /// its id so that saving updates it instead of adding another one.
    fn edit(account: &Account) -> Self {
        Self {
            editing: Some(account.id.clone()),
            vendor: VendorKey::of(account),
            name: account.name.clone(),
            protocol: account.protocol,
            host: account.host.clone(),
            port_str: account.port.to_string(),
            username: account.username.clone(),
            password: account.password.clone(),
            tls: account.tls,
            smtp_enabled: account.smtp.is_some(),
            smtp_host: account
                .smtp
                .as_ref()
                .map(|smtp| smtp.host.clone())
                .unwrap_or_default(),
            smtp_port_str: account
                .smtp
                .as_ref()
                .map(|smtp| smtp.port.to_string())
                .unwrap_or_else(|| SmtpSettings::default_port(TlsMode::Implicit).to_string()),
            smtp_tls: account
                .smtp
                .as_ref()
                .map(|smtp| smtp.tls)
                .unwrap_or(TlsMode::Implicit),
            smtp_own_login: account
                .smtp
                .as_ref()
                .is_none_or(|smtp| smtp.login.is_none()),
            smtp_username: account
                .smtp
                .as_ref()
                .and_then(|smtp| smtp.login.as_ref())
                .map(|login| login.username.clone())
                .unwrap_or_default(),
            smtp_password: account
                .smtp
                .as_ref()
                .and_then(|smtp| smtp.login.as_ref())
                .map(|login| login.password.clone())
                .unwrap_or_default(),
            plugin: account.provider.clone(),
            client_id: crate::plugin::provider::client_id_of(account),
            client_secret: crate::plugin::provider::client_secret_of(account),
        }
    }

    /// Every required field is filled in (and the ports are ports). The
    /// outgoing server is required only while the account is meant to send.
    fn is_valid(&self) -> bool {
        // A plugin account asks for an address and nothing else: the module
        // owns the endpoints, and the sign-in is the browser's business.
        if self.plugin.is_some() {
            return !self.name.trim().is_empty() && !self.username.trim().is_empty();
        }
        !self.name.trim().is_empty()
            && !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && self.port_str.trim().parse::<u16>().is_ok()
            && (!self.smtp_enabled
                || (!self.smtp_host.trim().is_empty()
                    && self.smtp_port_str.trim().parse::<u16>().is_ok()))
    }

    /// The port to save: what was typed, or the protocol/TLS default when
    /// that text is not a number.
    fn port(&self) -> u16 {
        self.port_str
            .trim()
            .parse()
            .unwrap_or_else(|_| self.protocol.default_port(self.tls))
    }

    /// The outgoing server the form describes, or ${BT}None${BT} while the Sending
    /// group is off. A port that is not a number falls back to the one the
    /// security choice normally uses, the same way the incoming port does.
    fn smtp(&self) -> Option<SmtpSettings> {
        if !self.smtp_enabled {
            return None;
        }
        Some(SmtpSettings {
            host: self.smtp_host.trim().to_string(),
            port: self
                .smtp_port_str
                .trim()
                .parse()
                .unwrap_or_else(|_| SmtpSettings::default_port(self.smtp_tls)),
            tls: self.smtp_tls,
            login: (!self.smtp_own_login).then(|| SmtpLogin {
                username: self.smtp_username.trim().to_string(),
                password: self.smtp_password.clone(),
            }),
        })
    }

    /// Fill the endpoint fields in from the provider the picker names. Only a
    /// preset with endpoints changes anything: Custom keeps whatever was typed,
    /// so switching away from a preset does not erase the server.
    /// A plugin provider picked: its vendor profile (when it has one) fills
    /// the endpoint columns the row still carries, and the module takes over
    /// the sign-in and the sending. The Gmail plugin answers to the same
    /// profile the IMAP preset filled, which is why the entry is a
    /// replacement rather than an addition.
    fn apply_plugin(&mut self, id: &str) {
        self.plugin = Some(id.to_string());
        if let Some(key) = vendor::VendorKey::for_provider(id) {
            self.vendor = key;
            self.apply_preset();
        }
        // The module submits on its own; an outgoing server here would be a
        // second sender the account does not have.
        self.smtp_enabled = false;
    }

    fn apply_preset(&mut self) {
        let profile = self.vendor.profile();
        if let Some(imap) = profile.imap {
            self.protocol = Protocol::Imap;
            self.host = imap.host.to_string();
            self.port_str = imap.port.to_string();
            self.tls = imap.tls;
        }
        if let Some(smtp) = profile.smtp {
            self.smtp_enabled = true;
            self.smtp_host = smtp.host.to_string();
            self.smtp_port_str = smtp.port.to_string();
            self.smtp_tls = smtp.tls;
            self.smtp_own_login = true;
            self.smtp_username.clear();
            self.smtp_password.clear();
        }
    }

    /// The account this form describes, under an id the caller picks — the
    /// edited account's own, or a fresh one for a new account. `previous` is
    /// the account being edited: its tokens survive an edit untouched,
    /// because the form never edits them, but a change of provider spends
    /// them — tokens of one provider are not credentials of another.
    fn to_account(&self, id: String, previous: Option<&Account>) -> Account {
        let provider = self.plugin.clone();
        let provider_config = if provider.is_some() && !self.client_id.trim().is_empty() {
            let mut config = serde_json::json!({ "client_id": self.client_id.trim() });
            if !self.client_secret.trim().is_empty() {
                config["client_secret"] = serde_json::json!(self.client_secret.trim());
            }
            Some(config.to_string())
        } else {
            None
        };
        let same_provider = previous.is_some_and(|previous| previous.provider == provider);
        let (access_token, refresh_token, token_expires_at) = if same_provider {
            let previous = previous.expect("checked");
            (
                previous.access_token.clone(),
                previous.refresh_token.clone(),
                previous.token_expires_at,
            )
        } else {
            (None, None, None)
        };
        Account {
            id,
            name: self.name.trim().to_string(),
            protocol: self.protocol,
            host: self.host.trim().to_string(),
            port: self.port(),
            username: self.username.trim().to_string(),
            password: self.password.clone(),
            tls: self.tls,
            smtp: if self.plugin.is_some() {
                // Submission belongs to the module; an outgoing server row
                // would be a second sender this account does not have.
                None
            } else {
                self.smtp()
            },
            provider,
            provider_config,
            access_token,
            refresh_token,
            token_expires_at,
        }
    }

    /// Whether saving this form reaches a different mailbox than `account`
    /// does. The endpoint — protocol, host, port, username — steers which
    /// mailbox is logged into, so a change there throws the cached mail
    /// away; the name is only a label and the password is a credential, so
    /// neither disturbs the cache.
    fn points_elsewhere(&self, account: &Account) -> bool {
        account.provider != self.plugin
            || account.protocol != self.protocol
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

    pub fn new(
        ctx: &egui::Context,
        plugins: std::sync::Arc<crate::plugin::discovery::Registry>,
    ) -> Self {
        // egui's built-in fonts only cover Latin/Cyrillic/Greek, so the CJK
        // fallback goes in immediately; the families picked in Preferences are
        // applied once the worker scan below has resolved their files.
        crate::fonts::install(ctx, None, None, None);

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
        let app = Self::assemble(ctx, accounts, mailboxes, &settings, plugins);

        // System-tray icon (best effort; StatusNotifierItem over D-Bus).
        crate::tray::spawn(app.ctx.clone());

        // Font families for Preferences → Appearance. The scan reads every
        // installed face's name table, so it runs on a worker thread; the
        // picked families are applied when the result arrives.
        sync::spawn(Job::ScanFonts, app.ctx.clone(), app.tx.clone());

        // The one sync nobody asks for: every account is swept at startup, so
        // the list is what the server has rather than what the last session
        // left. The title-bar button asks for the same sweep whenever it is
        // wanted, and a query nobody made is still nobody's — the automatic
        // sync on a clock is the preference, and it is off unless asked for.
        for account in app.accounts.clone() {
            sync::spawn(
                Job::Sync {
                    account,
                    purge: false,
                    folder: None,
                    save: false,
                },
                app.ctx.clone(),
                app.tx.clone(),
            );
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
        plugins: std::sync::Arc<crate::plugin::discovery::Registry>,
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
            // The column opens on the folder list, which is what an empty query
            // answers: the initial query counts as the applied one, so the first
            // frame is not a query *change* — nothing to search, and nothing to
            // drop a selection over.
            search_applied: Some(SearchQuery::default()),
            search_hits: Vec::new(),
            search_running: false,
            search_error: None,
            show_search: false,
            search_focus: false,
            compose: None,
            compose_spot: ComposeSpot::Popup,
            compose_anchor: None,
            show_preferences: false,
            prefs_tab: PrefsTab::Accounts,
            plugins,
            disabled_plugins: settings.disabled_plugins.iter().cloned().collect(),
            show_account_form: false,
            account_form: AccountForm::default(),
            confirm_delete: None,
            nav_collapsed: false,
            // The list's remembered width starts at its default; the first
            // frame adopts whatever egui has stored, and every frame after
            // that follows the splitter (see `ui`).
            list_width: LIST_DEFAULT_WIDTH,
            list_capped: false,
            pane_popup: false,
            list_scrolls: false,
            collapsed_accounts: HashSet::new(),
            was_narrow: false,
            folder_icons,
            app_icon: None,
            glass: Glass::default(),
            theme: Theme::parse(&settings.theme),
            // A file that names no accent, or one no accent answers to.
            accent: Accent::parse(settings.accent.as_deref().unwrap_or_default()),
            system_dark: system_prefers_dark(),
            storage_warning: crate::db::warning(),
            account_error: None,
            plugin_signing_in: None,
            notice: None,
            images: HashMap::new(),
            stylesheets: HashMap::new(),
            inline_textures: HashMap::new(),
            html_renderer: HtmlRenderer::parse(settings.html_renderer.as_deref().unwrap_or("")),
            // Absent in settings.json means the app's own defaults: nothing
            // syncs on a timer until it is asked to, and `AUTO_SYNC_MINUTES`
            // apart when it is.
            auto_sync: settings.auto_sync.unwrap_or(false),
            auto_sync_every: auto_sync_interval(settings.auto_sync_minutes),
            auto_synced_at: Instant::now(),
            // And here too: a file that says nothing about notifications gets
            // them, because that is what the app is for.
            notifications: settings.notifications.unwrap_or(true),
            // Minimize stays the window manager's until the reader asks for
            // the tray behaviour.
            minimize_to_tray: settings.minimize_to_tray.unwrap_or(false),
            // Closing quits until the reader asks it to hide instead.
            close_to_tray: settings.close_to_tray.unwrap_or(false),
            // All presets on unless the reader switched one off; a key this
            // build does not know is ignored, and Custom cannot be switched.
            disabled_vendors: settings
                .disabled_vendors
                .iter()
                .filter_map(|key| VendorKey::parse(key))
                .filter(|key| *key != VendorKey::Custom)
                .collect(),
            // Settled on the first frame, from the mail the database loaded.
            tray_badge: false,
            font_families: None,
            ui_font: settings.ui_font.clone(),
            mono_font: settings.mono_font.clone(),
            cjk_font: settings.cjk_font.clone(),
            cjk_missing: None,
            gpu: settings.gpu.clone(),
            gpu_scan: None,
            gpu_scanning: false,
            blitz_pages: HashMap::new(),
            blitz_order: Vec::new(),
            blitz_cancel: crate::blitz_render::Cancellation::default(),
            pending_blitz_key: None,
            blitz_paper: HashMap::new(),
            body: BodyCache::default(),
            scrolled_message: None,
            body_menu: None,
            body_menu_open: false,
            list_column: egui::Rect::ZERO,
            list_scroll: 0.0,
            nav_scroll: 0.0,
            mail_scroll: 0.0,
            // Absent in settings.json means the app's own default: the list
            // groups mail into conversations.
            threaded: settings.threaded.unwrap_or(true),
            open_threads: HashSet::new(),
            open_rev: 0,
            picking_files: false,
            list_filters: ListFilters::default(),
            list_sort: ListSort::default(),
            list_rows: None,
            select_mode: false,
            selected_mails: HashSet::new(),
            // The labels are the database's; the filter and the popup's box are
            // this session's, and start empty.
            labels: crate::db::load_labels().unwrap_or_default(),
            label_filter: Vec::new(),
            label_view: None,
            label_draft: String::new(),
            labels_rev: 0,
        };

        app.selected_account = app.accounts.first().map(|a| a.id.clone());
        app.selected_folder = Some("INBOX".to_string());

        app
    }

    // ----- event plumbing ---------------------------------------------------

    // ----- actions ----------------------------------------------------------

    // ----- queries ----------------------------------------------------------

    // ----- Blitz pages ------------------------------------------------------

    /// Write the current UI settings back to `settings.json`.
    fn persist_settings(&self) {
        let _ = storage::save_settings(&Settings {
            theme: self.theme.as_str().to_string(),
            accent: Some(self.accent.key.to_string()),
            html_renderer: Some(self.html_renderer.as_str().to_string()),
            ui_font: self.ui_font.clone(),
            mono_font: self.mono_font.clone(),
            cjk_font: self.cjk_font.clone(),
            threaded: Some(self.threaded),
            auto_sync: Some(self.auto_sync),
            auto_sync_minutes: Some((self.auto_sync_every.as_secs() / SECS_PER_MINUTE) as u32),
            notifications: Some(self.notifications),
            minimize_to_tray: Some(self.minimize_to_tray),
            close_to_tray: Some(self.close_to_tray),
            disabled_vendors: self
                .disabled_vendors
                .iter()
                .map(|key| key.as_str().to_string())
                .collect(),
            disabled_plugins: self.disabled_plugins.iter().cloned().collect(),
            gpu: self.gpu.clone(),
        });
    }

    /// Whether a provider preset is offered: Custom always is, a vendor only
    /// while it is switched on in Preferences → Accounts.
    fn vendor_enabled(&self, key: VendorKey) -> bool {
        vendor::enabled(key, &self.disabled_vendors)
    }

    /// The plugin providers an account can run through: every loaded
    /// module's provider contributions, id and display name. This list is
    /// the whole of adding a provider to the form — the Outlook module
    /// lands here as its own entry the day it exists.
    fn plugin_providers(&self) -> Vec<(String, String)> {
        self.plugins
            .plugins()
            .iter()
            .flat_map(|loaded| {
                loaded.manifest.providers.iter().map(|contribution| {
                    (
                        contribution.id.clone(),
                        contribution
                            .name
                            .clone()
                            .unwrap_or_else(|| contribution.id.clone()),
                    )
                })
            })
            .collect()
    }

    /// The presets the account form offers, in the picker's order.
    fn enabled_vendors(&self) -> Vec<VendorKey> {
        VendorKey::ALL
            .iter()
            .copied()
            .filter(|key| self.vendor_enabled(*key))
            .collect()
    }

    /// The vendor whose policy applies to an account: Custom when the preset
    /// its endpoints name is switched off, so a disabled vendor behaves like a
    /// plain server — no preset in the form, no Sent-mailbox preference.
    fn effective_vendor(&self, account: &Account) -> VendorKey {
        vendor::effective_key(VendorKey::of(account), &self.disabled_vendors)
    }

    /// Re-install the egui fonts from the picked family names. Reads one file
    /// per pick, which is cheap; the directory scan behind
    /// `font_families` is a worker job.
    fn apply_fonts(&mut self) {
        let families = self.font_families.as_deref().unwrap_or_default();
        let pick = |name: &Option<String>| {
            name.as_deref()
                .and_then(|name| crate::fonts::find(families, name))
                .cloned()
        };
        let ui = pick(&self.ui_font);
        let mono = pick(&self.mono_font);
        let cjk = pick(&self.cjk_font);
        // The coverage note is about the face that is actually installed, not
        // about the name in settings.json: a pick that no longer resolves falls
        // through to the automatic face (see `fonts::install`).
        self.cjk_missing = cjk.as_ref().map(|family| {
            (
                family.name.clone(),
                crate::fonts::missing_cjk_samples(family),
            )
        });
        crate::fonts::install(&self.ctx, ui.as_ref(), mono.as_ref(), cjk.as_ref());
    }

    /// Apply the selected theme to the context, resolving "system" against
    /// the cached OS preference and putting the chosen accent into the scheme
    /// it resolves to. The result is what the hand-painted parts of the UI
    /// read back through `theme::of`.
    fn apply_theme(&self, ctx: &egui::Context) {
        theme::install(ctx, self.scheme());
    }

    /// The scheme in force: the theme the reader picked, resolved against the
    /// OS preference where it says "system", with their accent in it. The
    /// context (`apply_theme`) and the Appearance page's preview both read it
    /// from here, so what the preview shows is what the app draws with.
    fn scheme(&self) -> ColorScheme {
        self.theme.scheme(self.system_dark).with_accent(self.accent)
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

    /// What the reading pane shows for the selected mail: the conversation it
    /// belongs to, oldest first, when the list is threading and its model covers
    /// the folder — or the mail on its own, which is what a folder whose model
    /// has not been derived yet, and threading turned off, get.
    ///
    /// The mails are copied because the pane draws them with `&mut self` (the
    /// body cache, the inline images) while they live in the account state; a
    /// conversation is a handful of mails, and their bodies are the only large
    /// part of them.
    fn open_conversation(&self) -> Option<Conversation> {
        let selected = self.selected_message_view()?;
        let alone = Conversation {
            key: selected.uid.clone(),
            messages: vec![selected],
        };
        if !self.threaded {
            return Some(alone);
        }
        let (Some(account), Some(folder)) = (
            self.selected_account.as_deref(),
            self.selected_folder.as_deref(),
        ) else {
            return Some(alone);
        };
        let Some(rows) = self
            .list_rows
            .as_ref()
            .filter(|rows| rows.account == account && rows.folder == folder)
        else {
            return Some(alone);
        };
        let Some(messages) = self.folder_messages_of(account, folder) else {
            return Some(alone);
        };
        let uid = self.selected_message.as_deref().unwrap_or_default();
        let Some(thread) = rows
            .threads
            .iter()
            .find(|thread| thread.messages.len() > 1 && thread.holds(messages, uid))
        else {
            return Some(alone);
        };
        Some(Conversation {
            key: thread.key.clone(),
            messages: thread
                .messages
                .iter()
                .map(|&index| messages[index].clone())
                .collect(),
        })
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

    /// Whether the bar draws one of the seven mail actions enabled.
    ///
    /// In view mode the answer is the mail the reading pane shows, the way it
    /// always was. **Select mode is the other answer**: reply, reply all,
    /// forward, label, flag and export are all about one mail — the one on the
    /// pane — and the list is what the click is about now, so they are drawn
    /// dimmed and sense nothing; Delete, which takes a whole selection, is the
    /// one left live, and only while something is checked.
    fn mail_action_enabled(&self, action: MailAction) -> bool {
        if self.select_mode {
            action == MailAction::Delete && !self.selected_mails.is_empty()
        } else {
            self.has_selected_message()
        }
    }

    // ----- UI ----------------------------------------------------------------
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
        // The badge is the Inbox's unread mail, so it is settled from the state
        // the frame's events left behind — not from what those events were.
        self.update_tray_badge();
        self.attach_dropped();
        self.apply_theme(ctx);
        // The automatic sync is a frame's business too: it is what asks for the
        // frame its clock needs, and what starts the sweep when it is up.
        let _ = self.pump_auto_sync(ctx);

        // The navigation is the fixed-width column, the message list the
        // semi-fixed one, and the reading pane the flexible remainder
        // (`plan_columns`).
        // A narrowing window gives way in one order: the navigation collapses
        // first, then the list is capped at what leaves the reading pane its
        // minimum, then the pane becomes a popup over the list, and the list
        // itself never narrows past its rows — a window narrower than that
        // scrolls it sideways. The thresholds are the columns' own widths
        // rather than constants: what the splitters left the two panels at,
        // plus the reading pane's minimum, is the total the three need, and
        // the navigation is the one column whose width the two content columns
        // can have for themselves.
        let window = ctx.viewport_rect().width();
        let nav_width = panel_width(ctx, "nav")
            .unwrap_or(NAV_DEFAULT_WIDTH)
            .max(NAV_MIN_WIDTH);
        let plan = plan_columns(window, nav_width, self.list_width, self.nav_collapsed);
        // Collapse the navigation when the window crosses into having less
        // than the three columns need; re-expanding is a manual choice and
        // stays until the next crossing.
        if plan.collapses_nav && !self.was_narrow {
            self.nav_collapsed = true;
        }
        self.was_narrow = plan.collapses_nav;
        self.pane_popup = plan.popup;
        self.list_scrolls = plan.list_scrolls;
        // The navigation records its offset again while it draws; clearing it
        // here is what keeps a column that is not there — the popup layout, or
        // a panel the slide has finished closing — from leaving the bar glass.
        self.nav_scroll = 0.0;

        // A compose opened on the spot stays in the pane while the mail it
        // answers is on it; once that mail has left — another folder opened,
        // the mail deleted — the draft moves to the popup window rather than
        // sitting in a pane it no longer belongs to (`compose_anchor_on_pane`).
        if self.compose.is_some()
            && self.compose_spot == ComposeSpot::Pane
            && !self.compose_anchor_on_pane()
        {
            self.compose_spot = ComposeSpot::Popup;
        }

        // The chrome — title bar, navigation, and the gutter the reading
        // pane's card floats in — is the scheme's panel color; the message list
        // and the card are the lighter content surface.
        let scheme = theme::of(ctx);
        // egui's panel separators are off in both schemes: the palette marks
        // the columns — the list's hairlines, and the card floating in the
        // chrome's gutter — so a separator would only double them.

        // No top panel: the title bar is an overlay (`titlebar_area`), so the
        // columns run from the window's top edge and each can scroll under the
        // bar. What each column keeps clear of the bar is its own business —
        // the list's top padding scrolls away with its rows (`LIST_PADDING`),
        // and the navigation and the reading pane put the bar's height inside
        // their scroll areas as space, so their content passes under the glass
        // as well.
        // The navigation is the fixed-width column: it takes the width its
        // splitter left it before either of the other two is laid out, and it
        // gives that width up — by sliding out — only when the window can no
        // longer hold the list at its own width beside the reading pane's
        // minimum. The slide runs on egui's own animation time
        // (`Style::animation_time`, 0.2 s), and the panel's stored width
        // survives it — a collapsed column reopens at the width the splitter
        // left it at, not at the width it slid to.
        //
        // Collapsing is the title-bar toggle's business (and the narrow-window
        // rule's, above): `drag_to_open(false)` keeps a collapsed column
        // from leaving an invisible grab strip — and a hover line — across the
        // message list's left edge, and the expanded flag the panel is handed
        // is a copy, so that a splitter drag past the minimum keeps clamping
        // there the way it always has instead of shutting the column by
        // itself. In the popup layout it is not shown at all: a window that
        // cannot hold the two content columns side by side has nothing left for
        // a third. The flag survives, and the column is back the moment the
        // window can hold it again.
        if !plan.popup {
            let mut expanded = !self.nav_collapsed;
            egui::Panel::left("nav")
                .resizable(true)
                .min_size(NAV_MIN_WIDTH)
                .default_size(NAV_DEFAULT_WIDTH)
                .show_separator_line(false)
                .drag_to_open(false)
                .show_collapsible(ui, &mut expanded, |ui| self.nav(ui));
        }

        // The message list is the semi-fixed column: it starts at the width
        // its splitter left it, and the splitter cannot take it past
        // `LIST_MAX_WIDTH`. The window caps it — rather than the reading pane
        // — at what leaves that pane its `CONTENT_MIN_WIDTH`. Capped, it is
        // placed at the cap outright rather than merely clamped by it: clamped,
        // egui would remember the narrowest width the cap reached and sit there
        // while the window widens. The cap is not a split the user can make —
        // the window is the one making it — so the splitter is off while it
        // holds. The width to restore to is the app's own (`list_width`),
        // because egui would otherwise only remember the cap.
        //
        // No padding on its frame: the column's padding lives inside its scroll
        // area (see folder_messages), so the scroll bar can sit on the column's
        // edge instead of on the padding. Its rect is recorded by `mail_list`
        // for the splitter repaint below, and its surface is the card
        // `mail_list` paints, as tall as the list it holds — so the panel
        // behind it is the chrome, which is what shows below a folder that does
        // not fill the window.
        if !plan.popup {
            let mut list = egui::Panel::left("mail_list")
                .resizable(true)
                .min_size(LIST_MIN_WIDTH)
                .default_size(LIST_DEFAULT_WIDTH)
                .max_size(LIST_MAX_WIDTH)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(scheme.panel));
            match plan.list_max {
                Some(cap) => {
                    set_panel_width(ctx, "mail_list", cap);
                    list = list.max_size(cap).resizable(false);
                    self.list_capped = true;
                }
                None => {
                    // The cap has lifted, and what egui remembers is the last
                    // width it capped at: back to the remembered one, which
                    // the frames below then keep following the splitter.
                    if self.list_capped {
                        set_panel_width(ctx, "mail_list", self.list_width);
                        self.list_capped = false;
                    }
                    if let Some(width) = panel_width(ctx, "mail_list") {
                        self.list_width = width.clamp(LIST_MIN_WIDTH, LIST_MAX_WIDTH);
                    }
                }
            }
            list.show(ui, |ui| self.mail_list(ui));
        }

        // The reading pane is the flexible column: it takes whatever the two
        // side columns leave. In the popup layout it is an area over the list
        // instead, and the list is the one column that fills the window
        // (`mail_content` draws both shapes, and the band with the back
        // button is its own).
        if !plan.popup {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.mail_content(ui));
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(scheme.panel))
                .show(ui, |ui| self.mail_list(ui));
        }
        let list_column = self.list_column;

        // The popup pane, over the list a click on one of its mails opened.
        // An area rather than a panel, and one shown after the list so it is
        // above it; the title bar is moved above it in turn — a new area is
        // otherwise born above the bar, and the window's own buttons belong
        // on top whatever the layout below them is. The modals stay above it
        // too (egui keeps them in the foreground layer).
        if plan.popup && self.open_conversation().is_some() {
            let rect = ctx.viewport_rect();
            egui::Area::new(egui::Id::new("mail-popup"))
                .order(egui::Order::Middle)
                .fixed_pos(rect.min)
                .movable(false)
                .show(ctx, |ui| {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        self.mail_content(ui);
                    });
                });
            ctx.move_to_top(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("titlebar"),
            ));
        } else if plan.popup {
            // Nothing is open over the list, and the pane's placeholder —
            // the thing that resets its scroll offset — never draws in this
            // layout. Reset it here, so the bar is not glass over a list
            // that never scrolled.
            self.mail_scroll = 0.0;
        }

        // egui paints a brighter line along a panel boundary while its splitter
        // is hovered or dragged, and this UI wants the splitters invisible: put
        // those two pixels back to the chrome every frame, so a hovered
        // splitter looks exactly like a resting one. Painted after the panels,
        // so it lands on top of whatever they drew there; the list's own
        // hairlines sit a pixel inside the column and are untouched. The popup
        // layout has no splitters — its one boundary is the window's edge.
        if !plan.popup {
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
        }

        // The title bar, over the columns it belongs to: the list scrolls
        // under it and it is glass while it does (`titlebar_area`). Drawn
        // before the compose window, so a draft floats over it, and before
        // the modals, which egui keeps in the foreground layer.
        self.titlebar_area(ctx);
        // The window's own resize border: the frame has no decorations, so
        // these strips are the only pointer way to resize it
        // (`window_resize_grips`).
        self.window_resize_grips(ctx);

        // The compose modal: over the columns and the bar, and under any modal
        // opened after it (egui stacks the foreground layer in the order its
        // areas are shown). Only the popup spot has a window of its own —
        // the pane spot is a card of the pane (`compose_inline_card`).
        if self.compose.is_some() && self.compose_spot == ComposeSpot::Popup {
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

/// What the chip run keeps clear of the head row above it.
const LABEL_CHIPS_TOP_GAP: f32 = 6.0;

/// What the three columns become at one window width — the plan a frame's
/// layout is drawn from (`KimoApp::ui`). The navigation has a fixed
/// width, the message list is semi-fixed (a remembered width, capped by the window
/// and by its own maximum), and the reading pane is the remainder: it is
/// the column that grows when the window does.
///
/// A narrowing window gives way in one order: the navigation collapses
/// first, then the list is capped at what leaves the reading pane its
/// minimum, then the pane stops being a column at all and becomes a popup
/// over the list, and the list itself never gets narrower than its rows can
/// read — it scrolls sideways instead.
///
/// `nav` and `list` are the widths to remember the two resizable columns
/// by, what their splitters left them at; `nav_collapsed` says whether the
/// navigation is taking room right now. The pane is the remainder, so the
/// one minimum it has is enforced here rather than by a panel of its own.
struct ColumnPlan {
    /// The reading pane is a popup over the list rather than a column beside
    /// it: not even the two minimums fit side by side. A click on a mail
    /// opens it, its back button closes it (`mail_content`).
    popup: bool,
    /// The widest the message list may be this frame: `None` when it keeps
    /// its remembered width, and the room beside the reading pane's minimum
    /// when the window has less than that for it.
    list_max: Option<f32>,
    /// The window is narrower than the list's minimum: the list keeps the
    /// minimum and scrolls sideways rather than crushing its rows.
    list_scrolls: bool,
    /// The window has less than the three columns need with the list at its
    /// remembered width: the navigation gives way. This is the crossing
    /// `KimoApp::ui` collapses it on — re-expanding stays a manual choice
    /// until the next one.
    collapses_nav: bool,
}

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
    /// Room for the bar's own content: the first group, the search box, the
    /// mail actions and the drag surface. Empty — never inside out — when the
    /// sync cell, the cog and the window controls are all the bar can hold.
    /// Where the two middle groups go inside it is not this rect's business:
    /// they are anchored to the columns below the bar (`titlebar_anchors`).
    content: egui::Rect,
    /// **Sync now**, immediately left of the cog: the one cell of the bar that
    /// is about the whole app rather than about the mail the pane shows, which
    /// is why it stands with the cog instead of with the left group.
    sync: egui::Rect,
    /// The preferences cog, immediately left of the window controls.
    cog: egui::Rect,
    /// Minimize / maximize / close, ending at the window's right edge.
    controls: egui::Rect,
}

/// Where the bar's two column-anchored groups go.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TitlebarAnchors {
    /// The search box's left edge: the message list's own left edge, clamped
    /// clear of the first group — a collapsed navigation column puts the list at
    /// the window's edge, and the box may not land on that group.
    search: f32,
    /// How wide the plan makes the box (`titlebar_search_width`), or `None` when
    /// the bar has no room for it at all.
    search_width: Option<f32>,
    /// The filter cell beside the box — they are one group, a bar's own cell
    /// apart — or `None` when the room the box leaves is too little for it.
    filter: Option<egui::Rect>,
    /// The sort cell after the filter, or `None` when the room ran out before
    /// it: the two are given up in the order they are drawn in.
    sort: Option<egui::Rect>,
    /// The mail actions' rect — the reading pane's own row, as wide as the run —
    /// or `None` when the pane has no room for it beside the window controls.
    actions: Option<egui::Rect>,
}

/// One of the window's eight resize grips: the strip (or corner square) that
/// answers a press with a `BeginResize` for `direction`, and the cursor the
/// pointer wears while it is on one.
#[derive(Clone, Copy, Debug)]
struct ResizeGrip {
    rect: egui::Rect,
    direction: egui::viewport::ResizeDirection,
    cursor: egui::CursorIcon,
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

/// What to do about a page that is not cached yet.
#[derive(Debug, PartialEq, Eq)]
enum BlitzWait {
    /// The geometry has held still for `GEOMETRY_SETTLE`: render now.
    Start,
    /// Still settling; look again after this long.
    Wait(Duration),
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
    /// The row takes the scheme's selection fill: the mail open in the reading
    /// pane, or — in select mode — one whose box is checked.
    selected: bool,
    /// The mail is flagged, so the row takes the scheme's warning fill — unless
    /// it is selected or under the pointer, whose fills are stronger and are
    /// painted over it.
    flagged: bool,
    /// The mail has parts, so the row draws a paperclip at the end of its
    /// title line. Both lists can answer it: a folder's mail from what was
    /// stored with it, a search hit from what the query counted.
    has_attachments: bool,
    /// Where the row sits in the list: a mail on its own, a conversation's own
    /// row, or one mail of an open conversation.
    place: RowPlace<'a>,
    /// What the row's checkbox says, or `None` in view mode — where the avatar
    /// stands and the pointer alone brings an empty box over the row it is on.
    /// `Some` is select mode, and in it every row carries a box.
    checkbox: Option<CheckState>,
    /// The widget id the box's click target is registered under: one per row,
    /// stable between frames the way the chevron's key is.
    checkbox_id: egui::Id,
    /// The widget id of the row's own delete button — the one it offers under
    /// the pointer — one per row like the box's.
    delete_id: egui::Id,
    /// The widget id of the row's own flag button, which sits just inside the
    /// delete cell: a target on every row like the two others.
    flag_id: egui::Id,
}

/// What a row's checkbox says about the mail the row stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CheckState {
    /// Nothing the row stands for is checked.
    Off,
    /// Some of what it stands for is: a conversation whose mails were checked
    /// one by one in the list under it.
    Partial,
    /// All of it is.
    On,
}

/// Where a card sits in the message list, which is what it draws around
/// itself: a mail on its own, a conversation's own row (inset, with the chevron
/// that opens it and the count of what it holds), or one mail of an open
/// conversation (a step further in, with a smaller avatar, and nothing drawn
/// down the chevron's column).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowPlace<'a> {
    Plain,
    /// `open` is whether the thread's mails are under it, `count` how many
    /// there are, and `key` what the chevron's own click target is identified
    /// by — an id that moves between frames would lose the pointer state.
    Thread {
        open: bool,
        count: usize,
        key: &'a str,
    },
    InThread,
}

impl RowPlace<'_> {
    /// How far the row's avatar and text are inset from the column's edge. Every
    /// row is inset, a conversation's and a mail's alike: the chevron's column
    /// is reserved on all of them, so the avatars line up down the whole list,
    /// and a mail with nothing to open leaves that column empty rather than
    /// starting a step further left than its neighbours. A mail of an *open*
    /// conversation steps in one more (`THREAD_STEP`): the step, with the
    /// smaller avatar it draws, is what says the mail belongs to the
    /// conversation above it — there is no guide line to say it.
    fn indent(self) -> f32 {
        match self {
            RowPlace::InThread => THREAD_INDENT + THREAD_STEP,
            _ => THREAD_INDENT,
        }
    }

    /// The avatar the row draws its sender in — the diameter of the circle and
    /// the size of the initial on it — a smaller one for a mail of an open
    /// conversation (see `indent`).
    fn avatar(self) -> (f32, f32) {
        match self {
            RowPlace::InThread => (THREAD_AVATAR_DIAMETER, THREAD_AVATAR_FONT_SIZE),
            _ => (AVATAR_DIAMETER, AVATAR_FONT_SIZE),
        }
    }
}

/// What one row's clicks were: the row itself, the checkbox that stands in the
/// avatar's place, and — for a conversation — the chevron that opens and closes
/// it. The three are separate targets, which is what keeps one click from being
/// another: the row opens the conversation in the reading pane (or, in select
/// mode, leaves it), the chevron opens the mails in the list, and the box
/// checks the mail the row stands for.
struct RowResponse {
    row: egui::Response,
    chevron: Option<egui::Response>,
    /// The box, on every row: it is a target whether or not it is drawn — see
    /// `message_card`.
    checkbox: egui::Response,
    /// The row's own delete button: a target on every row like the box, drawn
    /// when the pointer is on the row (see `message_card`).
    delete: egui::Response,
    /// The row's own flag button, just inside the delete cell: the title bar's
    /// flag action — the one mail action its head leaves to the bar — offered
    /// where the mail is listed.
    flag: egui::Response,
}

/// One of the app's text fields: an `egui::TextEdit` in Kimo's own box.
///
/// The box is drawn here rather than by egui for the reason `field_frame`
/// gives: the outline a *focused* field wears is the one thing the app wants in
/// its own colour — the reader's accent — and egui's slot for that line is the
/// selected-text colour. So the field is added with a frame that draws nothing,
/// and the box is painted around the field's own rect instead: the theme's
/// input fill under the text, and over it the border — or the accent, with the
/// keyboard in the field. It is a line like every other box in the app:
/// `FIELD_STROKE` wide, drawn inside the box, in the corner radius of the
/// field's own state.
///
/// **What the box is**, per `FieldBox`: the form's own, the quiet one whose
/// line is only there while the pointer or the keyboard is on the field (the
/// compose form's address lines and Subject — `docs/ui-style.md`), or none at
/// all (the answer's own lines under their mail, and the writing area, whose box
/// is the room the card gives it rather than the field's own height —
/// `compose_body`).
struct Field<'t> {
    edit: egui::TextEdit<'t>,
    enabled: bool,
    boxed: FieldBox,
    /// Where the text sits inside the box: `FIELD_MARGIN` unless the caller
    /// needs room for a glyph drawn in there with it — the search bar's
    /// magnifier and clear cross (`SEARCH_BAR_MARGIN`). It is the *frame's*
    /// margin rather than `TextEdit::margin`, because a field given a frame of
    /// its own ignores that one: egui builds its frame out of `margin` only
    /// when the caller hands it none, and `Field` hands it `field_frame`.
    margin: egui::Margin,
}

/// How a field draws its box (`Field`). The line is Kimo's in every case that
/// has one: the theme's border while a form's field waits, the reader's accent
/// while the pointer or the keyboard is on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldBox {
    /// The form's box: the input fill under the text, the theme's resting line
    /// around it, the accent while the keyboard is in the field.
    Boxed,
    /// The quiet box: the fill, and no line until the pointer or the keyboard
    /// is on the field — then the accent, and nothing again once they leave.
    Quiet,
    /// No box: a line of a letter rather than a form's.
    Flat,
}

impl<'t> Field<'t> {
    fn new(edit: egui::TextEdit<'t>) -> Self {
        Self {
            edit,
            enabled: true,
            boxed: FieldBox::Boxed,
            margin: FIELD_MARGIN,
        }
    }

    /// The field whose text keeps `margin` room inside the box, for a glyph
    /// drawn in there with it (`SEARCH_BAR_MARGIN`).
    fn margin(mut self, margin: egui::Margin) -> Self {
        self.margin = margin;
        self
    }

    /// The field a worker owns for the moment: greyed out, and no longer taking
    /// the keyboard (`Ui::add_enabled`).
    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The field whose line comes out with the pointer or the keyboard
    /// (`FieldBox::Quiet`): the compose form's address lines.
    fn quiet(mut self) -> Self {
        self.boxed = FieldBox::Quiet;
        self
    }

    /// The field without a box: a line of a letter rather than a form's.
    fn flat(mut self) -> Self {
        self.boxed = FieldBox::Flat;
        self
    }
}

impl egui::Widget for Field<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let Self {
            edit,
            enabled,
            boxed,
            margin,
        } = self;
        let edit = edit.frame(field_frame(margin));
        if boxed == FieldBox::Flat {
            return ui.add_enabled(enabled, edit);
        }
        // A slot under the text for the box: the box's rect is the field's own,
        // known only once the field has been laid out, and its fill has to be
        // under the text rather than over it.
        let slot = ui.painter().add(egui::Shape::Noop);
        let field = ui.add_enabled(enabled, edit);
        let scheme = theme::of(ui.ctx());
        let visuals = ui.style().interact(&field);
        // The line per `FieldBox`: the form's waits with the theme's own border
        // and takes the accent with the keyboard; the quiet one is only ever the
        // pointer's or the keyboard's, and there is none while neither is on it.
        let stroke = if field.has_focus() || (boxed == FieldBox::Quiet && field.hovered()) {
            egui::Stroke::new(FIELD_STROKE, scheme.accent)
        } else if boxed == FieldBox::Quiet {
            egui::Stroke::NONE
        } else {
            visuals.bg_stroke
        };
        ui.painter().set(
            slot,
            egui::Shape::Rect(egui::epaint::RectShape::new(
                field.rect,
                visuals.corner_radius,
                scheme.input,
                stroke,
                egui::StrokeKind::Inside,
            )),
        );
        field
    }
}

/// What an attachment card's click targets came to: the card itself, and — where
/// the card is on the compose form — the small cross in its corner, which is the
/// same thing said out loud rather than left to a click on the card's body.
struct AttachmentCard {
    card: egui::Response,
    close: Option<egui::Response>,
}

/// What one account row shows beyond its name and the account it belongs to:
/// the unread total, whether its folders are folded, and the state of its own
/// sync.
struct AccountRowState {
    unread: usize,
    collapsed: bool,
    syncing: bool,
    failed: bool,
}

/// What one account row's clicks were: the row (which folds the section) and
/// the sync cell at its end (which syncs that account alone).
struct AccountRowResponse {
    row: egui::Response,
    sync: egui::Response,
}

/// What a folder row wears at its left edge: the icon of its kind, or the
/// label glyph a Gmail label's synthetic folder shares with the label group.
enum RowIcon<'a> {
    Texture(&'a egui::TextureHandle),
    Label,
}

// Title bar window-control glyphs, painted as vector shapes so they render
// identically regardless of which fonts are installed.

// The title bar's mail-action glyphs — reply, reply all, forward, label, flag,
// export, delete — painted as vector shapes from their own 24×24 outlines (the
// box the icon set the folder icons come from is drawn in), so they render
// identically regardless of which fonts are installed.

/// What one row of the bar's two menus draws at its left. The filter's marks say
/// what a view is about, the sort's say which way the list runs and by what —
/// and where the list already draws the same thing somewhere (the flag, the
/// paperclip), it is the same function, so the menu and the list say it with one
/// shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuIcon {
    All,
    Unread,
    Flagged,
    ToMe,
    HasFiles,
    /// An arrow down: the list runs from the newest mail at the top to the
    /// oldest at the foot, which is the sort direction the whole world draws
    /// this way round.
    Newest,
    /// And up, for the other way.
    Oldest,
    ByDate,
    ByFrom,
    BySubject,
}

impl ListFilter {
    fn icon(self) -> MenuIcon {
        match self {
            ListFilter::Unread => MenuIcon::Unread,
            ListFilter::Flagged => MenuIcon::Flagged,
            ListFilter::ToMe => MenuIcon::ToMe,
            ListFilter::HasFiles => MenuIcon::HasFiles,
        }
    }
}

impl SortOrder {
    fn icon(self) -> MenuIcon {
        match self {
            SortOrder::Newest => MenuIcon::Newest,
            SortOrder::Oldest => MenuIcon::Oldest,
        }
    }
}

impl SortBy {
    fn icon(self) -> MenuIcon {
        match self {
            SortBy::Date => MenuIcon::ByDate,
            SortBy::From => MenuIcon::ByFrom,
            SortBy::Subject => MenuIcon::BySubject,
        }
    }
}

#[cfg(test)]
mod tests;
